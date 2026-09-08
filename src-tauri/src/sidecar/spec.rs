//! 21 个 sidecar 方法的强类型定义（spec §3.2）
//! 方法名常量是 call 的第一参；参数用 serde_json::json! 构造后过 XxxParams 序列化。
//! 完整的参数/结果结构随各 Task 落地补充，本文件先交付方法名常量 + 核心消息结构。
use serde::{Deserialize, Serialize};

/// 方法名常量（21 个；与 Python sidecar methods.py 一一对应）
pub mod methods {
    pub const INIT: &str = "wx.init";
    pub const ACTIVATE: &str = "wx.activate";
    pub const GET_MY_INFO: &str = "wx.get_my_info";
    pub const IS_ONLINE: &str = "wx.is_online";
    pub const MSG_SEND: &str = "msg.send";
    pub const FILE_SEND: &str = "file.send";
    pub const MSG_QUOTE: &str = "msg.quote";
    pub const MSG_FORWARD: &str = "msg.forward";
    pub const CHAT_OPEN: &str = "chat.open";
    pub const CHAT_SEARCH: &str = "chat.search";
    pub const CHAT_HISTORY: &str = "chat.history";
    pub const LISTEN_ADD: &str = "listen.add";
    pub const LISTEN_REMOVE: &str = "listen.remove";
    pub const LISTEN_LIST: &str = "listen.list";
    pub const FRIENDS_NEW_REQUESTS: &str = "friends.new_requests";
    pub const FRIEND_ACCEPT: &str = "friend.accept";
    pub const MOMENTS_GET: &str = "moments.get";
    pub const MOMENTS_PUBLISH: &str = "moments.publish";
    pub const MEDIA_DOWNLOAD: &str = "media.download";
    pub const VOICE_TO_TEXT: &str = "voice.to_text";
    pub const UTIL_SLEEP: &str = "util.sleep";
}

/// wx.init / wx.get_my_info 的结果结构（Task 2 的 Python 侧与此对齐）
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct InitResult {
    pub licensed: bool,
    pub wxid: String,
    pub nickname: String,
    /// 失败三态（sidecar Task 2 契约）：licensed=未授权 / wechat_missing=微信未开；
    /// 旧 sidecar 二进制无此字段——default None 向后兼容。
    /// rename 对齐 Python 侧 camelCase（serde 默认蛇形会错位成 fail_reason）
    #[serde(default, rename = "failReason")]
    pub fail_reason: Option<String>,
}

/// message.received 通知 / chat.history 结果中的原始消息结构（spec §3.2 RawMessage）
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RawMessage {
    pub msg_id: String,
    pub chat_who: String,
    pub chat_type: String, // 'friend' | 'group'
    pub attr: String,      // 'friend' | 'system' | 'self'
    pub msg_type: String,  // 'text' | 'image' | 'voice' | ...
    pub sender: String,
    pub content: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 方法名常量必须是 dot/snake 命名的 JSON-RPC method 字符串
    #[test]
    fn test_method_constants() {
        assert_eq!(methods::INIT, "wx.init");
        assert_eq!(methods::ACTIVATE, "wx.activate");
        assert_eq!(methods::GET_MY_INFO, "wx.get_my_info");
        assert_eq!(methods::IS_ONLINE, "wx.is_online");
        assert_eq!(methods::MSG_SEND, "msg.send");
        assert_eq!(methods::FILE_SEND, "file.send");
        assert_eq!(methods::MSG_QUOTE, "msg.quote");
        assert_eq!(methods::MSG_FORWARD, "msg.forward");
        assert_eq!(methods::CHAT_OPEN, "chat.open");
        assert_eq!(methods::CHAT_SEARCH, "chat.search");
        assert_eq!(methods::CHAT_HISTORY, "chat.history");
        assert_eq!(methods::LISTEN_ADD, "listen.add");
        assert_eq!(methods::LISTEN_REMOVE, "listen.remove");
        assert_eq!(methods::LISTEN_LIST, "listen.list");
        assert_eq!(methods::FRIENDS_NEW_REQUESTS, "friends.new_requests");
        assert_eq!(methods::FRIEND_ACCEPT, "friend.accept");
        assert_eq!(methods::MOMENTS_GET, "moments.get");
        assert_eq!(methods::MOMENTS_PUBLISH, "moments.publish");
        assert_eq!(methods::MEDIA_DOWNLOAD, "media.download");
        assert_eq!(methods::VOICE_TO_TEXT, "voice.to_text");
        assert_eq!(methods::UTIL_SLEEP, "util.sleep");
    }

    /// RawMessage 与 Python 侧字段（snake_case）序列化对齐
    #[test]
    fn test_raw_message_serde_roundtrip() {
        let m = RawMessage {
            msg_id: "m1".into(),
            chat_who: "wxid_abc".into(),
            chat_type: "friend".into(),
            attr: "friend".into(),
            msg_type: "text".into(),
            sender: "wxid_abc".into(),
            content: "你好".into(),
        };
        let v = serde_json::to_value(&m).expect("序列化失败");
        assert_eq!(v["msg_id"], "m1");
        assert_eq!(v["chat_who"], "wxid_abc");
        let back: RawMessage = serde_json::from_value(v).expect("反序列化失败");
        assert_eq!(back.content, "你好");
    }

    /// InitResult failReason 契约：camelCase rename + 缺字段向后兼容（Task 3 I1）
    #[test]
    fn test_init_result_fail_reason_serde() {
        // Python 侧发 camelCase failReason → 解析为 Some
        let v: InitResult = serde_json::from_value(serde_json::json!({
            "licensed": false, "wxid": "", "nickname": "", "failReason": "licensed"
        }))
        .expect("反序列化失败");
        assert_eq!(v.fail_reason.as_deref(), Some("licensed"));
        // 旧 sidecar 无该字段 → None（serde default）
        let old: InitResult = serde_json::from_value(serde_json::json!({
            "licensed": true, "wxid": "w", "nickname": "n"
        }))
        .expect("旧包反序列化失败");
        assert_eq!(old.fail_reason, None);
    }
}
