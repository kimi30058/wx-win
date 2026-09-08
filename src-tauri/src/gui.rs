//! GUI 模式入口（Task 9）：tauri Builder 装配 + setup 装配 core 全链路。
//!
//! main.rs 按 `--cli` 参数分流（CLI 装配保持不动）；本模块只管 GUI：
//! - manage(AppStateCtx::shell(...))：setup 内【同步】manage 裸 AppStateCtx
//!   （invoke 时刻必有状态可寻——两段式装配消除 webview 先于装配完成的
//!   时序竞态；类型键与 State<'_, AppStateCtx> 严格一致，见 C1 注释）
//! - setup：shell manage → spawn async 装配（attach 事件桥 → spawn
//!   Supervisor → autoConnect 自动连接）
//! - 退出路径：窗口全关 → RunEvent::Exit → abort Supervisor → 回收 sidecar
//!   （对齐 CLI 的 graceful_shutdown 语义——活进程回收唯一路径）
//!
//! 事件桥 attach 在装配 spawn 的首步（先于 sidecar spawn / Supervisor
//! init 推进）——首批状态事件不丢（前端 init 经 get_app_state 亦可补齐）。

use std::io::IsTerminal;

use std::time::Duration;

use tauri::{Manager, RunEvent};

use wxauto_desktop::cli::graceful_shutdown;
use wxauto_desktop::config::default_config_path;

use crate::app_state::AppStateCtx;
// generate_handler! 需要各命令模块内宏生成的 __cmd__* 就位——通配导入
use crate::commands::*;
use crate::ui_events::UiEventBridge;
use crate::ui_log::{LogRing, UiLogLayer};
// registry().with(...) 组合层与 .init() 分别需 SubscriberExt /
// SubscriberInitExt trait 在作用域
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// 退出时等 sidecar 回收的上限（对齐 CLI 的 SHUTDOWN_TIMEOUT）
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// 运行日志环形缓冲上限（spec §5：1000 条）
const LOG_RING_CAPACITY: usize = 1000;

/// GUI 模式启动（阻塞跑事件循环；返回 = 窗口已关）
pub fn run_gui() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 日志初始化：registry 组合三层——
    // 1. fmt 层 → stderr（GUI 无控制台时输出被丢弃/重定向，dev 排障用）
    // 2. UI 层 → 内存环形缓冲（「运行日志」tab 数据源，经 mpsc 转发桥 emit）
    // 3. EnvFilter（默认 info；排障设 RUST_LOG）
    // ANSI 只在 stderr 是 tty 时开——GUI 子系统 stderr 常为管道/无效句柄,
    // 输出被重定向到文件或无 ANSI 解析的查看器时颜色码全变乱码
    // （2026-09-03 真机日志反馈）。tracing-subscriber 自身只认 NO_COLOR
    // 不检测 tty,这里显式判定。
    let ring = LogRing::new(LOG_RING_CAPACITY);
    let (log_tx, log_rx) = tauri::async_runtime::channel::<crate::ui_log::AppLogEntry>(256);
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(std::io::stderr().is_terminal())
                .with_writer(std::io::stderr),
        )
        .with(UiLogLayer::new(ring.clone(), log_tx.clone()))
        .with(env_filter)
        .init();

    // tauri 自管 runtime（内部 tokio）；async 装配经 setup 里的 spawn 进入
    let app = tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            get_config,
            save_config,
            get_listen_names,
            add_listen,
            remove_listen,
            manual_execute,
            connect,
            disconnect,
            get_app_state,
            get_recent_logs,
            clear_logs,
            activate_license,
            retry_init,
        ])
        .setup(move |app| {
            // ① 同步 manage 壳（invoke 从此可寻；装配在 OnceCell 内进行）。
            //    manage 裸 AppStateCtx——与 commands 的 State<'_, AppStateCtx>
            //    类型键严格一致（C1：StateManager 按 TypeId 匹配，多包一层
            //    Arc 即全量 invoke「state not managed」）。
            let bridge = UiEventBridge::new();
            let ctx = AppStateCtx::shell(
                default_config_path(),
                bridge.clone(),
                ring.clone(),
                // I-1：sidecar stderr sink 共用同一 mpsc（日志 tab 实时双来源）
                Some(log_tx),
            );
            // 装配任务持克隆（AppStateCtx: Clone——内部全 Arc 槽位，浅克隆
            // 共享同一 OnceCell/bridge；manage 侧仍是裸值，类型键不变）
            let ctx_for_setup = ctx.clone();
            app.manage(ctx);
            // ② async 装配（sidecar spawn 等）——commands 的 ready().await 等它
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = setup_async(&handle, &ctx_for_setup, &bridge, log_rx).await {
                    // 装配失败不退出窗口：前端 invoke 会拿到明确错误
                    //（manual_execute 等 reject「sidecar 启动失败…」），
                    // 设置页可排查；日志留痕。
                    tracing::error!(%e, "GUI 装配失败（sidecar 起不来等；窗口保留供排障）");
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

    // 事件循环 + 退出回收（窗口全关后进 RunEvent::Exit）
    app.run(|app, event| {
        if let RunEvent::Exit = event {
            let assembled = app
                .try_state::<AppStateCtx>()
                .and_then(|ctx| ctx.try_assembled());
            if let Some(a) = assembled {
                // 先停 Supervisor（I3：防退出窗口内按退避表再拉起孤儿 sidecar
                // ——对齐 CLI 退出路径的 supervisor_task.abort()）。
                // 中毒锁（panic 传染）在此退出路径视为无句柄可停——
                // graceful_shutdown 的 5s 超时兜底防挂死。
                if let Ok(mut g) = a.supervisor_task.lock() {
                    if let Some(handle) = g.take() {
                        handle.abort();
                    }
                }
                tauri::async_runtime::block_on(async move {
                    if !graceful_shutdown(&a.sidecar, SHUTDOWN_TIMEOUT).await {
                        tracing::error!("GUI 退出：sidecar 未能在超时内回收，进程可能残留");
                    }
                });
            }
            tracing::info!("wxauto-desktop GUI 已退出");
        }
    });
    Ok(())
}

/// async 装配：attach 事件桥 → 日志转发任务 → assemble（sidecar→session→
/// state→supervisor）→ spawn Supervisor（句柄留存供退出 abort）→
/// autoConnect 自动连接。
async fn setup_async(
    handle: &tauri::AppHandle,
    ctx: &AppStateCtx,
    bridge: &UiEventBridge,
    mut log_rx: tauri::async_runtime::Receiver<crate::ui_log::AppLogEntry>,
) -> Result<(), String> {
    // 事件桥先 attach（后续状态推进的事件全部可达前端）
    bridge.attach(handle.clone()).await;
    // 日志转发任务：mpsc → 桥 emit（emit 失败仅 tracing，不断主链路）。
    // bridge 已 attach 后再启动——转发任务的每条都能到达前端
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
    // 装配（幂等：commands 的 ready() 已触发过则直取结果）
    let assembled = ctx.assemble_into().await?;
    // Supervisor 主循环（sidecar 域：init 序列 + 崩溃退避重启）。
    // JoinHandle 留存 Assembled——GUI 退出先 abort（I3：杀 sidecar 的
    // await_exit 信号与 Supervisor 是同一进程退出，不 abort 会在退避
    // 表节奏上再拉新 sidecar 沦为孤儿）。
    let supervisor = assembled.supervisor.clone();
    let task = tokio::spawn(async move {
        supervisor.run().await;
    });
    if let Ok(mut g) = assembled.supervisor_task.lock() {
        *g = Some(task);
    }
    // autoConnect：配置为真则自动连接（WS 域）
    if assembled.config.read().await.auto_connect {
        tracing::info!("autoConnect=true，自动连接服务端");
        if let Err(e) = assembled.start_link().await {
            tracing::warn!(%e, "自动连接失败（用户可手动重连）");
        }
    }
    tracing::info!("GUI 装配完成");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// C1 防回归：真实 mock-runtime 走一条完整 invoke 链路
    /// （generate_handler 注册 → manage(AppStateCtx) → State<'_, AppStateCtx>
    /// 类型键匹配 → 命令体执行 → resolve）。修复前 manage(Arc<AppStateCtx>)
    /// 与 State<AppStateCtx> 的 TypeId 不匹配，本测试的 invoke 会 reject
    /// 「state not managed」——这正是漏进主线的盲区（冒烟只测 WS 链路）。
    ///
    /// get_app_state 选作探针：无副作用、不依赖 sidecar（装配未完成时回
    /// SidecarBooting），专测 manage/State 类型键与命令分发本身。
    #[tokio::test]
    async fn test_invoke_get_app_state_managed_type_key_matches() {
        let app = tauri::test::mock_builder()
            .invoke_handler(tauri::generate_handler![get_app_state])
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .expect("mock app 构建失败");

        // 与生产 setup 相同的 manage 形态：裸 AppStateCtx（非 Arc 包裹）
        let ctx = AppStateCtx::shell(
            default_config_path(),
            UiEventBridge::new(),
            crate::ui_log::LogRing::new(10),
            None,
        );
        app.manage(ctx);

        let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .expect("mock webview 构建失败");

        let resp = tauri::test::get_ipc_response(
            &webview,
            tauri::webview::InvokeRequest {
                cmd: "get_app_state".into(),
                callback: tauri::ipc::CallbackFn(0),
                error: tauri::ipc::CallbackFn(1),
                url: "tauri://localhost".parse().expect("url 解析失败"),
                body: tauri::ipc::InvokeBody::default(),
                headers: Default::default(),
                invoke_key: tauri::test::INVOKE_KEY.to_string(),
            },
        )
        .map(|b| b.deserialize::<String>().expect("响应应为 String"));
        // 未装配也必须 resolve（回 SidecarBooting）而非 reject
        // 「state not managed」——reject 即 C1 回归
        match resp {
            Ok(state) => {
                assert!(
                    [
                        "SidecarDead",
                        "SidecarBooting",
                        "WxInit",
                        "Ready",
                        "Busy",
                        "Degraded"
                    ]
                    .contains(&state.as_str()),
                    "get_app_state 应返回六态名之一，实际: {state}"
                );
            }
            Err(e) => panic!("invoke reject 即 C1 回归（manage/State 类型键不匹配）: {e}"),
        }
    }

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
        // 与生产 setup 相同的 manage 形态（shell 增 log_ring/log_tx 参数后）
        let ctx = AppStateCtx::shell(default_config_path(), UiEventBridge::new(), ring, None);
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
        .map(|b| {
            b.deserialize::<Vec<serde_json::Value>>()
                .expect("响应应为数组")
        });
        let logs = resp.expect("invoke 不应 reject");
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0]["message"], "先");
        assert_eq!(logs[1]["level"], "warn");
    }
}
