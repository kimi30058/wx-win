//! 微信操作能力域：全操作唯一入口（串行队列 + 拟人延时 + action 映射）
//!
//! `WxSession::execute` 是所有微信操作的唯一路径：
//! 先入串行队列 → action 映射 sidecar 方法 → 调用 → 0.5~1s 拟人延时 → 放行下一条。
//! 串行化是铁律：wxautox4 是 UIA 自动化，并发操作会互相踩窗口焦点。

pub mod listener;

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Mutex;

use rand::Rng;

use crate::sidecar::protocol::RpcError;
use crate::sidecar::spec::methods;
use crate::sidecar::SidecarHandle;

/// 操作间拟人延时下限
const MIN_GAP: Duration = Duration::from_millis(500);
/// 操作间拟人延时上限
const MAX_GAP: Duration = Duration::from_millis(1000);

/// 微信会话：包住 sidecar 句柄，串行执行所有微信操作
pub struct WxSession {
    sidecar: Arc<Mutex<SidecarHandle>>,
    /// 串行队列：所有微信操作逐个执行（拿锁 → 调用 → 拟人间隙 → 释放）
    queue: Arc<Mutex<()>>,
}

impl WxSession {
    /// 构造会话。sidecar 用 `Arc<Mutex<>>` 包裹以支持多处共享（Task 1 的 call 是 `&mut self`）
    pub fn new(sidecar: Arc<Mutex<SidecarHandle>>) -> Self {
        Self {
            sidecar,
            queue: Arc::new(Mutex::new(())),
        }
    }

    /// 执行一条 WS action（16 个之一）；内部映射 sidecar 方法并串行化
    pub async fn execute(&self, action: &str, params: Value) -> Result<Value, RpcError> {
        // 先映射再排队：未知 action 直接拒绝，不占用队列
        let method = map_action(action)
            .ok_or_else(|| RpcError::Sidecar(format!("未知 action: {action}")))?;
        // 串行：拿队列锁 → 执行 → 拟人间隙 → 释放（guard 在函数退出时 drop）
        let _guard = self.queue.lock().await;
        let sidecar_params = transform_params(action, params);
        let result = self
            .sidecar
            .lock()
            .await
            .call(method, sidecar_params)
            .await?;
        let gap = rand_gap();
        tokio::time::sleep(gap).await;
        Ok(result)
    }

    /// 健康探测：wx.is_online 一次调用（不走拟人间隙，供 Supervisor 轮询）。
    /// 读 result payload 的 `online` 布尔——微信退出但 sidecar 存活时 RPC 仍是
    /// Ok({"online": false})，只看 Ok 会恒真（审查 I1）。
    pub async fn health_probe(&self) -> bool {
        self.sidecar
            .lock()
            .await
            .call(methods::IS_ONLINE, json!({}))
            .await
            .map(|v| v["online"].as_bool().unwrap_or(false))
            .unwrap_or(false)
    }

    /// 订阅 sidecar 通知流（message.received 等；透传给 agent_link 通知泵）
    pub async fn subscribe_notifications(&self) -> tokio::sync::broadcast::Receiver<Value> {
        self.sidecar.lock().await.subscribe_notifications()
    }

    /// 热替换 sidecar 句柄（Supervisor 重启路径，Task 6 关键集成点）：
    /// 锁内换新句柄，此后所有 execute/health_probe/subscribe 自动指向
    /// 新 sidecar，「所有操作唯一入口」语义不变。
    ///
    /// 旧句柄处置（审查 I2 更正：kill_on_drop 已随 Child 移交 reaper 移除，
    /// **drop 旧句柄不会杀进程**——SidecarHandle 无 Drop impl，只 drop
    /// 共享态句柄与 pid）：先显式 `shutdown()`（经 kill 通道请旧 reaper
    /// 代杀 + wait 回收）再 drop。活进程的回收唯一路径 = shutdown()，
    /// 若跳过则旧进程残留至自然退出（stdin 管道仍在会阻塞其迭代）。
    /// 旧句柄的通知订阅者收 Lagged/Closed 退出，通知泵等上层在重启后重订阅。
    pub async fn replace_sidecar(&self, new: SidecarHandle) {
        let mut old = {
            let mut guard = self.sidecar.lock().await;
            std::mem::replace(&mut *guard, new)
        };
        // 锁外善后旧句柄（kill 通道 + drop；避免占着串行入口）
        old.shutdown().await;
        drop(old);
    }

    /// 当前 sidecar 的退出观察通道（Supervisor 主循环等崩溃用）。
    /// 快照订阅不占锁：拿锁只取 watcher，watch 通道本身无锁轮询。
    pub async fn sidecar_exit_watcher(
        &self,
    ) -> tokio::sync::watch::Receiver<Option<std::process::ExitStatus>> {
        self.sidecar.lock().await.exit_watcher()
    }

    /// 直连调用（编排层动作，如 wx.init）：绕过 16-action 白名单但仍走
    /// sidecar 的 RPC 协议与超时。WS 下行指令一律走 execute，不从此进。
    pub async fn direct_call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.sidecar.lock().await.call(method, params).await
    }

    /// 直连调用 + 自定义超时（Supervisor 的 wx.init 用短超时快速判死）
    pub async fn direct_call_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RpcError> {
        self.sidecar
            .lock()
            .await
            .call_with_timeout(method, params, timeout)
            .await
    }
}

/// WS action → sidecar 方法（spec §2.3 ↔ §3.2 映射，16 对）
pub fn map_action(action: &str) -> Option<&'static str> {
    Some(match action {
        "send_message" => methods::MSG_SEND,
        "send_file" => methods::FILE_SEND,
        "quote_reply" => methods::MSG_QUOTE,
        "forward_message" => methods::MSG_FORWARD,
        "get_my_info" => methods::GET_MY_INFO,
        "get_friend_requests" => methods::FRIENDS_NEW_REQUESTS,
        "accept_friend" => methods::FRIEND_ACCEPT,
        "add_listen_chat" => methods::LISTEN_ADD,
        "remove_listen_chat" => methods::LISTEN_REMOVE,
        "list_listen_chats" => methods::LISTEN_LIST,
        "search_chat" => methods::CHAT_SEARCH,
        "get_chat_history" => methods::CHAT_HISTORY,
        "get_moments" => methods::MOMENTS_GET,
        "publish_moment" => methods::MOMENTS_PUBLISH,
        "download_media" => methods::MEDIA_DOWNLOAD,
        "voice_to_text" => methods::VOICE_TO_TEXT,
        _ => return None,
    })
}

/// 参数名适配：WS 帧字段名（who/text/at...）→ sidecar 方法参数名
fn transform_params(action: &str, p: Value) -> Value {
    match action {
        "quote_reply" => json!({
            "who": p["who"], "quoteContent": p["quoteContent"],
            "text": p["text"], "at": p.get("at"),
        }),
        "forward_message" => json!({
            "target": p["target"], "sourceWho": p["sourceWho"], "match": p["match"],
        }),
        "get_chat_history" => {
            json!({ "who": p["who"], "n": p.get("n").cloned().unwrap_or(json!(20)) })
        }
        "get_moments" => json!({ "count": p.get("count").cloned().unwrap_or(json!(10)) }),
        "download_media" | "voice_to_text" => json!({ "msgId": p["msgId"] }),
        // 大多数 action 参数名与 sidecar 一致（who/text/at/filepath/name/remark/tags/nickname...）
        _ => p,
    }
}

/// 拟人间隙（500~1000ms 均匀随机）：Task 4 引入 rand 后替换时间戳伪随机
fn rand_gap() -> Duration {
    let span = (MAX_GAP - MIN_GAP).as_millis() as u64;
    let jitter = rand::thread_rng().gen_range(0..=span);
    MIN_GAP + Duration::from_millis(jitter)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 16 个 action 必须全部映射到唯一 sidecar 方法（与 Task 1 spec.rs 常量对齐）
    #[test]
    fn test_map_action_all_16() {
        let expected = [
            ("send_message", "msg.send"),
            ("send_file", "file.send"),
            ("quote_reply", "msg.quote"),
            ("forward_message", "msg.forward"),
            ("get_my_info", "wx.get_my_info"),
            ("get_friend_requests", "friends.new_requests"),
            ("accept_friend", "friend.accept"),
            ("add_listen_chat", "listen.add"),
            ("remove_listen_chat", "listen.remove"),
            ("list_listen_chats", "listen.list"),
            ("search_chat", "chat.search"),
            ("get_chat_history", "chat.history"),
            ("get_moments", "moments.get"),
            ("publish_moment", "moments.publish"),
            ("download_media", "media.download"),
            ("voice_to_text", "voice.to_text"),
        ];
        for (action, method) in expected {
            assert_eq!(map_action(action), Some(method), "action {action} 映射错误");
        }
        assert_eq!(
            map_action("chat_open"),
            None,
            "非 16 action 之一应返回 None"
        );
        assert_eq!(map_action(""), None);
    }

    /// 拟人间隙始终落在 [500, 1000] ms 区间内
    #[test]
    fn test_rand_gap_bounds() {
        for _ in 0..200 {
            let g = rand_gap();
            assert!(g >= MIN_GAP && g <= MAX_GAP, "gap 越界: {g:?}");
        }
    }

    /// 参数适配：get_chat_history 缺省 n=20 / get_moments 缺省 count=10 / 其余原样透传
    #[test]
    fn test_transform_params_defaults() {
        let h = transform_params("get_chat_history", json!({ "who": "张三" }));
        assert_eq!(h["n"], 20);
        let m = transform_params("get_moments", json!({}));
        assert_eq!(m["count"], 10);
        let s = transform_params("send_message", json!({ "who": "a", "text": "t" }));
        assert_eq!(s["text"], "t");
        // quote_reply 显式适配字段
        let q = transform_params(
            "quote_reply",
            json!({ "who": "a", "quoteContent": "qc", "text": "t" }),
        );
        assert!(
            q.get("at").is_some(),
            "quote_reply 应补 at 字段（缺省 null）"
        );
    }
}
