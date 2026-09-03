//! JSON-RPC 2.0 over stdio 帧编解码（spec §3.2）
//! 每行一个 JSON 对象（换行分隔）：
//! - 请求：`{"jsonrpc":"2.0","id":1,"method":"wx.init","params":{}}`
//! - 响应：`{"jsonrpc":"2.0","id":1,"result":{...}}`
//! - 错误：`{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"..."}}`
//! - 通知：`{"jsonrpc":"2.0","method":"message.received","params":{...}}`（无 id）
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// stdio 上传输的单帧。untagged 反序列化按声明顺序尝试：
/// Response（有 id 无 method）不会误匹配 Request（id+method 都要求存在），
/// 通知（无 id）落到 Notification。
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RpcFrame {
    /// 请求（有 id）——仅 Rust→sidecar 方向；sidecar 输出中出现时按协议违规忽略
    Request {
        jsonrpc: String,
        id: u64,
        method: String,
        #[serde(default)]
        params: Value,
    },
    /// 响应（有 id + result 或 error）
    Response {
        jsonrpc: String,
        id: u64,
        #[serde(default)]
        result: Option<Value>,
        #[serde(default)]
        error: Option<RpcErrorObj>,
    },
    /// 通知（无 id）——sidecar→Rust 单向推送（message.received 等）
    Notification {
        jsonrpc: String,
        method: String,
        #[serde(default)]
        params: Value,
    },
}

/// JSON-RPC error 对象（code + message）
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RpcErrorObj {
    pub code: i64,
    pub message: String,
}

/// Rust 侧 RPC 错误统一类型（禁止 unwrap；IO 错误统一走 Io 变体）
/// Clone 供读循环退出时批量唤醒 pending 使用
#[derive(Debug, Clone, thiserror::Error)]
pub enum RpcError {
    #[error("RPC 超时")]
    Timeout,
    #[error("sidecar 错误: {0}")]
    Sidecar(String),
    #[error("IO 错误: {0}")]
    Io(String),
    #[error("sidecar 已退出")]
    Closed,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 三种帧形状的反序列化路由正确性（响应不会误判为请求/通知）
    #[test]
    fn test_frame_decode_routing() {
        let resp: RpcFrame =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"result":{"echo":"wx.init"}}"#)
                .expect("响应帧解析失败");
        match resp {
            RpcFrame::Response {
                id, result, error, ..
            } => {
                assert_eq!(id, 1);
                assert!(error.is_none());
                assert_eq!(result.expect("result 应存在")["echo"], "wx.init");
            }
            other => panic!("应解析为 Response，实际: {other:?}"),
        }

        let err: RpcFrame = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32000,"message":"微信未登录"}}"#,
        )
        .expect("错误帧解析失败");
        match err {
            RpcFrame::Response { id, error, .. } => {
                assert_eq!(id, 2);
                let e = error.expect("error 应存在");
                assert_eq!(e.code, -32000);
                assert_eq!(e.message, "微信未登录");
            }
            other => panic!("应解析为 Response，实际: {other:?}"),
        }

        let notify: RpcFrame = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"message.received","params":{"text":"hi"}}"#,
        )
        .expect("通知帧解析失败");
        match notify {
            RpcFrame::Notification { method, params, .. } => {
                assert_eq!(method, "message.received");
                assert_eq!(params["text"], "hi");
            }
            other => panic!("应解析为 Notification，实际: {other:?}"),
        }
    }
}
