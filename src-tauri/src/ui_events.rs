//! Rust→前端事件桥（Task 9）：core 事件帧 → tauri emit（GUI 模式专属）。
//!
//! core（sidecar/wx/transport/agent_link/state）不依赖 tauri（Task 5 起的
//! 解耦铁律）；本模块是 GUI 装配层，把 core 的两路出口接到 AppHandle::emit：
//! 1. `AgentLink::set_event_sink` / `set_command_sink`（上行 event 帧、指令日志）
//! 2. `AppStateMachine::with_on_change`（六态变化）+ Supervisor event_sink
//!
//! 事件契约（与 Task 8 前端 store 订阅严格对齐，字段错配=前端守卫静默丢弃）：
//! - `wxauto://state`       payload = 六态变体名字符串（如 "Ready"）
//! - `wxauto://status`      payload = { wsConnected, wxOnline }（组装帧，见下）
//! - `wxauto://message`     payload = event:message 帧的 data 部分
//! - `wxauto://command-log` payload = { requestId, action, success, error?, durationMs, ts }
//!
//! **status 组装语义（Task 8 审查硬性输入 #1）**：core 的 event:status 上行帧
//! （{kind,type,data,ts}，data 含 wxOnline/listeners/sidecarAlive）**没有**
//! wsConnected 字段——直接转发会被前端守卫丢弃（两灯永灰）。本桥拆开重装：
//! wxOnline 取帧内值，wsConnected 取 AgentLink::ws_connected() 现场快照。
//!
//! emit 失败仅 tracing（不断主链路——事件桥是旁路观察者，坏不掉业务）。
//! AppHandle 在 setup 之后才存在，故桥持 `RwLock<Option<AppHandle>>`，
//! 注入前的事件（sidecar 启动早期的状态推进）丢弃并记日志（前端 init 时
//! 会 invoke get_app_state 补齐当前值，不依赖历史事件回放）。

use serde_json::Value;
use tauri::AppHandle;
use tokio::sync::RwLock;
use wxauto_desktop::state::AppState;

/// 前端事件名（与 desktop/src/stores/app.ts 的 listen() 字面量一致）
pub const EVENT_STATE: &str = "wxauto://state";
pub const EVENT_STATUS: &str = "wxauto://status";
pub const EVENT_MESSAGE: &str = "wxauto://message";
pub const EVENT_COMMAND_LOG: &str = "wxauto://command-log";
/// 运行日志事件名（与 desktop/src/stores/app.ts 的 listen() 字面量一致）
pub const EVENT_APP_LOG: &str = "wxauto://app-log";

/// 事件发射后端：抹平 AppHandle 的 Runtime 泛型（生产 Wry / 测试 MockRuntime），
/// 桥内只持 trait object——attach 侧泛型收敛。
trait EventEmit: Send + Sync {
    fn emit_json(&self, event: &str, payload: &Value) -> Result<(), tauri::Error>;
}

impl<R: tauri::Runtime> EventEmit for AppHandle<R> {
    fn emit_json(&self, event: &str, payload: &Value) -> Result<(), tauri::Error> {
        use tauri::Emitter;
        self.emit(event, payload)
    }
}

/// 事件桥：持可选发射后端（setup 后注入；此前事件丢弃记日志）。
/// Clone 语义 = 共享同一份内部槽（各注入点拿到的是同一桥）。
#[derive(Clone, Default)]
pub struct UiEventBridge {
    handle: std::sync::Arc<RwLock<Option<Box<dyn EventEmit>>>>,
}

impl UiEventBridge {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注入 AppHandle（tauri setup 回调里调用一次；Runtime 泛型——
    /// 生产 Wry 与测试 MockRuntime 均可注入）
    pub async fn attach<R: tauri::Runtime + 'static>(&self, app: AppHandle<R>) {
        *self.handle.write().await = Some(Box::new(app));
    }

    /// emit 或记日志（handle 未注入 / emit 失败都只 tracing）
    async fn emit(&self, event: &str, payload: &Value) {
        let guard = self.handle.read().await;
        match guard.as_ref() {
            Some(app) => {
                if let Err(e) = app.emit_json(event, payload) {
                    tracing::warn!(event, %e, "前端事件 emit 失败（不影响主链路）");
                }
            }
            None => tracing::debug!(
                event,
                "AppHandle 未注入，事件丢弃（前端将经 get_app_state 补齐）"
            ),
        }
    }

    /// 六态变化 → `wxauto://state`（payload 为变体名字符串）。
    /// AppState 无 serde——显式 match 映射（勿依赖 Debug 格式化侥幸，
    /// Task 8 审查硬性输入 #2：前端守卫按精确字符串匹配六态名）。
    pub async fn emit_state(&self, state: &AppState) {
        let name = app_state_name(state);
        self.emit(EVENT_STATE, &Value::String(name.to_string()))
            .await;
    }

    /// core event 帧 → 前端事件（按 type 分发；status 重组载荷）。
    /// 这是 AgentLink event_sink 与 Supervisor event_sink 的统一入口。
    pub async fn forward_event_frame(&self, frame: &Value) {
        match frame["type"].as_str().unwrap_or_default() {
            "message" => {
                // event:message 帧 data 含前端全部所需字段（chatName/content/ts...）
                let data = frame["data"].clone();
                self.emit(EVENT_MESSAGE, &data).await;
            }
            "status" => {
                // 重组：wsConnected 来自 AgentLink 现场快照（帧内无此字段）
                let payload = serde_json::json!({
                    "wsConnected": frame["wsConnected"].as_bool().unwrap_or(false),
                    "wxOnline": frame["data"]["wxOnline"].as_bool().unwrap_or(false),
                });
                self.emit(EVENT_STATUS, &payload).await;
            }
            // friend_request 等其余事件：前端无订阅（Listen 页主动 invoke 拉取），
            // 不转发（避免无消费者的事件洪泛）
            other => {
                tracing::debug!(r#type = other, "无前端订阅的事件类型，跳过");
            }
        }
    }

    /// 指令日志条（command_sink 直通——on_command 组的字段即前端契约）
    pub async fn forward_command_log(&self, entry: &Value) {
        self.emit(EVENT_COMMAND_LOG, entry).await;
    }

    /// 运行日志条（转发任务直通；entry 序列化字段即前端契约）
    pub async fn forward_app_log(&self, entry: &Value) {
        self.emit(EVENT_APP_LOG, entry).await;
    }
}

/// AppState → 变体名（与前端 AppStateName 六值精确一致）
pub fn app_state_name(s: &AppState) -> &'static str {
    match s {
        AppState::SidecarDead => "SidecarDead",
        AppState::SidecarBooting => "SidecarBooting",
        AppState::WxInit => "WxInit",
        AppState::Ready => "Ready",
        AppState::Busy => "Busy",
        AppState::Degraded => "Degraded",
    }
}

/// 组装带 wsConnected 的 status 帧（供桥侧转发前统一注入该字段）：
/// AgentLink event_sink 收到的 event:status 帧先经此函数补上 wsConnected
/// 再走 forward_event_frame（单一出口，字段口径一处维护）。
pub fn with_ws_connected(frame: &Value, ws_connected: bool) -> Value {
    let mut f = frame.clone();
    f["wsConnected"] = Value::Bool(ws_connected);
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 六态名映射：与前端 APP_STATE_LABELS 键精确一致（错一个字符=前端守卫丢弃）
    #[test]
    fn test_app_state_name_all_six() {
        assert_eq!(app_state_name(&AppState::SidecarDead), "SidecarDead");
        assert_eq!(app_state_name(&AppState::SidecarBooting), "SidecarBooting");
        assert_eq!(app_state_name(&AppState::WxInit), "WxInit");
        assert_eq!(app_state_name(&AppState::Ready), "Ready");
        assert_eq!(app_state_name(&AppState::Busy), "Busy");
        assert_eq!(app_state_name(&AppState::Degraded), "Degraded");
    }

    /// status 帧注入 wsConnected（组装而非转发，Task 8 硬性输入 #1）
    #[test]
    fn test_with_ws_connected_injects_flag() {
        let frame = serde_json::json!({
            "kind": "event", "type": "status",
            "data": { "wxOnline": true, "listeners": 2, "sidecarAlive": true },
            "ts": 123
        });
        let out = with_ws_connected(&frame, true);
        assert_eq!(out["wsConnected"], true);
        assert_eq!(out["data"]["wxOnline"], true, "原字段保留");
    }

    /// app-log 转发：entry JSON 经桥发射（payload 结构即前端契约）
    #[tokio::test]
    async fn test_forward_app_log_emits_entry() {
        let bridge = UiEventBridge::new();
        // 未 attach 时 emit 走丢弃分支不 panic——本测试主要覆盖编译期
        // 契约与方法存在性；真实发射在 gui_bridge 集成测试覆盖。
        bridge
            .forward_app_log(&serde_json::json!({
                "ts": 1, "level": "info", "source": "rust", "message": "m"
            }))
            .await;
    }
}
