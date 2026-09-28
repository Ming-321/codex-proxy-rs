mod sessions;

use std::{
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

use gateway_core::{
    engine::{nested::ExecutionEffects, upstream_adapter::UpstreamAccountConnection},
    upstream::UpstreamSendState,
};
use gateway_host::outbound::{HttpRequest, ManagedWebSocket, WebSocketMessage};
use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::{
        host as wire,
        upstream_adapter::{self as adapter, UpstreamPathPurpose},
    },
};

use super::{denied, http::HttpCallbacks, invalid};
use crate::{
    RpcReply,
    adapter::upstream_adapter::target::{UpstreamTarget, protected_header},
};

pub(crate) use sessions::{ConnectionOwner, ConnectionPool};

#[derive(Default)]
pub(crate) struct SendWatermark(AtomicU8);

impl SendWatermark {
    pub(crate) fn observe(&self, state: UpstreamSendState) {
        self.0.fetch_max(
            match state {
                UpstreamSendState::NotSent => 0,
                UpstreamSendState::Sent => 1,
                UpstreamSendState::Ambiguous => 2,
            },
            Ordering::AcqRel,
        );
    }

    pub(crate) fn get(&self) -> UpstreamSendState {
        match self.0.load(Ordering::Acquire) {
            0 => UpstreamSendState::NotSent,
            1 => UpstreamSendState::Sent,
            _ => UpstreamSendState::Ambiguous,
        }
    }
}

pub(crate) struct ManagedUpstream {
    pub(crate) account: Arc<dyn UpstreamAccountConnection>,
    pub(crate) send_state: Arc<SendWatermark>,
    pub(crate) effects: Option<Arc<ExecutionEffects>>,
    target: Arc<UpstreamTarget>,
    socket: tokio::sync::Mutex<Option<(String, ManagedWebSocket)>>,
    pool: Arc<ConnectionPool>,
    owner: ConnectionOwner,
    websocket_allowed: bool,
}

impl ManagedUpstream {
    pub(crate) fn new(
        account: Arc<dyn UpstreamAccountConnection>,
        target: Arc<UpstreamTarget>,
        effects: Option<Arc<ExecutionEffects>>,
        pool: Arc<ConnectionPool>,
        owner: ConnectionOwner,
        connection: Option<&str>,
        websocket_allowed: bool,
    ) -> Result<Arc<Self>, PluginFault> {
        let socket = connection.map(|id| pool.take(id, &owner)).transpose()?;
        Ok(Arc::new(Self {
            account,
            target,
            effects,
            pool,
            owner,
            websocket_allowed,
            send_state: Arc::default(),
            socket: tokio::sync::Mutex::new(socket),
        }))
    }

    pub(crate) async fn retain_connection(&self) -> Result<String, PluginFault> {
        let socket = self.socket.lock().await.take().ok_or_else(denied)?;
        self.pool.put(self.owner.clone(), socket)
    }

    fn request(
        &self,
        request: adapter::UpstreamHttpRequest,
        body: Vec<u8>,
    ) -> Result<(HttpRequest, UpstreamPathPurpose), PluginFault> {
        let (target, purpose) = self.target.resolve(&request.path, &request.query)?;
        if request.headers.len() > 128
            || request
                .headers
                .iter()
                .any(|(name, _)| protected_header(name))
        {
            return Err(denied());
        }
        let authorization = self.account.authorization().map_err(|_| {
            PluginFault::new(
                ErrorCode::Rejected,
                "selected account authentication is unavailable",
            )
        })?;
        if request.headers.iter().any(|(name, _)| {
            authorization
                .iter()
                .any(|header| name.eq_ignore_ascii_case(header.name()))
        }) {
            return Err(denied());
        }
        let headers = request
            .headers
            .into_iter()
            .map(|(name, value)| (name, value.into_bytes()))
            .chain(authorization.into_iter().map(|header| {
                let (name, value) = header.into_parts();
                (name, value.to_vec())
            }))
            .collect();
        Ok((
            HttpRequest {
                method: request.method,
                url: target.into(),
                headers,
                body,
            },
            purpose,
        ))
    }
}

pub(super) async fn dispatch(
    http: &HttpCallbacks<'_>,
    method: &str,
    params: serde_json::Value,
    payload: Vec<u8>,
) -> Result<RpcReply, PluginFault> {
    let HttpCallbacks {
        client,
        authorization,
        scope,
        call,
        maximum_payload,
    } = *http;
    let managed = scope.upstream.as_ref().ok_or_else(denied)?;
    let timeout = || {
        call.deadline
            .checked_duration_since(tokio::time::Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .map(|remaining| remaining.min(Duration::from_secs(120)))
            .ok_or_else(|| PluginFault::new(ErrorCode::Timeout, "upstream deadline elapsed"))
    };
    match method {
        "host.upstream.http.do" | "host.upstream.http.do_stream" => {
            let request = serde_json::from_value(params).map_err(|_| invalid())?;
            let (request, purpose) = managed.request(request, payload)?;
            http.send(request, method.ends_with("do_stream"), Some(purpose))
                .await
        }
        "host.upstream.http.stream_read" | "host.upstream.http.stream_close" => {
            http.dispatch(
                if method.ends_with("stream_read") {
                    "host.http.stream_read"
                } else {
                    "host.http.stream_close"
                },
                params,
                payload,
            )
            .await
        }
        "host.upstream.websocket.open" => {
            if !payload.is_empty() || !managed.websocket_allowed {
                return Err(denied());
            }
            let request: adapter::UpstreamWebSocketRequest =
                serde_json::from_value(params).map_err(|_| invalid())?;
            let (request, purpose) = managed.request(
                adapter::UpstreamHttpRequest {
                    method: "GET".into(),
                    path: request.path,
                    query: request.query,
                    headers: request.headers,
                },
                vec![],
            )?;
            if purpose != UpstreamPathPurpose::Inference {
                return Err(denied());
            }
            let mut socket = managed.socket.try_lock().map_err(|_| denied())?;
            if let Some((url, _)) = socket.as_ref() {
                if *url != request.url {
                    return Err(denied());
                }
                return Ok(RpcReply {
                    result: serde_json::json!({"status":101,"headers":[],"stream":null}),
                    payload: vec![],
                });
            }
            let url = request.url.clone();
            let attempt = scope.start_upstream(Some(UpstreamPathPurpose::Inference));
            let response = client
                .open_websocket(
                    request,
                    managed.account.outbound_proxy(),
                    &authorization.network,
                    timeout()?,
                )
                .await;
            // 握手也已向上游发送账号身份；失败不能伪装成未出站或跳过账号反馈。
            attempt.finish(
                response
                    .as_ref()
                    .map_or_else(|error| error.send_state, |_| UpstreamSendState::Sent),
            );
            let mut response = response.map_err(super::http::http_error)?;
            let headers = response
                .headers
                .into_iter()
                .map(|(name, value)| String::from_utf8(value).map(|value| (name, value)))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| invalid())?;
            let mut payload = Vec::new();
            if let Some(body) = response.body.as_mut() {
                while let Some(chunk) = body
                    .read(64 * 1024)
                    .await
                    .map_err(super::http::http_error)?
                {
                    if payload.len() + chunk.len() > maximum_payload {
                        return Err(invalid());
                    }
                    payload.extend_from_slice(&chunk);
                }
            }
            if let Some(connection) = response.connection {
                *socket = Some((url, connection));
            }
            Ok(RpcReply {
                result: serde_json::to_value(wire::HttpResponse {
                    status: response.status,
                    headers,
                    stream: None,
                })
                .map_err(|_| invalid())?,
                payload,
            })
        }
        "host.upstream.websocket.send" => {
            let message: adapter::UpstreamWebSocketMessage =
                serde_json::from_value(params).map_err(|_| invalid())?;
            let message = match message.kind {
                adapter::WebSocketMessageKind::Text => {
                    WebSocketMessage::Text(String::from_utf8(payload).map_err(|_| invalid())?)
                }
                adapter::WebSocketMessageKind::Binary => WebSocketMessage::Binary(payload.into()),
            };
            let mut socket = managed.socket.try_lock().map_err(|_| denied())?;
            let (_, socket) = socket.as_mut().ok_or_else(denied)?;
            let attempt = scope.start_upstream(Some(UpstreamPathPurpose::Inference));
            let result = socket.send(message, timeout()?).await;
            attempt.finish(
                result
                    .as_ref()
                    .map_or_else(|error| error.send_state, |()| UpstreamSendState::Sent),
            );
            result.map_err(super::http::http_error)?;
            Ok(RpcReply {
                result: serde_json::json!({}),
                payload: vec![],
            })
        }
        "host.upstream.websocket.read" => {
            if params != serde_json::json!({}) || !payload.is_empty() {
                return Err(invalid());
            }
            let mut socket = managed.socket.try_lock().map_err(|_| denied())?;
            let (_, connection) = socket.as_mut().ok_or_else(denied)?;
            let result = connection
                .read(timeout()?)
                .await
                .map_err(super::http::http_error)?;
            let (kind, payload) = match result {
                Some(WebSocketMessage::Text(text)) => {
                    (Some(adapter::WebSocketMessageKind::Text), text.into_bytes())
                }
                Some(WebSocketMessage::Binary(bytes)) => {
                    (Some(adapter::WebSocketMessageKind::Binary), bytes.to_vec())
                }
                None => {
                    socket.take();
                    (None, vec![])
                }
            };
            if payload.len() > maximum_payload {
                socket.take();
                return Err(invalid());
            }
            Ok(RpcReply {
                result: serde_json::to_value(adapter::UpstreamWebSocketRead {
                    eof: kind.is_none(),
                    kind,
                })
                .map_err(|_| invalid())?,
                payload,
            })
        }
        "host.upstream.websocket.close" => {
            if params != serde_json::json!({}) || !payload.is_empty() {
                return Err(invalid());
            }
            managed.socket.try_lock().map_err(|_| denied())?.take();
            Ok(RpcReply {
                result: serde_json::json!({}),
                payload: vec![],
            })
        }
        _ => Err(denied()),
    }
}
