//! 入口（Task 7 CLI + Task 9 GUI 分流）：`--cli` 走 CLI 装配，默认 GUI。
//!
//! GUI：tauri Builder（commands + 事件桥 + setup 装配），见 gui.rs。
//! CLI：sidecar → WxSession → Supervisor → AgentLink 全链路（原 Task 7 装配
//! 原样保留——headless 联调 / CI 冒烟用）。
//!
//! 模块挂载：commands / ui_events / app_state / gui 依赖 tauri，只进 bin
//! （lib 保持 core 纯净不引 tauri）；bin 单测（cargo test 默认含 bin target）
//! 覆盖 GUI 装配的纯逻辑。
//!
//! 环境变量（两种模式通用）：
//! - WXAUTO_SERVER_URL：WS 网关地址（默认 ws://127.0.0.1:60021）
//! - WXAUTO_DEVICE_TOKEN：设备令牌（空则 URL 不挂 ?token=）
//! - WXAUTO_SIDECAR_CMD：sidecar 启动命令（默认锚定仓库根的
//!   `python3 sidecar-python/sidecar.py`，见 cli::resolve_sidecar_command）
//! - WXAUTO_MOCK=1：透传给 sidecar（Python 侧全方法假数据，Linux 开发用）
//!
//! 退出路径（Task 6 审查硬性输入 #1）：CLI Ctrl-C / GUI 窗口全关 →
//! graceful_shutdown 显式回收 sidecar（活进程回收唯一路径——kill_on_drop
//! 已移除）→ 退出。
mod app_state;
mod commands;
mod gui;
mod ui_events;
mod ui_log;

use std::io::IsTerminal;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use wxauto_desktop::agent_link::AgentLink;
use wxauto_desktop::cli::{build_ws_url, graceful_shutdown, wait_for_sigint};
use wxauto_desktop::config::{default_config_path, load_config};
use wxauto_desktop::sidecar::SidecarHandle;
use wxauto_desktop::state::{AppStateMachine, Supervisor};
use wxauto_desktop::wx::listener::ListenerRegistry;
use wxauto_desktop::wx::WxSession;

/// Ctrl-C 后等 sidecar 回收的上限
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // --cli 分流：默认 GUI（桌面 App 主形态）；CI/联调传 --cli 走 headless
    if std::env::args().any(|a| a == "--cli") {
        run_cli()
    } else {
        gui::run_gui()
    }
}

/// CLI 装配（Task 7 原样保留；见模块头注释）
#[tokio::main]
async fn run_cli() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        // stderr 非 tty（管道/重定向）时关 ANSI 防乱码,对齐 gui.rs
        .with_ansi(std::io::stderr().is_terminal())
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // 配置来源优先级：env > ~/.wxauto-desktop/config.json > 默认值
    let cfg = load_config(&default_config_path()).unwrap_or_else(|e| {
        tracing::warn!("配置文件读取失败（{e}），使用默认值");
        wxauto_desktop::config::Config::default()
    });
    let server = std::env::var("WXAUTO_SERVER_URL").unwrap_or_else(|_| cfg.server_url.clone());
    let token = std::env::var("WXAUTO_DEVICE_TOKEN").unwrap_or_default();
    let url = build_ws_url(&server, &token);
    tracing::info!(server = %server, token_len = token.len(), "CLI 装配开始");

    // 1. 首代 sidecar（Supervisor 不负责首启——只管退出后的退避重启）
    let sidecar = Arc::new(Mutex::new(SidecarHandle::spawn_default().await?));
    // 2. session + listeners（所有微信操作的唯一入口）
    let session = Arc::new(WxSession::new(sidecar.clone()));
    let listeners = Arc::new(ListenerRegistry::new(session.clone()));
    // 3. 状态机 + Supervisor（sidecar 崩溃退避重启 + init 序列 + 状态推进）
    let state = Arc::new(AppStateMachine::new());
    let supervisor = Arc::new(Supervisor::new(state, session.clone(), listeners.clone()));
    let supervisor_task = tokio::spawn({
        let sup = supervisor.clone();
        async move { sup.run().await }
    });
    // 4. AgentLink（WS 主循环 + 通知/心跳/好友轮询三泵，run 内部自 spawn）
    let link = Arc::new(AgentLink::new(session.clone(), listeners.clone(), url));
    tokio::spawn({
        let l = link.clone();
        async move { l.run().await }
    });

    tracing::info!("wxauto-desktop CLI 已启动，Ctrl-C 退出");
    // Ctrl-C → 停 Supervisor（防其在退出窗口按退避表再拉起 sidecar）→
    // 显式回收 sidecar（活进程回收唯一路径；幂等，对已死进程安全）
    wait_for_sigint().await;
    tracing::info!("收到 Ctrl-C，正在回收 sidecar…");
    supervisor_task.abort();
    if !graceful_shutdown(&sidecar, SHUTDOWN_TIMEOUT).await {
        tracing::error!("sidecar 未能在超时内回收，进程可能残留");
    }
    tracing::info!("wxauto-desktop CLI 已退出");
    Ok(())
}
