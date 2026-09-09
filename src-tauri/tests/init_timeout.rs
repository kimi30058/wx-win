//! wx.init 超时恢复集成测试（2026-09-09 真机事故）：
//! 冻结包冷启动 ~9s 吃满 INIT_TIMEOUT(10s) → Rust 超时放弃，但 sidecar
//! 活着、Supervisor 等不到进程退出 → 前端永卡「正在检测授权状态…」。
//!
//! 修复语义（本文件锚定）：
//! 1. Timeout 重试：第一次超时后隔 gap 再试——第二次 sidecar 已热，
//!    秒回 wechat_missing → init_fail 帧正常发出，sidecar 不被杀；
//! 2. Timeout 耗尽：重试全超时 → shutdown 当前 sidecar，交给 Supervisor
//!    主循环既有的 await_exit → 退避重启路径（不是永远干等）；
//! 3. Sidecar 业务错误帧 / Io / Closed 不重试（一个是确定性失败，两个是
//!    进程级死亡，重试无意义）。
//!
//! 复用 license_init.rs 的剧本 sidecar 模式（Linux 无 wxautox4）。

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Mutex;

use wxauto_desktop::sidecar::SidecarHandle;
use wxauto_desktop::state::{AppState, AppStateMachine, Supervisor};
use wxauto_desktop::wx::listener::ListenerRegistry;
use wxauto_desktop::wx::WxSession;

/// 慢热剧本：首个 wx.init 睡 slow（超时窗口内不回，制造 Timeout），
/// 之后所有 wx.init 调用秒回 licensed=true + wechat_missing——模拟
/// 「第一次超时的时间里 Python 完成导入，第二次调用秒返」的真实冷启动
/// 形态。慢睡必须放读循环之前（start 前睡）：若在循环内 sleep 会连后续
/// 请求一并阻塞（stdin 逐行串行处理，重试请求要排队等上一发睡完），
/// 那是「恒慢」形态而非「慢热」形态。
const SLOW_THEN_MISSING_SCRIPT_TMPL: &str = r#"
import sys, json, time
time.sleep({slow})
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    if req.get("method") == "wx.init":
        out = {"licensed": True, "wxid": "", "nickname": "",
               "failReason": "wechat_missing", "failDetail": "RuntimeError: 慢热模拟"}
    elif req.get("method") == "wx.is_online":
        out = {"online": True}
    else:
        out = {"ok": True}
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": out}), flush=True)
"#;

/// 恒慢剧本：每次 wx.init 都睡 slow——重试全部超时，验证耗尽后杀 sidecar。
const ALWAYS_SLOW_SCRIPT_TMPL: &str = r#"
import sys, json, time
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    if req.get("method") == "wx.init":
        time.sleep({slow})
        out = {"licensed": True, "wxid": "x", "nickname": "n"}
    else:
        out = {"ok": True}
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": out}), flush=True)
"#;

/// 装配剧本 sidecar + Supervisor（毫秒级 init 超时/重试注入 + event_sink
/// 收集帧；不跑 run 循环，直接驱动 retry_init——确定性无退避时序干扰）
async fn make_supervisor_with_script(
    script: &str,
) -> (
    Arc<Supervisor>,
    Arc<AppStateMachine>,
    Arc<StdMutex<Vec<Value>>>,
    Arc<WxSession>,
) {
    let handle = SidecarHandle::spawn_with_python(script, &[])
        .await
        .expect("spawn 失败");
    let session = Arc::new(WxSession::new(Arc::new(Mutex::new(handle))));
    let listeners = Arc::new(ListenerRegistry::new(session.clone()));
    let state = Arc::new(AppStateMachine::new());
    let events: Arc<StdMutex<Vec<Value>>> = Arc::new(StdMutex::new(Vec::new()));
    let ev = events.clone();
    let sup = Supervisor::new(state.clone(), session.clone(), listeners)
        .with_event_sink(Box::new(move |v: Value| {
            ev.lock().unwrap().push(v);
        }))
        // 测试注入：init 超时 300ms、重试 2 次间隔 50ms（生产 10s/1+2 次/5s）
        .with_init_retry(Duration::from_millis(300), [Duration::from_millis(50); 2]);
    (Arc::new(sup), state, events, session)
}

/// 场景 1（事故主形态）：首次超时 → 重试命中 wechat_missing →
/// init_fail 帧发出 + 快照写入 + sidecar 不被杀（退出观察通道无信号）
#[tokio::test]
async fn timeout_retry_recovers_wechat_missing_frame() {
    // 启动前睡 0.5s：> 首发超时 300ms（第一发 Timeout），且 < 第二发
    // 完成窗口 0.65s（300ms 超时 + 50ms 间隔）——第二发请求到达时脚本
    // 已醒来，秒回。注意 3 发总尝试窗口 1.0s，sleep 若设 2s 会把三发
    // 全拖超时变成耗尽场景（场景 2），0.5s 才是「慢热」形态。
    let script = SLOW_THEN_MISSING_SCRIPT_TMPL.replace("{slow}", "0.5");
    let (sup, state, events, session) = make_supervisor_with_script(&script).await;

    sup.retry_init().await;

    assert_eq!(
        state.state().await,
        AppState::SidecarBooting,
        "wechat_missing 应留 Booting"
    );
    {
        let frames = events.lock().unwrap();
        let hit = frames
            .iter()
            .any(|f| f["type"] == "init_fail" && f["data"]["reason"] == "wechat_missing");
        assert!(hit, "重试命中后应收 init_fail 帧, 实收: {frames:?}");
    }
    // sidecar 仍活着（未被误杀）：退出观察通道此刻无退出信号
    let exit_rx = session.sidecar_exit_watcher().await;
    assert!(exit_rx.borrow().is_none(), "重试命中的超时不允许杀 sidecar");
    let snap = sup.last_init_fail().await;
    assert_eq!(
        snap.as_ref().map(|s| s.reason.as_str()),
        Some("wechat_missing"),
        "快照应含 wechat_missing（前端 get_init_fail_reason 兜底源）"
    );
}

/// 场景 1b：快路径回归——秒回剧本在注入小超时下单发命中（不进重试逻辑
/// 也不误伤），既有三态语义零变化
#[tokio::test]
async fn fast_response_short_circuits_without_retry() {
    // slow=0：首个 wx.init 也秒回
    let script = SLOW_THEN_MISSING_SCRIPT_TMPL.replace("{slow}", "0");
    let (sup, state, events, session) = make_supervisor_with_script(&script).await;

    sup.retry_init().await;

    assert_eq!(state.state().await, AppState::SidecarBooting);
    {
        let frames = events.lock().unwrap();
        assert!(
            frames
                .iter()
                .any(|f| f["type"] == "init_fail" && f["data"]["reason"] == "wechat_missing"),
            "秒回剧本应收 init_fail 帧, 实收: {frames:?}"
        );
    }
    let exit_rx = session.sidecar_exit_watcher().await;
    assert!(exit_rx.borrow().is_none(), "秒回场景无关杀进程");
}

/// 场景 2：重试全超时 → shutdown 当前 sidecar（退出观察通道收到信号），
/// 状态留 Booting、不写授权失败快照（transport 级不谎报）
#[tokio::test]
async fn timeout_exhaustion_shuts_down_sidecar() {
    // 每次 wx.init 都睡 2s：300ms 超时 × 3 次尝试（1+2 重试）全超时
    let script = ALWAYS_SLOW_SCRIPT_TMPL.replace("{slow}", "2");
    let (sup, state, _events, session) = make_supervisor_with_script(&script).await;

    // retry_init 在耗尽路径必须先返回（不能永挂）：外包一层兜底超时
    let done = tokio::time::timeout(Duration::from_secs(15), sup.retry_init()).await;
    assert!(done.is_ok(), "超时耗尽路径必须返回，不得永挂");

    assert_eq!(
        state.state().await,
        AppState::SidecarBooting,
        "transport 级超时不改状态"
    );
    assert_eq!(
        sup.last_init_fail().await,
        None,
        "超时耗尽不得写授权失败快照（归重启域）"
    );
    // 核心断言：sidecar 被杀——await_exit 返回 Some
    let mut exit_rx = session.sidecar_exit_watcher().await;
    let exited = tokio::time::timeout(
        Duration::from_secs(5),
        SidecarHandle::await_exit(&mut exit_rx),
    )
    .await
    .expect("等 sidecar 退出信号超时");
    assert!(exited.is_some(), "超时耗尽必须 shutdown sidecar 交重启循环");
}

/// 场景 3：RPC 错误帧（Sidecar 变体）不进超时重试——一次即走 init_fail
/// 链路（重试只救「活着但慢」，确定性业务失败重试无意义还拖时）
#[tokio::test]
async fn rpc_error_frame_skips_timeout_retry() {
    let script = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    if req.get("method") == "wx.init":
        err = {"code": -32603, "message": "ModuleNotFoundError: No module named 'requests'"}
        print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "error": err}), flush=True)
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {"ok": True}}), flush=True)
"#;
    let (sup, _state, _events, session) = make_supervisor_with_script(script).await;

    let start = tokio::time::Instant::now();
    sup.retry_init().await;
    let elapsed = start.elapsed();
    // 无 300ms 超时也无需 50ms×2 重试间隙：错误帧即时返回
    assert!(
        elapsed < Duration::from_millis(250),
        "Sidecar 错误帧应即时返回不重试, 实耗 {elapsed:?}"
    );
    let exit_rx = session.sidecar_exit_watcher().await;
    assert!(exit_rx.borrow().is_none(), "业务错误帧不允许杀 sidecar");
}
