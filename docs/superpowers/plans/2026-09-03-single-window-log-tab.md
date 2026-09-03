# desktop 单窗口化 + 运行日志 tab 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 消除 Windows 上 cmd 样式日志控制台窗口（App 只剩 Tauri 主窗口），并在主窗口内新增「运行日志」tab 实时查看 Rust + sidecar 日志（级别/来源过滤）。

**Architecture:** Rust 侧新增 `ui_log.rs`（`AppLogEntry` + `LogRing` 环形缓冲 + `UiLogLayer` tracing Layer + sidecar stderr 读循环），tracing 初始化改 registry 组合（fmt 控制台层保留 + UI 层新增）；sidecar stderr 从 `inherit()` 改 `piped()`（仅注入 sink 时）。前端 store 加 `appLog` 状态 + 新视图 `AppLog.vue`。spec 见 `docs/superpowers/specs/2026-09-03-single-window-log-tab-design.md`。

**Tech Stack:** Tauri 2 / Rust（tracing + tracing-subscriber 0.3 registry）/ Vue 3 + pinia + TDesign / tokio

## Global Constraints

- 所有代码注释与用户可见文案用简体中文（仓内既有风格）。
- Rust 运行时路径禁 `unwrap()`/`expect()`（测试代码除外）。
- TypeScript 禁 `any`/`as any`，用 `unknown` + 类型守卫（既有 `isRecord`/`str`/`num` 模式）。
- CLI 模式（`--cli`）行为零回归：不注入 log sink 时 sidecar stderr 仍 `inherit()`，`scripts/smoke_cli.sh` 必须绿。
- 事件契约 payload：`{ ts: number, level: 'error'|'warn'|'info'|'debug'|'trace', source: 'rust'|'sidecar', message: string }`，字段名与前端守卫精确一致。
- 环形上限：Rust `LogRing` 1000 条；mpsc 通道 256，满则丢行计数（不反压业务）。
- 每个任务以 `cargo test`（或对应验证命令）绿 + commit 收尾。工作目录均在 `/home/working/lyagent/desktop`。
- 本仓是独立 git 仓（分支 main，勿与主 checkout lyagent 混淆）。

---

### Task 1: `AppLogEntry` + `LogRing` 环形缓冲（Rust 核心数据结构）

**Files:**
- Create: `src-tauri/src/ui_log.rs`
- Modify: `src-tauri/src/main.rs`（挂 `mod ui_log;`）

**Interfaces:**
- Consumes: 无（纯新单元）
- Produces（后续任务依赖的确切签名）:
  - `pub struct AppLogEntry { pub ts: u64, pub level: AppLogLevel, pub source: AppLogSource, pub message: String }`（derive `Debug, Clone, serde::Serialize`；serde camelCase 字段名 `ts/level/source/message` 本就无下划线，直出）
  - `pub enum AppLogLevel { Error, Warn, Info, Debug, Trace }`（derive Serialize，`#[serde(rename_all = "lowercase")]`）
  - `pub enum AppLogSource { Rust, Sidecar }`（同上）
  - `impl AppLogSource { pub fn as_str(&self) -> &'static str }`
  - `impl AppLogLevel { pub fn as_str(&self) -> &'static str }`
  - `pub struct LogRing(Arc<std::sync::Mutex<VecDeque<AppLogEntry>>>)` + `impl Clone`
  - `impl LogRing { pub fn new(cap: usize) -> Self; pub fn push(&self, entry: AppLogEntry); pub fn snapshot(&self) -> Vec<AppLogEntry>; pub fn clear(&self) }`

**模块定位说明（给实现者）**：`main.rs` 头部现有 `mod app_state; mod commands; mod gui; mod ui_events;` 四行模块声明，`ui_log` 加入其中。`ui_log.rs` 是 bin 侧模块（依赖 tauri 的 UiEventBridge 引用在 Task 4 才加，本任务只做纯数据结构，不引 tauri）。

- [ ] **Step 1: 写失败测试（环形淘汰 + snapshot 顺序）**

在 `src-tauri/src/ui_log.rs` 底部写（先创建文件，只含测试 + 空 lib 结构会编译失败——TDD 红）：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_ring_evicts_oldest_beyond_capacity() {
        let ring = LogRing::new(3);
        for i in 0..5 {
            ring.push(AppLogEntry {
                ts: i,
                level: AppLogLevel::Info,
                source: AppLogSource::Rust,
                message: format!("m{i}"),
            });
        }
        let snap = ring.snapshot();
        assert_eq!(snap.len(), 3, "超限后只留 cap 条");
        assert_eq!(snap[0].message, "m2", "最旧的 m0/m1 被弹掉");
        assert_eq!(snap[2].message, "m4");
    }

    #[test]
    fn test_log_ring_snapshot_returns_oldest_first() {
        let ring = LogRing::new(10);
        ring.push(AppLogEntry {
            ts: 1,
            level: AppLogLevel::Warn,
            source: AppLogSource::Sidecar,
            message: "先".into(),
        });
        ring.push(AppLogEntry {
            ts: 2,
            level: AppLogLevel::Error,
            source: AppLogSource::Rust,
            message: "后".into(),
        });
        let snap = ring.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].message, "先", "旧在前（前端头插展示用）");
        assert_eq!(snap[1].message, "后");
    }

    #[test]
    fn test_log_ring_clear_empties() {
        let ring = LogRing::new(10);
        ring.push(AppLogEntry {
            ts: 1,
            level: AppLogLevel::Info,
            source: AppLogSource::Rust,
            message: "x".into(),
        });
        ring.clear();
        assert!(ring.snapshot().is_empty());
    }

    #[test]
    fn test_log_ring_clone_shares_storage() {
        let ring = LogRing::new(10);
        let clone = ring.clone();
        ring.push(AppLogEntry {
            ts: 1,
            level: AppLogLevel::Info,
            source: AppLogSource::Rust,
            message: "x".into(),
        });
        assert_eq!(clone.snapshot().len(), 1, "Clone 共享同一内部槽位");
    }

    #[test]
    fn test_app_log_entry_serde_fields_match_frontend_contract() {
        let e = AppLogEntry {
            ts: 1693700000000,
            level: AppLogLevel::Error,
            source: AppLogSource::Sidecar,
            message: "boom".into(),
        };
        let j = serde_json::to_value(&e).expect("测试可 expect");
        assert_eq!(j["ts"], 1693700000000);
        assert_eq!(j["level"], "error");
        assert_eq!(j["source"], "sidecar");
        assert_eq!(j["message"], "boom");
    }
}
```

- [ ] **Step 2: 跑测试确认红**

Run: `cd /home/working/lyagent/desktop/src-tauri && cargo test --bin wxauto-desktop ui_log 2>&1 | tail -5`
Expected: 编译错误（`AppLogEntry`/`LogRing` 未定义）。

- [ ] **Step 3: 写最小实现**

`src-tauri/src/ui_log.rs` 顶部（测试模块之上）：

```rust
//! 运行日志捕获（单窗口化改造）：内存环形缓冲 + tracing Layer + sidecar
//! stderr 读循环。GUI 的「运行日志」tab 数据源；CLI 模式完全不装配本模块
//! 的捕获链（stderr 仍 inherit，行为零回归）。
//!
//! 设计（spec 2026-09-03）：
//! - LogRing：Mutex<VecDeque>，上限淘汰最旧；snapshot 旧在前
//! - UiLogLayer（Task 2）：tracing 事件 → ring + mpsc → 桥 emit
//! - sidecar stderr（Task 3）：piped 逐行读 → 同一 ring（source=sidecar）

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// 日志级别（前端契约：小写字符串）
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AppLogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl AppLogLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            AppLogLevel::Error => "error",
            AppLogLevel::Warn => "warn",
            AppLogLevel::Info => "info",
            AppLogLevel::Debug => "debug",
            AppLogLevel::Trace => "trace",
        }
    }
}

/// 日志来源：rust=应用自身 / sidecar=Python 子进程 stderr
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AppLogSource {
    Rust,
    Sidecar,
}

impl AppLogSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            AppLogSource::Rust => "rust",
            AppLogSource::Sidecar => "sidecar",
        }
    }
}

/// 一条运行日志（`wxauto://app-log` 事件载荷，字段即前端契约）
#[derive(Debug, Clone, serde::Serialize)]
pub struct AppLogEntry {
    pub ts: u64,
    pub level: AppLogLevel,
    pub source: AppLogSource,
    pub message: String,
}

/// 环形缓冲：内部 Mutex<VecDeque>，满弹最旧。Clone 共享同一槽位
/// （tracing Layer / sidecar 读循环 / invoke 快照各持一份克隆）。
#[derive(Clone)]
pub struct LogRing {
    inner: Arc<Mutex<VecDeque<AppLogEntry>>>,
    cap: usize,
}

impl LogRing {
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::new())),
            cap,
        }
    }

    /// 追加一条；超上限弹最旧。锁中毒（panic 传染）按清空恢复——
    /// 日志缓冲是旁路观察者，不允许它把业务线程拖死。
    pub fn push(&self, entry: AppLogEntry) {
        let mut q = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if q.len() >= self.cap {
            q.pop_front();
        }
        q.push_back(entry);
    }

    /// 全量快照（旧在前；前端头插展示）
    pub fn snapshot(&self) -> Vec<AppLogEntry> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    /// 清空（前端「清空」按钮）
    pub fn clear(&self) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}
```

并在 `src-tauri/src/main.rs` 的 `mod ui_events;` 行后加：

```rust
mod ui_log;
```

- [ ] **Step 4: 跑测试确认绿**

Run: `cd /home/working/lyagent/desktop/src-tauri && cargo test --bin wxauto-desktop ui_log 2>&1 | tail -5`
Expected: `test result: ok. 5 passed`

- [ ] **Step 5: clippy + fmt**

Run: `cd /home/working/lyagent/desktop/src-tauri && cargo clippy --bin wxauto-desktop 2>&1 | tail -3 && cargo fmt`
Expected: 无 warning。

- [ ] **Step 6: Commit**

```bash
cd /home/working/lyagent/desktop
git add src-tauri/src/ui_log.rs src-tauri/src/main.rs
git commit -m "feat: 运行日志环形缓冲AppLogEntry+LogRing(单窗口化Task1)"
```

---

### Task 2: `UiLogLayer`（tracing Layer → ring + mpsc）

**Files:**
- Modify: `src-tauri/src/ui_log.rs`（追加 Layer 实现 + 测试）

**Interfaces:**
- Consumes: Task 1 的 `AppLogEntry/AppLogLevel/AppLogSource/LogRing`
- Produces:
  - `pub struct UiLogLayer { ... }`
  - `impl UiLogLayer { pub fn new(ring: LogRing, sink: tokio::sync::mpsc::Sender<AppLogEntry>) -> Self }`
  - `impl<S> tracing_subscriber::Layer<S> for UiLogLayer where S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>`
  - `pub fn now_ms() -> u64`（毫秒时间戳，测试与生产共用）

**给实现者的关键背景**：tracing-subscriber 0.3 的 `Layer::on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>)` 里，级别取 `event.metadata().level()`，消息字段用 `tracing::field::Empty` visitor 遍历收集（只收名为 `message` 的字段，其余跳过）。`*Level` 到 `AppLogLevel` 的映射：ERROR→Error、WARN→Warn、INFO→Info、DEBUG→Debug、TRACE→Trace。

- [ ] **Step 1: 写失败测试**

在 `ui_log.rs` 的 `mod tests` 内追加：

```rust
    /// Layer 经真实 tracing 订阅器收一条 info → ring 有对应 entry
    #[test]
    fn test_ui_log_layer_captures_tracing_event() {
        let ring = LogRing::new(10);
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let subscriber = tracing_subscriber::registry()
            .with(UiLogLayer::new(ring.clone(), tx));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("hello 日志");
        });
        let snap = ring.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].level, AppLogLevel::Info);
        assert_eq!(snap[0].source, AppLogSource::Rust);
        assert!(snap[0].message.contains("hello 日志"), "实际: {}", snap[0].message);
        // mpsc 侧也收到（转发任务的数据源）
        let got = rx.blocking_recv().expect("应收到一条");
        assert_eq!(got.message, snap[0].message);
    }

    /// 级别映射：error/warn 各归其位
    #[test]
    fn test_ui_log_layer_maps_levels() {
        let ring = LogRing::new(10);
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let subscriber = tracing_subscriber::registry()
            .with(UiLogLayer::new(ring.clone(), tx));
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!("e");
            tracing::warn!("w");
        });
        let snap = ring.snapshot();
        assert_eq!(snap[0].level, AppLogLevel::Error);
        assert_eq!(snap[1].level, AppLogLevel::Warn);
    }

    /// 通道满时丢行不阻塞（try_send 失败静默——旁路观察者不反压业务）
    #[test]
    fn test_ui_log_layer_drops_when_channel_full() {
        let ring = LogRing::new(10);
        // 容量 0 的通道：首次 try_send 即满
        let (tx, rx) = tokio::sync::mpsc::channel::<AppLogEntry>(0);
        drop(rx);
        let subscriber = tracing_subscriber::registry()
            .with(UiLogLayer::new(ring.clone(), tx));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("仍应进 ring");
        });
        assert_eq!(ring.snapshot().len(), 1, "mpsc 满不影响 ring 落条");
    }
```

- [ ] **Step 2: 跑测试确认红**

Run: `cd /home/working/lyagent/desktop/src-tauri && cargo test --bin wxauto-desktop ui_log 2>&1 | tail -5`
Expected: 编译错误（`UiLogLayer` 未定义）。

- [ ] **Step 3: 写最小实现**

在 `ui_log.rs` 的 `LogRing` impl 之后追加：

```rust
/// 毫秒时间戳（LogRing 条目与测试共用；集中一处便于将来换时钟注入）
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// tracing 事件 → AppLogEntry 的字段收集器（只收 `message` 字段）
struct MessageFieldVisitor {
    message: String,
}

impl tracing::field::Visit for MessageFieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        }
    }
}

/// tracing Layer：每条事件格式化成 AppLogEntry → push ring + try_send mpsc。
/// mpsc 满则丢行（旁路观察者不反压业务线程——与 UiEventBridge 同哲学）。
pub struct UiLogLayer {
    ring: LogRing,
    sink: tokio::sync::mpsc::Sender<AppLogEntry>,
}

impl UiLogLayer {
    pub fn new(ring: LogRing, sink: tokio::sync::mpsc::Sender<AppLogEntry>) -> Self {
        Self { ring, sink }
    }
}

impl<S> tracing_subscriber::Layer<S> for UiLogLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let level = match *event.metadata().level() {
            tracing::Level::ERROR => AppLogLevel::Error,
            tracing::Level::WARN => AppLogLevel::Warn,
            tracing::Level::INFO => AppLogLevel::Info,
            tracing::Level::DEBUG => AppLogLevel::Debug,
            tracing::Level::TRACE => AppLogLevel::Trace,
        };
        let mut visitor = MessageFieldVisitor {
            message: String::new(),
        };
        event.record(&mut visitor);
        let entry = AppLogEntry {
            ts: now_ms(),
            level,
            source: AppLogSource::Rust,
            message: visitor.message,
        };
        self.ring.push(entry.clone());
        let _ = self.sink.try_send(entry);
    }
}
```

- [ ] **Step 4: 跑测试确认绿**

Run: `cd /home/working/lyagent/desktop/src-tauri && cargo test --bin wxauto-desktop ui_log 2>&1 | tail -5`
Expected: `test result: ok. 8 passed`

- [ ] **Step 5: clippy + fmt + Commit**

```bash
cd /home/working/lyagent/desktop/src-tauri && cargo clippy --bin wxauto-desktop 2>&1 | tail -3 && cargo fmt
cd /home/working/lyagent/desktop
git add src-tauri/src/ui_log.rs
git commit -m "feat: UiLogLayer tracing Layer捕获日志进ring+mpsc(Task2)"
```

---

### Task 3: sidecar stderr 接管（piped + 逐行读进 ring，CLI 零回归）

**Files:**
- Modify: `src-tauri/src/sidecar/mod.rs`（stderr 策略 + 读循环）
- Test: `src-tauri/tests/sidecar_stderr.rs`（新建集成测试）

**Interfaces:**
- Consumes: `crate::ui_log::{AppLogEntry, AppLogLevel, AppLogSource, LogRing, now_ms}`（bin 侧路径——注意：`sidecar/mod.rs` 属于 **lib**（wxauto_desktop），不能引 bin 的 `crate::ui_log`！**因此本任务把落点抽成独立 sink trait 放在 lib 内**）
- Produces:
  - lib 内 `src-tauri/src/sidecar/mod.rs` 新增：
    ```rust
    /// sidecar stderr 行消费者（GUI 注入写 ring；CLI 不注入=stderr inherit）
    pub trait StderrSink: Send + Sync + 'static {
        fn consume(&self, line: String);
    }
    ```
  - `SidecarHandle::spawn_with_python` 与 `spawn_default` 各增加变体：`spawn_with_python_sunk(script, envs, sink: Option<Arc<dyn StderrSink>>)` 与 `spawn_default_sunk(sink: Option<Arc<dyn StderrSink>>)`；**原两函数保持签名不变**（内部转发到 sunk 变体传 `None`）
  - bin 侧 `ui_log.rs` 新增（Task 4 的 gui.rs 用）：
    ```rust
    pub struct RingStderrSink(pub LogRing);
    impl wxauto_desktop::sidecar::StderrSink for RingStderrSink {
        fn consume(&self, line: String) { ... }
    }
    ```

**给实现者的关键背景（必读）**：
1. `sidecar/mod.rs` 在 **lib crate**（`wxauto_desktop`），而 `ui_log.rs` 在 **bin crate**——lib 不能反向依赖 bin。所以 sink 以 trait 形式定义在 lib，实现在 bin。
2. stderr 级别判定（spec §4）：含 `ERROR` 或 `Traceback` 关键字（`contains`，大小写敏感）升 `Error`，否则 `Info`。
3. `do_spawn` 现在 `stderr(Stdio::inherit())` 由两个调用方各自设置。改法：`do_spawn(mut cmd: Command, stderr_pipe: Option<Stdio-mutex-读端>)` 不优雅——**推荐实现**：调用方根据 `sink.is_some()` 决定 `cmd.stderr(Stdio::piped())`，spawn 后 `child.stderr.take()` 传给 `do_spawn`，`do_spawn` 收 `Option<ChildStderr>`：Some 则 spawn 读循环（每行 `sink.consume(line)`），None 什么都不做（stderr 已 inherit）。`do_spawn` 签名改为 `async fn do_spawn(mut cmd: Command, stderr: Option<tokio::process::ChildStderr>, sink: Option<Arc<dyn StderrSink>>)`——注意必须 Some(stderr) 与 Some(sink) 同时出现才读。
4. 简化替代（等价、更少签名 churn）：`do_spawn(mut cmd: Command, stderr_sink: Option<Arc<dyn StderrSink>>)`，函数内部当 sink 是 Some 时先 `cmd.stderr(Stdio::piped())` 再 spawn。**用这个**。调用方只传 sink，stderr 管道决策收进 do_spawn 一处。
5. `spawn_reader` 的 stdout 读循环是现成模式参照（`BufReader::lines` + EOF 退出），stderr 读循环写成独立 `spawn_stderr_reader(sink, stderr)`。
6. 集成测试放 `tests/sidecar_stderr.rs`（集成测试 = lib 外部消费者，正好验证 pub trait 可从外部实现——与 `RingStderrSink` 在 bin 的真实形态同构。测试里写个 `VecSink(Mutex<Vec<String>>)` 实现 trait）。

- [ ] **Step 1: 写失败集成测试**

创建 `src-tauri/tests/sidecar_stderr.rs`：

```rust
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
    assert!(lines.iter().any(|l| l.contains("sidecar 启动")), "实际: {lines:?}");
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
```

- [ ] **Step 2: 跑测试确认红**

Run: `cd /home/working/lyagent/desktop/src-tauri && cargo test --test sidecar_stderr 2>&1 | tail -5`
Expected: 编译错误（`StderrSink`/`spawn_with_python_sunk` 未定义）。

- [ ] **Step 3: 实现 lib 侧 trait + sunk 变体**

`src-tauri/src/sidecar/mod.rs` 改动：

3a. 模块头 `use` 区加 `std::sync::Arc`（已有 `std::sync::Arc`——确认即可）。

3b. `NOTIFY_CHANNEL_CAPACITY` 常量后加 trait 定义：

```rust
/// sidecar stderr 行消费者（GUI 注入写日志环形缓冲；CLI 不注入=stderr
/// inherit 原行为——零回归铁律）。
pub trait StderrSink: Send + Sync + 'static {
    /// 消费一行（不含换行符）
    fn consume(&self, line: String);
}
```

3c. `spawn_with_python` 改为转发（原签名不变），并新增 sunk 变体：

```rust
    /// 用 python 解释器跑内联脚本（测试 / 嵌入式启动用；stderr inherit）
    pub async fn spawn_with_python(
        script: &str,
        envs: &[(&str, &str)],
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Self::spawn_with_python_sunk(script, envs, None).await
    }

    /// 同 spawn_with_python，但可注入 stderr sink（Some → piped 逐行消费）
    pub async fn spawn_with_python_sunk(
        script: &str,
        envs: &[(&str, &str)],
        stderr_sink: Option<Arc<dyn StderrSink>>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let mut cmd = Command::new(find_python());
        cmd.arg("-c")
            .arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        for (k, v) in envs {
            cmd.env(k, v);
        }
        Self::do_spawn(cmd, stderr_sink).await
    }
```

3d. `spawn_default` 同样改转发 + 新增 `spawn_default_sunk(sink)`：原三条分支逻辑不动，仅末尾 `cmd.stdin(Stdio::piped()).stdout(Stdio::piped())` 后不再设 stderr（决策移进 do_spawn）：

```rust
    /// 默认解析链启动（stderr inherit；原签名零回归）
    pub async fn spawn_default() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Self::spawn_default_sunk(None).await
    }

    /// 同 spawn_default，但可注入 stderr sink
    pub async fn spawn_default_sunk(
        stderr_sink: Option<Arc<dyn StderrSink>>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let mut cmd = if let Ok(cmd_str) = std::env::var("WXAUTO_SIDECAR_CMD") {
            let (program, args) = crate::cli::parse_sidecar_cmd(&cmd_str);
            let mut c = Command::new(program);
            c.args(args);
            c
        } else if let Some(bundled) = crate::cli::find_bundled_sidecar() {
            let mut c = Command::new(&bundled);
            apply_windows_no_window(&mut c);
            tracing::info!(path = %bundled, "使用内置 sidecar（安装态）");
            c
        } else {
            let mut c = Command::new("python3");
            c.arg("sidecar-python/sidecar.py");
            if let Some(root) = crate::cli::resolve_sidecar_workdir() {
                c.current_dir(root);
            }
            c
        };
        // CREATE_NO_WINDOW 三路径齐备（cmd 级/env 覆盖/回退）——piped 后
        // 双保险：Windows GUI 父进程下任何控制台闪窗都不允许
        apply_windows_no_window(&mut cmd);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
        Self::do_spawn(cmd, stderr_sink).await
    }
```

**注意**：env 覆盖分支（`WXAUTO_SIDECAR_CMD`）原来没加 `apply_windows_no_window`，本改动把 `apply_windows_no_window(&mut cmd)` 统一放分支合并之后（三路径齐备，spec §2 根因 3）。

3e. `do_spawn` 签名与 stderr 处理：

```rust
    /// 实际 spawn：取管道、建共享态、挂读循环 + 退出 reaper。
    /// stderr 策略：sink=Some → piped + 读循环逐行消费（GUI 日志 tab 数据
    /// 源）；None → inherit（CLI 原行为零回归）。
    async fn do_spawn(
        mut cmd: Command,
        stderr_sink: Option<Arc<dyn StderrSink>>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        if stderr_sink.is_some() {
            cmd.stderr(Stdio::piped());
        } else {
            cmd.stderr(Stdio::inherit());
        }
        let mut child = cmd.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("无 stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("无 stdout"))?;
        //（后续 Shared 构造 / spawn_reader / spawn_reaper 与原实现一致）
```

`spawn_reader(shared.clone(), stdout);` 之后加：

```rust
        if let (Some(sink), Some(stderr)) = (stderr_sink, child.stderr.take()) {
            spawn_stderr_reader(sink, stderr);
        }
```

3f. 文件底部（`spawn_reader` 函数后）加读循环：

```rust
/// sidecar stderr 读循环：逐行消费进 sink；EOF/IO 错退出（sidecar 死亡
/// 时管道关闭，reaper 负责回收，此处只管读）。空行跳过。
fn spawn_stderr_reader(sink: Arc<dyn StderrSink>, stderr: tokio::process::ChildStderr) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = line.trim_end();
                    if !trimmed.is_empty() {
                        sink.consume(trimmed.to_string());
                    }
                }
                Err(e) => {
                    tracing::warn!("读 sidecar stderr 出错: {e}，stderr 读循环退出");
                    break;
                }
            }
        }
    });
}
```

- [ ] **Step 4: 跑测试确认绿 + 全量回归**

Run: `cd /home/working/lyagent/desktop/src-tauri && cargo test --test sidecar_stderr 2>&1 | tail -5`
Expected: `test result: ok. 2 passed`

Run: `cd /home/working/lyagent/desktop/src-tauri && cargo test 2>&1 | tail -8`
Expected: 全部既有测试仍绿（重点 `sidecar_protocol`/`cli_lifecycle`/`wx_session` 等用 spawn 的套件）。

- [ ] **Step 5: bin 侧 RingStderrSink（含级别判定）**

在 bin 的 `src-tauri/src/ui_log.rs` 追加（`UiLogLayer` impl 之后）：

```rust
/// sidecar stderr → ring 的 sink 实现（含关键字升 error 判定，spec §4：
/// Python 侧无级别概念，统一 info；含 ERROR/Traceback 关键字升 error）
pub struct RingStderrSink(pub LogRing);

impl wxauto_desktop::sidecar::StderrSink for RingStderrSink {
    fn consume(&self, line: String) {
        let level = if line.contains("ERROR") || line.contains("Traceback") {
            AppLogLevel::Error
        } else {
            AppLogLevel::Info
        };
        self.0.push(AppLogEntry {
            ts: now_ms(),
            level,
            source: AppLogSource::Sidecar,
            message: line,
        });
    }
}
```

`mod tests` 追加：

```rust
    #[test]
    fn test_ring_stderr_sink_promotes_traceback_to_error() {
        let ring = LogRing::new(10);
        let sink = RingStderrSink(ring.clone());
        wxauto_desktop::sidecar::StderrSink::consume(&sink, "普通行".into());
        wxauto_desktop::sidecar::StderrSink::consume(&sink, "Traceback (most recent call last):".into());
        let snap = ring.snapshot();
        assert_eq!(snap[0].level, AppLogLevel::Info);
        assert_eq!(snap[0].source, AppLogSource::Sidecar);
        assert_eq!(snap[1].level, AppLogLevel::Error, "Traceback 应升 error");
    }
```

Run: `cd /home/working/lyagent/desktop/src-tauri && cargo test --bin wxauto-desktop ui_log 2>&1 | tail -3`
Expected: 9 passed。

- [ ] **Step 6: clippy + fmt + Commit**

```bash
cd /home/working/lyagent/desktop/src-tauri && cargo clippy --all-targets 2>&1 | tail -3 && cargo fmt
cd /home/working/lyagent/desktop
git add src-tauri/src/sidecar/mod.rs src-tauri/src/ui_log.rs src-tauri/tests/sidecar_stderr.rs
git commit -m "feat: sidecar stderr可注入sink接管+CREATE_NO_WINDOW三路径齐备(Task3)"
```

---

### Task 4: gui.rs 装配日志链 + `get_recent_logs`/`clear_logs` 命令 + main.rs windows_subsystem

**Files:**
- Modify: `src-tauri/src/gui.rs`（tracing 初始化 + 转发任务 + ring 进 AppStateCtx）
- Modify: `src-tauri/src/app_state.rs`（AppStateCtx 增 `log_ring` 字段）
- Modify: `src-tauri/src/commands.rs`（新命令）
- Modify: `src-tauri/src/main.rs`（windows_subsystem 属性）
- Test: `src-tauri/src/gui.rs` 内嵌测试 + `src-tauri/src/commands.rs` 测试（mock_runtime 模式，参照 gui.rs 现有 C1 测试）

**Interfaces:**
- Consumes: Task 1/2/3 的 `LogRing/UiLogLayer/RingStderrSink`；既有 `UiEventBridge`
- Produces:
  - `AppStateCtx` 新 pub 字段：`pub log_ring: crate::ui_log::LogRing`（`shell()` 构造参数增加）
  - `ui_events.rs` 新常量与方法：`pub const EVENT_APP_LOG: &str = "wxauto://app-log";` + `UiEventBridge::forward_app_log(&self, entry: &Value)`
  - tauri command：`get_recent_logs(ctx: State<'_, AppStateCtx>) -> Vec<serde_json::Value>`（ring 快照序列化，旧在前）；`clear_logs(ctx: State<'_, AppStateCtx>)`（ring.clear()）
  - `spawn_default_sunk`/`assemble_into` 注入点：`AppStateCtx::shell` 里建 ring，`assemble_into` 的 `SidecarHandle::spawn_default()` 改 `spawn_default_sunk(Some(Arc::new(RingStderrSink(ring))))`

**给实现者的关键背景**：
1. `AppStateCtx::shell(default_config_path(), bridge.clone())` 现有签名（app_state.rs）——加第三参 `log_ring: LogRing`。`Clone` impl 与 `shell()` 内构造同步改。调用点仅 gui.rs 两处（setup + C1 测试）。
2. gui.rs 现有 tracing 初始化是 `tracing_subscriber::fmt().with_ansi(...).with_env_filter(...).init()`——改成 registry 组合（fmt 层 `with_writer(std::io::stderr)` 保留控制台输出）。
3. 转发任务：`setup_async` 里 `tokio::spawn` 一个循环 `while let Some(entry) = rx.recv().await { bridge.forward_app_log(&serde_json::to_value(&entry).unwrap_or_default()).await }`——`to_value` 对该结构不会失败，`unwrap_or_default` 兜底即可（禁 unwrap 铁律）。
4. `for_test()`（app_state.rs 的 `#[cfg(test)]` 构造）也要补 `log_ring` 字段——传 `LogRing::new(10)` 即可。
5. `main.rs` 顶部加属性（release 消控制台 = cmd 窗口根因 1；debug 保留便于 dev）：

```rust
// Windows release 构建不附控制台窗口（debug 保留——dev 排障看日志）。
// 这是「只显示日志的 cmd 窗口」根因 1 的修复（spec §2）。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
```

注意：该属性必须放在 `main.rs` 文件最顶部（所有 `//!` 文档注释之后、`mod` 声明之前——内部属性要求在 item 之前，`//!` 是注释不冲突）。

- [ ] **Step 1: 写失败测试**

4a. `ui_events.rs` 的 `mod tests` 加：

```rust
    /// app-log 转发：entry JSON 经桥发射（payload 结构即前端契约）
    #[tokio::test]
    async fn test_forward_app_log_emits_entry() {
        let bridge = UiEventBridge::new();
        // 未 attach 时 emit 走丢弃分支不 panic——本测试主要覆盖编译期
        // 契约与方法存在性；真实发射在 gui_bridge 集成测试覆盖。
        bridge
            .forward_app_log(&serde_json::json!({
                "ts": 1, "level": "info", "source": "rust", "message": "m"
            }))
            .await;
    }
```

4b. `gui.rs` 的 `mod tests` 加（对齐既有 C1 测试的 mock_runtime 模式）：

```rust
    /// get_recent_logs 命令：ring 有两条时快照返回（旧在前）
    #[tokio::test]
    async fn test_invoke_get_recent_logs_returns_snapshot() {
        use crate::ui_log::{AppLogEntry, AppLogLevel, AppLogSource, LogRing};

        let app = tauri::test::mock_builder()
            .invoke_handler(tauri::generate_handler![get_recent_logs])
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .expect("mock app 构建失败");

        let ring = LogRing::new(10);
        ring.push(AppLogEntry {
            ts: 1,
            level: AppLogLevel::Info,
            source: AppLogSource::Rust,
            message: "先".into(),
        });
        ring.push(AppLogEntry {
            ts: 2,
            level: AppLogLevel::Warn,
            source: AppLogSource::Sidecar,
            message: "后".into(),
        });
        // 与生产 setup 相同的 manage 形态（shell 增 log_ring 参数后）
        let ctx = AppStateCtx::shell_for_test(default_config_path(), UiEventBridge::new(), ring);
        app.manage(ctx);

        let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .expect("mock webview 构建失败");

        let resp = tauri::test::get_ipc_response(
            &webview,
            tauri::webview::InvokeRequest {
                cmd: "get_recent_logs".into(),
                callback: tauri::ipc::CallbackFn(0),
                error: tauri::ipc::CallbackFn(1),
                url: "tauri://localhost".parse().expect("url 解析失败"),
                body: tauri::ipc::InvokeBody::default(),
                headers: Default::default(),
                invoke_key: tauri::test::INVOKE_KEY.to_string(),
            },
        )
        .map(|b| b.deserialize::<Vec<serde_json::Value>>().expect("响应应为数组"));
        let logs = resp.expect("invoke 不应 reject");
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0]["message"], "先");
        assert_eq!(logs[1]["level"], "warn");
    }
```

**注意**：`shell` 是同步构造但内部可能调 keyring 等——查现有 `shell` 实现：它只是存字段，测试可直接用。若 `shell(path, bridge, ring)` 直接可用就把上面 `shell_for_test` 改成 `shell`。

4c. `app_state.rs` 的 `for_test()` 同步补字段（编译需要）。

- [ ] **Step 2: 跑测试确认红**

Run: `cd /home/working/lyagent/desktop/src-tauri && cargo test --bin wxauto-desktop 2>&1 | tail -8`
Expected: 编译错误（`forward_app_log`/`get_recent_logs`/`shell` 参数不匹配）。

- [ ] **Step 3: 实现**

3a. `ui_events.rs`：

```rust
/// 运行日志事件名（与 desktop/src/stores/app.ts 的 listen() 字面量一致）
pub const EVENT_APP_LOG: &str = "wxauto://app-log";
```

`UiEventBridge` impl 内加：

```rust
    /// 运行日志条（转发任务直通；entry 序列化字段即前端契约）
    pub async fn forward_app_log(&self, entry: &Value) {
        self.emit(EVENT_APP_LOG, entry).await;
    }
```

3b. `app_state.rs`：`AppStateCtx` struct 加字段 `pub log_ring: crate::ui_log::LogRing,`；`shell()` 签名加参 `log_ring: crate::ui_log::LogRing`（构造体同步）；手写 `Clone` impl 补 `log_ring: self.log_ring.clone(),`；`for_test()` 补 `log_ring: crate::ui_log::LogRing::new(10),`；`assemble_into` 内 `SidecarHandle::spawn_default()` 改：

```rust
                // 1. 首代 sidecar（失败即装配失败——GUI 起不来要有明确报错）。
                //    stderr 注入 ring sink：sidecar 日志进「运行日志」tab
                //    （spec §4 source=sidecar；CLI 不走本装配，零回归）
                let stderr_sink: std::sync::Arc<
                    dyn wxauto_desktop::sidecar::StderrSink,
                > = std::sync::Arc::new(crate::ui_log::RingStderrSink(self.log_ring.clone()));
                let sidecar = SidecarHandle::spawn_default_sunk(Some(stderr_sink))
                    .await
                    .map_err(|e| format!("sidecar 启动失败: {e}"))?;
```

（`assemble_into` 的闭包里需先在闭包外取 `let log_ring = self.log_ring.clone();` 再 move 进闭包。）

3c. `commands.rs` 加两命令：

```rust
/// 运行日志快照（旧在前；前端 init 时补齐 attach 前的历史——
/// tauri 事件无重放，对齐 get_app_state 补首值模式）
#[tauri::command]
pub fn get_recent_logs(ctx: State<'_, AppStateCtx>) -> Vec<serde_json::Value> {
    ctx.log_ring
        .snapshot()
        .iter()
        .map(|e| serde_json::to_value(e).unwrap_or_default())
        .collect()
}

/// 清空运行日志（前端「清空」按钮）
#[tauri::command]
pub fn clear_logs(ctx: State<'_, AppStateCtx>) {
    ctx.log_ring.clear();
}
```

3d. `gui.rs`：

- `use` 区加 `use crate::ui_log::{LogRing, UiLogLayer};` 与 `use commands::*;`（已有通配）。
- `run_gui()` 开头的 tracing 初始化整体替换：

```rust
    // 日志初始化：registry 组合三层——
    // 1. fmt 层 → stderr（GUI 无控制台时输出被丢弃/重定向，dev 排障用）
    // 2. UI 层 → 内存环形缓冲（「运行日志」tab 数据源，经 mpsc 转发桥 emit）
    // ANSI 只在 stderr 是 tty 时开（2026-09-03 真机日志反馈，口径不变）
    let ring = LogRing::new(LOG_RING_CAPACITY);
    let (log_tx, mut log_rx) = tauri::async_runtime::channel::<crate::ui_log::AppLogEntry>(256);
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(std::io::stderr().is_terminal())
                .with_writer(std::io::stderr),
        )
        .with(UiLogLayer::new(ring.clone(), log_tx))
        .with(env_filter)
        .init();
```

- `run_gui` 里 `AppStateCtx::shell(default_config_path(), bridge.clone())` 改传三参 `AppStateCtx::shell(default_config_path(), bridge.clone(), ring.clone())`。
- `setup_async` 的 bridge attach 之后加转发任务：

```rust
    // 日志转发任务：mpsc → 桥 emit（emit 失败仅 tracing，不断主链路）
    {
        let bridge = bridge.clone();
        tauri::async_runtime::spawn(async move {
            while let Some(entry) = log_rx.recv().await {
                bridge
                    .forward_app_log(&serde_json::to_value(&entry).unwrap_or_default())
                    .await;
            }
        });
    }
```

（`setup_async` 签名需加参 `mut log_rx: tauri::async_runtime::Receiver<crate::ui_log::AppLogEntry>`，`run_gui` 的 spawn 调用同步改。）

- 文件顶部常量区加：

```rust
/// 运行日志环形缓冲上限（spec §5：1000 条）
const LOG_RING_CAPACITY: usize = 1000;
```

- `generate_handler!` 列表 `get_app_state,` 之后加 `get_recent_logs, clear_logs,`。

3e. `main.rs` 顶部（文档注释块之后）加：

```rust
// Windows release 构建不附控制台窗口（debug 保留——dev 排障看日志）。
// 「只显示日志的 cmd 窗口」根因 1 修复（spec §2）；debug_assertions 保证
// 开发构建行为不变。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
```

（放 `//!` 块与 `mod` 声明之间——内部属性必须在 item 之前。）

3f. gui.rs 既有 C1 测试 `test_invoke_get_app_state_managed_type_key_matches` 里 `AppStateCtx::shell(default_config_path(), UiEventBridge::new())` 改三参（补 `crate::ui_log::LogRing::new(10)`）。

- [ ] **Step 4: 跑测试确认绿**

Run: `cd /home/working/lyagent/desktop/src-tauri && cargo test --bin wxauto-desktop 2>&1 | tail -8`
Expected: 全绿（含既有 C1 测试与新 get_recent_logs 测试）。

Run: `cd /home/working/lyagent/desktop/src-tauri && cargo test 2>&1 | tail -5`
Expected: 全量绿。

- [ ] **Step 5: clippy + fmt + Commit**

```bash
cd /home/working/lyagent/desktop/src-tauri && cargo clippy --all-targets 2>&1 | tail -3 && cargo fmt
cd /home/working/lyagent/desktop
git add src-tauri/src/gui.rs src-tauri/src/app_state.rs src-tauri/src/commands.rs src-tauri/src/main.rs src-tauri/src/ui_events.rs
git commit -m "feat: GUI装配日志链(ring+Layer+stderr sink)+get_recent_logs+windows_subsystem(Task4)"
```

---

### Task 5: 前端 store `appLog` + 事件订阅 + 快照拉取

**Files:**
- Modify: `desktop/src/stores/app.ts`

**Interfaces:**
- Consumes: Task 4 的事件 `wxauto://app-log`（payload `{ts, level, source, message}`）与 invoke `get_recent_logs`/`clear_logs`
- Produces（Task 6 视图依赖）:
  - `export type AppLogLevelName = 'error' | 'warn' | 'info' | 'debug' | 'trace';`
  - `export type AppLogSourceName = 'rust' | 'sidecar';`
  - `export interface AppLogItem { ts: number; level: AppLogLevelName; source: AppLogSourceName; message: string; }`
  - `export const APP_LOG_RING_LIMIT = 1000;`
  - store state：`appLog: AppLogItem[]`（新在前，头插）
  - store action：`pushAppLog(item: AppLogItem): void`（头插+环形）；`clearAppLog(): Promise<void>`（invoke clear_logs + 清空本地）

**给实现者的关键背景**：既有守卫工具 `isRecord/str/num` 与 `parseCommandLogItem` 的宽容归一模式直接复用。级别/来源守卫用字面量联合收窄（参照 `isAppStateName` 的 `hasOwnProperty` 模式不适用——这里用数组 includes 收窄）。

- [ ] **Step 1: 写失败测试**

**背景**：desktop/ 前端目前**没有**测试框架（package.json 无 vitest）。为守住 store 纯逻辑，本任务引入 vitest（devDependency）并建一个 spec 文件。

```bash
cd /home/working/lyagent/desktop
pnpm add -D vitest
```

`package.json` scripts 加 `"test": "vitest run"`。

创建 `src/stores/app.spec.ts`：

```typescript
/**
 * appLog store 逻辑测试：环形上限 + 载荷守卫（非法字段宽容归一/非法级别回退 info）。
 * 只测纯逻辑（pushAppLog/parseAppLogItem），不触 tauri invoke（App 挂载才走）。
 */
import { describe, it, expect } from 'vitest';
import { createPinia, setActivePinia } from 'pinia';
import { useAppStore, APP_LOG_RING_LIMIT } from './app';

describe('appLog', () => {
  it('pushAppLog 头插且环形淘汰', () => {
    setActivePinia(createPinia());
    const store = useAppStore();
    for (let i = 0; i < APP_LOG_RING_LIMIT + 5; i += 1) {
      store.pushAppLog({ ts: i, level: 'info', source: 'rust', message: `m${i}` });
    }
    expect(store.appLog.length).toBe(APP_LOG_RING_LIMIT);
    expect(store.appLog[0].message).toBe(`m${APP_LOG_LIMIT_PLACEHOLDER}`);
  });
});
```

**注意**：上面 `APP_LOG_LIMIT_PLACEHOLDER` 处替换为真实表达式 `m${APP_LOG_RING_LIMIT + 4}`（最后的条目）；再补第二个用例：

```typescript
  it('pushAppLog 接受合法条目结构不变', () => {
    setActivePinia(createPinia());
    const store = useAppStore();
    store.pushAppLog({ ts: 1, level: 'warn', source: 'sidecar', message: 'x' });
    expect(store.appLog[0]).toEqual({ ts: 1, level: 'warn', source: 'sidecar', message: 'x' });
  });
```

（守卫函数 `parseAppLogItem` 若导出为内部函数，经 pushAppLog 的调用路径覆盖：在 init 事件订阅里用。此处直测 push；parse 的守卫测试放 Task 6 之前的补充——**简化：把 parse 逻辑并入 push 入口设计，即 push 接收 `unknown` 载荷由内部归一**。实现按此口径：`pushAppLog(payload: unknown)`，内部归一出 `AppLogItem`。上面两用例的入参按合法对象传，断言不变。）

- [ ] **Step 2: 跑测试确认红**

Run: `cd /home/working/lyagent/desktop && pnpm vitest run src/stores/app.spec.ts 2>&1 | tail -5`
Expected: FAIL（`APP_LOG_RING_LIMIT` 未导出 / `pushAppLog` 不存在）。

- [ ] **Step 3: 实现 store 改动**

`src/stores/app.ts`：

3a. 文件头注释的事件契约清单追加一行：

```
 * - `wxauto://app-log`  payload AppLogItem（运行日志，1000 环形）
```

3b. `CommandLogItem` 接口定义之后加类型与守卫：

```typescript
/** 运行日志级别（Rust AppLogLevel serde 小写变体名一一对应） */
export type AppLogLevelName = 'error' | 'warn' | 'info' | 'debug' | 'trace';

/** 运行日志来源：rust=应用自身 / sidecar=Python 子进程 stderr */
export type AppLogSourceName = 'rust' | 'sidecar';

/** 运行日志条（wxauto://app-log 载荷） */
export interface AppLogItem {
  ts: number;
  level: AppLogLevelName;
  source: AppLogSourceName;
  message: string;
}

/** 运行日志环形上限（Rust LogRing 同容量——spec §5） */
export const APP_LOG_RING_LIMIT = 1000;

const APP_LOG_LEVELS: AppLogLevelName[] = ['error', 'warn', 'info', 'debug', 'trace'];
const APP_LOG_SOURCES: AppLogSourceName[] = ['rust', 'sidecar'];

/** 运行日志载荷守卫：级别/来源非法回退 info/rust（单条脏数据不炸日志流） */
function parseAppLogItem(v: unknown): AppLogItem {
  const r = isRecord(v) ? v : {};
  const level = str(r.level) as AppLogLevelName;
  const source = str(r.source) as AppLogSourceName;
  return {
    ts: num(r.ts) || Date.now(),
    level: APP_LOG_LEVELS.includes(level) ? level : 'info',
    source: APP_LOG_SOURCES.includes(source) ? source : 'rust',
    message: str(r.message),
  };
}
```

3c. state 里 `commandLog` 之后加：

```typescript
    /** 运行日志（新条目头插；1000 环形） */
    appLog: [] as AppLogItem[],
```

3d. `init()` 的 command-log 订阅之后加：

```typescript
        unlisteners.push(
          await listen<unknown>('wxauto://app-log', (e) => {
            this.pushAppLog(e.payload);
          }),
        );
```

以及 `get_app_state` 快照之后加日志快照补齐（同模式）：

```typescript
        // 运行日志历史补齐（bridge attach 前的条目事件无重放——快照兜底）
        const logs = await invoke<unknown>('get_recent_logs');
        if (Array.isArray(logs)) {
          // 快照旧在前 → 头插后新在前；先整体置空防重复（init 幂等只跑一次）
          this.appLog = logs
            .map(parseAppLogItem)
            .reverse()
            .concat(this.appLog);
          if (this.appLog.length > APP_LOG_RING_LIMIT) {
            this.appLog.length = APP_LOG_RING_LIMIT;
          }
        }
```

3e. actions 里 `pushCommandLog` 之后加：

```typescript
    /** 运行日志入列：载荷归一 + 头插 + 1000 环形 */
    pushAppLog(payload: unknown) {
      this.appLog.unshift(parseAppLogItem(payload));
      if (this.appLog.length > APP_LOG_RING_LIMIT) this.appLog.length = APP_LOG_RING_LIMIT;
    },
    /** 清空运行日志（Rust ring + 本地双清） */
    async clearAppLog() {
      await invoke('clear_logs');
      this.appLog = [];
    },
```

- [ ] **Step 4: 跑测试确认绿 + typecheck**

Run: `cd /home/working/lyagent/desktop && pnpm vitest run src/stores/app.spec.ts 2>&1 | tail -5`
Expected: 2 passed。

Run: `cd /home/working/lyagent/desktop && pnpm typecheck 2>&1 | tail -3`
Expected: 无错误。

- [ ] **Step 5: Commit**

```bash
cd /home/working/lyagent/desktop
git add package.json pnpm-lock.yaml src/stores/app.ts src/stores/app.spec.ts
git commit -m "feat: store appLog状态+app-log事件订阅+快照补齐+vitest引入(Task5)"
```

---

### Task 6: `AppLog.vue` 视图 + App.vue 菜单接入

**Files:**
- Create: `desktop/src/views/AppLog.vue`
- Modify: `desktop/src/App.vue`（菜单项 + 视图分支）

**Interfaces:**
- Consumes: Task 5 的 `useAppStore().appLog/clearAppLog`、`AppLogItem/AppLogLevelName/AppLogSourceName`
- Produces: 无下游依赖（终端呈现）

**给实现者的关键背景**：视图风格参照 `CommandLog.vue`（t-card 包裹 + store 只读消费）。1000 条直接 v-for 渲染（spec：不引虚拟滚动）。TDesign 组件用 `t-select`（级别）/`t-checkbox-group`（来源）/`t-switch`（暂停滚动）/`t-button`（清空）。

- [ ] **Step 1: 写失败测试**

`src/views/AppLog.spec.ts`：

```typescript
/**
 * AppLog 视图纯逻辑测试：级别+来源过滤（视图挂载依赖 tdesign，过滤函数
 * 抽成模块内纯函数导出直测）。
 */
import { describe, it, expect } from 'vitest';
import { filterAppLog, type LogFilter } from './AppLog';

describe('filterAppLog', () => {
  const items = [
    { ts: 3, level: 'error', source: 'sidecar', message: 'e' },
    { ts: 2, level: 'info', source: 'rust', message: 'i' },
    { ts: 1, level: 'debug', source: 'rust', message: 'd' },
  ] as const;

  it('级别过滤：info 档含 warn/error，不含 debug', () => {
    const f: LogFilter = { minLevel: 'info', sources: ['rust', 'sidecar'] };
    expect(filterAppLog([...items], f).map((x) => x.message)).toEqual(['e', 'i']);
  });

  it('来源过滤：只看 sidecar', () => {
    const f: LogFilter = { minLevel: 'debug', sources: ['sidecar'] };
    expect(filterAppLog([...items], f).map((x) => x.message)).toEqual(['e']);
  });

  it('空来源=全不显示', () => {
    const f: LogFilter = { minLevel: 'debug', sources: [] };
    expect(filterAppLog([...items], f)).toEqual([]);
  });
});
```

- [ ] **Step 2: 跑测试确认红**

Run: `cd /home/working/lyagent/desktop && pnpm vitest run src/views/AppLog.spec.ts 2>&1 | tail -5`
Expected: FAIL（`./AppLog` 无 `filterAppLog` 导出）。

- [ ] **Step 3: 实现 AppLog.vue**

```vue
<template>
  <t-card title="运行日志" :bordered="false">
    <template #description>
      应用（rust）+ sidecar（python）运行日志实时流；最多保留 {{ APP_LOG_RING_LIMIT }} 条
    </template>
    <div class="toolbar">
      <t-select
        :value="filter.minLevel"
        class="toolbar__level"
        :options="LEVEL_OPTIONS"
        @change="(v: unknown) => (filter.minLevel = typeof v === 'string' ? (v as AppLogLevelName) : 'info')"
      />
      <t-checkbox-group
        :value="filter.sources"
        :options="SOURCE_OPTIONS"
        @change="(v: unknown) => {
          if (Array.isArray(v)) filter.sources = v.filter((s): s is AppLogSourceName => typeof s === 'string');
        }"
      />
      <t-switch v-model="paused" label="暂停滚动" />
      <t-button theme="default" variant="outline" size="small" @click="onClear">清空</t-button>
    </div>
    <div ref="scrollRef" class="logbox" @scroll="onScroll">
      <div
        v-for="(item, i) in visible"
        :key="`${item.ts}-${i}`"
        class="logline"
        :class="`logline--${item.level}`"
      >
        <span class="logline__ts">{{ formatTime(item.ts) }}</span>
        <span class="logline__source tag">{{ item.source }}</span>
        <span class="logline__msg">{{ item.message }}</span>
      </div>
      <div v-if="visible.length === 0" class="logbox__empty">暂无符合条件的日志</div>
    </div>
  </t-card>
</template>

<script setup lang="ts">
/**
 * 运行日志视图（spec §3.4 新增第七视图）：Rust + sidecar 日志实时流。
 * - 级别下拉（默认 info 及以上）+ 来源多选（默认全选）
 * - 自动贴底滚动；用户上滚暂停、回底部恢复（console 经典行为）
 * - 1000 条直接渲染（spec：不引虚拟滚动）
 */
import { computed, nextTick, reactive, ref, watch } from 'vue';
import type { CheckboxGroupValue, SelectOption } from 'tdesign-vue-next';
import { useAppStore, APP_LOG_RING_LIMIT, type AppLogLevelName, type AppLogSourceName, type AppLogItem } from '../stores/app';
import { filterAppLog, type LogFilter } from './AppLog';

const store = useAppStore();
const filter = reactive<LogFilter>({ minLevel: 'info', sources: ['rust', 'sidecar'] });
const paused = ref(false);
const scrollRef = ref<HTMLElement | null>(null);

const LEVEL_OPTIONS: SelectOption[] = [
  { label: 'ERROR 及以上', value: 'error' },
  { label: 'WARN 及以上', value: 'warn' },
  { label: 'INFO 及以上（默认）', value: 'info' },
  { label: 'DEBUG 及以上', value: 'debug' },
  { label: '全部（TRACE）', value: 'trace' },
];
const SOURCE_OPTIONS = [
  { label: 'rust', value: 'rust' },
  { label: 'sidecar', value: 'sidecar' },
];

const visible = computed(() => filterAppLog(store.appLog, filter));

/** 新日志到达且未暂停时贴底（watch 数组引用变化） */
watch(
  () => store.appLog,
  () => {
    if (paused.value) return;
    void nextTick(() => {
      const el = scrollRef.value;
      if (el) el.scrollTop = el.scrollHeight;
    });
  },
);

/** 用户上滚离底即暂停；回底恢复 */
function onScroll(): void {
  const el = scrollRef.value;
  if (!el) return;
  const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 8;
  paused.value = !atBottom;
}

async function onClear(): Promise<void> {
  await store.clearAppLog();
}

function formatTime(ts: number): string {
  return new Date(ts).toLocaleString('zh-CN', { hour12: false });
}
</script>

<style scoped>
.toolbar {
  display: flex;
  align-items: center;
  gap: 12px;
  margin-bottom: 12px;
}
.toolbar__level {
  width: 180px;
}
.logbox {
  height: 560px;
  overflow: auto;
  padding: 8px 12px;
  background: var(--td-bg-color-page, #f5f6f7);
  border-radius: 4px;
  font-family: var(--td-font-family-code, monospace);
  font-size: 12px;
}
.logline {
  display: flex;
  gap: 8px;
  padding: 1px 0;
  white-space: pre-wrap;
  word-break: break-all;
}
.logline__ts {
  color: var(--td-text-color-secondary);
  flex-shrink: 0;
}
.logline__source {
  flex-shrink: 0;
}
.logline--error .logline__msg {
  color: var(--td-error-color);
}
.logline--warn .logline__msg {
  color: var(--td-warning-color);
}
.logline--debug .logline__msg,
.logline--trace .logline__msg {
  color: var(--td-text-color-placeholder);
}
.tag {
  border: 1px solid var(--td-component-border);
  border-radius: 2px;
  padding: 0 4px;
  font-size: 11px;
  line-height: 18px;
}
.logbox__empty {
  color: var(--td-text-color-placeholder);
  text-align: center;
  padding-top: 24px;
}
</style>
```

同文件 `<script>` 之外（`.vue` 单文件不能被 spec 直接 import 纯函数——**把 `filterAppLog` 抽到同目录 `AppLogFilter.ts`，`.vue` 与 spec 都从它 import**）：

创建 `src/views/AppLogFilter.ts`：

```typescript
/**
 * 运行日志过滤纯函数（AppLog.vue 与 spec 共用；抽离 .vue 便于直测）。
 */
import type { AppLogItem, AppLogLevelName, AppLogSourceName } from '../stores/app';

export interface LogFilter {
  minLevel: AppLogLevelName;
  sources: AppLogSourceName[];
}

const LEVEL_ORDER: AppLogLevelName[] = ['trace', 'debug', 'info', 'warn', 'error'];

/** 级别过滤：minLevel 及以上保留（error 最高） */
export function filterAppLog(items: AppLogItem[], f: LogFilter): AppLogItem[] {
  const min = LEVEL_ORDER.indexOf(f.minLevel);
  return items.filter(
    (it) => LEVEL_ORDER.indexOf(it.level) >= min && f.sources.includes(it.source),
  );
}
```

`.vue` 里改 `import { filterAppLog, type LogFilter } from './AppLogFilter';`。spec 同样从 `./AppLogFilter` import（**修正 Step 1 的 import 路径为 `./AppLogFilter`**）。

- [ ] **Step 4: App.vue 接入**

`App.vue` 模板「指令日志」菜单项之后加：

```html
        <t-menu-item value="applog">
          <template #icon><t-icon name="file-paste" /></template>运行日志
        </t-menu-item>
```

视图分支 `CommandLog v-else-if` 之后加：

```html
        <AppLogView v-else-if="view === 'applog'" />
```

script import 区加：

```typescript
import AppLogView from './views/AppLog.vue';
```

- [ ] **Step 5: 跑测试 + typecheck + build**

Run: `cd /home/working/lyagent/desktop && pnpm vitest run 2>&1 | tail -5`
Expected: 全部 passed（app.spec + AppLog.spec）。

Run: `cd /home/working/lyagent/desktop && pnpm typecheck 2>&1 | tail -3 && pnpm build 2>&1 | tail -3`
Expected: 均无错误（build 产出 dist）。

- [ ] **Step 6: Commit**

```bash
cd /home/working/lyagent/desktop
git add src/views/AppLog.vue src/views/AppLogFilter.ts src/views/AppLog.spec.ts src/App.vue
git commit -m "feat: 运行日志视图(级别/来源过滤+自动贴底)+菜单接入(Task6)"
```

---

### Task 7: 全链路验证 + 冒烟 + 文档

**Files:**
- Modify: `desktop/WINDOWS-安装打包流程.txt`（如涉及打包行为说明——windows_subsystem 变化补一句）

**Interfaces:**
- Consumes: 全部前序任务
- Produces: 验证记录（commit message + 冒烟日志）

- [ ] **Step 1: Rust 全量测试**

Run: `cd /home/working/lyagent/desktop/src-tauri && cargo test 2>&1 | tail -10`
Expected: 全绿（bin + lib + 7 个集成测试文件）。

- [ ] **Step 2: clippy 全量**

Run: `cd /home/working/lyagent/desktop/src-tauri && cargo clippy --all-targets 2>&1 | tail -3`
Expected: 无 warning。

- [ ] **Step 3: CLI 冒烟（零回归硬验证）**

Run: `cd /home/working/lyagent/desktop && bash scripts/smoke_cli.sh binary 2>&1 | tail -15`
Expected: 脚本输出 PASS（结尾 `smoke] PASS` 字样）。若 target/debug/wxauto-desktop 未构建，先 `cargo build` 或用默认 cargo 模式。

- [ ] **Step 4: GUI 冒烟（mock 模式起日志链）**

Linux 无法开真窗口验证视觉，但可验证日志链不炸：

Run: `cd /home/working/lyagent/desktop/src-tauri && timeout 15 cargo run 2>/tmp/wxauto-gui-smoke.log; grep -c "GUI 装配完成\|app-log" /tmp/wxauto-gui-smoke.log || true`
Expected: 进程 15s 内被 timeout 杀掉（GUI 事件循环阻塞——正常）；日志无 panic/ERROR 级错误（`grep -c ERROR /tmp/wxauto-gui-smoke.log` 为 0 或仅业务警告）。

- [ ] **Step 5: 前端全量验证**

Run: `cd /home/working/lyagent/desktop && pnpm vitest run && pnpm typecheck && pnpm build 2>&1 | tail -3`
Expected: 全绿。

- [ ] **Step 6: 文档更新**

`WINDOWS-安装打包流程.txt` 末尾追加一段（若文件已有「常见问题」节则并入）：

```
【2026-09-03 单窗口化】
- release 构建已加 windows_subsystem=windows：安装态不再出现 cmd 日志窗口；
  排障需日志时看应用内「运行日志」tab（rust+sidecar 双来源，可过滤级别/来源）。
- debug 构建仍保留控制台（开发排障用）。
- sidecar stderr 已由 Rust 接管（piped）——不再直通控制台；CREATE_NO_WINDOW
  三条 spawn 路径齐备，任何路径不再闪黑窗。
```

- [ ] **Step 7: Commit**

```bash
cd /home/working/lyagent/desktop
git add WINDOWS-安装打包流程.txt
git commit -m "docs: 单窗口化+运行日志tab发版说明(Task7)"
```

---

## 自审记录（写计划后自查）

1. **Spec 覆盖**：§2 三处根因（windows_subsystem→Task 4、stderr piped→Task 3、CREATE_NO_WINDOW 三路径→Task 3 Step 3d）✓；§4 事件契约（Task 1 serde 测试 + Task 4 emit + Task 5 守卫）✓；§5 模块细节（LogRing 1000/mpsc 256→Task 2、get_recent_logs→Task 4、视图过滤/贴底→Task 6）✓；§6 边界（CLI 零回归→Task 3 双变体 + Task 7 冒烟、洪水有界→Task 2 丢行测试）✓；§7 测试策略全部落位 ✓。
2. **占位符扫描**：Task 5 Step 1 的 `APP_LOG_LIMIT_PLACEHOLDER` 已在文中显式标注替换方式（非留白）；无其他 TBD。
3. **类型一致性**：`AppLogEntry` 字段（ts/level/source/message）在 Task 1 定义、Task 4 serde 测试、Task 5 `AppLogItem` 同名同序 ✓；`LogRing::new(cap)`/`snapshot()`/`clear()` 三任务间签名一致 ✓；`spawn_default_sunk(Some(Arc<dyn StderrSink>))` Task 3 定义、Task 4 消费 ✓；`filterAppLog(items, f)` Task 6 内部自洽 ✓。
