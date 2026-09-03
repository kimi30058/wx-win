//! CLI 装配生命周期集成测试（Task 7）
//! 覆盖三条硬性设计输入中的两条可自动化路径：
//! 1. 退出时显式 shutdown：graceful_shutdown 对活进程发 kill 并等回收，
//!    之后所有 call 快速 Closed（进程死透的观测证据）
//! 2. sidecar 重启后通知泵重订阅：sidecar A 死（旧 broadcast Closed）→
//!    热替换 sidecar B → 泵自动重订阅 → B 的 message.received 仍能转发为
//!    event:message 到达服务端
//!
//! 第 3 条「Ctrl-C 处理」由冒烟脚本 desktop/scripts/smoke_cli.sh 真机验证。

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;
use wxauto_desktop::agent_link::AgentLink;
use wxauto_desktop::cli::{build_ws_url, graceful_shutdown};
use wxauto_desktop::sidecar::SidecarHandle;
use wxauto_desktop::wx::listener::ListenerRegistry;
use wxauto_desktop::wx::WxSession;

/// 长命 sidecar：读 stdin 阻塞（模拟正常活进程，不主动退出）
const SLEEPY_SCRIPT: &str = r#"
import sys
for line in sys.stdin:
    pass
"#;

/// 剧本 A：wx.get_my_info 回形状后发一条 message.received 通知并 sys.exit(0)
/// （模拟「上报一条消息后崩溃」的 sidecar）
const SCRIPT_A: &str = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    m = req["method"]
    if m == "wx.get_my_info":
        out = {"licensed": True, "wxid": "wxid_a", "nickname": "A", "online": True}
    elif m == "wx.is_online":
        out = {"online": True}
    else:
        out = {"ok": True}
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": out}), flush=True)
    if m == "wx.get_my_info":
        note = {"msg_id": "a1", "chat_who": "张三", "chat_type": "friend", "attr": "friend",
                "msg_type": "text", "sender": "张三", "content": "重启前消息"}
        print(json.dumps({"jsonrpc": "2.0", "method": "message.received", "params": note}), flush=True)
        sys.exit(0)
"#;

/// 剧本 B：wx.get_my_info / wx.is_online 回形状；msg.send 回 result 后发
/// content="重启后消息" 的通知（重启后的消息上报路径）
const SCRIPT_B: &str = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    m = req["method"]
    if m == "wx.get_my_info":
        out = {"licensed": True, "wxid": "wxid_b", "nickname": "B", "online": True}
    elif m == "wx.is_online":
        out = {"online": True}
    elif m == "msg.send":
        out = {"ok": True}
    else:
        out = {"ok": True}
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": out}), flush=True)
    if m == "msg.send":
        note = {"msg_id": "b1", "chat_who": "李四", "chat_type": "friend", "attr": "friend",
                "msg_type": "text", "sender": "李四", "content": "重启后消息"}
        print(json.dumps({"jsonrpc": "2.0", "method": "message.received", "params": note}), flush=True)
"#;

// ---------- 纯函数：URL 组装 ----------

/// token 非空拼 ?token=；空 token 不挂 query（服务端可不校验）
#[test]
fn test_build_ws_url_token_handling() {
    assert_eq!(
        build_ws_url("ws://127.0.0.1:60021", "abc"),
        "ws://127.0.0.1:60021/?token=abc"
    );
    assert_eq!(
        build_ws_url("ws://127.0.0.1:60021", ""),
        "ws://127.0.0.1:60021"
    );
}

// ---------- 显式 shutdown 语义（硬性输入 #1） ----------

/// graceful_shutdown 对活进程：返回 true + 之后 call 快速 Closed
/// （说明进程真被杀且读循环已退出——CI 上不留孤儿 python 进程）
#[tokio::test]
async fn test_graceful_shutdown_kills_live_sidecar() {
    let handle = SidecarHandle::spawn_with_python(SLEEPY_SCRIPT, &[])
        .await
        .expect("spawn 失败");
    let sidecar = Arc::new(Mutex::new(handle));

    let ok = graceful_shutdown(&sidecar, Duration::from_secs(5)).await;
    assert!(ok, "活进程的 graceful_shutdown 应成功回收");

    // 死透证据：closed 快速失败标志生效（不挂 30s 超时）
    let err = sidecar
        .lock()
        .await
        .call("wx.is_online", json!({}))
        .await
        .expect_err("进程已回收，call 应失败");
    assert!(
        matches!(err, wxauto_desktop::sidecar::RpcError::Closed),
        "应为 Closed，实际: {err:?}"
    );
}

/// graceful_shutdown 幂等：对已死进程再次调用也安全返回
#[tokio::test]
async fn test_graceful_shutdown_idempotent_on_dead_sidecar() {
    let handle = SidecarHandle::spawn_with_python(SLEEPY_SCRIPT, &[])
        .await
        .expect("spawn 失败");
    let sidecar = Arc::new(Mutex::new(handle));
    assert!(
        graceful_shutdown(&sidecar, Duration::from_secs(5)).await,
        "第一次 shutdown 应成功"
    );
    assert!(
        graceful_shutdown(&sidecar, Duration::from_secs(5)).await,
        "第二次 shutdown（已死）应幂等成功"
    );
}

// ---------- 通知泵重订阅（硬性输入 #2） ----------

/// sidecar A 上报一条消息后崩溃 → 旧 broadcast Closed → 热替换 sidecar B →
/// 通知泵自动重订阅 → B 的 msg.send 触发的通知仍能以 event:message 到达服务端。
/// （旧实现泵在 Closed 时 break 退出，B 的消息将永久丢失——本用例即防回归）
#[tokio::test]
async fn test_notification_pump_resubscribes_after_sidecar_restart() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    let (trigger_tx, mut trigger_rx) = mpsc::channel::<()>(1);

    // mock server：转发全部收帧；trigger 到达后下发 command r1
    // （trigger 与收帧并行 select——不能等收帧驱动，server 会阻塞在 ws.next()）
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let mut triggered = false;
        loop {
            tokio::select! {
                msg = ws.next() => match msg {
                    Some(Ok(Message::Text(t))) => {
                        let v: Value = serde_json::from_str(&t).expect("server 收帧应为 JSON");
                        let _ = tx.send(v);
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => panic!("server ws 错误: {e}"),
                    None => break,
                },
                _ = trigger_rx.recv(), if !triggered => {
                    triggered = true;
                    let cmd = json!({
                        "kind": "command", "requestId": "r1", "action": "send_message",
                        "params": {"who": "李四", "text": "hi"}
                    });
                    ws.send(Message::Text(cmd.to_string())).await.unwrap();
                }
            }
        }
    });

    // 装配：sidecar A + session（保留 sidecar Arc 以便热替换）
    let handle_a = SidecarHandle::spawn_with_python(SCRIPT_A, &[])
        .await
        .expect("spawn A 失败");
    let sidecar = Arc::new(Mutex::new(handle_a));
    let session = Arc::new(WxSession::new(sidecar.clone()));
    let listeners = Arc::new(ListenerRegistry::new(session.clone()));
    let link = Arc::new(AgentLink::new(
        session.clone(),
        listeners,
        format!("ws://{addr}/?token=t"),
    ));
    let runner = tokio::spawn({
        let l = link.clone();
        async move { l.run().await }
    });

    // 阶段 1：hello 到达 + A 的通知转发为 event:message（重启前消息）
    let mut got_before = false;
    let mut hello_ok = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while (!got_before || !hello_ok) && std::time::Instant::now() < deadline {
        let f = tokio::time::timeout(Duration::from_millis(2000), rx.recv())
            .await
            .expect("等帧超时")
            .expect("通道不应关闭");
        if f["kind"] == "hello" {
            hello_ok = true;
        }
        if f["kind"] == "event" && f["type"] == "message" && f["data"]["content"] == "重启前消息"
        {
            got_before = true;
        }
    }
    assert!(hello_ok, "应收到 hello");
    assert!(got_before, "sidecar A 的通知应转发为 event:message");

    // 阶段 2：A 已 exit（get_my_info 后自杀）→ 热替换 B
    // （等 A 的退出状态传播，避免替换发生在 reader 退出前）
    {
        let mut exit_rx = sidecar.lock().await.exit_watcher();
        let _ = SidecarHandle::await_exit(&mut exit_rx).await;
    }
    let handle_b = SidecarHandle::spawn_with_python(SCRIPT_B, &[])
        .await
        .expect("spawn B 失败");
    session.replace_sidecar(handle_b).await;

    // 阶段 3：等通知泵完成重订阅（Closed 传播 + 500ms 重订阅退避 + 余量）
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // 阶段 4：trigger 下发 command r1 → B 回 result + 通知 → 泵转发 event
    trigger_tx.send(()).await.expect("trigger 发送失败");
    let mut got_result = false;
    let mut got_after = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while (!got_result || !got_after) && std::time::Instant::now() < deadline {
        let f = tokio::time::timeout(Duration::from_millis(2000), rx.recv())
            .await
            .expect("等帧超时")
            .expect("通道不应关闭");
        if f["kind"] == "result" && f["requestId"] == "r1" {
            assert_eq!(f["success"], true, "B 的 send_message 应成功");
            got_result = true;
        }
        if f["kind"] == "event" && f["type"] == "message" && f["data"]["content"] == "重启后消息"
        {
            got_after = true;
        }
    }
    assert!(got_result, "B 应答 command r1 的 result");
    assert!(
        got_after,
        "sidecar 重启后通知泵应已重订阅，B 的通知必须到达服务端（旧实现此处丢消息）"
    );

    runner.abort();
    let _ = sidecar.lock().await.shutdown().await;
}
