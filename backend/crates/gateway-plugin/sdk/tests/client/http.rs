use super::session::{HostPeer, receive, send_call, shutdown, start_session};
use gateway_plugin_sdk::client::{CallFuture, CallReply, PluginCall, PluginHandler, write_frame};
use gateway_plugin_sdk::{ErrorCode, Frame, Message};
use gateway_plugin_sdk::{
    call::{
        host::HttpRequest,
        upstream_adapter::{UpstreamHttpRequest, UpstreamWebSocketRequest, WebSocketMessageKind},
    },
    client::UpstreamWebSocketUpgrade,
};
use serde_json::{Value, json};

struct NetworkHandler;

impl PluginHandler for NetworkHandler {
    fn call(&self, call: PluginCall) -> CallFuture<'_> {
        Box::pin(async move {
            if call.method == "websocket" {
                let upgrade = call
                    .host
                    .upstream_websocket(UpstreamWebSocketRequest {
                        path: "responses".into(),
                        query: vec![],
                        headers: vec![],
                    })
                    .await?;
                let UpstreamWebSocketUpgrade::Connected { mut connection, .. } = upgrade else {
                    let UpstreamWebSocketUpgrade::Rejected { status, body, .. } = upgrade else {
                        unreachable!()
                    };
                    return Ok(CallReply::unary(json!({"status":status}), body));
                };
                connection
                    .send(WebSocketMessageKind::Text, b"request".to_vec())
                    .await?;
                let (kind, body) = connection.read().await?.unwrap();
                assert_eq!(kind, WebSocketMessageKind::Text);
                assert!(connection.read().await?.is_none());
                assert!(connection.read().await?.is_none());
                connection.close().await?;
                assert!(connection.send(kind, vec![]).await.is_err());
                return Ok(CallReply::unary(json!({}), body));
            }
            let response = if call.method == "http" {
                call.host
                    .http(
                        HttpRequest {
                            method: "POST".into(),
                            url: "https://example.test/responses".into(),
                            headers: vec![],
                        },
                        call.payload,
                    )
                    .await?
            } else {
                call.host
                    .upstream_http(
                        UpstreamHttpRequest {
                            method: "POST".into(),
                            path: "responses".into(),
                            query: vec![],
                            headers: vec![],
                        },
                        call.payload,
                    )
                    .await?
            };
            assert_eq!(response.status, 200);
            if call.params["collect"] == true {
                return Ok(CallReply::unary(json!({}), response.body.collect(3).await?));
            }
            let mut body = response.body;
            let bytes = body.read().await?.unwrap();
            assert!(body.read().await?.is_none());
            assert!(body.read().await?.is_none());
            body.close().await?;
            body.close().await?;
            Ok(CallReply::unary(json!({}), bytes))
        })
    }
}

async fn reply_callback(
    host: &mut HostPeer,
    expected: &str,
    result: Value,
    payload: Vec<u8>,
) -> Frame {
    let frame = receive(host).await;
    let Message::Callback { id, method, .. } = &frame.message else {
        panic!("expected callback")
    };
    assert_eq!(method, expected);
    write_frame(
        &mut host.writer,
        &Frame {
            message: Message::Result { id: *id, result },
            payload,
        },
    )
    .await
    .unwrap();
    frame
}

#[tokio::test]
async fn http_response_objects_share_pull_semantics_and_retire_eof_handles() {
    for method in ["http", "upstream"] {
        let (mut host, task) = start_session(NetworkHandler).await;
        send_call(&mut host, 1, method, json!({}), b"request".to_vec()).await;
        let prefix = if method == "http" {
            "host.http"
        } else {
            "host.upstream.http"
        };
        let frame = reply_callback(
            &mut host,
            &format!("{prefix}.do_stream"),
            json!({"status":200,"headers":[],"stream":"private-handle"}),
            vec![],
        )
        .await;
        assert_eq!(frame.payload, b"request");
        let frame = reply_callback(
            &mut host,
            &format!("{prefix}.stream_read"),
            json!({"eof":false}),
            b"abc".to_vec(),
        )
        .await;
        assert!(
            matches!(frame.message, Message::Callback { params, .. } if params == json!({"stream":"private-handle","maximum_bytes":65536}))
        );
        reply_callback(
            &mut host,
            &format!("{prefix}.stream_read"),
            json!({"eof":true}),
            vec![],
        )
        .await;
        let result = receive(&mut host).await;
        assert!(matches!(result.message, Message::Result { id: 1, .. }));
        assert_eq!(result.payload, b"abc");
        shutdown(&mut host, task).await;
    }
}

#[tokio::test]
async fn bounded_collection_closes_the_host_stream_before_reporting_overflow() {
    let (mut host, task) = start_session(NetworkHandler).await;
    send_call(&mut host, 1, "upstream", json!({"collect":true}), vec![]).await;
    reply_callback(
        &mut host,
        "host.upstream.http.do_stream",
        json!({"status":200,"headers":[],"stream":"private-handle"}),
        vec![],
    )
    .await;
    reply_callback(
        &mut host,
        "host.upstream.http.stream_read",
        json!({"eof":false}),
        b"abcd".to_vec(),
    )
    .await;
    reply_callback(
        &mut host,
        "host.upstream.http.stream_close",
        json!({}),
        vec![],
    )
    .await;
    assert!(
        matches!(receive(&mut host).await.message, Message::Error { id: 1, error } if error.code == ErrorCode::Capacity)
    );
    shutdown(&mut host, task).await;
}

#[tokio::test]
async fn websocket_session_handles_messages_eof_and_http_rejection() {
    let (mut host, task) = start_session(NetworkHandler).await;
    send_call(&mut host, 1, "websocket", json!({}), vec![]).await;
    reply_callback(
        &mut host,
        "host.upstream.websocket.open",
        json!({"status":101,"headers":[],"stream":null}),
        vec![],
    )
    .await;
    let sent = reply_callback(&mut host, "host.upstream.websocket.send", json!({}), vec![]).await;
    assert_eq!(sent.payload, b"request");
    reply_callback(
        &mut host,
        "host.upstream.websocket.read",
        json!({"eof":false,"kind":"text"}),
        b"response".to_vec(),
    )
    .await;
    reply_callback(
        &mut host,
        "host.upstream.websocket.read",
        json!({"eof":true,"kind":null}),
        vec![],
    )
    .await;
    let response = receive(&mut host).await;
    assert!(matches!(response.message, Message::Result { id: 1, .. }));
    assert_eq!(response.payload, b"response");
    send_call(&mut host, 3, "websocket", json!({}), vec![]).await;
    reply_callback(
        &mut host,
        "host.upstream.websocket.open",
        json!({"status":401,"headers":[],"stream":null}),
        b"unauthorized".to_vec(),
    )
    .await;
    let response = receive(&mut host).await;
    assert!(
        matches!(response.message, Message::Result { id: 3, result } if result == json!({"status":401}))
    );
    assert_eq!(response.payload, b"unauthorized");
    shutdown(&mut host, task).await;
}
