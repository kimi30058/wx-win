//! sidecar stderr 接管测试（Task 3）：sink 注入 → stderr piped 逐行消费；
//! 未注入 → 行为不变（stderr inherit，本测试无法断言 inherit，只断言
//! spawn/call 正常——CLI 零回归由 smoke_cli.sh 兜底）。

use std::sync::{Arc, Mutex};

use wxauto_desktop::sidecar::{SidecarHandle, StderrSink};

/// 收集型 sink（模拟 bin 侧 RingStderrSink）
struct VecSink(Mutex<Vec<String>>);

impl StderrSink for VecSink {
    fn consume(&self, line: String) {
        self.0.lock().unwrap().push(line);
    }
}

/// 脚本：stdout 回 JSON-RPC 响应；stderr 打日志行（含一条 Traceback）
const SCRIPT: &str = r#"
import sys, json
print("sidecar 启动", file=sys.stderr, flush=True)
print("Traceback (most recent call last):", file=sys.stderr, flush=True)
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {"ok": True}}), flush=True)
"#;

#[tokio::test]
async fn test_stderr_lines_reach_injected_sink() {
    let sink = Arc::new(VecSink(Mutex::new(Vec::new())));
    let mut handle = SidecarHandle::spawn_with_python_sunk(SCRIPT, &[], Some(sink.clone()))
        .await
        .expect("spawn 失败");
    // 触发一轮 RPC（确保进程跑起来 + stderr 已刷）
    let resp = handle
        .call("wx.get_my_info", serde_json::json!({}))
        .await
        .expect("RPC 失败");
    assert_eq!(resp["ok"], true);
    // stderr 行异步到达：轮询上限 5s
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let n = sink.0.lock().unwrap().len();
        if n >= 2 || std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let lines = sink.0.lock().unwrap().clone();
    assert!(
        lines.iter().any(|l| l.contains("sidecar 启动")),
        "实际: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("Traceback")),
        "Traceback 行应到达: {lines:?}"
    );
    handle.shutdown().await;
}

#[tokio::test]
async fn test_spawn_without_sink_still_works() {
    // 未注入 sink：stderr inherit（原行为），RPC 正常
    let mut handle = SidecarHandle::spawn_with_python(SCRIPT, &[])
        .await
        .expect("spawn 失败");
    let resp = handle
        .call("wx.get_my_info", serde_json::json!({}))
        .await
        .expect("RPC 失败");
    assert_eq!(resp["ok"], true);
    handle.shutdown().await;
}
