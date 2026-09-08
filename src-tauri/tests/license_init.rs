//! 激活链路集成测试：init 失败三态 → init_fail 帧；retry_init 闭环。
//! 复用 state_machine.rs 的剧本 sidecar 策略（Linux 无 wxautox4）。

use std::sync::{Arc, Mutex as StdMutex};

use serde_json::Value;
use tokio::sync::Mutex;

use wxauto_desktop::sidecar::SidecarHandle;
use wxauto_desktop::state::{AppState, AppStateMachine, Supervisor};
use wxauto_desktop::wx::listener::ListenerRegistry;
use wxauto_desktop::wx::WxSession;

/// 未授权剧本：wx.init 回 licensed=false + failReason=licensed
const UNLICENSED_SCRIPT: &str = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    if req.get("method") == "wx.init":
        out = {"licensed": False, "wxid": "", "nickname": "", "failReason": "licensed"}
    elif req.get("method") == "wx.is_online":
        out = {"online": True}
    else:
        out = {"ok": True}
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": out}), flush=True)
"#;

/// 微信未开剧本：licensed=true + failReason=wechat_missing
const WECHAT_MISSING_SCRIPT: &str = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    if req.get("method") == "wx.init":
        out = {"licensed": True, "wxid": "", "nickname": "", "failReason": "wechat_missing"}
    elif req.get("method") == "wx.is_online":
        out = {"online": True}
    else:
        out = {"ok": True}
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": out}), flush=True)
"#;

/// 就绪剧本：wx.init 回 licensed=true（无 failReason）
const READY_SCRIPT: &str = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    if req.get("method") == "wx.init":
        out = {"licensed": True, "wxid": "wxid_t", "nickname": "测试"}
    elif req.get("method") == "wx.is_online":
        out = {"online": True}
    else:
        out = {"ok": True}
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": out}), flush=True)
"#;

/// 装配剧本 sidecar + Supervisor（event_sink 收集帧；不跑 run 循环，
/// 直接驱动 retry_init——确定性无退避时序干扰）
async fn make_supervisor(
    script: &str,
) -> (
    Arc<Supervisor>,
    Arc<AppStateMachine>,
    Arc<StdMutex<Vec<Value>>>,
) {
    let handle = SidecarHandle::spawn_with_python(script, &[])
        .await
        .expect("spawn 失败");
    let session = Arc::new(WxSession::new(Arc::new(Mutex::new(handle))));
    let listeners = Arc::new(ListenerRegistry::new(session.clone()));
    let state = Arc::new(AppStateMachine::new());
    let events: Arc<StdMutex<Vec<Value>>> = Arc::new(StdMutex::new(Vec::new()));
    let ev = events.clone();
    let sup = Supervisor::new(state.clone(), session, listeners).with_event_sink(Box::new(
        move |v: Value| {
            ev.lock().unwrap().push(v);
        },
    ));
    (Arc::new(sup), state, events)
}

#[tokio::test]
async fn unlicensed_init_stays_booting_and_emits_init_fail() {
    let (sup, state, events) = make_supervisor(UNLICENSED_SCRIPT).await;
    sup.retry_init().await;
    assert_eq!(
        state.state().await,
        AppState::SidecarBooting,
        "未授权必须留 Booting"
    );
    let frames = events.lock().unwrap();
    let hit = frames
        .iter()
        .any(|f| f["type"] == "init_fail" && f["data"]["reason"] == "licensed");
    assert!(hit, "应收 init_fail 帧 reason=licensed，实收: {frames:?}");
}

#[tokio::test]
async fn wechat_missing_init_stays_booting_with_reason() {
    let (sup, state, events) = make_supervisor(WECHAT_MISSING_SCRIPT).await;
    sup.retry_init().await;
    // licensed=true 但微信未开：同样不可进 WxInit
    assert_eq!(state.state().await, AppState::SidecarBooting);
    let frames = events.lock().unwrap();
    let hit = frames
        .iter()
        .any(|f| f["type"] == "init_fail" && f["data"]["reason"] == "wechat_missing");
    assert!(
        hit,
        "应收 init_fail 帧 reason=wechat_missing，实收: {frames:?}"
    );
}

#[tokio::test]
async fn ready_script_retry_init_reaches_ready() {
    let (sup, state, _events) = make_supervisor(READY_SCRIPT).await;
    sup.retry_init().await;
    assert_eq!(state.state().await, AppState::Ready, "授权+微信在 → Ready");
}
