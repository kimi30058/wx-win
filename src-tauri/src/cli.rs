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
}
