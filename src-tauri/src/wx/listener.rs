//! 监听名单注册表 + sidecar 重启后重放 + 名单持久化
//!
//! 本地维护一份监听名单（BTreeSet 去重有序），add/remove 同步穿透到 sidecar；
//! sidecar 重启后监听注册全部丢失，`resync()` 按名单逐个重放 add_listen_chat。
//!
//! 持久化（spec §4.1）：`new_seeded` 以 config.listen_names 为种子构造（重启
//! 恢复）；add/remove 穿透成功后经 persist hook 把最新名单落盘 config.json。
//! hook 失败只记 warn 不回滚内存名单（内存为准，下次写盘自愈）。

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use tokio::sync::RwLock;

use super::WxSession;
use crate::config::{save_config, Config};

pub struct ListenerRegistry {
    /// 本地监听名单（去重 + 有序，list/resync 顺序稳定）
    names: RwLock<BTreeSet<String>>,
    session: Arc<WxSession>,
    /// 持久化 hook（add/remove 成功后以最新名单快照调用；失败只记日志不回滚）
    persist_hook: RwLock<Option<Box<dyn Fn(Vec<String>) + Send + Sync>>>,
}

impl ListenerRegistry {
    /// 空名单构造（保持旧签名，内部转发 new_seeded）
    pub fn new(session: Arc<WxSession>) -> Self {
        Self::new_seeded(session, Vec::new())
    }

    /// 以存量配置为种子构造（启动恢复：config.listen_names → 内存名单）
    pub fn new_seeded(session: Arc<WxSession>, initial: Vec<String>) -> Self {
        Self {
            names: RwLock::new(initial.into_iter().collect()),
            session,
            persist_hook: RwLock::new(None),
        }
    }

    /// 注入持久化 hook（GUI/CLI 装配层；测试注入计数 hook）。
    /// async：persist_hook 是 tokio RwLock（notify_persist 为 async 读侧）
    pub async fn set_persist_hook(&self, hook: Box<dyn Fn(Vec<String>) + Send + Sync>) {
        *self.persist_hook.write().await = Some(hook);
    }

    /// 添加监听：先穿透 sidecar，成功才入本地名单（失败时名单不受污染）
    pub async fn add(&self, nickname: String) -> Result<(), String> {
        self.session
            .execute("add_listen_chat", json!({ "nickname": nickname }))
            .await
            .map_err(|e| e.to_string())?;
        self.names.write().await.insert(nickname);
        self.notify_persist().await;
        Ok(())
    }

    /// 移除监听：先穿透 sidecar，成功才出本地名单
    pub async fn remove(&self, nickname: &str) -> Result<(), String> {
        self.session
            .execute("remove_listen_chat", json!({ "nickname": nickname }))
            .await
            .map_err(|e| e.to_string())?;
        self.names.write().await.remove(nickname);
        self.notify_persist().await;
        Ok(())
    }

    /// 名单变化通知 hook（快照传入；hook 内部自行处理失败）
    async fn notify_persist(&self) {
        let snapshot: Vec<String> = self.names.read().await.iter().cloned().collect();
        if let Some(hook) = self.persist_hook.read().await.as_ref() {
            hook(snapshot);
        }
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

/// 落盘 hook 工厂：更新共享 Config 锁的 listen_names + save_config（GUI/CLI 装配用）。
/// hook 为同步闭包，内部 spawn 异步落盘（不阻塞 add/remove 主流程）；
/// 落盘失败只记 warn（名单以内存为准，下次写盘自愈——spec §4.1）。
pub fn file_persist_hook(
    cfg_lock: Arc<tokio::sync::RwLock<Config>>,
    path: PathBuf,
) -> Box<dyn Fn(Vec<String>) + Send + Sync> {
    Box::new(move |names: Vec<String>| {
        let cfg_lock = cfg_lock.clone();
        let path = path.clone();
        tokio::spawn(async move {
            {
                let mut cfg = cfg_lock.write().await;
                cfg.listen_names = names;
            }
            // 释写锁后再取读快照落盘（save_config 只需 &Config）
            let cfg = cfg_lock.read().await;
            if let Err(e) = save_config(&path, &cfg) {
                tracing::warn!(%e, "监听名单落盘失败（内存为准，下次写盘自愈）");
            }
        });
    })
}
