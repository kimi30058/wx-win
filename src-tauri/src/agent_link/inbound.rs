//! sidecar 通知 → 上行事件帧封装（spec §2.3 event 帧）
//!
//! sidecar 的 message.received 通知参数是 snake_case（msg_id/chat_who/chat_type/
//! attr/msg_type/sender/content），WS 帧字段统一 camelCase，在这里完成转换。
//! 图片/语音的二次处理（media.download / voice.to_text）由 AgentLink 编排
//! （需要 sidecar 会话句柄），本模块只负责纯组帧。
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

/// 进程内事件序号（evt-{unix_ms}-{seq}：同毫秒内 seq 递增保证字典序单调）
static EVENT_SEQ: AtomicU64 = AtomicU64::new(0);

/// 生成下一个事件 id（进程唯一；进程重启后 ms 段不同天然不撞——spec §3.1）
pub fn next_event_id() -> String {
    let ms = now_ms();
    let seq = EVENT_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("evt-{ms}-{seq}")
}

/// message.received 通知参数 → event:message 帧
/// - `downloaded_media`：图片二次 RPC（download_media）成功时的本地路径
/// - `voice_text`：语音二次 RPC（voice_to_text）成功时的转写文本（覆写原 content）
/// - 二次处理失败时两者传 None → 空字段照常上报（spec §5.1 降级）
pub fn message_event_from_notification(
    n: &Value,
    downloaded_media: Option<&str>,
    voice_text: Option<&str>,
) -> Value {
    let mut content = n["content"].as_str().unwrap_or_default().to_string();
    if let Some(t) = voice_text {
        content = t.to_string(); // 转写成功覆写（参考项目 3071-3077）
    }
    json!({
        "kind": "event",
        "type": "message",
        "data": {
            "chatName": n["chat_who"].as_str().unwrap_or_default(),
            "chatType": if n["chat_type"] == "group" { "group" } else { "friend" },
            "attr": n["attr"].as_str().unwrap_or("friend"),
            "msgType": n["msg_type"].as_str().unwrap_or("text"),
            "sender": n["sender"].as_str().unwrap_or_default(),
            "content": content,
            "isAt": false,
            "downloadedMedia": downloaded_media.unwrap_or(""),
        },
        "eventId": next_event_id(),
        "ts": now_ms(),
    })
}

/// 好友申请新增项 → event:friend_request 帧
pub fn friend_request_event(name: &str, msg: &str) -> Value {
    json!({
        "kind": "event",
        "type": "friend_request",
        "data": { "name": name, "msg": msg },
        "eventId": next_event_id(),
        "ts": now_ms(),
    })
}

/// 状态变化 → event:status 帧（wxOnline / 监听数 / sidecar 存活）
pub fn status_event(wx_online: bool, listeners: usize, sidecar_alive: bool) -> Value {
    json!({
        "kind": "event",
        "type": "status",
        "data": { "wxOnline": wx_online, "listeners": listeners, "sidecarAlive": sidecar_alive },
        "eventId": next_event_id(),
        "ts": now_ms(),
    })
}

/// 当前 Unix 毫秒时间戳（SystemTime 早于 epoch 的病态场景兜底 0）
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
