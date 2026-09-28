//! 已选账号的受管 WebSocket；连接归属与跨轮续接由宿主管理。

use super::{
    HostClient,
    http::{callback, empty_callback, invalid},
};
use crate::{
    ErrorCode, PluginFault,
    call::{
        host::HttpResponse,
        upstream_adapter::{
            UpstreamWebSocketMessage, UpstreamWebSocketRead, UpstreamWebSocketRequest,
            WebSocketMessageKind,
        },
    },
};

/// 握手拒绝仍保留 HTTP 状态及正文，供适配器解析上游错误。
pub enum UpstreamWebSocketUpgrade {
    Connected {
        headers: Vec<(String, String)>,
        connection: UpstreamWebSocket,
    },
    Rejected {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
}

/// 本次调用的单消费者连接入口；不暴露凭据或可跨账号复用的连接句柄。
///
/// 结束调用时宿主关闭连接；只有成功终态携带连接内续接状态时才保留连接。
/// 因此丢弃此对象不会提前关闭宿主尚待确认的续接连接。
pub struct UpstreamWebSocket {
    host: HostClient,
    closed: bool,
}

impl HostClient {
    /// 建立连接，续接时复用宿主恢复的同一连接。
    ///
    /// # Errors
    /// 目标、权限、握手或续接归属无效时失败。
    pub async fn upstream_websocket(
        &self,
        request: UpstreamWebSocketRequest,
    ) -> Result<UpstreamWebSocketUpgrade, PluginFault> {
        let (response, body): (HttpResponse, _) =
            callback(self, "host.upstream.websocket.open", request, vec![]).await?;
        if response.stream.is_some() || (response.status == 101 && !body.is_empty()) {
            return Err(invalid());
        }
        Ok(if response.status == 101 {
            UpstreamWebSocketUpgrade::Connected {
                headers: response.headers,
                connection: UpstreamWebSocket {
                    host: self.clone(),
                    closed: false,
                },
            }
        } else {
            UpstreamWebSocketUpgrade::Rejected {
                status: response.status,
                headers: response.headers,
                body,
            }
        })
    }
}

impl UpstreamWebSocket {
    /// 发送完整文本或二进制消息。
    ///
    /// # Errors
    /// 连接关闭、消息无效、网络失败或期限到达时失败。
    pub async fn send(
        &mut self,
        kind: WebSocketMessageKind,
        body: Vec<u8>,
    ) -> Result<(), PluginFault> {
        if self.closed {
            return Err(PluginFault::new(
                ErrorCode::InvalidInput,
                "WebSocket is closed",
            ));
        }
        empty_callback(
            &self.host,
            "host.upstream.websocket.send",
            UpstreamWebSocketMessage { kind },
            body,
        )
        .await
    }

    /// 按需读取完整消息，EOF 后再次读取返回 None；Ping/Pong 由宿主处理。
    ///
    /// # Errors
    /// 父调用结束、网络失败或期限到达时失败。
    pub async fn read(&mut self) -> Result<Option<(WebSocketMessageKind, Vec<u8>)>, PluginFault> {
        if self.closed {
            return Ok(None);
        }
        let (read, body): (UpstreamWebSocketRead, _) = callback(
            &self.host,
            "host.upstream.websocket.read",
            serde_json::json!({}),
            vec![],
        )
        .await?;
        match (read.eof, read.kind) {
            (true, None) if body.is_empty() => {
                self.closed = true;
                Ok(None)
            }
            (false, Some(kind)) => Ok(Some((kind, body))),
            _ => Err(invalid()),
        }
    }

    /// 提前关闭并放弃连接内续接，重复关闭不调用宿主。
    ///
    /// # Errors
    /// 父调用已结束或宿主拒绝释放时失败。
    pub async fn close(&mut self) -> Result<(), PluginFault> {
        if self.closed {
            return Ok(());
        }
        empty_callback(
            &self.host,
            "host.upstream.websocket.close",
            serde_json::json!({}),
            vec![],
        )
        .await?;
        self.closed = true;
        Ok(())
    }
}
