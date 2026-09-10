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
            // 入箱时刻取本地时钟（不采信 frame.ts：上游时钟偏移会导致误过期/永不过期）
            ts: now_ms(),
            frame: frame.clone(),
        };
        let mut list = lock_entries(entries);
        list.push(entry.clone());
        // 超容丢最旧（仅内存；磁盘多余行由后续 open/ack 重写自愈）
        if list.len() > MAX_ENTRIES {
            let drop_n = list.len() - MAX_ENTRIES;
            list.drain(0..drop_n);
            tracing::warn!(dropped = drop_n, "outbox 超容修剪（丢最旧）");
        }
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
