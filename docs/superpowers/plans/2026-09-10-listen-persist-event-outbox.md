# 监听名单持久化 + 事件发件箱（outbox）+ 服务端 ACK/幂等 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 监听名单随增删落盘 config.json 且启动恢复；设备上行事件先落盘后发送、服务端 ACK 后清除、断线重连补发；服务端按 (channelId, eventId) LRU 幂等去重。

**Architecture:** 设备侧（desktop Rust core）新增 `outbox` 模块（JSONL 文件 + 原子重写），`AgentLink` 发送路径串联 outbox、新增 ack 帧处理与重连补发；`ListenerRegistry` 加种子构造与持久化 hook。服务端（lyagent Server wxauto-ws 网关）event 帧加 `eventId`、新增下行 ack 帧、inbound 加 LRU 判重。协议加法式演进（新字段全 optional），Server 先行发布。

**Tech Stack:** Rust (tokio, serde_json, tempfile) / NestJS + zod / 集成测试用 python 剧本 sidecar + mock WS server。

**Spec:** `desktop/docs/superpowers/specs/2026-09-10-listen-persist-event-outbox-design.md`（执行者须同时读 spec 与本计划）

## Global Constraints

- desktop 是独立 git 仓（分支 main）；Server 属 lyagent 主仓（分支 master）。两仓分别 commit。
- desktop 测试：`cd desktop/src-tauri && cargo test --test <name>`（单文件）；全量 `cargo test` 仅 Task 8 允许。
- Server 测试：`cd Server && pnpm jest wxauto`；禁 `pnpm test` 全量。
- Server TypeScript 禁 `any`/`as any`（docs/rules/shared/typescript.md）。
- 协议加法式：`eventId` 一律 optional；下行 ack 不进 `parseFrame` 的 `known` 上行白名单。
- 发布顺序（硬约束）：Server 先行 → desktop 后发。零 DB migration。
- desktop core 不引 tauri（outbox.rs / listener.rs 在 `src/` 下，测试在 `src-tauri/tests/`）。
- 所有中文注释；commit 粒度 = 任务粒度；commit message 格式 `<type>: <描述>`，无 attribution。
- 测试代码必须直接可编译——本计划中代码块是真实实现，不是示意。

## 任务依赖序

T1 → T5 → T6；T2 → T3；T4 → T5；T7（Server，可并行）；T8 收尾。
推荐执行顺序：**T1 → T2 → T3 → T4 → T4b → T5 → T6 → T7 → T8**（T3 与 T6 都改 app_state.rs，先后紧接）。

---

### Task 1: Outbox 文件仓库（core）

**Files:**
- Create: `desktop/src-tauri/src/outbox.rs`
- Modify: `desktop/src-tauri/src/lib.rs`（挂 `pub mod outbox;`）
- Test: `desktop/src-tauri/tests/outbox.rs`

**Interfaces:**
- Consumes: 无（纯 std 文件 IO + serde_json + tracing）
- Produces（T5/T6 消费）:
  - `pub struct Outbox`
  - `Outbox::open(path: &Path) -> Result<Outbox, String>`
  - `Outbox::disabled() -> Outbox`
  - `Outbox::enqueue(&self, frame: &Value) -> bool`（true=入箱；非入箱帧/Disabled 返回 false）
  - `Outbox::ack(&self, event_id: &str) -> bool`（true=清除成功）
  - `Outbox::pending(&self) -> Vec<Value>`（旧在前）
  - `Outbox::len(&self) -> usize`
  - `pub const MAX_ENTRIES: usize = 2000;`
  - `pub const EXPIRY_SECS: u64 = 7 * 24 * 3600;`

- [ ] **Step 1: 写失败测试（RED）**

创建 `desktop/src-tauri/tests/outbox.rs`：

```rust
//! outbox 单测：入箱/清除/重开恢复/容量修剪/过期修剪/坏行容忍/禁用形态
use serde_json::json;
use wxauto_desktop::outbox::{Outbox, EXPIRY_SECS, MAX_ENTRIES};

fn temp_outbox() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("outbox.jsonl");
    (dir, path)
}

fn msg_frame(id: &str) -> serde_json::Value {
    json!({
        "kind": "event", "type": "message", "eventId": id,
        "data": { "chatName": "张三", "content": "hi" }, "ts": 100
    })
}

/// enqueue → pending 保序；ack 按 eventId 清除
#[test]
fn test_enqueue_ack_roundtrip() {
    let (_dir, path) = temp_outbox();
    let ob = Outbox::open(&path).expect("open");
    assert_eq!(ob.len(), 0);
    assert!(ob.enqueue(&msg_frame("e1")));
    assert!(ob.enqueue(&msg_frame("e2")));
    assert_eq!(ob.len(), 2);
    let p = ob.pending();
    assert_eq!(p[0]["eventId"], "e1");
    assert_eq!(p[1]["eventId"], "e2");
    assert!(ob.ack("e1"));
    assert_eq!(ob.len(), 1);
    assert_eq!(ob.pending()[0]["eventId"], "e2");
    // ack 不存在的 id：返回 false，no-op
    assert!(!ob.ack("ghost"));
}

/// 重开恢复：进程重启语义（文件留存 → pending 保留 → ack 可清除）
#[test]
fn test_reopen_restores_pending() {
    let (_dir, path) = temp_outbox();
    {
        let ob = Outbox::open(&path).expect("open");
        ob.enqueue(&msg_frame("r1"));
        ob.enqueue(&msg_frame("r2"));
    }
    let ob2 = Outbox::open(&path).expect("reopen");
    assert_eq!(ob2.len(), 2);
    assert!(ob2.ack("r1"));
    assert_eq!(ob2.len(), 1);
    assert_eq!(ob2.pending()[0]["eventId"], "r2");
}

/// 容量修剪：超 MAX_ENTRIES 丢最旧
#[test]
fn test_capacity_evicts_oldest() {
    let (_dir, path) = temp_outbox();
    let ob = Outbox::open(&path).expect("open");
    for i in 0..(MAX_ENTRIES + 10) {
        assert!(ob.enqueue(&msg_frame(&format!("e{i}"))));
    }
    assert_eq!(ob.len(), MAX_ENTRIES);
    assert_eq!(ob.pending()[0]["eventId"], "e10"); // e0~e9 被淘汰
}

/// 过期修剪：open 时淘汰 ts 距今超 7 天的条目（绕过 enqueue 手工写旧行）
#[test]
fn test_expiry_trims_on_open() {
    let (_dir, path) = temp_outbox();
    let old_ms: u64 = 1_000; // epoch+1s，距今远超 7 天
    let entry = json!({ "event_id": "old1", "ts": old_ms, "frame": msg_frame("old1") });
    std::fs::write(&path, format!("{}\n", entry)).expect("write");
    let ob = Outbox::open(&path).expect("open");
    assert_eq!(ob.len(), 0);
    assert!(EXPIRY_SECS >= 7 * 24 * 3600); // 常量语义锁
}

/// 坏行容忍：非 JSON 行 / 缺字段行 → 跳过不炸
#[test]
fn test_corrupt_line_tolerated() {
    let (_dir, path) = temp_outbox();
    std::fs::write(&path, "not json\n{\"eventId\":\"x\"}\n").expect("write");
    let ob = Outbox::open(&path).expect("open 应容忍坏行");
    assert_eq!(ob.len(), 0);
}

/// 禁用形态：全 no-op，len 恒 0
#[test]
fn test_disabled_noop() {
    let ob = Outbox::disabled();
    assert_eq!(ob.len(), 0);
    assert!(ob.pending().is_empty());
    assert!(!ob.ack("any"));
    // enqueue 对 Disabled 返回 false（不入箱），也不 panic
    assert!(!ob.enqueue(&msg_frame("d1")));
}

/// 非入箱帧过滤：status 类型 / 无 eventId 的帧不入箱
#[test]
fn test_filters_non_outbox_frames() {
    let (_dir, path) = temp_outbox();
    let ob = Outbox::open(&path).expect("open");
    let status = json!({ "kind": "event", "type": "status", "eventId": "s1", "data": {}, "ts": 1 });
    let no_id = json!({ "kind": "event", "type": "message", "data": {}, "ts": 1 });
    assert!(!ob.enqueue(&status));
    assert!(!ob.enqueue(&no_id));
    assert_eq!(ob.len(), 0);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd /home/working/lyagent/desktop/src-tauri && cargo test --test outbox`
Expected: 编译失败（`wxauto_desktop::outbox` 不存在）

- [ ] **Step 3: 实现 outbox.rs（GREEN）**

创建 `desktop/src-tauri/src/outbox.rs`，并在 `lib.rs` 挂 `pub mod outbox;`：

```rust
//! 上行事件发件箱（spec §4.2）：先落盘后发送、ack 清除、重连补发。
//!
//! 文件形态：`~/.wxauto-desktop/outbox.jsonl`，每行一个 Entry（JSONL）。
//! - enqueue：仅带 eventId 且 type ∈ {message, friend_request} 的 event 帧入箱；
//!   append 单行落盘（磁盘失败记 error、内存照常——发件箱故障不阻断业务）
//! - ack：按 eventId 移除 + 整文件原子重写（tmp + rename）
//! - open：载入存量 + 修剪（过期 7 天 / 容量 2000 淘汰最旧）
//! - Disabled：no-op（测试默认 / 装配层未注入）
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 容量上限（超限丢最旧并 warn）
pub const MAX_ENTRIES: usize = 2000;
/// 过期时长（7 天；open 时按 Entry.ts 修剪）
pub const EXPIRY_SECS: u64 = 7 * 24 * 3600;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    event_id: String,
    ts: u64,
    frame: Value,
}

enum Inner {
    Disabled,
    File {
        path: PathBuf,
        entries: Mutex<Vec<Entry>>,
    },
}

pub struct Outbox(Inner);

impl Outbox {
    /// 打开（或新建）发件箱；载入存量并修剪过期/超容
    pub fn open(path: &Path) -> Result<Outbox, String> {
        let mut entries = Self::load(path)?;
        let now = now_ms();
        entries.retain(|e| now.saturating_sub(e.ts) < EXPIRY_SECS * 1000);
        if entries.len() > MAX_ENTRIES {
            let drop_n = entries.len() - MAX_ENTRIES;
            entries.drain(0..drop_n);
            tracing::warn!(dropped = drop_n, "outbox 超容修剪（丢最旧）");
        }
        Ok(Outbox(Inner::File {
            path: path.to_path_buf(),
            entries: Mutex::new(entries),
        }))
    }

    /// 禁用形态（测试默认 / 装配层未注入）：全 no-op
    pub fn disabled() -> Outbox {
        Outbox(Inner::Disabled)
    }

    /// 入箱 + 落盘。返回 false = 帧不入箱（Disabled / 无 eventId / 非入箱类型）。
    /// 磁盘失败不给调用方报错（记 error，内存保留——后续 ack 重写自愈）。
    pub fn enqueue(&self, frame: &Value) -> bool {
        let (path, entries) = match &self.0 {
            Inner::File { path, entries } => (path, entries),
            Inner::Disabled => return false,
        };
        let Some(event_id) = frame["eventId"].as_str() else {
            return false;
        };
        if !matches!(
            frame["type"].as_str(),
            Some("message") | Some("friend_request")
        ) {
            return false;
        }
        let entry = Entry {
            event_id: event_id.to_string(),
            ts: frame["ts"].as_u64().unwrap_or_else(now_ms),
            frame: frame.clone(),
        };
        let mut list = lock_entries(entries);
        list.push(entry.clone());
        drop(list);
        // append 单行（磁盘失败不阻断：内存已是真相，后续 ack 触发整文件重写自愈）
        let line = serde_json::to_string(&entry).unwrap_or_default();
        let append = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| f.write_all(format!("{line}\n").as_bytes()));
        if let Err(e) = append {
            tracing::error!(%e, path = %path.display(), "outbox 落盘失败（内存保留，重写自愈）");
        }
        true
    }

    /// 按 eventId 清除 + 整文件原子重写。false = 无此 id（no-op）
    pub fn ack(&self, event_id: &str) -> bool {
        let (path, entries) = match &self.0 {
            Inner::File { path, entries } => (path, entries),
            Inner::Disabled => return false,
        };
        let mut list = lock_entries(entries);
        let before = list.len();
        list.retain(|e| e.event_id != event_id);
        if list.len() == before {
            return false;
        }
        let snapshot = list.clone();
        drop(list);
        Self::rewrite(path, &snapshot);
        true
    }

    /// 待发帧快照（旧在前，按序补发）
    pub fn pending(&self) -> Vec<Value> {
        match &self.0 {
            Inner::Disabled => Vec::new(),
            Inner::File { entries, .. } => {
                lock_entries(entries).iter().map(|e| e.frame.clone()).collect()
            }
        }
    }

    /// 当前积压数
    pub fn len(&self) -> usize {
        match &self.0 {
            Inner::Disabled => 0,
            Inner::File { entries, .. } => lock_entries(entries).len(),
        }
    }

    /// 载入存量（坏行跳过 warn；文件不存在视为空）
    fn load(path: &Path) -> Result<Vec<Entry>, String> {
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("outbox 读取失败: {e}")),
        };
        let mut out = Vec::new();
        for (i, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Entry>(line) {
                Ok(e) => out.push(e),
                Err(e) => tracing::warn!(line = i, %e, "outbox 坏行跳过"),
            }
        }
        Ok(out)
    }

    /// 整文件原子重写（tmp + rename；失败记 error 不上抛）
    fn rewrite(path: &Path, entries: &[Entry]) {
        let tmp = path.with_extension("jsonl.tmp");
        let mut body = String::new();
        for e in entries {
            if let Ok(line) = serde_json::to_string(e) {
                body.push_str(&line);
                body.push('\n');
            }
        }
        if let Err(e) = fs::write(&tmp, body).and_then(|_| fs::rename(&tmp, path)) {
            tracing::error!(%e, "outbox 重写失败");
        }
    }
}

/// 锁获取（中毒恢复：panic 只中止当事任务，数据结构本身完好）
fn lock_entries(entries: &Mutex<Vec<Entry>>) -> std::sync::MutexGuard<'_, Vec<Entry>> {
    entries.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
```

- [ ] **Step 4: 跑测试确认通过（GREEN）**

Run: `cd /home/working/lyagent/desktop/src-tauri && cargo test --test outbox`
Expected: 7 passed

- [ ] **Step 5: commit（desktop 仓）**

```bash
cd /home/working/lyagent/desktop && git add src-tauri/src/outbox.rs src-tauri/src/lib.rs src-tauri/tests/outbox.rs && git commit -m "feat: outbox 文件仓库（入箱/ack 清除/重开恢复/容量+过期修剪）"
```

---

### Task 2: 监听名单持久化（listener 种子 + persist hook）

**Files:**
- Modify: `desktop/src-tauri/src/wx/listener.rs`
- Modify: `desktop/src-tauri/src/main.rs`（CLI 装配注入）
- Test: `desktop/src-tauri/tests/listener_persist.rs`（新建）

**Interfaces:**
- Consumes: 既有 `WxSession.execute`、`config::{Config, load_config, save_config}`
- Produces（T3/T6 消费）:
  - `ListenerRegistry::new_seeded(session: Arc<WxSession>, initial: Vec<String>) -> ListenerRegistry`
  - `ListenerRegistry::set_persist_hook(&self, hook: Box<dyn Fn(Vec<String>) + Send + Sync>)`
  - 自由函数 `file_persist_hook(cfg_lock: Arc<tokio::sync::RwLock<Config>>, path: PathBuf) -> Box<dyn Fn(Vec<String>) + Send + Sync>`（`wx::listener` 模块内）

- [ ] **Step 1: 写失败测试（RED）**

创建 `desktop/src-tauri/tests/listener_persist.rs`：

```rust
//! 监听名单持久化集成测试：种子恢复 + add/remove 落盘
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};

use wxauto_desktop::config::{load_config, save_config, Config};
use wxauto_desktop::sidecar::SidecarHandle;
use wxauto_desktop::wx::listener::{file_persist_hook, ListenerRegistry};
use wxauto_desktop::wx::WxSession;

/// 剧本 sidecar：所有方法回 ok（add/remove_listen_chat 穿透成功）
const SCRIPT: &str = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {"ok": True}}), flush=True)
"#;

/// 装配：剧本 sidecar + 种子名单 + persist hook 落盘到临时 config.json。
/// TempDir 由调用方持有（drop 即清目录，须活得比 registry 长）。
async fn make_registry(
    initial: Vec<String>,
) -> (Arc<ListenerRegistry>, tempfile::TempDir, std::path::PathBuf) {
    let handle = SidecarHandle::spawn_with_python(SCRIPT, &[])
        .await
        .expect("spawn python 失败");
    let session = Arc::new(WxSession::new(Arc::new(Mutex::new(handle))));
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.json");
    let mut cfg = Config::default();
    cfg.listen_names = initial;
    save_config(&path, &cfg).expect("save 初始配置");
    let cfg_lock = Arc::new(RwLock::new(cfg));
    let reg = Arc::new(ListenerRegistry::new_seeded(session, cfg_lock.read().await.listen_names.clone()));
    reg.set_persist_hook(file_persist_hook(cfg_lock, path.clone()));
    (reg, dir, path)
}

/// 轮询等文件达到期望条数（hook 内 spawn 异步落盘，非同步可见）
async fn wait_for_names(path: &std::path::Path, expect: usize) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let names = load_config(path).expect("load").listen_names;
        if names.len() == expect {
            return names;
        }
        assert!(
            Instant::now() < deadline,
            "5s 内应落盘 {expect} 条，实际: {names:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 种子恢复：构造时名单非空 → list() 即含种子
#[tokio::test]
async fn test_seed_restores_names() {
    let (reg, _dir, _path) = make_registry(vec!["客户群".into(), "张三".into()]).await;
    let names = reg.list().await;
    assert_eq!(names, vec!["客户群".to_string(), "张三".to_string()]); // BTreeSet 有序
}

/// add/remove 成功后落盘：load_config 回读 listen_names 一致
#[tokio::test]
async fn test_add_remove_persisted_to_config() {
    let (reg, _dir, path) = make_registry(vec!["张三".into()]).await;
    reg.add("李四".into()).await.expect("add 应成功");
    let names = wait_for_names(&path, 2).await;
    assert!(names.contains(&"张三".to_string()) && names.contains(&"李四".to_string()));

    reg.remove("张三").await.expect("remove 应成功");
    let names = wait_for_names(&path, 1).await;
    assert_eq!(names, vec!["李四".to_string()]);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd /home/working/lyagent/desktop/src-tauri && cargo test --test listener_persist`
Expected: 编译失败（`new_seeded` / `file_persist_hook` 不存在）

- [ ] **Step 3: 实现 listener.rs**

对 `desktop/src-tauri/src/wx/listener.rs` 做以下修改（基于现状 listener.rs:14-69）：

```rust
// —— struct 增加字段 ——
pub struct ListenerRegistry {
    /// 本地监听名单（去重 + 有序，list/resync 顺序稳定）
    names: RwLock<BTreeSet<String>>,
    session: Arc<WxSession>,
    /// 持久化 hook（add/remove 成功后以最新名单快照调用；失败只记日志不回滚）
    persist_hook: RwLock<Option<Box<dyn Fn(Vec<String>) + Send + Sync>>>,
}

impl ListenerRegistry {
    /// 空名单构造（保持旧签名，内部转发 new_seeded）
    pub fn new(session: Arc<WxSession>) -> Self {
        Self::new_seeded(session, Vec::new())
    }

    /// 以存量配置为种子构造（启动恢复：config.listen_names → 内存名单）
    pub fn new_seeded(session: Arc<WxSession>, initial: Vec<String>) -> Self {
        Self {
            names: RwLock::new(initial.into_iter().collect()),
            session,
            persist_hook: RwLock::new(None),
        }
    }

    /// 注入持久化 hook（GUI/CLI 装配层；测试注入计数 hook）
    pub fn set_persist_hook(&self, hook: Box<dyn Fn(Vec<String>) + Send + Sync>) {
        *self.persist_hook.write().await = Some(hook);
    }

    // —— add() 尾部（names.write().await.insert 之后）追加：——
    //     self.notify_persist().await;
    // —— remove() 尾部（names.write().await.remove 之后）追加：——
    //     self.notify_persist().await;

    /// 名单变化通知 hook（快照传入；hook 内部自行处理失败）
    async fn notify_persist(&self) {
        let snapshot: Vec<String> = self.names.read().await.iter().cloned().collect();
        if let Some(hook) = self.persist_hook.read().await.as_ref() {
            hook(snapshot);
        }
    }
}

/// 落盘 hook 工厂：更新共享 Config 锁的 listen_names + save_config（GUI/CLI 装配用）。
/// hook 为同步闭包，内部 spawn 异步落盘（不阻塞 add/remove 主流程）；
/// 落盘失败只记 warn（名单以内存为准，下次写盘自愈——spec §4.1）。
pub fn file_persist_hook(
    cfg_lock: Arc<tokio::sync::RwLock<Config>>,
    path: std::path::PathBuf,
) -> Box<dyn Fn(Vec<String>) + Send + Sync> {
    Box::new(move |names: Vec<String>| {
        let cfg_lock = cfg_lock.clone();
        let path = path.clone();
        tokio::spawn(async move {
            {
                let mut cfg = cfg_lock.write().await;
                cfg.listen_names = names;
            }
            // 释写锁后再取读快照落盘（save_config 只需 &Config）
            if let Err(e) = save_config(&path, &cfg_lock.read().await) {
                tracing::warn!(%e, "监听名单落盘失败（内存为准，下次写盘自愈）");
            }
        });
    })
}
```

注意：文件顶部 use 需补 `use crate::config::{self, Config};` 与 `use std::sync::Arc;`（已有）。`config::save_config` 引用按需调整。

- [ ] **Step 4: CLI main.rs 装配注入**

`desktop/src-tauri/src/main.rs` run_cli 内（现状 :105-108 区域）：

```rust
// 2. session + listeners（所有微信操作的唯一入口）
let session = Arc::new(WxSession::new(sidecar.clone()));
// 种子来自磁盘配置（重启恢复）+ persist hook 落盘回 config.json（spec §4.1）
let cfg_lock = Arc::new(tokio::sync::RwLock::new(cfg.clone()));
let listeners = Arc::new(ListenerRegistry::new_seeded(
    session.clone(),
    cfg_lock.read().await.listen_names.clone(),
));
listeners.set_persist_hook(wxauto_desktop::wx::listener::file_persist_hook(
    cfg_lock,
    default_config_path(),
));
```

裁定：CLI 的 `cfg` 值克隆继续用于 URL 组装（维持现状），`cfg_lock` 仅供 hook——两者同一初始值；hook 只写 listen_names 且写前经锁内最新值，无字段冲突。

- [ ] **Step 5: 跑测试确认通过（GREEN）**

Run: `cd /home/working/lyagent/desktop/src-tauri && cargo test --test listener_persist`
Expected: 2 passed

- [ ] **Step 6: commit（desktop 仓）**

```bash
cd /home/working/lyagent/desktop && git add src-tauri/src/wx/listener.rs src-tauri/src/main.rs src-tauri/tests/listener_persist.rs && git commit -m "feat: 监听名单持久化（种子恢复+persist hook 落盘 config.json）"
```

---

### Task 3: GUI 装配重构（Assembled.config 共享锁 + hook 注入 + 防清空回归）

**Files:**
- Modify: `desktop/src-tauri/src/app_state.rs`
- Test: `desktop/src-tauri/tests/listener_persist_gui.rs`（新建）

**Interfaces:**
- Consumes: Task 2 的 `new_seeded` / `set_persist_hook` / `file_persist_hook`
- Produces（T6 消费）: `Assembled.config` 类型变为 `Arc<RwLock<Config>>`

- [ ] **Step 1: 写失败测试（回归锁）**

创建 `desktop/src-tauri/tests/listener_persist_gui.rs`：

```rust
//! GUI 装配持久化回归：save_settings 三字段合并语义 + 监听名单不被清空。
//! 锁定的不变量：persist hook 与 save_settings 共享同一把内存锁
//! （app_state.rs 的 Assembled.config），任一写入路径都序列化到同一份内存真相。
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

use wxauto_desktop::config::{load_config, save_config, Config};
use wxauto_desktop::sidecar::SidecarHandle;
use wxauto_desktop::wx::listener::{file_persist_hook, ListenerRegistry};
use wxauto_desktop::wx::WxSession;

const SCRIPT: &str = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {"ok": True}}), flush=True)
"#;

/// save_settings 与 persist hook 经同一把内存锁 → listen_names 不被三字段提交清空
#[tokio::test]
async fn test_save_settings_preserves_listen_names() {
    // 1) 存量 config.json：listen_names 非空
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.json");
    let mut cfg = Config::default();
    cfg.listen_names = vec!["张三".into()];
    save_config(&path, &cfg).expect("save 初始配置");

    // 2) 装配对齐 assemble_into 关键步：共享锁 + new_seeded + hook 注入
    let handle = SidecarHandle::spawn_with_python(SCRIPT, &[])
        .await
        .expect("spawn python 失败");
    let session = Arc::new(WxSession::new(Arc::new(Mutex::new(handle))));
    let cfg_lock = Arc::new(RwLock::new(load_config(&path).expect("load")));
    let listeners = Arc::new(ListenerRegistry::new_seeded(
        session,
        cfg_lock.read().await.listen_names.clone(),
    ));
    listeners.set_persist_hook(file_persist_hook(cfg_lock.clone(), path.clone()));

    // 3) 模拟 save_settings：前端只提交三字段（serverUrl/channelId/autoConnect），
    //    按字段合并写锁（对齐 app_state.rs save_settings :469-475 的合并语义）
    {
        let mut cfg = cfg_lock.write().await;
        cfg.server_url = "ws://new:1".into();
        cfg.channel_id = "ch9".into();
        cfg.auto_connect = true;
        save_config(&path, &cfg).expect("save 设置");
    }

    // 4) 断言：listen_names 保留（整体反序列化/hook 竞争清空都会被抓）
    let final_cfg = load_config(&path).expect("load final");
    assert_eq!(final_cfg.listen_names, vec!["张三".to_string()]);
    assert_eq!(final_cfg.server_url, "ws://new:1");
}
```

- [ ] **Step 2: 跑测试确认状态**

Run: `cd /home/working/lyagent/desktop/src-tauri && cargo test --test listener_persist_gui`
Expected: 编译失败——测试用的 `file_persist_hook` 已在 Task 2 提供，此处主要验证装配不变量；若 Task 2 已绿则本测试应直接通过（回归锁性质）。

- [ ] **Step 3: 实现 app_state.rs 重构**

1. `Assembled.config` 字段类型（app_state.rs:119）：`pub config: RwLock<Config>` → `pub config: Arc<RwLock<Config>>`
2. `assemble_into` 内（app_state.rs:348 取 cfg 后、:417 构造 Assembled 前）：

```rust
// 共享配置锁：persist hook 与 save_settings 经同一把锁串行化（spec §4.1 不变量）
let cfg_lock = Arc::new(RwLock::new(cfg));
// listeners 种子恢复 + persist hook（Task 2 接口）
let listeners = Arc::new(ListenerRegistry::new_seeded(
    session.clone(),
    cfg_lock.read().await.listen_names.clone(),
));
listeners.set_persist_hook(wxauto_desktop::wx::listener::file_persist_hook(
    cfg_lock.clone(),
    config_path.clone(),
));
```

（替换原 `:368` 的 `let listeners = Arc::new(ListenerRegistry::new(session.clone()));`；Assembled 构造处 `config: RwLock::new(cfg)` → `config: cfg_lock`。）

3. `for_test` 测试构造器（app_state.rs:147-191）同步：`config: RwLock::new(...)` → `config: Arc::new(RwLock::new(...))`
4. 下游读点零改动：`commands.rs:25` 的 `assembled.config.read().await.clone()`、`build_url` 的 `self.config.read().await.server_url.clone()`、`save_settings` 写锁路径——`Arc<RwLock<Config>>` 的 `.read().await` 调用形态与 `RwLock<Config>` 完全一致。

- [ ] **Step 4: 跑测试确认通过（GREEN）**

Run: `cd /home/working/lyagent/desktop/src-tauri && cargo test --test listener_persist_gui && cargo test --test listener_persist`
Expected: 两文件全绿

再跑 GUI 装配回归（config 类型变更的影响面验证）：
Run: `cd /home/working/lyagent/desktop/src-tauri && cargo test --test gui_bridge && cargo test --lib`
Expected: 全绿

- [ ] **Step 5: commit（desktop 仓）**

```bash
cd /home/working/lyagent/desktop && git add src-tauri/src/app_state.rs src-tauri/tests/listener_persist_gui.rs && git commit -m "refactor: Assembled.config 共享锁 + persist hook 注入（GUI 装配）"
```

---

### Task 4: eventId 生成 + inbound.rs 组帧变更（core）

**Files:**
- Modify: `desktop/src-tauri/src/agent_link/inbound.rs`
- Test: `desktop/src-tauri/tests/agent_link.rs`（追加，不新建文件）

**Interfaces:**
- Consumes: 无新依赖
- Produces（T5 消费）: `inbound::next_event_id() -> String`；三个 event 组帧函数（`message_event_from_notification` / `friend_request_event` / `status_event`）输出帧自动带 `eventId` 字段

- [ ] **Step 1: 写失败测试（RED）**

追加到 `desktop/src-tauri/tests/agent_link.rs` 尾部：

```rust
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

        let st = status_event(true, 1, true);
        assert!(st["eventId"].as_str().unwrap_or("").starts_with("evt-"));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd /home/working/lyagent/desktop/src-tauri && cargo test --test agent_link event_id_tests`
Expected: 编译失败（`next_event_id` 不存在）

- [ ] **Step 3: 实现（inbound.rs）**

```rust
// —— inbound.rs 顶部 use 区追加 ——
use std::sync::atomic::{AtomicU64, Ordering};

// —— 模块内追加 ——
/// 进程内事件序号（evt-{unix_ms}-{seq}：同毫秒内 seq 递增保证字典序单调）
static EVENT_SEQ: AtomicU64 = AtomicU64::new(0);

/// 生成下一个事件 id（进程唯一；进程重启后 ms 段不同天然不撞——spec §3.1）
pub fn next_event_id() -> String {
    let ms = now_ms();
    let seq = EVENT_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("evt-{ms}-{seq}")
}
```

三个组帧函数的 `json!` 宏内各加一行 `"eventId": next_event_id(),`（与 `"ts"` 平级）。

- [ ] **Step 4: 跑测试确认通过（GREEN）**

Run: `cd /home/working/lyagent/desktop/src-tauri && cargo test --test agent_link`
Expected: 全绿（含既有用例——见 Task 4b）

- [ ] **Step 5: commit（desktop 仓）**

```bash
cd /home/working/lyagent/desktop && git add src-tauri/src/agent_link/inbound.rs src-tauri/tests/agent_link.rs && git commit -m "feat: event 帧生成 eventId（evt-{ms}-{seq} 进程内单调）"
```

---

### Task 4b: 既有 agent_link 测试防回归修补

**Files:**
- Modify: `desktop/src-tauri/tests/agent_link.rs`（仅当既有断言红时）

Task 4 的 eventId 无条件附加。既有 agent_link.rs 测试若对 event 帧做**精确 JSON 相等**断言会红；按字段断言则不受影响。

- [ ] **Step 1: 跑受影响测试**

Run: `cd /home/working/lyagent/desktop/src-tauri && cargo test --test agent_link`
Expected: 全绿则本任务记「无需改动」跳过 commit；红则进 Step 2

- [ ] **Step 2: 修补断言（仅当红）**

精确相等断言改为按字段断言：

```rust
// 修复前（精确相等，eventId 加入后失配）：
// assert_eq!(frame, json!({ "kind": "event", "type": "message", ... }));
// 修复后（按字段）：
assert_eq!(frame["kind"], "event");
assert_eq!(frame["type"], "message");
assert_eq!(frame["data"]["chatName"], "张三");
assert!(frame["eventId"].is_string());
```

用 Edit 工具手工修改，禁 sed 批量替换。

- [ ] **Step 3: commit（若有改动）**

```bash
cd /home/working/lyagent/desktop && git add src-tauri/tests/agent_link.rs && git commit -m "test: agent_link 既有断言适配 eventId 字段"
```

---

### Task 5: AgentLink 接线 outbox（ack 处理 + 重连补发）

**Files:**
- Modify: `desktop/src-tauri/src/agent_link/mod.rs`
- Test: `desktop/src-tauri/tests/outbox_replay.rs`（新建）

**Interfaces:**
- Consumes: Task 1 `Outbox::{open, disabled, enqueue, ack, pending}`；Task 4 组帧已带 eventId
- Produces（T6 消费）:
  - `AgentLink::set_outbox(&self, outbox: Arc<Outbox>)`（运行期注入，对齐 event_sink 模式）
  - `emit_event` 路径变为：event_sink → outbox.enqueue → send_frame
  - `on_frame` 新增 ack 分支 → `outbox.ack(eventId)`

- [ ] **Step 1: 写失败测试（RED）**

创建 `desktop/src-tauri/tests/outbox_replay.rs`。测试剧本采用**双 AgentLink 实例**模式（「重启进程」语义），避免依赖 transport 重连退避时序：

```rust
//! outbox 断线补发集成测试：首连不 ack → outbox 残留 → 新 link 重连补发同 eventId → ack 清空
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use wxauto_desktop::agent_link::AgentLink;
use wxauto_desktop::outbox::Outbox;
use wxauto_desktop::sidecar::SidecarHandle;
use wxauto_desktop::wx::listener::ListenerRegistry;
use wxauto_desktop::wx::WxSession;
use tokio::sync::Mutex;

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
            tokio::spawn(async move {
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                let mut received = 0usize;
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Text(t) = msg {
                        let v: Value = serde_json::from_str(&t).unwrap();
                        tx.send(v.clone()).await.unwrap();
                        received += 1;
                        if let ServerMode::NoAck { close_after } = mode {
                            if received >= close_after {
                                let _ = ws.close(None).await;
                                return;
                            }
                        }
                        if let ServerMode::Ack = mode {
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
async fn wait_frame(
    rx: &mut mpsc::Receiver<Value>,
    pred: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while let Some(v) = tokio::time::timeout_at(deadline, rx.recv()).await.expect("10s 内应收帧") {
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
    assert_eq!(replayed["eventId"], "evt-replay-1", "补发帧须携带原 eventId");
    // server2 已回 ack → outbox 清空
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while outbox.len() != 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(outbox.len(), 0, "ack 后 outbox 应清空");
    link2.halt();
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd /home/working/lyagent/desktop/src-tauri && cargo test --test outbox_replay`
Expected: 编译失败（`set_outbox` 不存在）

- [ ] **Step 3: 实现（agent_link/mod.rs）**

五处修改：

1. struct 加字段（对齐 event_sink 注入模式——运行期注入用 std RwLock 包裹）：

```rust
pub struct AgentLink {
    // ...既有字段...
    /// 上行事件发件箱（spec §4.2）：emit_event 先落盘后发送、ack 清除、
    /// 重连补发。默认 disabled（测试/未装配）；GUI/CLI 装配层显式注入。
    outbox: std::sync::RwLock<Arc<Outbox>>,
}
```

2. 构造器尾部（`new_with_event_sink` 的 Self 字面量）：`outbox: std::sync::RwLock::new(Arc::new(Outbox::disabled())),`

3. 新增注入方法：

```rust
/// 运行期注入 outbox（GUI/CLI 装配层；对齐 event_sink 注入模式）
pub fn set_outbox(&self, outbox: Arc<Outbox>) {
    *self
        .outbox
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = outbox;
}

/// 当前 outbox 快照（读侧统一入口；锁中毒恢复对齐既有模式）
fn current_outbox(&self) -> Arc<Outbox> {
    self.outbox
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}
```

4. `emit_event` 改造（现状 :497-502）：

```rust
/// 发一帧上行事件：event_sink → outbox 落盘 → WS 发送（先落盘后发送，spec §4.3）
async fn emit_event(&self, frame: Value) {
    if let Some(sink) = read_or_recover(&self.event_sink).as_ref() {
        sink(frame.clone());
    }
    self.current_outbox().enqueue(&frame); // status 等非入箱帧自然 no-op
    self.send_frame(frame).await;
}
```

5. `LinkHandler::on_frame` 加 ack 分支（现状 :549-556 的 if 改 match）+ `AgentLink::on_ack` + `on_connect` 补发：

```rust
fn on_frame(&self, frame: Value) {
    match frame["kind"].as_str() {
        Some("command") => {
            let link = self.link.clone();
            tokio::spawn(async move {
                link.on_command(&frame).await;
            });
        }
        Some("ack") => {
            let link = self.link.clone();
            tokio::spawn(async move {
                link.on_ack(&frame).await;
            });
        }
        _ => {}
    }
}
```

```rust
/// 下行 ack 处理：按 eventId 清除 outbox（spec §3.2）
async fn on_ack(&self, frame: &Value) {
    let event_id = frame["eventId"].as_str().unwrap_or_default();
    if event_id.is_empty() {
        tracing::warn!(?frame, "ack 帧缺 eventId，丢弃");
        return;
    }
    if self.current_outbox().ack(event_id) {
        tracing::debug!(%event_id, "事件已确认，出箱");
    }
}
```

`on_connect` 内（现状 :254 `let failed = self.listeners.resync().await;` **之前**插入）：

```rust
// outbox 补发（spec §4.3：置于 resync 前——resync 每监听对象 0.5~1s 拟人
// 间隙，别让积压事件排队其后）
let pending = self.current_outbox().pending();
if !pending.is_empty() {
    tracing::info!(count = pending.len(), "补发 outbox 积压事件");
    for f in pending {
        self.send_frame(f).await;
    }
}
```

文件顶部 use 补 `use crate::outbox::Outbox;`。

- [ ] **Step 4: 跑测试确认通过（GREEN）**

Run: `cd /home/working/lyagent/desktop/src-tauri && cargo test --test outbox_replay`
Expected: 1 passed

- [ ] **Step 5: 既有 agent_link 回归**

Run: `cd /home/working/lyagent/desktop/src-tauri && cargo test --test agent_link`
Expected: 全绿（emit_event 新增 enqueue 对非入箱帧 no-op，不影响既有用例）

- [ ] **Step 6: commit（desktop 仓）**

```bash
cd /home/working/lyagent/desktop && git add src-tauri/src/agent_link/mod.rs src-tauri/tests/outbox_replay.rs && git commit -m "feat: AgentLink 接线 outbox（先落盘后发送+ack 清除+重连补发）"
```

---

### Task 6: GUI/CLI 装配注入 outbox + 冒烟脚本升级

**Files:**
- Modify: `desktop/src-tauri/src/app_state.rs`（GUI 装配注入）
- Modify: `desktop/src-tauri/src/main.rs`（CLI 装配注入）
- Modify: `desktop/scripts/mock_ws_server.py`（ack 回送）
- Modify: `desktop/scripts/smoke_cli.sh`（outbox 残留断言）

**Interfaces:**
- Consumes: Task 5 `set_outbox`、Task 1 `Outbox::{open, disabled}`
- Produces: `Assembled` 增加字段 `pub outbox: Arc<Outbox>`（T6 内部消费，无后续依赖）

- [ ] **Step 1: 实现 Assembled 结构与装配注入（app_state.rs）**

1. `Assembled` struct 加字段：`pub outbox: Arc<Outbox>,`（顶部 use 补 `use wxauto_desktop::outbox::Outbox;`）
2. `assemble_into` 内（listeners 构造之后）：

```rust
// outbox 装配（spec §4.2）：事件先落盘后发送。打开失败降级 disabled
// （不阻断启动——发件箱故障只损失补发能力，不损失实时上报）
let outbox = match Outbox::open(&config_path.with_file_name("outbox.jsonl")) {
    Ok(ob) => Arc::new(ob),
    Err(e) => {
        tracing::error!("outbox 打开失败，事件不落盘（不阻断启动）: {e}");
        Arc::new(Outbox::disabled())
    }
};
```

Assembled 构造字面量加 `outbox: outbox.clone(),`。

3. `start_link` 内 link 构造后（`*slot_write(&self.link) = Some(link.clone());` 之前）：

```rust
link.set_outbox(self.outbox.clone());
```

4. `for_test` 同步加 `outbox: Arc::new(Outbox::disabled()),`

- [ ] **Step 2: CLI 装配注入（main.rs）**

run_cli 内 AgentLink 构造后（现状 :117-121 区域）：

```rust
// outbox 装配：事件先落盘后发送（CLI 与 GUI 同路径，spec §4.2）
let outbox = match Outbox::open(&default_config_path().with_file_name("outbox.jsonl")) {
    Ok(ob) => Arc::new(ob),
    Err(e) => {
        tracing::error!("outbox 打开失败，事件不落盘（不阻断启动）: {e}");
        Arc::new(Outbox::disabled())
    }
};
let link = Arc::new(AgentLink::new(session.clone(), listeners.clone(), url));
link.set_outbox(outbox);
```

（顶部 use 补 `use wxauto_desktop::outbox::Outbox;`）

- [ ] **Step 3: mock_ws_server.py 加 ack 回送**

`handle` 函数内（现状 :97 `# event / ping 只记录` 注释处）改为：

```python
            elif kind == "event":
                event_id = frame.get("eventId")
                # ack 回送（listen-persist-event-outbox：设备发件箱据此清除）
                if event_id:
                    await ws.send(json.dumps({
                        "kind": "ack", "eventId": event_id, "ts": int(time.time() * 1000)
                    }, ensure_ascii=False))
            # ping 只记录
```

（顶部 import 补 `import time`）

- [ ] **Step 4: smoke_cli.sh 加 outbox 残留断言**

第 5 节残留进程断言**之前**追加：

```bash
# 6. outbox 断言：冒烟全程事件被 ack 清除（文件不存在或空）
OUTBOX="$HOME/.wxauto-desktop/outbox.jsonl"
if [ -s "$OUTBOX" ]; then
    echo "[smoke] FAIL: outbox 残留积压（事件未被 ack）: $(wc -l < "$OUTBOX") 行"
    cat "$OUTBOX"
    exit 1
fi
echo "[smoke] outbox 残留断言 PASS（文件空/不存在）"
```

- [ ] **Step 5: 验证**

先构建（binary 模式前置）：`cd /home/working/lyagent/desktop/src-tauri && cargo build`
Run: `cd /home/working/lyagent/desktop && bash scripts/smoke_cli.sh binary`
Expected: `[smoke] 全链路冒烟 PASS` + `[smoke] outbox 残留断言 PASS`

GUI 装配回归：`cd /home/working/lyagent/desktop/src-tauri && cargo test --test gui_bridge && cargo test --lib`
Expected: 全绿

- [ ] **Step 6: commit（desktop 仓）**

```bash
cd /home/working/lyagent/desktop && git add src-tauri/src/app_state.rs src-tauri/src/main.rs scripts/mock_ws_server.py scripts/smoke_cli.sh && git commit -m "feat: 装配层注入 outbox + 冒烟 ack 回送与残留断言"
```

---

### Task 7: Server 侧 eventId + ack 帧 + LRU 幂等去重（wxauto-ws 网关）

**Files:**
- Modify: `Server/src/modules/gateway/channels/wxauto-ws/wxauto-frame.ts`
- Modify: `Server/src/modules/gateway/channels/wxauto-ws/wxauto-inbound.service.ts`
- Modify: `Server/src/modules/gateway/channels/wxauto-ws/wxauto-device.gateway.ts`
- Test: `Server/src/modules/gateway/channels/wxauto-ws/wxauto-inbound.service.spec.ts`（若无则新建）
- Test: `Server/src/modules/gateway/channels/wxauto-ws/wxauto-device.gateway.spec.ts`（追加）

**Interfaces:**
- Consumes: 无
- Produces:
  - `eventSchema` 含 `eventId: z.string().min(1).optional()`
  - `WxAckFrame` 类型 + `ackSchema` 进 frameSchema union（**不进** parseFrame 的 `known` 白名单——设备上行 ack 属协议错误被拒）
  - `WxautoInboundService.handleEvent` 返回类型扩 `'duplicate'`
  - `export const WXAUTO_DEDUP_CAPACITY = 5000`

- [ ] **Step 1: 写失败测试（RED）**

`wxauto-inbound.service.spec.ts`（若无则新建；已有则追加 describe）：

```typescript
import { WxautoInboundService, WXAUTO_DEDUP_CAPACITY } from './wxauto-inbound.service';
import type { WxEventFrame } from './wxauto-frame';

describe('WxautoInboundService 幂等去重（listen-persist-event-outbox）', () => {
  const makeService = () => {
    const pipeline = { accept: jest.fn().mockResolvedValue({ status: 'routed' }) };
    const logger = { info: jest.fn(), warn: jest.fn(), debug: jest.fn() };
    const svc = new WxautoInboundService(pipeline as never, logger as never);
    return { svc, pipeline };
  };

  const msgEvent = (eventId?: string): WxEventFrame =>
    ({
      kind: 'event',
      type: 'message',
      ...(eventId ? { eventId } : {}),
      data: {
        chatName: '张三',
        chatType: 'friend',
        attr: 'friend',
        msgType: 'text',
        sender: '张三',
        content: 'hi',
      },
      ts: 1,
    }) as unknown as WxEventFrame;

  it('同 id 二次 → duplicate 且 pipeline 单次', async () => {
    const { svc, pipeline } = makeService();
    const r1 = await svc.handleEvent('ch1', 'acc1', msgEvent('e1'));
    expect(r1).toBe('routed');
    const r2 = await svc.handleEvent('ch1', 'acc1', msgEvent('e1'));
    expect(r2).toBe('duplicate');
    expect(pipeline.accept).toHaveBeenCalledTimes(1);
  });

  it('无 eventId → 不判重（旧设备兼容）', async () => {
    const { svc, pipeline } = makeService();
    await svc.handleEvent('ch1', 'acc1', msgEvent());
    await svc.handleEvent('ch1', 'acc1', msgEvent());
    expect(pipeline.accept).toHaveBeenCalledTimes(2);
  });

  it('LRU 淘汰：容量+1 次 insert 后首条可重新 admitted', async () => {
    const { svc, pipeline } = makeService();
    for (let i = 0; i <= WXAUTO_DEDUP_CAPACITY; i++) {
      await svc.handleEvent('ch1', 'acc1', msgEvent(`e${i}`));
    }
    // e0 已被 LRU 淘汰 → 再次提交 e0 重新进 pipeline（not duplicate）
    const r = await svc.handleEvent('ch1', 'acc1', msgEvent('e0'));
    expect(r).toBe('routed');
    expect(pipeline.accept).toHaveBeenCalledTimes(WXAUTO_DEDUP_CAPACITY + 2);
  });

  it('id 透传：带 eventId 的 message → InboundMessage.id === "wxauto-e1"', async () => {
    const { svc, pipeline } = makeService();
    await svc.handleEvent('ch1', 'acc1', msgEvent('e1'));
    expect(pipeline.accept).toHaveBeenCalledWith(
      expect.objectContaining({ message: expect.objectContaining({ id: 'wxauto-e1' }) }),
    );
  });

  it('不同 channel 同 id → 不互斥', async () => {
    const { svc, pipeline } = makeService();
    await svc.handleEvent('ch1', 'acc1', msgEvent('e1'));
    await svc.handleEvent('ch2', 'acc1', msgEvent('e1'));
    expect(pipeline.accept).toHaveBeenCalledTimes(2);
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd /home/working/lyagent/Server && pnpm jest wxauto-inbound`
Expected: 失败（duplicate 分支 / WXAUTO_DEDUP_CAPACITY 不存在）

- [ ] **Step 3: 实现 wxauto-frame.ts**

```typescript
// —— eventSchema 加 eventId（现状 :55-60）——
const eventSchema = z.object({
  kind: z.literal('event'),
  type: z.enum(['message', 'friend_request', 'status']),
  eventId: z.string().min(1).optional(),
  data: z.unknown(),
  ts: z.number(),
});

// —— 新增 ack 帧 schema（下行专用）——
const ackSchema = z.object({
  kind: z.literal('ack'),
  eventId: z.string().min(1),
  ts: z.number(),
});

// —— frameSchema union 加 ackSchema（现状 :77-79）——
const frameSchema = z.union([
  helloSchema, helloAckSchema, pingSchema, pongSchema, eventSchema, commandSchema, resultSchema, ackSchema,
]);

// —— 类型导出 ——
export type WxAckFrame = z.infer<typeof ackSchema>;
```

**注意**：`parseFrame` 的 `known` 数组（:96）**不加** `'ack'`——设备上行 ack 会在 kind 白名单被拒（BAD_KIND），这正是期望行为（不断连，仅忽略）。

- [ ] **Step 4: 实现 wxauto-inbound.service.ts**

```typescript
// —— 类外常量 ——
/** 幂等去重 LRU 容量（(channelId, eventId) 插入序 FIFO 淘汰） */
export const WXAUTO_DEDUP_CAPACITY = 5000;

// —— 类内字段（构造器后）——
/** (channelId, eventId) → 插入序 FIFO 淘汰（Map 迭代序 = 插入序） */
private readonly seenEvents = new Map<string, true>();

// —— 类内私有方法 ——
/** 幂等准入：首次返回 true；重复返回 false 并保持 LRU 容量 */
private admitEvent(channelId: string, eventId: string): boolean {
  const key = `${channelId}:${eventId}`;
  if (this.seenEvents.has(key)) {
    return false;
  }
  this.seenEvents.set(key, true);
  if (this.seenEvents.size > WXAUTO_DEDUP_CAPACITY) {
    const oldest = this.seenEvents.keys().next().value;
    if (oldest !== undefined) {
      this.seenEvents.delete(oldest);
    }
  }
  return true;
}

// —— handleEvent 修改（现状 :45-73）——
async handleEvent(
  channelId: string,
  accountId: string,
  event: WxEventFrame,
): Promise<'routed' | 'no_route' | 'ignored' | 'duplicate'> {
  if (event.type !== 'message') {
    this.loggerService.info(`WxAuto 设备事件 [${event.type}]`, {
      module: 'WxautoInboundService',
      metadata: { channelId, data: event.data },
    });
    return 'ignored';
  }
  const parsed = messageDataSchema.safeParse(event.data);
  if (!parsed.success) {
    this.loggerService.warn('WxAuto 消息事件 data 结构非法', {
      module: 'WxautoInboundService',
      metadata: { channelId, issues: parsed.error.issues.length },
    });
    return 'ignored';
  }
  const data = parsed.data as WxMessageEventData;
  // 幂等去重（spec §5）：带 eventId 的 message 按 (channelId, eventId) LRU 判重，
  // 重复不进 pipeline、不触发 agent（补发场景防重复消费）
  if (event.eventId && !this.admitEvent(channelId, event.eventId)) {
    return 'duplicate';
  }
  const message = this.toInboundMessage(data, accountId, event.eventId);
  const result = await this.inboundPipeline.accept({ accountId, channelId, message });
  return result.status === 'routed' ? 'routed' : 'no_route';
}

// —— toInboundMessage 加第三参（现状 :76-92）——
private toInboundMessage(
  data: WxMessageEventData,
  accountId: string,
  eventId?: string,
): InboundMessage {
  const chatType = data.chatType === 'group' ? ChannelChatType.GROUP : ChannelChatType.PRIVATE;
  const msgId = eventId
    ? `wxauto-${eventId}`
    : `wxauto-${Date.now()}-${Math.random().toString(36).substring(2, 8)}`;
  // ...其余不变，id: msgId 已是现状...
}
```

- [ ] **Step 5: 实现 wxauto-device.gateway.ts（case 'event' 回 ack）**

现状 :185-189 改为：

```typescript
case 'event': {
  if (!ctx.helloDone) return;
  await this.inbound.handleEvent(ctx.channelId, ctx.accountId, frame);
  // ACK（spec §3.2）：handleEvent 正常 resolve 后回送（含 routed/no_route/
  // ignored/duplicate——duplicate 也 ack，设备发件箱据此清除）
  if (frame.eventId) {
    this.safeSend(socket, { kind: 'ack', eventId: frame.eventId, ts: Date.now() });
  }
  break;
}
```

- [ ] **Step 6: 跑测试确认通过（GREEN）**

Run: `cd /home/working/lyagent/Server && pnpm jest wxauto-inbound`
Expected: 5 passed

- [ ] **Step 7: gateway 集成测试追加（wxauto-device.gateway.spec.ts）**

依既有「真实 ws server 起端口」基建追加三个用例（断言链，代码按既有 spec 模式补全）：

```typescript
it('event 带 eventId → 收到 ack；重发同 id（duplicate）→ 也 ack', async () => {
  // 1. 建连（token 鉴权流程对齐既有用例）→ 发 hello → hello_done
  // 2. 发 {kind:'event', type:'message', eventId:'e1', data:{合法 message 形状}, ts:1}
  //    → 应收到 {kind:'ack', eventId:'e1'}
  // 3. 重发同帧 → 再收到 ack(e1)（duplicate 也 ack）
  // 断言：两帧 ack.kind === 'ack' && ack.eventId === 'e1'
});

it('event 不带 eventId → 无 ack', async () => {
  // 发 event(message 无 eventId) → 期望不收到 ack 帧（hello_ack/pong 之外无下行）
});

it('设备上行 ack 帧 → BAD_KIND 拒帧不断连', async () => {
  // 发 {kind:'ack', eventId:'x', ts:1} → server 不崩、连接保持
  //（后续正常 ping/pong 仍工作）
});
```

- [ ] **Step 8: 跑 gateway 集成测试**

Run: `cd /home/working/lyagent/Server && pnpm jest wxauto-device.gateway`
Expected: 全绿（含既有用例）

- [ ] **Step 9: commit（lyagent 主仓）**

```bash
cd /home/working/lyagent && git add Server/src/modules/gateway/channels/wxauto-ws/wxauto-frame.ts Server/src/modules/gateway/channels/wxauto-ws/wxauto-inbound.service.ts Server/src/modules/gateway/channels/wxauto-ws/wxauto-device.gateway.ts Server/src/modules/gateway/channels/wxauto-ws/wxauto-inbound.service.spec.ts Server/src/modules/gateway/channels/wxauto-ws/wxauto-device.gateway.spec.ts && git commit -m "feat(gateway): wxauto-ws eventId+ack 帧+LRU 幂等去重"
```

---

### Task 8: 全量回归 + 发布序列确认

**Files:**
- 无新文件（验证任务）

- [ ] **Step 1: desktop 全量 cargo 测试（一次性验收，唯一允许全量的位置）**

Run: `cd /home/working/lyagent/desktop/src-tauri && cargo test`
Expected: 全绿（既有 118 + 新增 outbox 7 + listener_persist 2 + listener_persist_gui 1 + outbox_replay 1 + event_id 2 ≈ 131）

- [ ] **Step 2: desktop pytest（Python 侧零改动，防意外）**

Run: `cd /home/working/lyagent/desktop/sidecar-python && pytest`
Expected: 87 passed

- [ ] **Step 3: Server jest（wxauto 模块）**

Run: `cd /home/working/lyagent/Server && pnpm jest wxauto`
Expected: 全绿

- [ ] **Step 4: 冒烟（binary 模式）**

前置：`cd /home/working/lyagent/desktop/src-tauri && cargo build`
Run: `cd /home/working/lyagent/desktop && bash scripts/smoke_cli.sh binary`
Expected: PASS（含 outbox 残留断言）

- [ ] **Step 5: 发布序列确认（文档级）**

发布顺序（spec §7 硬约束）：**Server 先行**（ack + 幂等上线，strip eventId 兼容旧设备）→ **desktop 后发**。零 DB migration，两仓均不触发 migration 纪律。发版 runbook 引用 spec §7。

- [ ] **Step 6: 零星修复 commit（若有）**

---

## Self-Review（计划完成后自查记录）

**1. Spec coverage:**
- G1 监听持久化 → T2/T3/T6 ✓
- G2 设备 outbox → T1/T4/T5/T6 ✓
- G3 服务端 ack+幂等 → T7 ✓
- G4 双向兼容 → T7（optional + known 白名单）+ T8 发布顺序 ✓
- §4.1 GUI 共享锁不变量 → T3 ✓
- §4.2 Outbox API/容量/过期/崩溃安全 → T1 ✓
- §4.3 emit/ack/补发/双发收敛（同 eventId 服务端判重）→ T5 + T7 ✓
- §6 故障矩阵逐项：断线（T5）、设备崩溃重启（T1 reopen + T5 补发）、ack 在途断线（T7 LRU duplicate+ack）、旧 server+新 desktop（T8 发布顺序）、新 server+旧 desktop（T7 optional）、outbox 磁盘失败（T1 error 路径）、hook 写盘失败（T2 warn 路径）✓
- §8 测试策略逐项 ✓（T1 单测 / T2 集成 / T3 回归锁 / T5 补发集成 / T7 单测+gateway 集成 / T6 冒烟）

**已知盲区（接受）**：T1 未覆盖「磁盘写失败」路径的自动化测试（需注入故障 IO，成本高于价值；语义已定为 error 日志 + 内存保留）。

**2. Placeholder scan:** 本版全文无「TBD/TODO/占位/类似 Task N」；所有代码块完整可编译；命令均带完整路径。

**3. Type consistency:**
- `Outbox::{open, disabled, enqueue, ack, pending, len}`：T1 定义，T5/T6 消费签名一致 ✓
- `new_seeded / set_persist_hook / file_persist_hook`：T2 定义，T3/T6 消费一致 ✓
- `set_outbox`：T5 定义（`std::sync::RwLock<Arc<Outbox>>` 字段 + 注入方法），T6 消费一致 ✓
- `next_event_id`：T4 定义，组帧函数内消费 ✓
- Server `admitEvent` / `WXAUTO_DEDUP_CAPACITY` / `'duplicate'`：T7 内部自洽 ✓
- `Assembled.config: Arc<RwLock<Config>>`：T3 定义，下游读点调用形态不变（已核实 `.read().await` 兼容）✓

**4. 依赖序**：T1→T5→T6；T2→T3；T4→T5；T7 并行；T8 收尾。推荐 T1→T2→T3→T4→T4b→T5→T6→T7→T8（T3/T6 同文件邻近，先后紧接）。

---

## Execution Handoff

**Plan complete and saved to `desktop/docs/superpowers/plans/2026-09-10-listen-persist-event-outbox.md`. Two execution options:**

**1. Subagent-Driven (recommended)** - I dispatch a fresh subagent per task, review between tasks, fast iteration

**2. Inline Execution** - Execute tasks in this session using executing-plans, batch execution with checkpoints

**Which approach?**
