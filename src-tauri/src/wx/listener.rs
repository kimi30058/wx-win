//! 监听名单注册表 + sidecar 重启后重放
//!
//! 本地维护一份监听名单（BTreeSet 去重有序），add/remove 同步穿透到 sidecar；
//! sidecar 重启后监听注册全部丢失，`resync()` 按名单逐个重放 add_listen_chat。

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::json;
use tokio::sync::RwLock;

use super::WxSession;

pub struct ListenerRegistry {
    /// 本地监听名单（去重 + 有序，list/resync 顺序稳定）
    names: RwLock<BTreeSet<String>>,
    session: Arc<WxSession>,
}

impl ListenerRegistry {
    pub fn new(session: Arc<WxSession>) -> Self {
        Self {
            names: RwLock::new(BTreeSet::new()),
            session,
        }
    }

    /// 添加监听：先穿透 sidecar，成功才入本地名单（失败时名单不受污染）
    pub async fn add(&self, nickname: String) -> Result<(), String> {
        self.session
            .execute("add_listen_chat", json!({ "nickname": nickname }))
            .await
            .map_err(|e| e.to_string())?;
        self.names.write().await.insert(nickname);
        Ok(())
    }

    /// 移除监听：先穿透 sidecar，成功才出本地名单
    pub async fn remove(&self, nickname: &str) -> Result<(), String> {
        self.session
            .execute("remove_listen_chat", json!({ "nickname": nickname }))
            .await
            .map_err(|e| e.to_string())?;
        self.names.write().await.remove(nickname);
        Ok(())
    }

    /// sidecar 重启后重放全部监听（忽略单个失败，汇总失败名单）
    pub async fn resync(&self) -> Vec<String> {
        let names: Vec<String> = self.names.read().await.iter().cloned().collect();
        let mut failed = Vec::new();
        for n in names {
            if self
                .session
                .execute("add_listen_chat", json!({ "nickname": n }))
                .await
                .is_err()
            {
                failed.push(n);
            }
        }
        failed
    }

    /// 当前监听名单（有序快照）
    pub async fn list(&self) -> Vec<String> {
        self.names.read().await.iter().cloned().collect()
    }
}
