//! bootstrap 自检报告（spec §6）：每次启动把 sidecar 探测链各级结果
//! 覆盖写 `~/.wxauto-desktop/logs/bootstrap.log`——装机远程排障
//! 「报一个文件」即可。
//!
//! 挂 lib 说明（与 Task 6 的 file_log 挂 bin 相反）：唯一消费方是
//! lib 侧 `sidecar::spawn_default_sunk`（探测链在其函数体内，lib 不能
//! 反向依赖 bin 模块——同 StderrSink trait 挂 lib 的注入理由）；
//! 本模块零 GUI 依赖（std + dirs，config.rs 同款），挂 lib 不破坏
//! 「core 不引 tauri」铁律。日期换算与 file_log.rs 同算法族
//! （Hinnant civil-from-days），内联一份避免跨 crate 目标的私有依赖。

use std::io::Write;
use std::path::{Path, PathBuf};

/// bootstrap 报告路径：~/.wxauto-desktop/logs/bootstrap.log
/// （home 拿不到 → None，探测链留痕整体跳过——诊断通道永不阻断启动）
pub fn log_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".wxauto-desktop").join("logs").join("bootstrap.log"))
}

/// 启动报告头（覆盖写 = 一次启动一份；父目录惰性创建；失败静默——
/// 报告是旁路诊断，任何 IO 失败不阻断 sidecar 启动）
pub fn start_report(path: &Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        path,
        format!("[{}] [probe] ===== 启动 =====\n", stamp(now_ms())),
    );
}

/// 追加一行探测结果（打开失败静默——同上不阻断）
pub fn probe_line(path: &Path, msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
    {
        let _ = writeln!(f, "[{}] [probe] {}", stamp(now_ms()), msg);
    }
}

/// ms → "YYYY-MM-DD HH:MM:SS"（UTC；Hinnant civil-from-days，与
/// file_log.rs 同算法族——u64 输入恒非负，era 除法无需负数修正分支）
fn stamp(ts_ms: u64) -> String {
    let day = ts_ms / 1000 / 86400;
    let s = (ts_ms / 1000) % 86400;
    let z = day + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        s / 3600,
        (s % 3600) / 60,
        s % 60
    )
}

/// 毫秒时间戳（与 ui_log::now_ms 同语义；bootstrap 挂 lib，bin 的
/// ui_log 不可见，内联一份）
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 报告函数直测（spec §6）：start + probe 行写入；二次启动覆盖——
    /// 旧启动的 probe 行不残留（覆盖语义 = 一次启动一份）
    #[test]
    fn test_bootstrap_report_overwrites_per_startup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bootstrap.log");
        start_report(&path);
        probe_line(&path, "env WXAUTO_SIDECAR_CMD: 未设置");
        probe_line(&path, "bundled: 未命中");
        let content = std::fs::read_to_string(&path).expect("应写入");
        assert!(content.contains("[probe]"), "探测行前缀");
        assert!(content.contains("env WXAUTO_SIDECAR_CMD: 未设置"));
        assert!(content.contains("bundled: 未命中"));
        // 二次启动覆盖：旧 probe 行不残留，新文件只剩 start 行
        start_report(&path);
        let content2 = std::fs::read_to_string(&path).expect("应写入");
        assert!(content2.contains("[probe]"), "start 行仍带 [probe] 前缀");
        assert!(
            !content2.contains("bundled: 未命中"),
            "覆盖语义：旧启动内容不残留"
        );
        assert_eq!(content2.lines().count(), 1, "覆盖后只剩 start 行");
    }

    /// stamp：ms → "YYYY-MM-DD HH:MM:SS"（UTC；纪元日 + 跨天 + 时分秒）
    #[test]
    fn test_stamp_epoch_and_day_boundary() {
        assert_eq!(stamp(0), "1970-01-01 00:00:00");
        assert_eq!(stamp(86_400_000), "1970-01-02 00:00:00");
        assert_eq!(stamp(86_400_000 + 3_723_000), "1970-01-02 01:02:03");
    }
}
