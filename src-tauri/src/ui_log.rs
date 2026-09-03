//! 运行日志捕获（单窗口化改造）：内存环形缓冲 + tracing Layer + sidecar
//! stderr 读循环。GUI 的「运行日志」tab 数据源；CLI 模式完全不装配本模块
//! 的捕获链（stderr 仍 inherit，行为零回归）。
//!
//! 本模块在 bin 目标下，`pub` 项不会被外部链接引用；`as_str` 与
//! `Debug/Trace` 变体的消费方在 Task 3/4 才接入（项级 `#[allow(dead_code)]`
//! 精确放行）。
//!
//! 设计（spec 2026-09-03）：
//! - LogRing：Mutex<VecDeque>，上限淘汰最旧；snapshot 旧在前
//! - UiLogLayer：tracing 事件 → ring + mpsc → 桥 emit
//! - sidecar stderr（Task 3）：piped 逐行读 → 同一 ring（source=sidecar）

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// 日志级别（前端契约：小写字符串）
#[allow(dead_code)]
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
    #[allow(dead_code)]
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
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AppLogSource {
    Rust,
    Sidecar,
}

impl AppLogSource {
    #[allow(dead_code)]
    pub fn as_str(&self) -> &'static str {
        match self {
            AppLogSource::Rust => "rust",
            AppLogSource::Sidecar => "sidecar",
        }
    }
}

/// 一条运行日志（`wxauto://app-log` 事件载荷，字段即前端契约）
#[allow(dead_code)]
#[derive(Debug, Clone, serde::Serialize)]
pub struct AppLogEntry {
    pub ts: u64,
    pub level: AppLogLevel,
    pub source: AppLogSource,
    pub message: String,
}

/// 环形缓冲：内部 Mutex<VecDeque>，满弹最旧。Clone 共享同一槽位
/// （tracing Layer / sidecar 读循环 / invoke 快照各持一份克隆）。
#[allow(dead_code)]
#[derive(Clone)]
pub struct LogRing {
    inner: Arc<Mutex<VecDeque<AppLogEntry>>>,
    cap: usize,
}

impl LogRing {
    #[allow(dead_code)]
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::new())),
            cap,
        }
    }

    /// 追加一条；超上限弹最旧。锁中毒（panic 传染）按清空恢复——
    /// 日志缓冲是旁路观察者，不允许它把业务线程拖死。
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    pub fn snapshot(&self) -> Vec<AppLogEntry> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    /// 清空（前端「清空」按钮）
    #[allow(dead_code)]
    pub fn clear(&self) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

/// 毫秒时间戳（LogRing 条目与测试共用；集中一处便于将来换时钟注入）
#[allow(dead_code)]
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// tracing 事件 → AppLogEntry 的字段收集器（只收 `message` 字段）
#[allow(dead_code)]
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
#[allow(dead_code)]
pub struct UiLogLayer {
    ring: LogRing,
    sink: tokio::sync::mpsc::Sender<AppLogEntry>,
}

impl UiLogLayer {
    #[allow(dead_code)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

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

    /// Layer 经真实 tracing 订阅器收一条 info → ring 有对应 entry
    #[test]
    fn test_ui_log_layer_captures_tracing_event() {
        let ring = LogRing::new(10);
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let subscriber = tracing_subscriber::registry().with(UiLogLayer::new(ring.clone(), tx));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("hello 日志");
        });
        let snap = ring.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].level, AppLogLevel::Info);
        assert_eq!(snap[0].source, AppLogSource::Rust);
        assert!(
            snap[0].message.contains("hello 日志"),
            "实际: {}",
            snap[0].message
        );
        // mpsc 侧也收到（转发任务的数据源）
        let got = rx.blocking_recv().expect("应收到一条");
        assert_eq!(got.message, snap[0].message);
    }

    /// 级别映射：error/warn 各归其位
    #[test]
    fn test_ui_log_layer_maps_levels() {
        let ring = LogRing::new(10);
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let subscriber = tracing_subscriber::registry().with(UiLogLayer::new(ring.clone(), tx));
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
        // tokio mpsc 禁 buffer=0；用容量 1 且 drop 接收端：try_send 即失败（Closed）
        let (tx, rx) = tokio::sync::mpsc::channel::<AppLogEntry>(1);
        drop(rx);
        let subscriber = tracing_subscriber::registry().with(UiLogLayer::new(ring.clone(), tx));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("仍应进 ring");
        });
        assert_eq!(ring.snapshot().len(), 1, "mpsc 满不影响 ring 落条");
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
