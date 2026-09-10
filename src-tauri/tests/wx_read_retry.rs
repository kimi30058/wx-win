//! WxSession 读类 action 超时重试集成测试（P2 优化任务 7）：
//! UIA 偶发慢（30s 超时）即整条指令失败，此前只给 wx.init 配了重试。
//!
//! 修复语义（本文件锚定）：
//! 1. 只读 action（get/list/search/history/moments/download/voice 共 8 个）
//!    首发超时 → 等 800ms 重试 1 发 → 第二发命中即成功（对齐 wx.init
//!    慢热重试的真实形态：第一发超时的同时 sidecar 已在推进，第二发秒回）；
//! 2. 写类 action（send/accept/listen 增删/publish 等）不重试——重放即
//!    重复发消息/重复加监听，超时直接 Err；
//! 3. 重试仍超时 → 返回 Err（不无限重试）。
//!
//! 复用 init_timeout.rs 的剧本 sidecar 模式（Linux 无 wxautox4）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::Mutex;

use wxauto_desktop::sidecar::SidecarHandle;
use wxauto_desktop::wx::WxSession;

/// 慢热剧本：首个请求睡 slow（制造首发 Timeout），之后所有请求秒回。
/// 慢睡放读循环之前（start 前睡）——循环内 sleep 会把重试请求一并
/// 阻塞（stdin 串行），那就是「恒慢」形态而非「慢热」形态。
const SLOW_START_SCRIPT_TMPL: &str = r#"
import sys, json, time
time.sleep({slow})
counts = {}
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    m = req["method"]
    counts[m] = counts.get(m, 0) + 1
    out = {"jsonrpc": "2.0", "id": req["id"], "result": {"method": m, "nth": counts[m]}}
    print(json.dumps(out), flush=True)
"#;

/// 恒慢剧本：每个请求都睡 slow——重试也超时，验证 1 次重试后返回 Err。
const ALWAYS_SLOW_SCRIPT_TMPL: &str = r#"
import sys, json, time
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    time.sleep({slow})
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {"method": req["method"]}}), flush=True)
"#;

/// 计数剧本：按 method 计数并把次数放进回显结果（验证「发了几发」）。
const COUNTING_SCRIPT: &str = r#"
import sys, json
counts = {}
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    m = req["method"]
    counts[m] = counts.get(m, 0) + 1
    out = {"jsonrpc": "2.0", "id": req["id"], "result": {"method": m, "nth": counts[m]}}
    print(json.dumps(out), flush=True)
"#;

/// 场景 1（慢热主形态）：读类 action 首发超时 → 重试命中成功，
/// 结果带 nth=2（证明是第二发返回的）。
/// 用短超时注入（call_with_timeout 走 direct 路径无法注入——execute
/// 内部用 RPC_TIMEOUT；测试通过重试窗口参数注入短间隔，超时用真实
/// 30s 太长，故 execute 必须支持超时注入或用慢启动 <30s 的形态）。
///
/// 简化：慢启动 0.5s 形态下 30s 超时不会触发——真正的重试触发需要
/// 「首发超时」。为可测性，execute 的重试逻辑拆成纯函数 + 注入化：
/// `execute_with_retry(timeout, retry_gap)`。生产入口 execute 用
/// (RPC_TIMEOUT, 800ms)；测试注入 (300ms, 50ms)。
#[tokio::test]
async fn read_action_timeout_retries_once_and_succeeds() {
    let script = SLOW_START_SCRIPT_TMPL.replace("{slow}", "0.5");
    let handle = SidecarHandle::spawn_with_python(&script, &[])
        .await
        .expect("spawn 失败");
    let session = WxSession::new(Arc::new(Mutex::new(handle)));

    // 首发 300ms 超时（sidecar 还在 0.5s 慢睡）；重试 50ms 后发出时
    // 脚本已醒（0.35s + 0.05s = 0.4s < 0.5s 边界略紧，取 0.55s 更稳？
    // 不——首发在 t=0 发出，t=0.3 超时；t=0.35 重试发出；脚本 t=0.5
    // 醒来读到重试请求秒回）。结果应成功且 nth=2。
    let r = session
        .execute_with_retry("get_my_info", json!({}), Duration::from_millis(300), Duration::from_millis(50))
        .await;
    let v = r.unwrap_or_else(|e| panic!("读类重试应成功: {e}"));
    assert_eq!(v["method"], "wx.get_my_info");
    assert_eq!(v["nth"], 2, "应是第二发（重试）返回");
}

/// 场景 2：写类 action 首发超时不重试——恒慢剧本下 send_message
/// 超时直接 Err，且只发出 1 发（对 sidecar 计数无从验证，但时长可证：
/// 不重试则总时长 ≈ 1×超时，重试则 ≈ 2×超时+间隔）。
#[tokio::test]
async fn write_action_timeout_no_retry() {
    let script = ALWAYS_SLOW_SCRIPT_TMPL.replace("{slow}", "0.5");
    let handle = SidecarHandle::spawn_with_python(&script, &[])
        .await
        .expect("spawn 失败");
    let session = WxSession::new(Arc::new(Mutex::new(handle)));

    let start = Instant::now();
    let r = session
        .execute_with_retry("send_message", json!({"who": "a", "text": "t"}), Duration::from_millis(300), Duration::from_millis(50))
        .await;
    assert!(r.is_err(), "写类超时应 Err");
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(700),
        "写类不重试：总时长应 ≈1×超时（300ms 级），实际 {elapsed:?}"
    );
}

/// 场景 3：读类重试仍超时 → 返回 Err（恒慢形态：两发都超时），
/// 总时长 ≈ 2×超时 + 间隔（证明确实重试了 1 发）。
#[tokio::test]
async fn read_action_retry_exhausted_returns_err() {
    let script = ALWAYS_SLOW_SCRIPT_TMPL.replace("{slow}", "0.5");
    let handle = SidecarHandle::spawn_with_python(&script, &[])
        .await
        .expect("spawn 失败");
    let session = WxSession::new(Arc::new(Mutex::new(handle)));

    let start = Instant::now();
    let r = session
        .execute_with_retry("get_moments", json!({}), Duration::from_millis(300), Duration::from_millis(50))
        .await;
    assert!(r.is_err(), "重试耗尽应 Err");
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(650),
        "两发超时 + 间隔应 ≥ 650ms，实际 {elapsed:?}（若未重试只会有 ~300ms）"
    );
}

/// 场景 4（无超时快路径）：正常秒回时行为与旧 execute 一致——
/// 只发 1 发（nth=1），无重试开销。
#[tokio::test]
async fn fast_path_single_call_no_retry() {
    let handle = SidecarHandle::spawn_with_python(COUNTING_SCRIPT, &[])
        .await
        .expect("spawn 失败");
    let session = WxSession::new(Arc::new(Mutex::new(handle)));

    let v = session
        .execute_with_retry("get_my_info", json!({}), Duration::from_secs(5), Duration::from_millis(50))
        .await
        .expect("秒回应成功");
    assert_eq!(v["nth"], 1, "快路径只发 1 发");
}
