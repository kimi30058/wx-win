//! 文件日志通道（spec §5 三通道之二）：ring 写入侧并联文件 sink。
//!
//! 设计：
//! - FileSink：`~/.wxauto-desktop/logs/app-YYYYMMDD.log` 追加写，utf-8-sig
//!   （BOM 便于记事本），按天分文件；日期 = epoch 秒 → UTC 天纯整数换算
//!   （不引 chrono）
//! - FileLogLayer：tracing Layer 形态，与 UiLogLayer 平级——Rust 事件
//!   一次 emit 三 fan-out（fmt stderr / UI ring / 文件）
//! - sidecar stderr 行：RingStderrSink.consume 并联调用 FileSink::write
//!   （经全局 OnceLock 句柄——见 install_sidecar_file_sink）
//! - 失败降级：目录创建/打开失败 → 一次性 stderr 告警后本 sink 永久降级
//!   关闭（任何错误不阻断启动，spec 铁律）。告警不走 tracing——Layer 写
//!   失败若 emit warn 事件会再进本 Layer 持同一把锁，非重入锁直接死锁。
//! - 保留 7 天：启动时 cleanup_old_logs 删文件名天序号超龄的 app-*.log
//!
//! 本模块挂 bin（依赖 ui_log 的 AppLogEntry 等类型，ui_log 在 bin——
//! lib 保持 core 纯净不引 GUI 装配件，与 commands/gui 同层）。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::ui_log::{now_ms, now_secs, AppLogEntry, AppLogLevel, AppLogSource};

/// 日志保留天数（spec §5：7 天）
pub const LOG_KEEP_DAYS: u64 = 7;

/// epoch 秒 → UTC 天序号（civil-from-days 算法，Howard Hinnant 经典式）
fn day_number(epoch_secs: u64) -> i64 {
    (epoch_secs as i64) / 86400
}

/// UTC 天序号 → "YYYYMMDD"
fn day_stamp_from_number(z: i64) -> String {
    // days → civil（Hinnant 算法）
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}")
}

/// epoch 秒 → "YYYYMMDD"（按天分文件的文件名日期）
fn day_stamp(epoch_secs: u64) -> String {
    day_stamp_from_number(day_number(epoch_secs))
}

/// ms → "HH:MM:SS"（当日秒，UTC）
fn time_of_day(ts_ms: u64) -> String {
    let s = (ts_ms / 1000) % 86400;
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// 单行格式（与日志 tab 展示同构）：[UTC 日期 时间] [来源] [级别] 消息
fn format_line(e: &AppLogEntry) -> String {
    format!(
        "[{} {}] [{}] [{}] {}\n",
        day_stamp(e.ts / 1000),
        time_of_day(e.ts),
        e.source.as_str(),
        e.level.as_str(),
        e.message
    )
}

/// 失败告警直写 stderr（禁走 tracing——防 Layer 重入死锁，见模块头）
fn warn_stderr(msg: &str) {
    let _ = writeln!(std::io::stderr(), "[warn] file_log: {msg}");
}

/// 文件 sink：目录惰性创建；跨天自动切文件；写失败一次性告警后永久降级
/// （降级语义=后续 write 均 no-op，日志永不阻断业务）
pub struct FileSink {
    dir: PathBuf,
    state: Mutex<Option<(String, std::fs::File)>>, // (当前天, 句柄)；None=待开/已降级
    dead: Mutex<bool>,
}

impl FileSink {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            state: Mutex::new(None),
            dead: Mutex::new(false),
        }
    }

    /// 追加一条。任何 IO 失败：一次性 stderr 告警 + 永久降级（后续 no-op）。
    pub fn write(&mut self, entry: AppLogEntry) {
        if *self
            .dead
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            return;
        }
        let day = day_stamp(entry.ts / 1000);
        let mut guard = match self.state.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let need_open = match guard.as_ref() {
            Some((d, _)) => *d != day,
            None => true,
        };
        if need_open {
            if let Err(e) = std::fs::create_dir_all(&self.dir) {
                warn_stderr(&format!(
                    "日志目录创建失败({e})，dir={}，文件通道降级关闭",
                    self.dir.display()
                ));
                self.degrade(&mut guard);
                return;
            }
            let path = self.dir.join(format!("app-{day}.log"));
            match OpenOptions::new().create(true).append(true).open(&path) {
                Ok(f) => {
                    // utf-8-sig：新文件首写 BOM（已存在文件长度 0 时也补）
                    let need_bom = f.metadata().map(|m| m.len() == 0).unwrap_or(false);
                    let mut f = f;
                    if need_bom {
                        let _ = f.write_all("\u{feff}".as_bytes());
                    }
                    *guard = Some((day.clone(), f));
                }
                Err(e) => {
                    warn_stderr(&format!(
                        "日志文件打开失败({e})，path={}，文件通道降级关闭",
                        path.display()
                    ));
                    self.degrade(&mut guard);
                    return;
                }
            }
        }
        if let Some((_, f)) = guard.as_mut() {
            let _ = f.write_all(format_line(&entry).as_bytes());
            let _ = f.flush();
        }
    }

    /// 永久降级：关句柄 + 置死标志（一次性告警已在调用侧发出）
    fn degrade(&self, guard: &mut Option<(String, std::fs::File)>) {
        *guard = None;
        *self
            .dead
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
    }
}

/// 全局文件 sink 句柄（RingStderrSink 并联写入口；None = 未装配/降级关闭）
static SIDECAR_FILE_SINK: OnceLock<Mutex<Option<FileSink>>> = OnceLock::new();

/// 装配 sidecar 行文件通道（GUI/CLI 启动时调用；None 即不装配）
pub fn install_sidecar_file_sink(dir: Option<PathBuf>) {
    if let Some(dir) = dir {
        let _ = SIDECAR_FILE_SINK.set(Mutex::new(Some(FileSink::new(dir))));
    }
}

/// sidecar stderr 行 → 落盘（RingStderrSink.consume 并联调用；未装配 no-op）
pub fn write_sidecar_entry(entry: &AppLogEntry) {
    let Some(cell) = SIDECAR_FILE_SINK.get() else {
        return;
    };
    let mut guard = match cell.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if let Some(sink) = guard.as_mut() {
        sink.write(entry.clone());
    }
}

/// 清理超龄日志（启动调用；只匹配 app-YYYYMMDD.log；失败静默——清理是
/// 尽力而为的卫生工作，不因它挡启动）。龄期以文件名内嵌天序号为准
/// （比 mtime 稳定——文件拷贝/覆盖不改变其归属日）。
pub fn cleanup_old_logs(dir: &Path, keep_days: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let cutoff_days = day_number(now_secs()) - keep_days as i64;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("app-") || !name.ends_with(".log") {
            continue;
        }
        // 文件名里的天序号优先（比 mtime 稳定）；解析失败跳过
        let day = name.trim_start_matches("app-").trim_end_matches(".log");
        if day.len() == 8 && day.chars().all(|c| c.is_ascii_digit()) {
            let y: i64 = day[..4].parse().unwrap_or(0);
            let m: i64 = day[4..6].parse().unwrap_or(0);
            let d: i64 = day[6..8].parse().unwrap_or(0);
            if y > 0 && m > 0 {
                // civil → days 逆变换（Hinnant）：先估 era 级
                let yy = if m <= 2 { y - 1 } else { y };
                let mm = if m > 2 { m - 3 } else { m + 9 };
                let dd = d - 1;
                let days =
                    365 * yy + yy / 4 - yy / 100 + yy / 400 + (153 * mm + 2) / 5 + dd - 719468;
                if days < cutoff_days {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}

/// 默认日志目录：~/.wxauto-desktop/logs（与 config.json 同根，spec §5）
pub fn default_log_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".wxauto-desktop").join("logs"))
}

// ── tracing Layer 形态（Rust 事件三 fan-out 之一）──

/// tracing Layer：事件 → FileSink（内部独立 FileSink，不经全局句柄——
/// Rust 事件与 sidecar 行共用目录，各持句柄跨天各自切文件）
pub struct FileLogLayer {
    sink: Mutex<FileSink>,
}

impl<S> tracing_subscriber::Layer<S> for FileLogLayer
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
        let mut guard = match self.sink.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.write(AppLogEntry {
            ts: now_ms(),
            level,
            source: AppLogSource::Rust,
            message: crate::ui_log::extract_message(event),
        });
    }
}

/// 装配文件 Layer + 启动清理（GUI/CLI 共用；目录拿不到/建不出返回 None
/// 不装配——日志系统永不阻断启动）
pub fn file_log_layer() -> Option<FileLogLayer> {
    let dir = default_log_dir()?;
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn_stderr(&format!("日志目录创建失败({e})，文件日志不装配"));
        return None;
    }
    cleanup_old_logs(&dir, LOG_KEEP_DAYS);
    Some(FileLogLayer {
        sink: Mutex::new(FileSink::new(dir)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// epoch 秒 → UTC YYYYMMDD（不引 chrono 的纯整数换算）
    #[test]
    fn test_epoch_day_filename() {
        assert_eq!(day_stamp(0), "19700101");
        assert_eq!(day_stamp(1), "19700101");
        assert_eq!(day_stamp(86399), "19700101");
        assert_eq!(day_stamp(86400), "19700102");
    }

    /// 一条 entry 落盘：行格式含时间戳/来源/级别/消息；文件名按天
    #[test]
    fn test_format_line_writes_to_daily_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut sink = FileSink::new(dir.path().to_path_buf());
        sink.write(crate::ui_log::AppLogEntry {
            ts: 1000,
            level: crate::ui_log::AppLogLevel::Error,
            source: crate::ui_log::AppLogSource::Sidecar,
            message: "boom 测试".into(),
        });
        let name = format!("app-{}.log", day_stamp(1000 / 1000));
        let content = std::fs::read_to_string(dir.path().join(name)).expect("应写入");
        assert!(content.contains('\u{feff}'), "utf-8-sig BOM 首写");
        assert!(content.contains("[sidecar]"), "来源标记");
        assert!(content.contains("[error]"), "级别小写");
        assert!(content.contains("boom 测试"), "消息保留");
    }

    /// 清理：只删 app-*.log 且只删超龄的
    #[test]
    fn test_cleanup_removes_only_stale_app_logs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let old = dir.path().join(format!("app-{}.log", day_stamp(0)));
        let now_name = format!("app-{}.log", day_stamp(now_secs()));
        std::fs::write(&old, "x").unwrap();
        std::fs::write(dir.path().join(&now_name), "x").unwrap();
        std::fs::write(dir.path().join("config.json"), "x").unwrap();
        cleanup_old_logs(dir.path(), 7);
        assert!(!old.exists(), "超龄删除");
        assert!(dir.path().join(&now_name).exists(), "保留期内不删");
        assert!(dir.path().join("config.json").exists(), "非 app-*.log 不动");
    }

    /// 降级：目录不可写（只读父目录下不存在路径）→ write 静默不抛，
    /// 且降级后状态一致（dead 置位——后续条目短路 no-op）
    #[test]
    fn test_write_failure_silent() {
        let mut sink = FileSink::new(PathBuf::from("/nonexistent-root/x/y"));
        sink.write(crate::ui_log::AppLogEntry {
            ts: 1,
            level: crate::ui_log::AppLogLevel::Info,
            source: crate::ui_log::AppLogSource::Rust,
            message: "x".into(),
        }); // 不 panic 即通过
        assert!(
            *sink
                .dead
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            "失败后应置 dead 短路后续"
        );
    }

    /// 跨天切文件：同 sink 两条不同天 → 两个日文件各含一条
    #[test]
    fn test_rotates_across_day_boundary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut sink = FileSink::new(dir.path().to_path_buf());
        sink.write(AppLogEntry {
            ts: 86_400_000, // epoch day 1 = 1970-01-02
            level: AppLogLevel::Info,
            source: AppLogSource::Rust,
            message: "day1".into(),
        });
        sink.write(AppLogEntry {
            ts: 172_800_000, // epoch day 2 = 1970-01-03
            level: AppLogLevel::Info,
            source: AppLogSource::Rust,
            message: "day2".into(),
        });
        let f1 = std::fs::read_to_string(dir.path().join("app-19700102.log"))
            .expect("第 2 天文件应存在");
        let f2 = std::fs::read_to_string(dir.path().join("app-19700103.log"))
            .expect("第 3 天文件应存在");
        assert!(f1.contains("day1"));
        assert!(f2.contains("day2"));
    }

    /// FileLogLayer 经真实 tracing 订阅器收一条 error → 落盘含 rust/error
    #[test]
    fn test_file_log_layer_writes_tracing_event() {
        use tracing_subscriber::layer::SubscriberExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let subscriber = tracing_subscriber::registry().with(FileLogLayer {
            sink: Mutex::new(FileSink::new(dir.path().to_path_buf())),
        });
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!("layer 落盘验证");
        });
        let name = format!("app-{}.log", day_stamp(now_secs()));
        let content = std::fs::read_to_string(dir.path().join(name)).expect("Layer 事件应落盘");
        assert!(content.contains("[rust]"), "来源=Rust");
        assert!(content.contains("[error]"), "级别映射");
        assert!(content.contains("layer 落盘验证"), "message 字段提取");
    }

    /// 行格式四要素完整性（日期 时间 来源 级别 消息）
    #[test]
    fn test_format_line_shape() {
        let line = format_line(&AppLogEntry {
            ts: 86_400_000 + 3_723_000, // 1970-01-02 01:02:03 UTC
            level: AppLogLevel::Warn,
            source: AppLogSource::Sidecar,
            message: "m".into(),
        });
        assert_eq!(line, "[19700102 01:02:03] [sidecar] [warn] m\n");
    }

    /// sidecar 并联通道：未装配（install 未调用）时 write_sidecar_entry no-op
    /// （全局 OnceLock 进程内只能 set 一次，本用例依赖测试二进制默认未装配
    /// ——file_log 其余测试不调 install，顺序无关恒成立）
    #[test]
    fn test_write_sidecar_entry_noop_when_not_installed() {
        // 不装配直接调：不应 panic（未装配 no-op 语义）
        write_sidecar_entry(&AppLogEntry {
            ts: 1,
            level: AppLogLevel::Info,
            source: AppLogSource::Sidecar,
            message: "x".into(),
        });
    }
}
