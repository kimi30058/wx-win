//! 前端 invoke 命令（Task 9）：与 agent 指令共用 session 串行队列。
//!
//! tauri command 语义：Ok → resolve、Err → reject（无 {success} 包装帧——
//! Task 8 审查硬性输入 #4，前端 manualExecute 已按判别联合实现）。
//! 参数名与前端 invoke 字面量直配（config/action/params/nickname）。
//!
//! 每命令先 `ctx.ready().await`——两段式装配下 webview 先于装配完成的
//! invoke 在此等待（而非报「state not managed」）；装配失败拿到原因。

use serde_json::Value;
use tauri::State;

use crate::app_state::{config_to_json, extract_settings_patch, AppStateCtx};

/// 读配置（camelCase 序列化；token 不回传——keyring 只写不读）
#[tauri::command]
pub async fn get_config(ctx: State<'_, AppStateCtx>) -> Result<Value, String> {
    let assembled = ctx.ready().await?;
    let cfg = assembled.config.read().await.clone();
    config_to_json(&cfg)
}

/// 保存配置：按字段提取合并（serverUrl/channelId/autoConnect；
/// token 可选走 keyring——spawn_blocking 包裹）。勿整体反序列化
/// Config（会清空 listenNames 等非提交字段，Task 8 硬性输入 #3）。
#[tauri::command]
pub async fn save_config(ctx: State<'_, AppStateCtx>, config: Value) -> Result<(), String> {
    let patch = extract_settings_patch(&config);
    let token = config["token"].as_str().map(str::to_string);
    ctx.save_settings(patch, token).await
}

/// 当前监听名单（有序快照）
#[tauri::command]
pub async fn get_listen_names(ctx: State<'_, AppStateCtx>) -> Result<Vec<String>, String> {
    let assembled = ctx.ready().await?;
    Ok(assembled.listeners.list().await)
}

/// 添加监听（穿透 sidecar 成功才入本地名单）
#[tauri::command]
pub async fn add_listen(ctx: State<'_, AppStateCtx>, nickname: String) -> Result<(), String> {
    let assembled = ctx.ready().await?;
    assembled.listeners.add(nickname).await
}

/// 移除监听
#[tauri::command]
pub async fn remove_listen(ctx: State<'_, AppStateCtx>, nickname: String) -> Result<(), String> {
    let assembled = ctx.ready().await?;
    assembled.listeners.remove(&nickname).await
}

/// 手动执行 sidecar action（16 个之一；与 agent 指令共用串行队列）。
/// Ok → 业务载荷 resolve；Err → 错误字符串 reject。
#[tauri::command]
pub async fn manual_execute(
    ctx: State<'_, AppStateCtx>,
    action: String,
    params: Value,
) -> Result<Value, String> {
    let assembled = ctx.ready().await?;
    assembled
        .session
        .execute(&action, params)
        .await
        .map_err(|e| e.to_string())
}

/// 连接服务端（组装 URL + 新建 AgentLink + run；旧连接先 halt）
#[tauri::command]
pub async fn connect(ctx: State<'_, AppStateCtx>) -> Result<(), String> {
    let assembled = ctx.ready().await?;
    assembled.start_link().await
}

/// 断开服务端（halt：transport 循环 + 三泵协作退出）
#[tauri::command]
pub async fn disconnect(ctx: State<'_, AppStateCtx>) -> Result<(), String> {
    let assembled = ctx.ready().await?;
    assembled.stop_link().await
}

/// 六态快照名（同步低开销查询；前端补齐首帧用）。
/// 装配未完成时回 Booting（真实状态装配后经事件桥推达）。
#[tauri::command]
pub fn get_app_state(ctx: State<'_, AppStateCtx>) -> String {
    match ctx.try_assembled() {
        Some(a) => a.state_snapshot(),
        None => "SidecarBooting".to_string(),
    }
}

/// 运行日志快照（旧在前；前端 init 时补齐 attach 前的历史——
/// tauri 事件无重放，对齐 get_app_state 补首值模式）
#[tauri::command]
pub fn get_recent_logs(ctx: State<'_, AppStateCtx>) -> Vec<serde_json::Value> {
    ctx.log_ring
        .snapshot()
        .iter()
        .map(|e| serde_json::to_value(e).unwrap_or_default())
        .collect()
}

/// 清空运行日志（前端「清空」按钮）
#[tauri::command]
pub fn clear_logs(ctx: State<'_, AppStateCtx>) {
    ctx.log_ring.clear();
}
