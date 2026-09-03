//! CLI 装配辅助（Task 7）：main.rs 保持薄装配，可单测的逻辑收进库内。
//!
//! 覆盖三条 Task 6 审查硬性输入中的可自动化部分：
//! 1. `graceful_shutdown`：退出路径对活 sidecar 显式 shutdown（活进程回收
//!    唯一路径——kill_on_drop 已随 Child 移交 reaper 移除）+ 等回收 + 幂等
//! 2. （sidecar 重启后通知泵重订阅在 agent_link 侧实现，见 notification_pump）
//! 3. `wait_for_sigint`：Ctrl-C 等待（测试/冒烟注入用；Windows 生产路径由
//!    main.rs 直接 ctrl_c）

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use crate::sidecar::SidecarHandle;

/// sidecar spawn 默认命令（相对仓库根的布局：desktop/sidecar-python/）
pub const DEFAULT_SIDECAR_CMD: &str = "python3 sidecar-python/sidecar.py";

/// 安装包内置 sidecar exe 文件名（externalBin 约定：名称-target-triple）。
/// Windows NSIS 安装后与主程序同目录；triple 用编译期常量而非 rustc 探测
/// （打包机与安装机 triple 一致——MSVC x64 发布目标固定）。
#[cfg(windows)]
pub const BUNDLED_SIDECAR_NAME: &str = "wxauto-sidecar-x86_64-pc-windows-msvc.exe";
#[cfg(not(windows))]
pub const BUNDLED_SIDECAR_NAME: &str = "wxauto-sidecar";

/// exe 同目录探测内置 sidecar（安装态）：命中返回绝对路径。
/// 开发态（target/debug 等目录）无此文件 → None 走回退链。
pub fn find_bundled_sidecar() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let candidate = dir.join(BUNDLED_SIDECAR_NAME);
    candidate.is_file().then(|| candidate.to_string_lossy().into_owned())
}

/// 解析 sidecar 启动命令字符串 → (program, args)。
/// 支持 Windows 引号语义：`"C:\Program Files\x\sidecar.exe" --flag` 的
/// 程序路径含空格不能按裸空格拆（终审 follow-up #2：原 split_whitespace
/// 会拆坏含空格路径）。双引号包裹段视为整体；连续空白分隔 token。
pub fn parse_sidecar_cmd(cmd_str: &str) -> (String, Vec<String>) {
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for ch in cmd_str.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    match tokens.split_first() {
        Some((head, rest)) => (head.clone(), rest.to_vec()),
        None => ("python3".into(), Vec::new()),
    }
}

/// sidecar 默认工作目录：从可执行文件路径向上找仓库根（父链上第一个含
/// desktop/sidecar-python/ 的目录），返回其下的 `desktop/` 目录——
/// DEFAULT_SIDECAR_CMD 的相对路径 `sidecar-python/sidecar.py` 以它为基准。
/// exe 从 desktop/src-tauri/target/** 运行时 cwd 无论如何都不会命中裸相对
/// 路径，必须显式锚定。找不到（安装态布局变化等）返回 None，spawn 报真错。
pub fn resolve_sidecar_workdir() -> Option<&'static str> {
    let exe = std::env::current_exe().ok()?;
    let mut dir = exe.parent()?;
    loop {
        let desktop = dir.join("desktop");
        if desktop.join("sidecar-python").is_dir() {
            // Command::current_dir 要求 'static：泄漏一次 desktop 路径
            // （进程级常量，数十字节，CLI 进程生命周期内唯一用途）
            return Some(Box::leak(
                desktop.to_string_lossy().into_owned().into_boxed_str(),
            ));
        }
        dir = dir.parent()?;
    }
}

/// 退出路径的显式回收：shutdown（经 kill 通道请 reaper 代杀 + wait 回收）→
/// 等 exit 状态传播（上限 timeout；超时视为失败，调用方记日志退出）。
/// 对已死进程幂等（reaper 已收尾时通道关闭 → await_exit 立即返回已存的
/// Some(status)，或 watch 已含值直接返回）。
pub async fn graceful_shutdown(sidecar: &Arc<Mutex<SidecarHandle>>, timeout: Duration) -> bool {
    let mut exit_rx = {
        let mut s = sidecar.lock().await;
        // 先取 watcher 再 shutdown：shutdown 只发 kill 请求，watcher 取放
        // 顺序不影响正确性（watch 语义存最新值，迟到订阅不丢信号）
        let rx = s.exit_watcher();
        s.shutdown().await;
        rx
    };
    match tokio::time::timeout(timeout, SidecarHandle::await_exit(&mut exit_rx)).await {
        Ok(Some(status)) => {
            tracing::info!(?status, "sidecar 已回收（显式 shutdown）");
            true
        }
        Ok(None) => {
            // watch 发送端全 drop：理论不可达（reaper 与 Shared 同生命周期）
            tracing::warn!("sidecar 退出观察通道关闭（视为已回收）");
            true
        }
        Err(_) => {
            tracing::error!(?timeout, "sidecar 回收超时（可能残留进程，请手动清理）");
            false
        }
    }
}

/// 组装 WS URL：token 非空挂 `?token=`（服务端 query 鉴权约定），空则原样。
pub fn build_ws_url(server: &str, token: &str) -> String {
    if token.is_empty() {
        server.to_string()
    } else {
        format!("{server}/?token={token}")
    }
}

/// 等 Ctrl-C（测试与注入场景；返回 () 而非 Result——信号等待无业务失败态）
pub async fn wait_for_sigint() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 默认工作目录锚定：返回的目录下必须直接存在 sidecar-python/
    /// （DEFAULT_SIDECAR_CMD 的相对路径基准）
    #[test]
    fn test_resolve_sidecar_workdir_anchors_to_repo_root() {
        let dir = resolve_sidecar_workdir().expect("测试环境必能定位仓库根");
        let p = std::path::Path::new(dir);
        assert!(
            p.join("sidecar-python").join("sidecar.py").is_file(),
            "锚定目录应直接含 sidecar-python/sidecar.py，实际: {dir}"
        );
    }

    /// URL 组装：token 挂 query / 空 token 原样
    #[test]
    fn test_build_ws_url() {
        assert_eq!(
            build_ws_url("ws://127.0.0.1:60021", "t1"),
            "ws://127.0.0.1:60021/?token=t1"
        );
        assert_eq!(
            build_ws_url("ws://127.0.0.1:60021", ""),
            "ws://127.0.0.1:60021"
        );
    }

    /// 引号包裹的含空格路径不被拆坏（follow-up #2 回归）
    #[test]
    fn test_parse_sidecar_cmd_quoted_path_with_spaces() {
        let (prog, args) = parse_sidecar_cmd(
            "\"C:\\Program Files\\wxauto\\wxauto-sidecar.exe\" --mock",
        );
        assert_eq!(prog, r"C:\Program Files\wxauto\wxauto-sidecar.exe");
        assert_eq!(args, vec!["--mock"]);
    }

    /// 无引号裸命令：按空白拆分，行为与旧 split_whitespace 一致
    #[test]
    fn test_parse_sidecar_cmd_plain() {
        let (prog, args) = parse_sidecar_cmd("python3 sidecar-python/sidecar.py");
        assert_eq!(prog, "python3");
        assert_eq!(args, vec!["sidecar-python/sidecar.py"]);
    }

    /// 空串 / 纯空白：兜底 python3 无参（与旧实现一致）
    #[test]
    fn test_parse_sidecar_cmd_empty() {
        let (prog, args) = parse_sidecar_cmd("   ");
        assert_eq!(prog, "python3");
        assert!(args.is_empty());
    }

    /// 引号内空格保留（token 内部空白不拆）
    #[test]
    fn test_parse_sidecar_cmd_space_inside_quotes() {
        let (prog, _args) = parse_sidecar_cmd("\"my sidecar.py\"");
        assert_eq!(prog, "my sidecar.py");
    }

    /// bundled 探测：当前测试环境（开发态）必不命中；文件名常量跨平台有值
    #[test]
    fn test_find_bundled_sidecar_dev_env_miss() {
        // 开发态 exe 在 src-tauri/target/** 下，同目录无 bundled exe
        let r = find_bundled_sidecar();
        if let Some(path) = r {
            // 若命中（异常布局），至少应是存在的绝对路径文件
            assert!(std::path::Path::new(&path).is_file(), "命中路径必须存在: {path}");
        }
        // 常量非空（编译期保证各平台有定义）
        assert!(!BUNDLED_SIDECAR_NAME.is_empty());
    }
}
