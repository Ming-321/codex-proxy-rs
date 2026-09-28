use std::time::Duration;

use bytes::Bytes;
use futures::{SinkExt as _, StreamExt as _};
use gateway_core::account::OutboundProxy;
use hyper_util::rt::TokioIo;
use tokio::sync::OwnedSemaphorePermit;
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        handshake::{client::generate_key, derive_accept_key},
        protocol::{Role, WebSocketConfig},
    },
};

use super::{HttpBody, HttpClient, HttpError, HttpErrorKind, HttpRequest, NetworkPolicy};

const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

/// 无内容型 Debug，连接由持有者独占；丢弃时关闭底层连接并释放容量。
pub struct ManagedWebSocket {
    socket: WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>,
    _permit: OwnedSemaphorePermit,
}

pub enum WebSocketMessage {
    Text(String),
    Binary(Bytes),
}

pub struct WebSocketResponse {
    pub status: u16,
    pub headers: Vec<(String, Vec<u8>)>,
    pub connection: Option<ManagedWebSocket>,
    pub body: Option<HttpBody>,
}

impl HttpClient {
    /// 使用与 HTTP 相同的 DNS、代理、证书和容量边界执行 HTTP/1.1 Upgrade。
    pub async fn open_websocket(
        &self,
        request: HttpRequest,
        proxy: Option<&OutboundProxy>,
        network: &NetworkPolicy,
        timeout: Duration,
    ) -> Result<WebSocketResponse, HttpError> {
        if request.method != "GET"
            || !request.body.is_empty()
            || request
                .headers
                .iter()
                .any(|(name, _)| name.to_ascii_lowercase().starts_with("sec-websocket-"))
        {
            return Err(HttpError::invalid("WebSocket request"));
        }
        let key = generate_key();
        let mut response = self
            .send(request, proxy, network, timeout, None, Some(&key))
            .await?;
        if response.status != 101 {
            let response = response.into_http();
            return Ok(WebSocketResponse {
                status: response.status,
                headers: response.headers,
                connection: None,
                body: Some(response.body),
            });
        }
        let headers = response.response.headers();
        if headers
            .get("sec-websocket-accept")
            .and_then(|value| value.to_str().ok())
            != Some(derive_accept_key(key.as_bytes()).as_str())
            || !headers
                .get("upgrade")
                .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"))
            || !headers
                .get("connection")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| {
                    value
                        .split(',')
                        .any(|part| part.trim().eq_ignore_ascii_case("upgrade"))
                })
            || headers.contains_key("sec-websocket-extensions")
            || headers.contains_key("sec-websocket-protocol")
        {
            return Err(HttpError::sent("WebSocket handshake"));
        }
        let upgraded = tokio::time::timeout_at(
            response.deadline,
            hyper::upgrade::on(&mut response.response),
        )
        .await
        .map_err(|_| timeout_error())?
        .map_err(|_| HttpError::sent("WebSocket upgrade"))?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_MESSAGE_BYTES));
        let socket =
            WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Client, Some(config))
                .await;
        Ok(WebSocketResponse {
            status: response.status,
            headers: response.headers,
            connection: Some(ManagedWebSocket {
                socket,
                _permit: response.permit,
            }),
            body: None,
        })
    }
}

impl ManagedWebSocket {
    pub async fn send(
        &mut self,
        message: WebSocketMessage,
        timeout: Duration,
    ) -> Result<(), HttpError> {
        let message = match message {
            WebSocketMessage::Text(text) if text.len() <= MAX_MESSAGE_BYTES => {
                Message::Text(text.into())
            }
            WebSocketMessage::Binary(bytes) if bytes.len() <= MAX_MESSAGE_BYTES => {
                Message::Binary(bytes)
            }
            _ => return Err(HttpError::invalid("WebSocket message limits")),
        };
        tokio::time::timeout(timeout, self.socket.send(message))
            .await
            .map_err(|_| timeout_error())?
            .map_err(|_| HttpError::sent("WebSocket send"))
    }

    pub async fn read(&mut self, timeout: Duration) -> Result<Option<WebSocketMessage>, HttpError> {
        tokio::time::timeout(timeout, async {
            loop {
                match self.socket.next().await {
                    Some(Ok(Message::Text(text))) => {
                        return Ok(Some(WebSocketMessage::Text(text.to_string())));
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        return Ok(Some(WebSocketMessage::Binary(bytes)));
                    }
                    Some(Ok(Message::Ping(_))) => self
                        .socket
                        .flush()
                        .await
                        .map_err(|_| HttpError::sent("WebSocket heartbeat"))?,
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None => return Ok(None),
                    Some(Ok(Message::Frame(_))) => return Err(HttpError::sent("WebSocket frame")),
                    Some(Err(_)) => return Err(HttpError::sent("WebSocket receive")),
                }
            }
        })
        .await
        .map_err(|_| timeout_error())?
    }
}

fn timeout_error() -> HttpError {
    HttpError::sent("WebSocket deadline").with_kind(HttpErrorKind::Timeout)
}
