//! 配置持久化（spec §3.4 设置视图 ↔ 本模块）
//!
//! 配置项存 JSON 文件（`~/.wxauto-desktop/config.json`，dirs 定位 home）；
//! token 单独走系统凭据管理器（keyring crate：Windows Credential Manager /
//! macOS Keychain / Linux Secret Service）。
//! **core 不引 tauri**（Task 5 起的解耦铁律）——tauri-plugin-store 留给
//! Task 9 GUI 侧可选替换，本模块即轻量 JSON 方案。
//!
//! 降级语义：keyring 在无 Secret Service 的 Linux 环境（无 dbus / CI）调用
//! 报错 → 函数返回 Err（描述含原因），**绝不 panic**；调用方（GUI）据此
//! 提示「token 仅本次会话有效」或引导安装 gnome-keyring。
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// keyring 服务名（同机多环境共享一个服务，按 account 区分）
const KEYRING_SERVICE: &str = "wxauto-desktop";
/// keyring 账户名（单用户单 token；多账号是服务端侧概念，设备侧只有一条）
const KEYRING_ACCOUNT: &str = "device-token";

/// 设备配置（spec §3.4）：连接与行为参数。
/// 字段名 camelCase 序列化（与前端 store / 服务端约定一致）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    /// 服务端 WS 地址（含 channelId 路径或查询参数，整个 URL 原样存）
    pub server_url: String,
    /// 渠道 ID（服务端分配；与 server_url 配套）
    pub channel_id: String,
    /// 启动即自动连接
    pub auto_connect: bool,
    /// 监听名单（昵称列表；sidecar 重启后由 ListenerRegistry 重放）
    pub listen_names: Vec<String>,
    /// 拟人延时下限（毫秒）
    pub delay_min_ms: u64,
    /// 拟人延时上限（毫秒）
    pub delay_max_ms: u64,
    /// webhook 告警地址（空=禁用；2026-09-10 P1）
    #[serde(default)]
    pub webhook_url: String,
    /// webhook 自定义模板（空=通用 JSON {title,detail,ts,device}）
    #[serde(default)]
    pub webhook_template: String,
}

impl Default for Config {
    fn default() -> Self {
        // 默认值与 wx/mod.rs 的 MIN_GAP/MAX_GAP（500~1000ms）对齐
        Self {
            server_url: "ws://127.0.0.1:60021".into(),
            channel_id: String::new(),
            auto_connect: false,
            listen_names: Vec::new(),
            delay_min_ms: 500,
            delay_max_ms: 1000,
            webhook_url: String::new(),
            webhook_template: String::new(),
        }
    }
}

/// 默认配置文件路径：`~/.wxauto-desktop/config.json`。
/// home 取不到（极端环境）回退当前目录下的 .wxauto-desktop（保证有路径可写）。
pub fn default_config_path() -> PathBuf {
    let base = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join(".wxauto-desktop").join("config.json")
}

/// 加载配置：文件不存在 → Default（首启场景）；存在但损坏 → Err（不静默吞坏配置，
/// 调用方提示用户修复或删除）。
pub fn load_config(path: &Path) -> Result<Config, String> {
    match std::fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content).map_err(|e| format!("配置文件解析失败: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(format!("配置文件读取失败: {e}")),
    }
}

/// 保存配置（序列化 + 建父目录 + 原子替换写）。
pub fn save_config(path: &Path, cfg: &Config) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("配置目录创建失败: {e}"))?;
    }
    let content = serde_json::to_string_pretty(cfg).map_err(|e| format!("配置序列化失败: {e}"))?;
    // 先写临时文件再原子改名：半途崩溃不留截断的坏配置
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, content).map_err(|e| format!("配置写入失败: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("配置落盘失败: {e}"))?;
    Ok(())
}

/// 从系统凭据管理器取 token。
/// 无可用 keyring 后端（如 Linux 无 Secret Service）→ Err（描述原因），不 panic。
pub fn keyring_get_token() -> Result<String, String> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)
        .and_then(|e| e.get_password())
        .map_err(|e| format!("读取 token 失败（系统凭据服务不可用或未存储）: {e}"))
}

/// 存 token 到系统凭据管理器（同上，失败优雅返回 Err）。
pub fn keyring_set_token(token: &str) -> Result<(), String> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)
        .and_then(|e| e.set_password(token))
        .map_err(|e| format!("写入 token 失败（系统凭据服务不可用）: {e}"))
}

/// 删除 token（登出用；不存在视为成功）
pub fn keyring_delete_token() -> Result<(), String> {
    match keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT).and_then(|e| e.delete_credential())
    {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("删除 token 失败: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 默认值与 wx 域拟人间隙常量对齐（500~1000ms）
    #[test]
    fn test_config_default_gap_alignment() {
        let c = Config::default();
        assert_eq!(c.delay_min_ms, 500);
        assert_eq!(c.delay_max_ms, 1000);
        assert!(!c.auto_connect);
    }

    /// camelCase 序列化（前端 store 字段名对齐）
    #[test]
    fn test_config_serde_camel_case() {
        let c = Config::default();
        let v = serde_json::to_value(&c).expect("序列化失败");
        assert!(v.get("serverUrl").is_some(), "字段应为 serverUrl");
        assert!(v.get("channelId").is_some(), "字段应为 channelId");
        assert!(v.get("autoConnect").is_some(), "字段应为 autoConnect");
        assert!(v.get("listenNames").is_some(), "字段应为 listenNames");
        assert!(v.get("delayMinMs").is_some(), "字段应为 delayMinMs");
        assert!(v.get("delayMaxMs").is_some(), "字段应为 delayMaxMs");
    }

    /// 新增 webhook 两字段：默认空串（=禁用）+ camelCase 序列化
    #[test]
    fn test_webhook_fields_default_and_camel() {
        let c = Config::default();
        assert_eq!(c.webhook_url, "", "默认禁用");
        assert_eq!(c.webhook_template, "");
        let v = serde_json::to_value(Config::default()).unwrap();
        assert_eq!(v["webhookUrl"], "");
        assert!(v.get("webhook_template").is_none(), "序列化必须是 camelCase");
    }
}
