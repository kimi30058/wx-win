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

// ══════════ I1：init_fail 快照缓存（事件先于前端 listen 注册发出的兜底） ══════════

/// 未授权 init 后 last_init_fail 缓存 reason（前端 init 经
/// get_init_fail_reason 命令拉快照——tauri 事件无重放，每 sidecar 世代
/// 只发一次 init_fail，listen 注册晚于 emit 即永久丢失，授权灯恒灰）
#[tokio::test]
async fn last_init_fail_caches_unlicensed_reason() {
    let (sup, _state, _events) = make_supervisor(UNLICENSED_SCRIPT).await;
    sup.retry_init().await;
    assert_eq!(
        sup.last_init_fail().await.as_deref(),
        Some("licensed"),
        "未授权 init 后应缓存 reason=licensed 供前端快照兜底"
    );
}

/// 微信未开剧本同样缓存 wechat_missing
#[tokio::test]
async fn last_init_fail_caches_wechat_missing_reason() {
    let (sup, _state, _events) = make_supervisor(WECHAT_MISSING_SCRIPT).await;
    sup.retry_init().await;
    assert_eq!(
        sup.last_init_fail().await.as_deref(),
        Some("wechat_missing")
    );
}

/// 成功路径清缓存：授权通过推进 WxInit 前 last_init_fail 置 None——
/// 前端拉快照不再看到陈旧的失败原因（授权灯正常转绿）
#[tokio::test]
async fn last_init_fail_cleared_on_success() {
    // 先用未授权剧本缓存 reason，再换授权剧本重跑——验证成功路径清 None
    let (sup, _state, _events) = make_supervisor(READY_SCRIPT).await;
    sup.retry_init().await;
    assert_eq!(
        sup.last_init_fail().await,
        None,
        "授权通过（推进 WxInit 前）应清空 last_init_fail"
    );
}

/// RPC 失败（sidecar 死）不写缓存：那属于 Supervisor 重启域，
/// get_init_fail_reason 快照必须保持 None（不得谎报授权失败）
#[tokio::test]
async fn last_init_fail_not_set_on_rpc_error() {
    // 直接对死会话构造 Supervisor：spawn 一个立即退出的「sidecar」
    let die_script = r#"
import sys
sys.stdin.read()
"#;
    let handle = SidecarHandle::spawn_with_python(die_script, &[])
        .await
        .expect("spawn 失败");
    // 先关 stdin 让子进程退出 → 会话句柄死亡
    let session = Arc::new(WxSession::new(Arc::new(Mutex::new(handle))));
    drop(session.clone());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let listeners = Arc::new(ListenerRegistry::new(session.clone()));
    let state = Arc::new(AppStateMachine::new());
    let sup = Supervisor::new(state.clone(), session, listeners);
    sup.retry_init().await;
    assert_eq!(
        sup.last_init_fail().await,
        None,
        "RPC 失败（init.is_none()）不得写 last_init_fail——sidecar 死亡归重启域"
    );
}
