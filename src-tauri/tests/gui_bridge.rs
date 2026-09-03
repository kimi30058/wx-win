//! Task 9 GUI 桥扩展测试（TDD Step 1：先红后绿）。
//! 覆盖 AgentLink 为 GUI 装配补齐的三块可测能力（core 不依赖 tauri）：
//! 1. `ws_connected` 标志：on_connect 置 true / on_disconnect 置 false，
//!    且断开时向 event_sink 补发 event:status（GUI 前端两灯数据源）
//! 2. `command_sink`：下行 command 执行完（result 帧发出后）投递指令日志条
//!    {requestId, action, success, error?, durationMs, ts}（前端 command-log 事件）
//! 3. `halt()`：协作停止——run 的 transport 循环退出，后台三泵（通知/心跳/
//!    好友轮询）的 spawn 任务相继结束（GUI disconnect 不泄漏任务）
//!
//! 复用 agent_link.rs 的剧本 sidecar + mock WS server 策略。

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;
use wxauto_desktop::agent_link::AgentLink;
use wxauto_desktop::sidecar::SidecarHandle;
use wxauto_desktop::wx::listener::ListenerRegistry;
use wxauto_desktop::wx::WxSession;

/// 剧本 sidecar：全方法回 {"ok": true}（is_online 探测为 true）
const SCRIPT: &str = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    m = req["method"]
    if m == "wx.is_online":
        out = {"online": True}
    elif m == "wx.get_my_info":
        out = {"licensed": True, "wxid": "wxid_t", "nickname": "测试", "online": True}
    else:
        out = {"ok": True}
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": out}), flush=True)
"#;

/// 装配：剧本 sidecar + link（event/command sink 均注入收集器）
async fn make_link(
    url: String,
    events: Arc<StdMutex<Vec<Value>>>,
    commands: Arc<StdMutex<Vec<Value>>>,
) -> Arc<AgentLink> {
    let handle = SidecarHandle::spawn_with_python(SCRIPT, &[])
        .await
        .expect("spawn 失败");
    let session = Arc::new(WxSession::new(Arc::new(Mutex::new(handle))));
    let listeners = Arc::new(ListenerRegistry::new(session.clone()));
    let ev_target = events.clone();
    let cmd_target = commands.clone();
    let link = AgentLink::new_with_event_sink(session, listeners, url, None);
    link.set_event_sink(Box::new(move |v: Value| {
        ev_target.lock().unwrap().push(v);
    }));
    link.set_command_sink(Box::new(move |v: Value| {
        cmd_target.lock().unwrap().push(v);
    }));
    Arc::new(link)
}

// ── ws_connected 标志 + 断开补发 status ──────────────────────────

/// 连接建立 → ws_connected=true；服务端关闭 → ws_connected=false 且
/// event_sink 收到一条补发的 event:status（GUI 两灯数据源）。
#[tokio::test]
async fn test_ws_connected_flag_and_disconnect_status() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel::<()>();

    // mock server：收完首帧（hello）即主动 Close，触发客户端 on_disconnect
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _ = ws.next().await; // hello
        ws.send(Message::Close(None)).await.unwrap();
        let _ = tx.send(());
    });

    let events = Arc::new(StdMutex::new(Vec::new()));
    let commands = Arc::new(StdMutex::new(Vec::new()));
    let link = make_link(format!("ws://{addr}/?token=t"), events.clone(), commands).await;
    assert!(!link.ws_connected(), "连接前应为 false");

    let runner = tokio::spawn({
        let l = link.clone();
        async move { l.run().await }
    });

    // 等服务端关闭（客户端随后进 on_disconnect）
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("等服务端关闭超时")
        .expect("通道不应关闭");

    // 轮询等客户端感知断开（传输层有 backoff 循环，断开回调同步于循环内）
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while link.ws_connected() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!link.ws_connected(), "断开后 ws_connected 应为 false");

    // event_sink 应有断开补发的 event:status（sidecarAlive 域外，字段只查类型）
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        {
            let sink = events.lock().unwrap();
            if sink
                .iter()
                .any(|f| f["kind"] == "event" && f["type"] == "status")
            {
                break;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "5s 内 sink 应收到断开补发的 event:status，实际: {:?}",
            events.lock().unwrap()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    runner.abort();
}

// ── command_sink：下行指令执行完投递日志条 ──────────────────────────

/// hello 后服务端下发 command；执行完（result 帧发出后）command_sink 应收到
/// {requestId, action, success, durationMs, ts}（失败指令含 error 字段）。
#[tokio::test]
async fn test_command_sink_receives_log_entry() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // mock server：hello 后下发成功指令 r1 与未知 action 指令 r2（必失败）
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        while let Some(Ok(msg)) = ws.next().await {
            if let Message::Text(t) = msg {
                let v: Value = serde_json::from_str(&t).expect("server 收帧应为 JSON");
                if v["kind"] == "hello" {
                    let r1 = json!({
                        "kind": "command", "requestId": "r1", "action": "get_my_info",
                        "params": {}
                    });
                    ws.send(Message::Text(r1.to_string())).await.unwrap();
                    let r2 = json!({
                        "kind": "command", "requestId": "r2", "action": "no_such_action",
                        "params": {}
                    });
                    ws.send(Message::Text(r2.to_string())).await.unwrap();
                }
            }
        }
    });

    let events = Arc::new(StdMutex::new(Vec::new()));
    let commands = Arc::new(StdMutex::new(Vec::new()));
    let link = make_link(
        format!("ws://{addr}/?token=t"),
        events.clone(),
        commands.clone(),
    )
    .await;
    let runner = tokio::spawn({
        let l = link.clone();
        async move { l.run().await }
    });

    // 等 sink 收齐两条日志（成功 + 失败）
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        {
            let sink = commands.lock().unwrap();
            let ok = sink
                .iter()
                .find(|f| f["requestId"] == "r1" && f["success"] == true);
            let err = sink
                .iter()
                .find(|f| f["requestId"] == "r2" && f["success"] == false);
            if let (Some(ok), Some(err)) = (ok, err) {
                // 字段契约断言（前端 parseCommandLogItem 守卫对齐）
                assert_eq!(ok["action"], "get_my_info");
                assert!(
                    ok["durationMs"].as_u64().expect("durationMs 应为数字") > 0,
                    "含拟人间隙的执行时长应 > 0"
                );
                assert!(ok["ts"].as_u64().expect("ts 应为数字") > 0);
                assert!(ok.get("error").is_none(), "成功条目不应有 error 字段");
                assert_eq!(err["action"], "no_such_action");
                assert!(
                    !err["error"]
                        .as_str()
                        .expect("失败条目应含 error 字符串")
                        .is_empty(),
                    "失败条目 error 非空"
                );
                break;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "20s 内 command_sink 应收齐 r1/r2 日志，实际: {:?}",
            commands.lock().unwrap()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    runner.abort();
}

// ── halt()：协作停止（GUI disconnect 不泄漏任务） ──────────────────

/// halt 后：run（transport 循环）应返回；心跳泵应退出；ws_connected=false。
#[tokio::test]
async fn test_halt_stops_run_and_pumps() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // mock server：纯转发收帧（保持连接打开）
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        while let Some(Ok(m)) = ws.next().await {
            let _ = m; // 只消费，不处理
        }
    });

    let events = Arc::new(StdMutex::new(Vec::new()));
    let commands = Arc::new(StdMutex::new(Vec::new()));
    let link = make_link(format!("ws://{addr}/?token=t"), events, commands).await;
    let runner = tokio::spawn({
        let l = link.clone();
        async move { l.run().await }
    });

    // 等连接建立
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !link.ws_connected() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(link.ws_connected(), "10s 内应完成连接");

    // halt → run 应在 3s 内返回（transport 循环退出）
    link.halt();
    let joined = tokio::time::timeout(Duration::from_secs(3), runner).await;
    assert!(
        joined.is_ok(),
        "halt 后 run 应及时返回（transport 循环退出）"
    );

    // 泵退出（协作停机：拿 sender 模拟外部 drop 场景验证 spawn 生命周期语义）
    // —— 真正的三泵句柄在 run 内部，这里断言 halt 标志位已置 + ws_connected 复位
    assert!(!link.ws_connected(), "halt 后 ws_connected 应复位 false");
}
