//! outbox 断线补发集成测试：首连不 ack → outbox 残留 → 新 link 重连补发同 eventId → ack 清空
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use wxauto_desktop::agent_link::AgentLink;
use wxauto_desktop::outbox::Outbox;
use wxauto_desktop::sidecar::SidecarHandle;
use wxauto_desktop::wx::listener::ListenerRegistry;
use wxauto_desktop::wx::WxSession;

/// 剧本 sidecar：wx.get_my_info / wx.is_online 回固定形状，其余回 ok
const SCRIPT: &str = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    m = req["method"]
    if m == "wx.get_my_info":
        out = {"licensed": True, "wxid": "wxid_t", "nickname": "测试", "online": True}
    elif m == "wx.is_online":
        out = {"online": True}
    else:
        out = {"ok": True}
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": out}), flush=True)
"#;

async fn make_link(url: String, outbox: Arc<Outbox>) -> Arc<AgentLink> {
    let handle = SidecarHandle::spawn_with_python(SCRIPT, &[])
        .await
        .expect("spawn python 失败");
    let session = Arc::new(WxSession::new(Arc::new(Mutex::new(handle))));
    let listeners = Arc::new(ListenerRegistry::new(session.clone()));
    let link = Arc::new(AgentLink::new(session, listeners, url));
    link.set_outbox(outbox);
    link
}

/// mock server 模式：
/// - NoAck：收满 n 帧后主动断开（不回 ack）
/// - Ack：收到带 eventId 的 event 帧即回 ack
/// Clone：accept 循环每连接复制一份（mode 本体随监听任务存活）
#[derive(Clone)]
enum ServerMode {
    NoAck { close_after: usize },
    Ack,
}

async fn spawn_server(mode: ServerMode) -> (std::net::SocketAddr, mpsc::Receiver<Value>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let tx = tx.clone();
            let mode = mode.clone();
            tokio::spawn(async move {
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                let mut received = 0usize;
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Text(t) = msg {
                        let v: Value = serde_json::from_str(&t).unwrap();
                        tx.send(v.clone()).await.unwrap();
                        received += 1;
                        if let ServerMode::NoAck { close_after } = &mode {
                            if received >= *close_after {
                                let _ = ws.close(None).await;
                                return;
                            }
                        }
                        if let ServerMode::Ack = &mode {
                            if v["kind"] == "event" {
                                if let Some(id) = v["eventId"].as_str() {
                                    let ack = json!({ "kind": "ack", "eventId": id, "ts": 1 });
                                    let _ = ws.send(Message::Text(ack.to_string())).await;
                                }
                            }
                        }
                    }
                }
            });
        }
    });
    (addr, rx)
}

/// 收帧通道里等第一条满足谓词的帧（带超时）
async fn wait_frame(rx: &mut mpsc::Receiver<Value>, pred: impl Fn(&Value) -> bool) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while let Some(v) = tokio::time::timeout_at(deadline, rx.recv())
        .await
        .expect("10s 内应收帧")
    {
        if pred(&v) {
            return v;
        }
    }
    panic!("10s 内未等到期望帧");
}

#[tokio::test]
async fn test_outbox_replay_after_reconnect() {
    let dir = tempfile::tempdir().expect("tempdir");
    let outbox_path = dir.path().join("outbox.jsonl");
    let outbox = Arc::new(Outbox::open(&outbox_path).unwrap());

    // —— 第一幕：link1 首连，server1 收 hello+event 后断开且不 ack ——
    let (addr1, mut rx1) = spawn_server(ServerMode::NoAck { close_after: 2 }).await;
    let link1 = make_link(format!("ws://{addr1}"), outbox.clone()).await;
    tokio::spawn({
        let l = link1.clone();
        async move { l.run().await }
    });
    // 等 hello 到达（确保连接就位再发事件，事件不会卡在门闩缓冲前）
    wait_frame(&mut rx1, |v| v["kind"] == "hello").await;
    link1
        .emit_test_event(json!({
            "kind": "event", "type": "message", "eventId": "evt-replay-1",
            "data": { "chatName": "张三", "content": "断线测试" }, "ts": 100
        }))
        .await;
    // server1 收到 event 后断开（close_after=2：hello+event）
    wait_frame(&mut rx1, |v| v["kind"] == "event").await;
    link1.halt();

    // outbox 残留 1 条（已入箱未被 ack）
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while outbox.len() != 1 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(outbox.len(), 1, "未 ack 前应残留 1 条");

    // —— 第二幕：link2（同 outbox）连 ack server → 补发同 eventId → 清空 ——
    let (addr2, mut rx2) = spawn_server(ServerMode::Ack).await;
    let link2 = make_link(format!("ws://{addr2}"), outbox.clone()).await;
    tokio::spawn({
        let l = link2.clone();
        async move { l.run().await }
    });
    // 等 hello（补发在 hello 后）
    wait_frame(&mut rx2, |v| v["kind"] == "hello").await;
    // 补发帧到达 server2，且 eventId 与首发一致
    let replayed = wait_frame(&mut rx2, |v| v["kind"] == "event").await;
    assert_eq!(
        replayed["eventId"], "evt-replay-1",
        "补发帧须携带原 eventId"
    );
    // server2 已回 ack → outbox 清空
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while outbox.len() != 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(outbox.len(), 0, "ack 后 outbox 应清空");
    link2.halt();
}
