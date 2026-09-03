//! WS 客户端实现：自动重连（1s→30s 指数退避 ±20% 抖动）
//!
//! 连接循环：connect_async 成功 → 排空断线窗口积压的旧帧 → 回调 on_connect
//! （上层重放 hello——保证 hello 是每条新连接的第一帧）→
//! 双向泵（ws 收 → handler.on_frame；WsSender channel 出 → ws 发）→
//! 断开/出错 → 回调 on_disconnect → backoff_delay(attempt) 退避后重连。
//!
//! 通道在 `new()` 时创建：`sender()` 克隆 tx 给上层，`run(self)` 解构取走 rx
//! （内部 tx 随解构 drop，外部 WsSender 全 drop 时 rx.recv() 返回 None → run 退出）。
//! 断线期间经 WsSender 发出的帧会积压在 unbounded 通道——重连成功后先排空再发 hello。
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;

use super::{DeviceTransport, TransportHandler};

pub struct WsTransport {
    url: String,
    tx: tokio::sync::mpsc::UnboundedSender<Value>,
    rx: tokio::sync::mpsc::UnboundedReceiver<Value>,
}

/// 对外发送通道（new 时创建通道；run 把 rx 接到 ws sink）
#[derive(Clone)]
pub struct WsSender(pub tokio::sync::mpsc::UnboundedSender<Value>);

impl WsSender {
    /// 发送一帧 JSON（unbounded 无背压；接收端已随 run 退出时静默失败）
    pub fn send(&self, frame: Value) {
        let _ = self.0.send(frame);
    }
}

impl WsTransport {
    pub fn new(url: String) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self { url, tx, rx }
    }

    /// 取发送端（可在 run 前后多次调用，各持有者共享同一通道）
    pub fn sender(&self) -> WsSender {
        WsSender(self.tx.clone())
    }
}

impl DeviceTransport for WsTransport {
    async fn run(self, handler: Box<dyn TransportHandler>) {
        // 解构：内部 tx 立即 drop——之后「外部 WsSender 全 drop」即可触发 rx.recv()=None 退出
        let Self { url, tx, mut rx } = self;
        drop(tx);
        let mut attempt: u32 = 0;
        loop {
            match tokio_tungstenite::connect_async(url.as_str()).await {
                Ok((mut ws, _resp)) => {
                    attempt = 0;
                    // 日志剥 query：url 含 ?token=<设备令牌>，整串落日志即泄漏
                    let log_url = url.split('?').next().unwrap_or(url.as_str());
                    tracing::info!("ws 已连接: {log_url}");
                    // 排空断线窗口积压在通道里的旧帧：重连后 hello 必须是第一帧
                    // （Task 3/4 审查输入 #1——旧 ping/result 先到会破坏服务端会话语义）
                    let mut drained = 0usize;
                    while let Ok(_stale) = rx.try_recv() {
                        drained += 1;
                    }
                    if drained > 0 {
                        tracing::info!(count = drained, "重连前排空积压旧帧");
                    }
                    // 每次连接成功都回调（含重连），上层借此重放 hello
                    handler.on_connect();
                    // 双向泵：ws 收 → handler.on_frame；rx 出 → ws 发
                    loop {
                        tokio::select! {
                            inbound = ws.next() => match inbound {
                                Some(Ok(Message::Text(t))) => {
                                    match serde_json::from_str::<Value>(&t) {
                                        Ok(v) => handler.on_frame(v),
                                        Err(e) => tracing::warn!("ws 文本帧 JSON 解析失败，丢弃: {e}"),
                                    }
                                }
                                Some(Ok(Message::Close(c))) => {
                                    let reason = c
                                        .map(|f| f.reason.into_owned())
                                        .unwrap_or_else(|| "服务端关闭".into());
                                    handler.on_disconnect(reason);
                                    break;
                                }
                                Some(Ok(_)) => {} // Binary/Ping/Pong：本协议纯 JSON 文本，忽略
                                Some(Err(e)) => {
                                    handler.on_disconnect(format!("连接错误: {e}"));
                                    break;
                                }
                                None => {
                                    handler.on_disconnect("连接关闭".into());
                                    break;
                                }
                            },
                            outbound = rx.recv() => match outbound {
                                Some(v) => {
                                    if let Err(e) = ws.send(Message::Text(v.to_string())).await {
                                        handler.on_disconnect(format!("发送失败: {e}"));
                                        break;
                                    }
                                }
                                None => return, // 发送端全 drop → 退出
                            },
                        }
                    }
                }
                Err(e) => handler.on_disconnect(format!("连接失败: {e}")),
            }
            // 重连退避（attempt 从 0 起：首次重连约 1s）
            let delay = backoff_delay(attempt);
            attempt += 1;
            tracing::warn!("ws 断开，{:?} 后第 {} 次重连", delay, attempt);
            tokio::time::sleep(delay).await;
        }
    }
}

/// 退避：base=1s<<attempt（封顶 30s）±20% 抖动（纯函数可单测）
pub fn backoff_delay(attempt: u32) -> Duration {
    let base_ms: u64 = (1000u64 << attempt.min(5)).min(30_000);
    let jitter = rand::thread_rng().gen_range(base_ms * 8 / 10..=base_ms * 6 / 5);
    Duration::from_millis(jitter)
}

#[cfg(test)]
mod tests {
    use super::backoff_delay;
    use std::time::Duration;

    /// 退避必须落在 capped-base 的 ±20% 区间内（base=1s<<attempt，30s 封顶）。
    /// 注意：base 计算必须与实现同样带 30s 封顶——brief 骨架用未封顶的 1<<5=32s
    /// 算期望区间时，attempt≥5 会随机落在区间外（实现最低 24s < 期望下界 25.6s）。
    #[test]
    fn test_backoff_range() {
        for attempt in 0..10 {
            let d = backoff_delay(attempt);
            let base_ms = (1000u64 << attempt.min(5)).min(30_000);
            let base = Duration::from_millis(base_ms);
            assert!(
                d >= base * 4 / 5 && d <= base * 6 / 5,
                "attempt={attempt} d={d:?}"
            );
        }
    }

    /// 封顶语义：attempt≥5 后 base 恒为 30s → 延迟恒在 [24s, 36s]，不再增长
    #[test]
    fn test_backoff_cap_at_30s() {
        for attempt in [5u32, 6, 20, 100] {
            let d = backoff_delay(attempt);
            assert!(
                d >= Duration::from_secs(24) && d <= Duration::from_secs(36),
                "attempt={attempt} 应封顶在 30s±20%，实际 {d:?}"
            );
        }
    }

    /// 首次退避（attempt=0）在 [0.8s, 1.2s]
    #[test]
    fn test_backoff_first_attempt_about_1s() {
        let d = backoff_delay(0);
        assert!(d >= Duration::from_millis(800) && d <= Duration::from_millis(1200));
    }
}
