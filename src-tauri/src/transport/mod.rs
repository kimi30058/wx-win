//! 与 NestJS 的传输抽象（trait 隔离，未来可换 MQTT/gRPC 等）
pub mod ws;

use serde_json::Value;
use std::future::Future;

pub trait TransportHandler: Send + Sync {
    /// 连接建立（每次重连成功都回调；上层重放 hello）
    fn on_connect(&self);
    /// 收到一帧（已 JSON 解析）
    fn on_frame(&self, frame: Value);
    /// 连接断开（含拒绝码；上层决定是否提示 token 失效）
    fn on_disconnect(&self, reason: String);
}

pub trait DeviceTransport {
    fn run(self, handler: Box<dyn TransportHandler>) -> impl Future<Output = ()> + Send;
}
