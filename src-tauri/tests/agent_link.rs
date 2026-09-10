//! agent_link 编排层集成测试（TDD Step 1：先写测试再实现）
//! 策略：
//! - 纯函数：CommandTracker 幂等（begin 在途拒绝 / finish 后可重来）、
//!   inbound 组帧（sidecar snake_case 通知 → WS camelCase 帧）
//! - 端到端：python 剧本 sidecar + mock WS server，
//!   验证 hello（wxid/nickname 取自 wx.get_my_info）→ command → result、
//!   message.received → 图片/语音二次 RPC → event:message、初始 event:status
//! - 重连语义（审查输入 #1）：断线期间积压的旧帧不得先于新 hello 到达服务端

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;
use wxauto_desktop::agent_link::inbound::{
    friend_request_event, message_event_from_notification, status_event,
};
use wxauto_desktop::agent_link::pending::CommandTracker;
use wxauto_desktop::agent_link::AgentLink;
use wxauto_desktop::sidecar::SidecarHandle;
use wxauto_desktop::wx::listener::ListenerRegistry;
use wxauto_desktop::wx::{WxHealth, WxSession};

/// 剧本 sidecar：
/// - msg.send 按 text 内容回放 image / voice 两条 message.received 通知之一再回 result
/// - media.download / voice.to_text / wx.get_my_info / wx.is_online 回固定形状
/// - 其余方法回显 method（供 resync 等）
const SCRIPT: &str = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    m = req["method"]
    if m == "msg.send":
        p = req.get("params", {})
        if p.get("text") == "img":
            note = {"msg_id": "m1", "chat_who": "张三", "chat_type": "friend", "attr": "friend",
                    "msg_type": "image", "sender": "张三", "content": "[图片]"}
        else:
            note = {"msg_id": "m2", "chat_who": "客户群", "chat_type": "group", "attr": "friend",
                    "msg_type": "voice", "sender": "李四", "content": "[语音]"}
        print(json.dumps({"jsonrpc": "2.0", "method": "message.received", "params": note}), flush=True)
        print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {"ok": True}}), flush=True)
    elif m == "media.download":
        print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {"path": "C:/tmp/m1.png"}}), flush=True)
    elif m == "voice.to_text":
        print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {"text": "语音转写内容"}}), flush=True)
    elif m == "wx.get_my_info":
        print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {"licensed": True, "wxid": "wxid_test", "nickname": "测试号", "online": True}}), flush=True)
    elif m == "wx.is_online":
        print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {"online": True}}), flush=True)
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {"method": m}}), flush=True)
"#;

/// spawn 剧本 sidecar 并装配 AgentLink（监听注册表为空名单）
async fn make_link(url: String) -> Arc<AgentLink> {
    let handle = SidecarHandle::spawn_with_python(SCRIPT, &[])
        .await
        .expect("spawn 失败（需本机有 python3）");
    let session = Arc::new(WxSession::new(Arc::new(Mutex::new(handle))));
    let listeners = Arc::new(ListenerRegistry::new(session.clone()));
    Arc::new(AgentLink::new(session, listeners, url))
}

/// spawn 剧本 sidecar 并装配带 event_sink 的 AgentLink，返回 (link, 收集器)
async fn make_link_with_sink(url: String) -> (Arc<AgentLink>, Arc<StdMutex<Vec<Value>>>) {
    let handle = SidecarHandle::spawn_with_python(SCRIPT, &[])
        .await
        .expect("spawn 失败（需本机有 python3）");
    let session = Arc::new(WxSession::new(Arc::new(Mutex::new(handle))));
    let listeners = Arc::new(ListenerRegistry::new(session.clone()));
    let sink_events: Arc<StdMutex<Vec<Value>>> = Arc::new(StdMutex::new(Vec::new()));
    let sink_target = sink_events.clone();
    let link = AgentLink::new_with_event_sink(
        session,
        listeners,
        url,
        Some(Box::new(move |v: Value| {
            sink_target.lock().unwrap().push(v.clone());
        })),
    );
    (Arc::new(link), sink_events)
}

// ---------- 纯函数：CommandTracker 幂等 ----------

/// 幂等语义：首次 begin true；同 id 在途 begin false；finish 后同 id 可重新 begin
#[tokio::test]
async fn test_command_tracker_idempotent() {
    let tracker = CommandTracker::new();
    let first = tracker.begin("req-1".into()).await;
    assert!(first, "首次执行");
    let dup = tracker.begin("req-1".into()).await;
    assert!(!dup, "重复 requestId 拒绝");
    tracker.finish("req-1").await;
    // finish 后可再次 begin（服务端重发场景）
    assert!(
        tracker.begin("req-1".into()).await,
        "完成后同 id 可重新执行"
    );
}

/// 不同 id 互不干扰；finish 只影响自己的 id
#[tokio::test]
async fn test_command_tracker_independent_ids() {
    let tracker = CommandTracker::new();
    assert!(tracker.begin("a".into()).await);
    assert!(tracker.begin("b".into()).await, "不同 id 不应被去重");
    tracker.finish("a").await;
    assert!(tracker.begin("a".into()).await, "a 已完成可重来");
    assert!(!tracker.begin("b".into()).await, "b 仍在途");
}

// ---------- 纯函数：inbound 组帧（snake_case → camelCase） ----------

/// message.received 通知参数（snake_case）→ event:message 帧（camelCase）全字段对齐
#[test]
fn test_message_event_from_notification_camel() {
    let params = json!({
        "msg_id": "m1", "chat_who": "张三", "chat_type": "group", "attr": "friend",
        "msg_type": "image", "sender": "李四", "content": "[图片]", "is_at": true
    });
    let f = message_event_from_notification(&params, Some("C:/x.png"), None);
    assert_eq!(f["kind"], "event");
    assert_eq!(f["type"], "message");
    assert_eq!(f["data"]["chatName"], "张三");
    assert_eq!(f["data"]["chatType"], "group");
    assert_eq!(f["data"]["attr"], "friend");
    assert_eq!(f["data"]["msgType"], "image");
    assert_eq!(f["data"]["sender"], "李四");
    assert_eq!(f["data"]["content"], "[图片]");
    assert_eq!(f["data"]["isAt"], true);
    assert_eq!(f["data"]["downloadedMedia"], "C:/x.png");
    assert!(f["ts"].as_u64().expect("ts 应为数字") > 0);
}

/// 语音转写覆写 content；非 group 的 chat_type 归一为 friend；缺省字段兜底
#[test]
fn test_message_event_voice_overwrite_and_defaults() {
    let params = json!({
        "msg_id": "m2", "chat_who": "王五", "chat_type": "whatever",
        "msg_type": "voice", "sender": "王五", "content": "[语音]"
    });
    let f = message_event_from_notification(&params, None, Some("转写文本"));
    assert_eq!(f["data"]["content"], "转写文本", "转写成功应覆写原文");
    assert_eq!(f["data"]["chatType"], "friend", "非 group 一律归一 friend");
    assert_eq!(f["data"]["downloadedMedia"], "");

    // 转写失败（None）保留原内容
    let f2 = message_event_from_notification(&params, None, None);
    assert_eq!(f2["data"]["content"], "[语音]");

    // 空 params 全字段兜底默认值
    let f3 = message_event_from_notification(&json!({}), None, None);
    assert_eq!(f3["data"]["msgType"], "text");
    assert_eq!(f3["data"]["attr"], "friend");
    assert_eq!(f3["data"]["chatType"], "friend");
    assert_eq!(f3["data"]["content"], "");

    // 旧 sidecar 帧缺 is_at 键 → isAt 兜底 false
    let f4 = message_event_from_notification(&json!({}), None, None);
    assert_eq!(f4["data"]["isAt"], false);
}

/// friend_request / status 事件帧字段
#[test]
fn test_friend_request_and_status_event_fields() {
    let f = friend_request_event("小明", "请求添加好友");
    assert_eq!(f["kind"], "event");
    assert_eq!(f["type"], "friend_request");
    assert_eq!(f["data"]["name"], "小明");
    assert_eq!(f["data"]["msg"], "请求添加好友");
    assert!(f["ts"].as_u64().expect("ts 应为数字") > 0);

    let s = status_event(true, "online", 3, true);
    assert_eq!(s["kind"], "event");
    assert_eq!(s["type"], "status");
    assert_eq!(s["data"]["wxOnline"], true);
    assert_eq!(s["data"]["wxState"], "online");
    assert_eq!(s["data"]["listeners"], 3);
    assert_eq!(s["data"]["sidecarAlive"], true);

    // P1 三态：Unreachable 时 wxOnline 折叠 false，wxState 保留 probe_timeout
    let s2 = status_event(false, "probe_timeout", 0, true);
    assert_eq!(s2["data"]["wxOnline"], false);
    assert_eq!(s2["data"]["wxState"], "probe_timeout");
}

// ---------- 端到端 ----------

/// 全链路：hello（wxid/nickname 来自 wx.get_my_info）→ command → result →
/// message.received → 二次 RPC（media.download / voice.to_text）→ event:message → 初始 status
#[tokio::test]
async fn test_e2e_hello_result_and_media_events() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();

    // mock server：转发全部收帧给测试；hello 后下发 command r1，r1 result 后下发 r2
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        while let Some(Ok(msg)) = ws.next().await {
            if let Message::Text(t) = msg {
                let v: Value = serde_json::from_str(&t).expect("server 收帧应为 JSON");
                let _ = tx.send(v.clone());
                if v["kind"] == "hello" {
                    let cmd = json!({
                        "kind": "command", "requestId": "r1", "action": "send_message",
                        "params": {"who": "张三", "text": "img"}
                    });
                    ws.send(Message::Text(cmd.to_string())).await.unwrap();
                }
                if v["kind"] == "result" && v["requestId"] == "r1" {
                    let cmd = json!({
                        "kind": "command", "requestId": "r2", "action": "send_message",
                        "params": {"who": "客户群", "text": "voice"}
                    });
                    ws.send(Message::Text(cmd.to_string())).await.unwrap();
                }
            }
        }
    });

    let link = make_link(format!("ws://{addr}/?token=t")).await;
    // P2 任务 8c：hello 帧带 channelId（排障时服务端日志可直接核对渠道身份）
    link.set_channel_id("ch-e2e-001".into());
    let runner = tokio::spawn({
        let l = link.clone();
        async move { l.run().await }
    });

    let mut hello = None;
    let mut r1 = None;
    let mut r2 = None;
    let mut ev_img = None;
    let mut ev_voice = None;
    let mut status = None;
    let deadline = Duration::from_secs(20);
    while hello.is_none()
        || r1.is_none()
        || r2.is_none()
        || ev_img.is_none()
        || ev_voice.is_none()
        || status.is_none()
    {
        let f = match tokio::time::timeout(deadline, rx.recv()).await {
            Ok(Some(f)) => f,
            Ok(None) => panic!("server 帧通道提前关闭"),
            Err(_) => panic!("20s 内未凑齐全部期望帧"),
        };
        match (
            f["kind"].as_str().unwrap_or(""),
            f["type"].as_str().unwrap_or(""),
        ) {
            ("hello", _) if hello.is_none() => hello = Some(f),
            ("result", _) if f["requestId"] == "r1" && r1.is_none() => r1 = Some(f),
            ("result", _) if f["requestId"] == "r2" && r2.is_none() => r2 = Some(f),
            ("event", "message") if f["data"]["msgType"] == "image" && ev_img.is_none() => {
                ev_img = Some(f)
            }
            ("event", "message") if f["data"]["msgType"] == "voice" && ev_voice.is_none() => {
                ev_voice = Some(f)
            }
            ("event", "status") if status.is_none() => status = Some(f),
            _ => {}
        }
    }
    runner.abort();

    // hello 帧：wxid/nickname 取自 wx.get_my_info；版本字段对齐
    let h = hello.expect("刚确认过存在");
    assert_eq!(h["protocolVersion"], 1);
    assert_eq!(h["appVersion"], env!("CARGO_PKG_VERSION"));
    assert_eq!(h["wxid"], "wxid_test");
    assert_eq!(h["nickname"], "测试号");
    assert!(h.get("hostname").is_some(), "hello 应含 hostname 字段");
    assert_eq!(h["channelId"], "ch-e2e-001", "P2 8c：hello 应带 channelId");
    assert!(h["ts"].as_u64().expect("ts 应为数字") > 0);

    // result 帧：requestId 回填、success、data 透传
    let res1 = r1.expect("刚确认过存在");
    assert_eq!(res1["requestId"], "r1");
    assert_eq!(res1["success"], true);
    assert_eq!(res1["data"]["ok"], true);
    assert_eq!(res1["error"], Value::Null, "成功时 error 应为 null");
    let res2 = r2.expect("刚确认过存在");
    assert_eq!(res2["requestId"], "r2");
    assert_eq!(res2["success"], true);

    // event:image：snake_case→camelCase + 二次 RPC media.download 填 downloadedMedia
    let img = ev_img.expect("刚确认过存在");
    assert_eq!(img["data"]["chatName"], "张三");
    assert_eq!(img["data"]["chatType"], "friend");
    assert_eq!(img["data"]["msgType"], "image");
    assert_eq!(img["data"]["downloadedMedia"], "C:/tmp/m1.png");

    // event:voice：voice.to_text 转写覆写 content；群聊 chatType 透传
    let voice = ev_voice.expect("刚确认过存在");
    assert_eq!(voice["data"]["chatType"], "group");
    assert_eq!(voice["data"]["sender"], "李四");
    assert_eq!(voice["data"]["content"], "语音转写内容");

    // 初始 status：wxOnline/sidecarAlive 来自 wx.is_online 探测
    let st = status.expect("刚确认过存在");
    assert_eq!(st["data"]["wxOnline"], true);
    assert_eq!(st["data"]["sidecarAlive"], true);
}

/// 重连语义（审查输入 #1）：断线期间积压的旧帧必须被排空，
/// 重连后服务端收到的第一帧是新 hello，而不是先于 hello 的旧 ping。
#[tokio::test]
async fn test_reconnect_drains_stale_frames_hello_first() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();

    // mock server：conn1 读首帧后主动 Close；conn2 持续转发
    tokio::spawn(async move {
        let (s1, _) = listener.accept().await.unwrap();
        let mut ws1 = tokio_tungstenite::accept_async(s1).await.unwrap();
        if let Some(Ok(Message::Text(t))) = ws1.next().await {
            let _ = tx.send(serde_json::from_str(&t).expect("conn1 首帧应为 JSON"));
        }
        ws1.send(Message::Close(None)).await.unwrap();
        // 客户端将退避重连
        let (s2, _) = listener.accept().await.unwrap();
        let mut ws2 = tokio_tungstenite::accept_async(s2).await.unwrap();
        while let Some(Ok(m)) = ws2.next().await {
            if let Message::Text(t) = m {
                let _ = tx.send(serde_json::from_str(&t).expect("conn2 帧应为 JSON"));
            }
        }
    });

    let link = make_link(format!("ws://{addr}/?token=t")).await;
    let runner = tokio::spawn({
        let l = link.clone();
        async move { l.run().await }
    });

    // conn1 首帧应为 hello
    let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("等 conn1 首帧超时")
        .expect("通道不应关闭");
    assert_eq!(first["kind"], "hello", "conn1 首帧应为 hello");

    // 断线窗口：客户端进入 backoff 后发 3 帧旧 ping（将积压在 unbounded 通道）
    tokio::time::sleep(Duration::from_millis(600)).await;
    let sender = link.sender().await.expect("run 装配后应有发送端");
    for i in 0..3 {
        sender.send(json!({"kind": "ping", "wxOnline": false, "ts": 9000 + i}));
    }

    // conn2 首帧必须是新 hello（积压 ping 已被排空，不得先于 hello 到达）
    let second = tokio::time::timeout(Duration::from_secs(8), rx.recv())
        .await
        .expect("等 conn2 首帧超时（含 backoff+get_my_info 时长）")
        .expect("通道不应关闭");
    assert_eq!(
        second["kind"], "hello",
        "重连后第一帧必须是 hello 而非积压旧帧，实际: {second}"
    );

    // 后续短窗口内不允许出现积压 ping（只可能是 on_connect 的新事件帧）
    let mut leaked = Vec::new();
    while let Ok(Some(f)) = tokio::time::timeout(Duration::from_millis(800), rx.recv()).await {
        if f["kind"] == "ping" {
            leaked.push(f);
        }
    }
    assert!(leaked.is_empty(), "积压旧 ping 不应到达服务端: {leaked:?}");
    runner.abort();
}

/// event_sink 解耦桥：上行事件（event:*）应同时投给注入的 sink（GUI 模式由
/// Task 9 注入 tauri emit；core 不依赖 tauri）。hello/result/ping 不进 sink。
#[tokio::test]
async fn test_event_sink_receives_upstream_events_only() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();

    // mock server：hello 后下发一条 command（触发 msg.send 的 image 通知）
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        while let Some(Ok(msg)) = ws.next().await {
            if let Message::Text(t) = msg {
                let v: Value = serde_json::from_str(&t).expect("server 收帧应为 JSON");
                let _ = tx.send(v.clone());
                if v["kind"] == "hello" {
                    let cmd = json!({
                        "kind": "command", "requestId": "r1", "action": "send_message",
                        "params": {"who": "张三", "text": "img"}
                    });
                    ws.send(Message::Text(cmd.to_string())).await.unwrap();
                }
            }
        }
    });

    let (link, sink_events) = make_link_with_sink(format!("ws://{addr}/?token=t")).await;
    let runner = tokio::spawn({
        let l = link.clone();
        async move { l.run().await }
    });

    // 等服务端收到 image event（WS 侧对齐后 sink 侧也已投递）
    let mut got = None;
    while got.is_none() {
        let f = tokio::time::timeout(Duration::from_secs(20), rx.recv())
            .await
            .expect("等帧超时")
            .expect("通道不应关闭");
        if f["kind"] == "event" && f["type"] == "message" && f["data"]["msgType"] == "image" {
            got = Some(f);
        }
    }
    runner.abort();

    let sink = sink_events.lock().unwrap();
    // sink 里应有 event:message(image) 与初始 event:status；且只含 kind=event 的帧
    assert!(
        sink.iter().any(|f| f["kind"] == "event"
            && f["type"] == "message"
            && f["data"]["downloadedMedia"] == "C:/tmp/m1.png"),
        "sink 应收到 image event（含二次 RPC 结果），实际: {sink:?}"
    );
    assert!(
        sink.iter().any(|f| f["type"] == "status"),
        "sink 应收到初始 status 事件，实际: {sink:?}"
    );
    assert!(
        sink.iter().all(|f| f["kind"] == "event"),
        "sink 只应收 event 帧（hello/result/ping 不得进入），实际: {sink:?}"
    );
}

// ---------- P1 三态健康探测（承 I1：health_probe 读 payload.online）----------

/// wx.is_online 回 {"online": false}（微信退出、sidecar 存活）→ Offline：
/// 只看 RPC Ok 会恒真（I1），折叠 bool 则与卡死不可分（P1）。
#[tokio::test]
async fn test_probe_health_reads_online_payload_false() {
    // 剧本：wx.is_online 回 online=false（其余随意）
    let script = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    m = req["method"]
    if m == "wx.is_online":
        out = {"online": False}
    else:
        out = {"online": True}
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": out}), flush=True)
"#;
    let handle = SidecarHandle::spawn_with_python(script, &[])
        .await
        .expect("spawn 失败");
    let session = WxSession::new(Arc::new(Mutex::new(handle)));
    assert_eq!(
        session.probe_health().await,
        WxHealth::Offline,
        "wx.is_online 回 online=false 时必须判 Offline（非 Online 防恒真，非 Unreachable 防误报卡死）"
    );
}

/// 对照组：online=true → Online（防止修过头变恒 false）
#[tokio::test]
async fn test_probe_health_reads_online_payload_true() {
    let handle = SidecarHandle::spawn_with_python(SCRIPT, &[])
        .await
        .expect("spawn 失败");
    let session = WxSession::new(Arc::new(Mutex::new(handle)));
    assert_eq!(
        session.probe_health().await,
        WxHealth::Online,
        "wx.is_online 回 online=true 时应判 Online"
    );
}

/// P1 核心剧本：sidecar 收帧后永不应答（UIA 挂起占死单线程的事故形态，
/// 2026-09-10）→ 短超时探测必须判 Unreachable，而不是折叠成「微信掉线」。
#[tokio::test]
async fn test_probe_health_silent_sidecar_is_unreachable() {
    // 剧本：读走请求但不回帧（模拟 dispatch 卡死在 UIA 调用里）
    let script = r#"
import sys
for line in sys.stdin:
    pass  # 吞帧不答：调用方超时
"#;
    let handle = SidecarHandle::spawn_with_python(script, &[])
        .await
        .expect("spawn 失败");
    let session = WxSession::new(Arc::new(Mutex::new(handle)));
    assert_eq!(
        session.probe_health_with(Duration::from_millis(300)).await,
        WxHealth::Unreachable,
        "sidecar 静默无应答必须判 Unreachable（卡死域）——旧 bool 语义下这被误报为微信掉线"
    );
}

// ---------- 修复 I2：hello 前竞态窗（get_my_info 慢时新帧不得先发） ----------

/// on_connect 里 hello 前的 get_my_info 走串行队列+拟人间隙（可达数秒），
/// 此窗口内心跳 ping / 通知泵 event:message 不得先于 hello 发出。
/// 剧本：wx.get_my_info sleep 2s；等 get_my_info 进入后注入一条 event 帧与
/// 一条 ping（经 sender 直发），断言服务端第一帧仍是 hello。
#[tokio::test]
async fn test_hello_gate_holds_frames_during_slow_get_my_info() {
    let script = r#"
import sys, json, time
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    m = req["method"]
    if m == "wx.get_my_info":
        time.sleep(2)
        out = {"licensed": True, "wxid": "wxid_slow", "nickname": "慢速号", "online": True}
    elif m == "wx.is_online":
        out = {"online": True}
    else:
        out = {"method": m}
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": out}), flush=True)
"#;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();

    // mock server：只收帧转发（不下发 command，聚焦 hello-first 语义）
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        while let Some(Ok(m)) = ws.next().await {
            if let Message::Text(t) = m {
                let _ = tx.send(serde_json::from_str(&t).expect("server 帧应为 JSON"));
            }
        }
    });

    let handle = SidecarHandle::spawn_with_python(script, &[])
        .await
        .expect("spawn 失败");
    let session = Arc::new(WxSession::new(Arc::new(Mutex::new(handle))));
    let listeners = Arc::new(ListenerRegistry::new(session.clone()));
    let link = Arc::new(AgentLink::new(
        session,
        listeners,
        format!("ws://{addr}/?token=t"),
    ));

    let runner = tokio::spawn({
        let l = link.clone();
        async move { l.run().await }
    });

    // 等 on_connect 上门闩（get_my_info 2s 慢速窗口内），轮询门闩状态而非盲等
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !link.is_hello_pending() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        link.is_hello_pending(),
        "10s 内 on_connect 应已上门闩（get_my_info 慢速窗口）"
    );

    // 竞态窗口内注入两条「新帧」：经 send_frame 路径的 ping + 经 emit 路径的 event。
    // 门生效期间它们应被缓冲，hello 发出后再 flush。
    link.send_test_frame(json!({"kind": "ping", "wxOnline": false, "ts": 7000}))
        .await;
    link.emit_test_event(json!({
        "kind": "event", "type": "friend_request",
        "data": {"name": "竞态窗", "msg": "m"}, "ts": 7001
    }))
    .await;

    // 第一帧必须是 hello（wxid 来自慢速 get_my_info，证明 hello 等完了它）
    let first = tokio::time::timeout(Duration::from_secs(15), rx.recv())
        .await
        .expect("等第一帧超时")
        .expect("通道不应关闭");
    assert_eq!(
        first["kind"], "hello",
        "慢 get_my_info 期间注入的帧不得先于 hello，实际第一帧: {first}"
    );
    assert_eq!(
        first["wxid"], "wxid_slow",
        "hello 应携带慢速 get_my_info 的结果"
    );

    // hello 之后注入的 ping 与 event 应被 flush（顺序不苛求，但都必须到达）
    let mut got_ping = false;
    let mut got_event = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while !(got_ping && got_event) && std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(1500), rx.recv()).await {
            Ok(Some(f)) => {
                if f["kind"] == "ping" && f["ts"] == 7000 {
                    got_ping = true;
                }
                if f["kind"] == "event" && f["type"] == "friend_request" && f["ts"] == 7001 {
                    got_event = true;
                }
            }
            _ => break,
        }
    }
    assert!(got_ping, "缓冲的 ping 应在 hello 后 flush 到达");
    assert!(got_event, "缓冲的 event 应在 hello 后 flush 到达");
    runner.abort();
}

// ── 终审必修 2b 回归：门闩 armed 不晚于 on_connect 回调返回 ──────────

/// LinkHandler::on_connect 是 sync 回调（spawn 后立即返回）：旧实现把
/// 门闩 armed 放进 spawn 的 async 体里，spawn 调度延迟窗口内到达的帧
/// （on_frame 的 command → result）会先于 armed 判定直接发送，抢在 hello
/// 前面。修复后同步段先 armed——本测试在「ws 连接建立、on_connect 回调
/// 已返回」的最早时刻立即注入一帧，断言它进缓冲（hello 之前不可见）。
/// 用慢速 get_my_info（2s）保证注入必然落在 hello 发出之前。
#[tokio::test]
async fn test_gate_armed_immediately_after_on_connect_callback() {
    let script = r#"
import sys, json, time
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    m = req["method"]
    if m == "wx.get_my_info":
        time.sleep(2)
        out = {"licensed": True, "wxid": "wxid_armed", "nickname": "armed号", "online": True}
    elif m == "wx.is_online":
        out = {"online": True}
    else:
        out = {"method": m}
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": out}), flush=True)
"#;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();

    // mock server：纯收帧转发
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        while let Some(Ok(m)) = ws.next().await {
            if let Message::Text(t) = m {
                let _ = tx.send(serde_json::from_str(&t).expect("server 帧应为 JSON"));
            }
        }
    });

    let handle = SidecarHandle::spawn_with_python(script, &[])
        .await
        .expect("spawn 失败");
    let session = Arc::new(WxSession::new(Arc::new(Mutex::new(handle))));
    let listeners = Arc::new(ListenerRegistry::new(session.clone()));
    let link = Arc::new(AgentLink::new(
        session,
        listeners,
        format!("ws://{addr}/?token=t"),
    ));

    let runner = tokio::spawn({
        let l = link.clone();
        async move { l.run().await }
    });

    // ws_connected 只在 async on_connect 体内置 true（晚于同步 armed），
    // 故不能以它为 armed 信号——以「run 装配完 sender」为界轮询：transport
    // 的 on_connect 回调必然已在本轮询观察到 sender 之后、hello 发出之前
    // （get_my_info 2s 慢速窗口）的某刻执行。这里直接轮询门闩 armed。
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !link.is_hello_pending() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        link.is_hello_pending(),
        "10s 内门闩应已 armed（on_connect 同步段置位）"
    );

    // armed 观察到后立即注入两帧（模拟 spawn 调度延迟窗口内到达的新帧）
    link.send_test_frame(json!({"kind": "ping", "wxOnline": true, "ts": 8100}))
        .await;
    link.emit_test_event(json!({
        "kind": "event", "type": "friend_request",
        "data": {"name": "armed窗", "msg": "m"}, "ts": 8101
    }))
    .await;

    // 第一帧必须是 hello；缓冲帧在 hello 后到达
    let first = tokio::time::timeout(Duration::from_secs(15), rx.recv())
        .await
        .expect("等第一帧超时")
        .expect("通道不应关闭");
    assert_eq!(
        first["kind"], "hello",
        "on_connect 后立即注入的帧不得先于 hello，实际第一帧: {first}"
    );
    let mut got_ping = false;
    let mut got_event = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while !(got_ping && got_event) && std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(1500), rx.recv()).await {
            Ok(Some(f)) => {
                if f["kind"] == "ping" && f["ts"] == 8100 {
                    got_ping = true;
                }
                if f["kind"] == "event" && f["ts"] == 8101 {
                    got_event = true;
                }
            }
            _ => break,
        }
    }
    assert!(got_ping, "缓冲的 ping 应在 hello 后 flush 到达");
    assert!(got_event, "缓冲的 event 应在 hello 后 flush 到达");
    runner.abort();
}

// ── eventId 生成与组帧（listen-persist-event-outbox Task 4）──

mod event_id_tests {
    use super::*;
    use wxauto_desktop::agent_link::inbound::next_event_id;

    /// eventId 形状 evt-{ms}-{seq} 且进程内单调（同毫秒 seq 递增保证字典序单调）
    #[test]
    fn test_next_event_id_shape_and_monotonic() {
        let a = next_event_id();
        let b = next_event_id();
        assert!(a.starts_with("evt-"), "前缀: {a}");
        assert!(b > a, "序号递增保证字典序单调: {a} < {b}");
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(parts.len(), 3, "evt-{{ms}}-{{seq}} 三段: {a}");
        assert!(parts[1].parse::<u64>().is_ok(), "ms 段应为数字: {a}");
        assert!(parts[2].parse::<u64>().is_ok(), "seq 段应为数字: {a}");
    }

    /// 三种 event 组帧函数输出帧带 eventId
    #[test]
    fn test_event_frames_carry_event_id() {
        let note = json!({
            "msg_id": "m1", "chat_who": "张三", "chat_type": "friend", "attr": "friend",
            "msg_type": "text", "sender": "张三", "content": "hi"
        });
        let f = message_event_from_notification(&note, None, None);
        assert!(f["eventId"].as_str().unwrap_or("").starts_with("evt-"));

        let fr = friend_request_event("王五", "请求添加好友");
        assert!(fr["eventId"].as_str().unwrap_or("").starts_with("evt-"));

        let st = status_event(true, "online", 1, true);
        assert!(st["eventId"].as_str().unwrap_or("").starts_with("evt-"));
    }
}
