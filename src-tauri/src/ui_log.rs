//! 运行日志捕获（单窗口化改造）：内存环形缓冲 + tracing Layer + sidecar
//! stderr 读循环。GUI 的「运行日志」tab 数据源；CLI 模式完全不装配本模块
//! 的捕获链（stderr 仍 inherit，行为零回归）。
//!
//! 本模块在 bin 目标下，`pub` 项不会被外部链接引用；`as_str`、
//! `Debug/Trace` 变体的消费方在 Task 2/3/4 才接入，暂放行 dead_code。
//!
//! 设计（spec 2026-09-03）：
//! - LogRing：Mutex<VecDeque>，上限淘汰最旧；snapshot 旧在前
//! - UiLogLayer（Task 2）：tracing 事件 → ring + mpsc → 桥 emit
//! - sidecar stderr（Task 3）：piped 逐行读 → 同一 ring（source=sidecar）

// Task 1 只落纯数据结构，`as_str` 与 Debug/Trace 变体的消费方在后续任务
// 接入；本模块挂在 bin 目标下，`pub` 不构成对外 API，dead_code 先放行。
#![allow(dead_code)]

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
        let mut q = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        assert_eq!(j["ts"], 1693700000000u64);
        assert_eq!(j["level"], "error");
        assert_eq!(j["source"], "sidecar");
        assert_eq!(j["message"], "boom");
    }
}
