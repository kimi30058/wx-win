//! 前端 invoke 命令（Task 9）：与 agent 指令共用 session 串行队列。
//!
//! tauri command 语义：Ok → resolve、Err → reject（无 {success} 包装帧——
//! Task 8 审查硬性输入 #4，前端 manualExecute 已按判别联合实现）。
//! 参数名与前端 invoke 字面量直配（config/action/params/nickname）。
//!
//! 每命令先 `ctx.ready().await`——两段式装配下 webview 先于装配完成的
//! invoke 在此等待（而非报「state not managed」）；装配失败拿到原因。

use std::time::Duration;

use serde_json::{json, Value};
use tauri::State;
use wxauto_desktop::sidecar::spec::methods;

use crate::app_state::{config_to_json, extract_settings_patch, AppStateCtx};

/// 激活超时：authenticate 可能走网络校验，比 init 的 10s 宽松
const ACTIVATE_TIMEOUT: Duration = Duration::from_secs(30);

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

/// 最近一次 init 失败原因快照（I1 兜底：init_fail 事件先于前端 listen
/// 注册发出即永久丢失——tauri 事件无重放且每 sidecar 世代只发一次，
/// 前端 init 拉本快照落 initFailReason）。None = 无失败或已成功清除。
/// 装配未完成时回 None（Supervisor 尚未跑 init，无失败可言）。
/// Err 分支仅为满足 tauri「async command 带引用参数须返回 Result」约束，
/// 实际不产生（装配失败回 Ok(None)——授权失败快照与装配失败是两域）。
#[tauri::command]
pub async fn get_init_fail_reason(ctx: State<'_, AppStateCtx>) -> Result<Option<String>, String> {
    match ctx.ready().await {
        Ok(a) => Ok(a.supervisor.last_init_fail().await),
        Err(_) => Ok(None),
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

/// 激活 wxautox4（直连 sidecar wx.activate；绕过 16-action 白名单——
/// 激活是编排层动作，与 wx.init 同类）。成功即内联重试 init 一次
/// （失败即止：微信未开场景每次白耗 UIA 扫描，由用户择机 retry_init）。
#[tauri::command]
pub async fn activate_license(ctx: State<'_, AppStateCtx>, code: String) -> Result<Value, String> {
    let code = code.trim().to_string();
    if code.is_empty() {
        return Err("激活码不能为空".to_string());
    }
    let assembled = ctx.ready().await?;
    let result = assembled
        .session
        .direct_call_with_timeout(methods::ACTIVATE, json!({ "code": code }), ACTIVATE_TIMEOUT)
        .await
        .map_err(|e| format!("激活请求失败：{e}"))?;
    if result["ok"].as_bool().unwrap_or(false) {
        assembled.supervisor.retry_init().await;
    }
    Ok(result)
}

/// 手动重跑 init 序列（激活页「重新初始化」按钮——sidecar 活着时
/// Supervisor 不会自动重跑 init，必须有显式入口）
#[tauri::command]
pub async fn retry_init(ctx: State<'_, AppStateCtx>) -> Result<(), String> {
    let assembled = ctx.ready().await?;
    assembled.supervisor.retry_init().await;
    Ok(())
}
