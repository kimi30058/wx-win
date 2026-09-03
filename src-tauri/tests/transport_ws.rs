//! WS 传输层集成测试（TDD Step 1：先写测试再实现）
//! 策略：本地起 tokio-tungstenite mock server，
//! 验证 连接建立回调 on_connect / 双向帧收发（ping→pong）/ 服务端关闭后 on_disconnect。

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use wxauto_desktop::transport::ws::WsTransport;
use wxauto_desktop::transport::{DeviceTransport, TransportHandler};

/// 测试 handler：收集 on_connect 次数 / 收到的帧 / 断连原因
#[derive(Default)]
struct Collector {
    connects: Mutex<u32>,
    frames: Mutex<Vec<Value>>,
    disconnects: Mutex<Vec<String>>,
}

impl Collector {
    fn shared(self) -> (Arc<Self>, SharedCollector) {
        let arc = Arc::new(self);
        let handler = SharedCollector(arc.clone());
        (arc, handler)
    }
}

/// Arc 适配层：同一份 Collector 既给 handler 又给测试主线程断言
struct SharedCollector(Arc<Collector>);

impl TransportHandler for SharedCollector {
    fn on_connect(&self) {
        let mut c = self.0.connects.lock().unwrap();
        *c += 1;
    }
    fn on_frame(&self, frame: Value) {
        self.0.frames.lock().unwrap().push(frame);
    }
    fn on_disconnect(&self, reason: String) {
        self.0.disconnects.lock().unwrap().push(reason);
    }
}

/// 起 mock server：收 ping 回 pong；`close_after_pong=true` 时回完 pong 立即发 Close
fn spawn_mock_server(listener: tokio::net::TcpListener, close_after_pong: bool) {
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        while let Some(Ok(msg)) = ws.next().await {
            if let Message::Text(t) = msg {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["kind"] == "ping" {
                    let pong = json!({ "kind": "pong", "ts": v["ts"] }).to_string();
                    ws.send(Message::Text(pong)).await.unwrap();
                    if close_after_pong {
                        // 回完 pong 主动关闭，触发客户端 on_disconnect
                        ws.send(Message::Close(None)).await.unwrap();
                        break;
                    }
                }
            }
        }
    });
}

/// 场景 1：能连上、on_connect 回调、发 ping 收 pong（双向泵验证）
#[tokio::test]
async fn test_ws_connect_and_frame_roundtrip() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    spawn_mock_server(listener, false);

    let (collector, handler) = Collector::default().shared();
    let t = WsTransport::new(format!("ws://{addr}/?token=t1"));
    // 发送端：经 unbounded channel 往 ws 发 ping
    let sender = t.sender();
    let runner = tokio::spawn(t.run(Box::new(handler)));

    // 等连接建立（on_connect 已回调）后发 ping，等 pong 回来
    tokio::time::sleep(Duration::from_millis(300)).await;
    sender
        .0
        .send(json!({ "kind": "ping", "ts": 42 }))
        .expect("发送端应存活");
    tokio::time::sleep(Duration::from_millis(500)).await;
    runner.abort();

    let frames = collector.frames.lock().unwrap();
    assert_eq!(
        *collector.connects.lock().unwrap(),
        1,
        "on_connect 应恰好回调 1 次"
    );
    assert!(
        frames.iter().any(|f| f["kind"] == "pong" && f["ts"] == 42),
        "应收到 ts=42 的 pong，实际 {frames:?}"
    );
}

/// 场景 2：服务端主动 Close → 客户端回调 on_disconnect
#[tokio::test]
async fn test_ws_on_disconnect_on_server_close() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    spawn_mock_server(listener, true);

    let (collector, handler) = Collector::default().shared();
    let t = WsTransport::new(format!("ws://{addr}/?token=t2"));
    let sender = t.sender();
    let runner = tokio::spawn(t.run(Box::new(handler)));

    tokio::time::sleep(Duration::from_millis(300)).await;
    sender
        .0
        .send(json!({ "kind": "ping", "ts": 1 }))
        .expect("发送端应存活");
    // 等 Close 帧到达并触发 on_disconnect
    tokio::time::sleep(Duration::from_millis(500)).await;
    runner.abort();

    let disconnects = collector.disconnects.lock().unwrap();
    assert!(
        !disconnects.is_empty(),
        "服务端 Close 后应回调 on_disconnect"
    );
}
