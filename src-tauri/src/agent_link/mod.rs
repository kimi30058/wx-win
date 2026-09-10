//! 编排层：transport × sidecar × session 的装配与帧流转（spec §5.1）
//!
//! `AgentLink::run` 是主装配入口（消费式跑 transport）：
//! - on_connect：取 wx.get_my_info 组 hello 帧（拿不到发空值——服务端容忍），
//!   随后重放监听名单 resync + 上报初始 status 事件。
//!   积压旧帧的排空由 `WsTransport::run` 在回调 on_connect 之前完成
//!   （rx 归 transport 所有，Task 3/4 审查输入 #1：hello 必须是新连接第一帧）
//! - on_frame(command)：CommandTracker requestId 幂等 → WxSession.execute → result 帧
//! - 通知泵：订阅 sidecar message.received，图片/语音先二次 RPC 再组 event:message
//! - 心跳泵：30s ping（wxOnline 来自 wx.is_online 探测）
//! - 好友轮询泵：60~300s 随机间隔拉 friends.new_requests，新增项发 event:friend_request
//!
//! 事件桥解耦：`event_sink` 可注入（GUI 模式 Task 9 注入 tauri emit；CLI/测试为 None），
//! core 不依赖 tauri。
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::outbox::Outbox;
use crate::transport::ws::{WsSender, WsTransport};
use crate::transport::{DeviceTransport, TransportHandler};
use crate::wx::listener::ListenerRegistry;
use crate::wx::WxSession;

pub mod inbound;
pub mod pending;

use pending::CommandTracker;

/// 事件/指令日志桥类型（GUI 注入 tauri emit；CLI/测试为 None）。
/// Arc 包裹供跨任务持有（on_disconnect 的 spawn 绕开借生命周期）。
pub type ValueSink = Arc<dyn Fn(Value) + Send + Sync>;

/// hello 前门闩状态：pending=true 期间非 hello 帧入 buffer（终审必修 2a：
/// 标志与缓冲同锁，判定/开闩操作原子完成，消除 load→lock await 窗口竞态）
#[derive(Default)]
struct Gate {
    pending: bool,
    buffer: Vec<Value>,
}

// ── 锁中毒恢复（必修 3）：持锁方 panic 只中止当事任务，锁内数据结构本身
// 仍然完好——into_inner 取回守卫值继续运行，不让一次 panic 级联瘫痪整个
// App 的所有后续操作（铁律：运行时路径禁裸 unwrap）。──

/// RwLock 读守卫中毒恢复（Option<ValueSink> 专用：读侧克隆，锁立即释放）
fn read_sink(
    lock: &RwLock<Option<ValueSink>>,
) -> Result<
    Option<ValueSink>,
    std::sync::PoisonError<std::sync::RwLockReadGuard<'_, Option<ValueSink>>>,
> {
    lock.read().map(|g| g.clone())
}

/// RwLock 写守卫中毒恢复（事件/指令桥注入路径）
fn write_sink<'a>(
    lock: &'a RwLock<Option<ValueSink>>,
) -> std::sync::LockResult<std::sync::RwLockWriteGuard<'a, Option<ValueSink>>> {
    lock.write()
}

/// 读守卫中毒恢复（通用）：panic 方只改数据不改结构，取回守卫继续用
fn read_or_recover<'a, T>(lock: &'a RwLock<T>) -> std::sync::RwLockReadGuard<'a, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 心跳间隔（spec §5.1：30s ping）
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// 好友申请轮询最小间隔（60s）
const FRIEND_POLL_MIN_SECS: u64 = 60;
/// 好友申请轮询随机跨度（60~300s：min + 0..=240）
const FRIEND_POLL_JITTER_SECS: u64 = 240;

/// 三态健康告警文案（纯函数可单测；P1：卡死与掉线分开告警）
fn health_alert_text(h: &crate::wx::WxHealth) -> (&'static str, &'static str) {
    match h {
        crate::wx::WxHealth::Online => ("微信已恢复", "心跳探测恢复在线"),
        crate::wx::WxHealth::Offline => (
            "微信掉线",
            "心跳探测离线（微信关闭/掉登录/窗口不可操作）",
        ),
        crate::wx::WxHealth::Unreachable => (
            "sidecar 无响应",
            "心跳探测超时——sidecar 疑似 UIA 卡死，watchdog 将自动重启恢复",
        ),
    }
}

pub struct AgentLink {
    session: Arc<WxSession>,
    listeners: Arc<ListenerRegistry>,
    tracker: CommandTracker,
    /// WS 发送端：run 装配后可用（on_connect 需要等它就位）。
    /// Arc 包裹：halt（同步上下文 spawn）经克隆共享 drop 通道句柄。
    sender: Arc<Mutex<Option<WsSender>>>,
    /// hello 前门闩 + 门闩期间缓冲的待发帧（hello 发出后按序 flush）。
    /// 标志入缓冲同一把锁（终审必修 2a）：send_frame 的「判定+入缓冲」与
    /// on_connect 的「开门闩+take 缓冲」必须原子——分离时 load 与 lock 之间
    /// 的 await 窗口可插入 store(false)+take，帧滞留缓冲到下次重连。
    /// get_my_info 走串行队列+拟人间隙（可达数秒），此窗口内新帧不得先于 hello（审查 I2）。
    gate: Mutex<Gate>,
    pub_url: String,
    /// 事件桥（GUI 注入 tauri emit；None 时只发 WS 不复制）。只收 event 帧。
    /// GUI 装配在 setup 回调里（构造之后），故运行期可注入。
    event_sink: RwLock<Option<ValueSink>>,
    /// 指令日志桥（GUI 桥 wxauto://command-log；on_command 完成点投递）。
    /// 与 event_sink 同为运行期注入。
    command_sink: RwLock<Option<ValueSink>>,
    /// webhook 告警客户端（GUI 注入；None = 未装配告警）。运行期注入，
    /// 模式照抄 event_sink。
    alert_sink: RwLock<Option<std::sync::Arc<crate::alert::AlertClient>>>,
    /// WS 连接态（on_connect 置 true / on_disconnect 置 false）。
    /// GUI 前端「服务器连接」灯数据源（Task 9）。
    ws_connected: AtomicBool,
    /// 协作停止标志（GUI disconnect）：halt 置 true → run 的 transport 循环
    /// 与后台三泵（通知/心跳/好友轮询）感知后退出，不泄漏任务。
    halted: AtomicBool,
    /// 停机信号通道：halt 发信号；transport/泵 select 感知（消除 backoff
    /// 睡眠窗口的停机延迟——30s 退避期间 halt 不该等到睡醒才退出）。
    halt_tx: tokio::sync::watch::Sender<bool>,
    /// 渠道 ID（P2 任务 8c：Config.channelId 透进 hello 帧——连接身份由
    /// token JWT 决定，此字段纯排障用途，服务端 hello schema 非严格
    /// zod，多余字段向后兼容）。run 之前注入；空串时 hello 带空值。
    channel_id: RwLock<String>,
    /// 上行事件发件箱（spec §4.2）：emit_event 先落盘后发送、ack 清除、
    /// 重连补发。默认 disabled（测试/未装配）；GUI/CLI 装配层显式注入。
    /// std RwLock（对齐 event_sink 模式——写锁临界区无 await 点）。
    outbox: RwLock<Arc<Outbox>>,
}

impl AgentLink {
    pub fn new(session: Arc<WxSession>, listeners: Arc<ListenerRegistry>, url: String) -> Self {
        Self::new_with_event_sink(session, listeners, url, None)
    }

    /// 带 event_sink 构造（Task 9 GUI 桥）；CLI/测试传 None
    pub fn new_with_event_sink(
        session: Arc<WxSession>,
        listeners: Arc<ListenerRegistry>,
        url: String,
        event_sink: Option<Box<dyn Fn(Value) + Send + Sync>>,
    ) -> Self {
        Self {
            session,
            listeners,
            tracker: CommandTracker::new(),
            sender: Arc::new(Mutex::new(None)),
            gate: Mutex::new(Gate::default()),
            pub_url: url,
            event_sink: RwLock::new(event_sink.map(Arc::from)),
            command_sink: RwLock::new(None),
            alert_sink: RwLock::new(None),
            ws_connected: AtomicBool::new(false),
            halted: AtomicBool::new(false),
            halt_tx: tokio::sync::watch::Sender::new(false),
            channel_id: RwLock::new(String::new()),
            outbox: RwLock::new(Arc::new(Outbox::disabled())),
        }
    }

    /// 运行期注入/替换事件桥（GUI setup 回调里装配后注入 tauri emit）
    pub fn set_event_sink(&self, sink: Box<dyn Fn(Value) + Send + Sync>) {
        *write_sink(&self.event_sink).unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(Arc::from(sink));
    }

    /// 运行期注入指令日志桥（GUI 桥 wxauto://command-log）
    pub fn set_command_sink(&self, sink: Box<dyn Fn(Value) + Send + Sync>) {
        *write_sink(&self.command_sink).unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(Arc::from(sink));
    }

    /// 注入 webhook 告警客户端（GUI 装配段调用）。直写 + 中毒恢复
    /// （write_sink 是 ValueSink 专用签名，此处类型不同）
    pub fn set_alert_sink(&self, client: std::sync::Arc<crate::alert::AlertClient>) {
        *self
            .alert_sink
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(client);
    }

    /// 心跳健康态翻转告警（P1 三态：变化才发天然防风暴）
    async fn on_health_changed(&self, health: &crate::wx::WxHealth) {
        if let Some(alert) = self
            .alert_sink
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            let (title, detail) = health_alert_text(health);
            alert.send(title, detail);
        }
    }

    /// 运行期注入渠道 ID（P2 任务 8c：GUI/CLI 装配后、run 之前调；
    /// on_connect 每次（含重连）组 hello 帧读取）
    pub fn set_channel_id(&self, channel_id: String) {
        *self
            .channel_id
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = channel_id;
    }

    /// 运行期注入 outbox（GUI/CLI 装配层；对齐 event_sink 注入模式）
    pub fn set_outbox(&self, outbox: Arc<Outbox>) {
        *self
            .outbox
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = outbox;
    }

    /// 当前 outbox 快照（读侧统一入口；锁中毒恢复对齐既有模式）
    fn current_outbox(&self) -> Arc<Outbox> {
        self.outbox
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// WS 连接态快照（GUI「服务器连接」灯）
    pub fn ws_connected(&self) -> bool {
        self.ws_connected.load(Ordering::Acquire)
    }

    /// 协作停止（GUI disconnect）：后续 run 与后台泵感知后退出。
    /// 幂等；停机后不再重连（connect 重建新 AgentLink）。
    /// 三路并进：①halted 标志（泵轮询感知）②watch 信号（transport 的
    /// select 感知——backoff 睡眠中也能立即唤醒）③drop 发送端（兜底）。
    pub fn halt(&self) {
        self.halted.store(true, Ordering::Release);
        self.ws_connected.store(false, Ordering::Release);
        let _ = self.halt_tx.send(true);
        let sender = self.sender.clone();
        tokio::spawn(async move {
            *sender.lock().await = None;
        });
    }

    /// run 装配后取 WS 发送端（测试 / CLI 用）
    pub async fn sender(&self) -> Option<WsSender> {
        self.sender.lock().await.clone()
    }

    /// 主循环：装配 transport + 三个后台泵，跑 transport（阻塞当前 task）
    /// 由 tauri async runtime / CLI main spawn。
    /// 幂等防御：halt 过的实例不再启动（connect 侧重建新实例）。
    pub async fn run(self: Arc<Self>) {
        if self.halted.load(Ordering::Acquire) {
            tracing::info!("AgentLink 已 halt，跳过启动");
            return;
        }
        let transport = WsTransport::new(self.pub_url.clone());
        *self.sender.lock().await = Some(transport.sender());
        let mut halt_rx = self.halt_tx.subscribe();

        // 通知泵：sidecar message.received → 二次 RPC → event 帧
        let notify_rx = self.session.subscribe_notifications().await;
        tokio::spawn({
            let link = self.clone();
            async move { link.notification_pump(notify_rx).await }
        });
        // 心跳泵：30s ping + wxOnline（持续 1s 短周期探测 ws_connected 供 halt 感知）
        tokio::spawn({
            let link = self.clone();
            async move { link.heartbeat_loop().await }
        });
        // 好友申请轮询泵：60~300s 随机
        tokio::spawn({
            let link = self.clone();
            async move { link.friend_poll_loop().await }
        });

        // transport 与 halt 信号赛跑：halt 时 run 立即结束（不必等 backoff 睡醒）
        tokio::select! {
            _ = transport.run(Box::new(LinkHandler { link: self.clone() })) => {}
            _ = halt_rx.changed() => {
                tracing::info!("transport 感知 halt 信号，退出连接循环");
            }
        }
    }

    /// 连接建立（含重连）：hello → resync → 初始 status。
    /// - 积压旧帧已由 transport 在本回调之前排空
    /// - get_my_info 可拖数秒（串行队列+拟人间隙），期间 hello 前门闩兜住新帧（审查 I2）
    async fn on_connect(&self) {
        // WS 连接态置位（GUI「服务器连接」灯）
        self.ws_connected.store(true, Ordering::Release);

        // wxid/nickname 从 sidecar wx.get_my_info 取；拿不到发空值 hello（服务端容忍）
        // 门闩已由 LinkHandler::on_connect 同步段上好（必修 2b：armed 不晚于
        // spawn 返回，spawn 调度延迟窗口内到达的帧已被缓冲）
        let info = self
            .session
            .execute("get_my_info", json!({}))
            .await
            .unwrap_or(json!({}));
        let frame = json!({
            "kind": "hello",
            "protocolVersion": 1,
            "appVersion": env!("CARGO_PKG_VERSION"),
            "wxid": info["wxid"].as_str().unwrap_or(""),
            "nickname": info["nickname"].as_str().unwrap_or(""),
            "hostname": crate::alert::hostname(),
            "channelId": read_or_recover(&self.channel_id).clone(),
            "ts": inbound::now_ms(),
        });
        // hello 本身不走 send_frame 门（直接发，防自锁）
        if let Some(tx) = self.sender.lock().await.as_ref() {
            tx.send(frame);
        }

        // 开门闩并取走缓冲帧（同锁原子：开闩瞬间的并发 send_frame 要么见
        // pending=true 入缓冲被本次取走，要么见 pending=false 直接发送——
        // 无缝、不丢、不重复）
        let buffered: Vec<Value> = {
            let mut g = self.gate.lock().await;
            g.pending = false;
            std::mem::take(&mut g.buffer)
        };
        if !buffered.is_empty() {
            tracing::debug!(count = buffered.len(), "hello 后 flush 缓冲帧");
        }
        for f in buffered {
            self.send_frame(f).await;
        }

        // outbox 补发（spec §4.3：置于 resync 前——resync 每监听对象 0.5~1s 拟人
        // 间隙，别让积压事件排队其后）
        let pending = self.current_outbox().pending();
        if !pending.is_empty() {
            tracing::info!(count = pending.len(), "补发 outbox 积压事件");
            for f in pending {
                self.send_frame(f).await;
            }
        }

        // 监听名单重放（sidecar 或服务端重启后都需要）
        let failed = self.listeners.resync().await;
        if !failed.is_empty() {
            tracing::warn!(?failed, "部分监听重注册失败");
        }

        // 初始 status 事件（三态健康现场探测；sidecarAlive=true——泵在跑即存活）
        let health = self.session.probe_health().await;
        let listeners = self.listeners.list().await.len();
        self.emit_event(inbound::status_event(
            health.is_online(),
            health.as_str(),
            listeners,
            true,
        ))
        .await;
    }

    /// 下行 command 处理：幂等 → session 执行 → result 帧 → 指令日志投递
    async fn on_command(&self, frame: &Value) {
        let request_id = frame["requestId"].as_str().unwrap_or_default().to_string();
        let action = frame["action"].as_str().unwrap_or_default().to_string();
        let params = frame["params"].clone();
        if request_id.is_empty() || action.is_empty() {
            tracing::warn!(?frame, "command 帧缺少 requestId/action，丢弃");
            return;
        }
        if !self.tracker.begin(request_id.clone()).await {
            tracing::debug!(%request_id, "重复 requestId 在途，忽略");
            return; // 重复 requestId 在途，忽略
        }
        let started = std::time::Instant::now();
        let result = self.session.execute(&action, params).await;
        let (success, data, error) = match result {
            Ok(v) => (true, v, Value::Null),
            Err(e) => (false, Value::Null, json!(e.to_string())),
        };
        // finish 必须先于 result 帧发送：服务端收到 result 后可能立即重发同 id
        self.tracker.finish(&request_id).await;
        let rf = json!({
            "kind": "result",
            "requestId": request_id,
            "success": success,
            "data": data,
            "error": error,
            "ts": inbound::now_ms(),
        });
        self.send_frame(rf).await;
        // 指令日志（GUI wxauto://command-log；失败条目带 error 字段）
        let mut log = json!({
            "requestId": request_id,
            "action": action,
            "success": success,
            "durationMs": started.elapsed().as_millis() as u64,
            "ts": inbound::now_ms(),
        });
        if !success {
            log["error"] = error.clone();
        }
        if let Some(sink) = read_or_recover(&self.command_sink).as_ref() {
            sink(log);
        }
    }

    /// 下行 ack 处理：按 eventId 清除 outbox（spec §3.2）
    async fn on_ack(&self, frame: &Value) {
        let event_id = frame["eventId"].as_str().unwrap_or_default();
        if event_id.is_empty() {
            tracing::warn!(?frame, "ack 帧缺 eventId，丢弃");
            return;
        }
        if self.current_outbox().ack(event_id) {
            tracing::debug!(%event_id, "事件已确认，出箱");
        }
    }

    /// 通知泵：message.received → 图片/语音二次 RPC → event:message 帧。
    /// 二次处理失败降级空字段照常上报（spec §5.1）。
    ///
    /// 重订阅语义（Task 6 审查硬性输入 #2）：sidecar 崩溃/重启后旧 broadcast
    /// 发送端随 Shared drop → RecvError::Closed。泵不退出，改为 500ms 退避后
    /// 对【当前】session 内句柄重新订阅（Supervisor 已热替换新句柄则拿到
    /// 新通道；尚未替换则拿到死句柄的通道再 Closed 一轮，直至新 sidecar 就位）。
    /// 无重订阅的旧实现泵在 sidecar 重启后永久沉默——消息事件全部丢失。
    async fn notification_pump(&self, mut rx: tokio::sync::broadcast::Receiver<Value>) {
        /// Closed 后重订阅退避（给 Supervisor 热替换留窗口，避免忙转）
        const RESUBSCRIBE_BACKOFF: Duration = Duration::from_millis(500);
        let mut halt_rx = self.halt_tx.subscribe();
        loop {
            // recv 挂 halt 信号（GUI disconnect 泵即时退出）
            let note = tokio::select! {
                n = rx.recv() => n,
                _ = halt_rx.changed() => {
                    tracing::info!("通知泵感知 halt，退出");
                    return;
                }
            };
            match note {
                Ok(note) => {
                    if note["method"] != "message.received" {
                        continue;
                    }
                    let params = note["params"].clone();
                    let msg_type = params["msg_type"].as_str().unwrap_or("text").to_string();
                    let msg_id = params["msg_id"].as_str().unwrap_or_default().to_string();

                    // 图片 → download_media；语音 → voice_to_text（失败降级 None）
                    let mut downloaded: Option<String> = None;
                    let mut voice: Option<String> = None;
                    if msg_type == "image" {
                        downloaded = self
                            .session
                            .execute("download_media", json!({ "msgId": msg_id }))
                            .await
                            .ok()
                            .and_then(|v| v["path"].as_str().map(str::to_string));
                        if downloaded.is_none() {
                            tracing::warn!(%msg_id, "图片下载失败，降级空字段上报");
                        }
                    } else if msg_type == "voice" {
                        voice = self
                            .session
                            .execute("voice_to_text", json!({ "msgId": msg_id }))
                            .await
                            .ok()
                            .and_then(|v| v["text"].as_str().map(str::to_string));
                        if voice.is_none() {
                            tracing::warn!(%msg_id, "语音转写失败，降级原内容上报");
                        }
                    }

                    let frame = inbound::message_event_from_notification(
                        &params,
                        downloaded.as_deref(),
                        voice.as_deref(),
                    );
                    self.emit_event(frame).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    // 慢订阅者丢旧保新（通道容量 256）：消息事件不可全量保，记日志继续
                    tracing::warn!(skipped = n, "通知泵落后，跳过 {n} 条通知");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    // sidecar 死亡（旧 Shared drop）。退避后对当前 session 句柄重订阅：
                    // Supervisor 已替换新句柄 → 新通道恢复收流；尚未替换 → 死句柄通道
                    // 立即再 Closed，回到这里继续退避等待。泵永不退出。
                    tracing::warn!("sidecar 通知通道已关闭，500ms 后重订阅（等 sidecar 重启）");
                    tokio::time::sleep(RESUBSCRIBE_BACKOFF).await;
                    rx = self.session.subscribe_notifications().await;
                }
            }
        }
    }

    /// 心跳循环（独立 spawn）：30s ping + 三态健康探测（P1）。
    ///
    /// ping 帧的 wxOnline 保持 bool（服务端/前端旧契约）：仅 Online 为 true，
    /// Offline/Unreachable 都发 false（保守可用性口径）。健康态变化时向
    /// event_sink 补发 status 事件（带 wxState 三态字段，GUI 微信灯实时性
    /// 来源）+ webhook 三态告警（卡死与掉线分开）；睡眠经 select 挂 halt
    /// 信号（停机响应即时）。
    pub async fn heartbeat_loop(&self) {
        let mut last_health: Option<crate::wx::WxHealth> = None;
        let mut halt_rx = self.halt_tx.subscribe();
        loop {
            // 1s 心跳分片（挂 halt 信号：停机即醒）
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                _ = halt_rx.changed() => {
                    tracing::info!("心跳泵感知 halt，退出");
                    return;
                }
            }
            if self.halted.load(Ordering::Acquire) {
                tracing::info!("心跳泵感知 halt，退出");
                return;
            }
            // 剩余 29s 合并睡眠（同样挂 halt 信号）
            tokio::select! {
                _ = tokio::time::sleep(HEARTBEAT_INTERVAL.saturating_sub(Duration::from_secs(1))) => {}
                _ = halt_rx.changed() => {
                    tracing::info!("心跳泵感知 halt，退出");
                    return;
                }
            }
            if self.halted.load(Ordering::Acquire) {
                tracing::info!("心跳泵感知 halt，退出");
                return;
            }
            let health = self.session.probe_health().await;
            self.send_frame(json!({
                "kind": "ping",
                "wxOnline": health.is_online(),
                "ts": inbound::now_ms(),
            }))
            .await;
            // 健康态变化 → GUI 补发 status（wxState 三态）+ webhook 告警
            if last_health != Some(health) {
                last_health = Some(health);
                // webhook 告警挂点：翻转才发（天然防风暴）
                self.on_health_changed(&health).await;
                let listeners = self.listeners.list().await.len();
                self.emit_event(inbound::status_event(
                    health.is_online(),
                    health.as_str(),
                    listeners,
                    true,
                ))
                .await;
            }
        }
    }

    /// 好友申请轮询循环：60~300s 随机间隔，新增项（按 name 去重）发 event:friend_request
    pub async fn friend_poll_loop(&self) {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut halt_rx = self.halt_tx.subscribe();
        loop {
            let gap = FRIEND_POLL_MIN_SECS
                + rand::Rng::gen_range(&mut rand::thread_rng(), 0..=FRIEND_POLL_JITTER_SECS);
            // 分片睡眠挂 halt 信号（整段睡眠会拖长停机窗口）
            let deadline = tokio::time::Instant::now() + Duration::from_secs(gap);
            loop {
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    break;
                }
                let remain = (deadline - now).min(Duration::from_secs(1));
                tokio::select! {
                    _ = tokio::time::sleep(remain) => {}
                    _ = halt_rx.changed() => {
                        tracing::info!("好友轮询泵感知 halt，退出");
                        return;
                    }
                }
                if self.halted.load(Ordering::Acquire) {
                    tracing::info!("好友轮询泵感知 halt，退出");
                    return;
                }
            }
            if let Ok(v) = self.session.execute("get_friend_requests", json!({})).await {
                if let Some(reqs) = v["requests"].as_array() {
                    for r in reqs {
                        let name = r["name"].as_str().unwrap_or_default().to_string();
                        if !name.is_empty() && seen.insert(name.clone()) {
                            self.emit_event(inbound::friend_request_event(&name, "请求添加好友"))
                                .await;
                        }
                    }
                }
            }
        }
    }

    /// 发一帧到 WS。hello 前门闩（gate.pending）期间非 hello 帧进缓冲，
    /// hello 发出后由 on_connect flush（审查 I2：慢 get_my_info 窗口内
    /// 心跳/事件帧不得先于 hello 到达服务端）。判定+入缓冲同锁原子（必修 2a）。
    async fn send_frame(&self, frame: Value) {
        {
            let mut g = self.gate.lock().await;
            if g.pending {
                g.buffer.push(frame);
                return;
            }
        }
        if let Some(tx) = self.sender.lock().await.as_ref() {
            tx.send(frame);
        }
    }

    /// 发一帧上行事件：event_sink → outbox 落盘 → WS 发送（先落盘后发送，spec §4.3）
    async fn emit_event(&self, frame: Value) {
        if let Some(sink) = read_or_recover(&self.event_sink).as_ref() {
            sink(frame.clone());
        }
        // 先落盘后发送：发送失败/断线时条目已在箱，重连补发兜底；
        // status 等非入箱帧（type 不在 {message, friend_request}）自然 no-op
        self.current_outbox().enqueue(&frame);
        self.send_frame(frame).await;
    }

    /// 测试辅助：外部注入一条事件帧（走与通知泵相同的 emit 路径，验证门闩）
    pub async fn emit_test_event(&self, frame: Value) {
        self.emit_event(frame).await;
    }

    /// 测试辅助：外部注入一帧（走与心跳泵相同的 send_frame 路径，验证门闩）
    pub async fn send_test_frame(&self, frame: Value) {
        self.send_frame(frame).await;
    }

    /// 测试辅助：hello 前门闩是否处于 armed 状态（try_lock 窄窗探测，
    /// 拿不到锁视为未上闩——探测失败只会让测试重试轮询）
    pub fn is_hello_pending(&self) -> bool {
        match self.gate.try_lock() {
            Ok(g) => g.pending,
            Err(_) => false,
        }
    }

    /// 测试辅助：是否已被 halt（连接管理并发回归断言用）
    pub fn is_halted_for_test(&self) -> bool {
        self.halted.load(Ordering::Acquire)
    }
}

/// TransportHandler 桥：持 Arc<AgentLink>（无 unsafe；生命周期与 run 绑定）
struct LinkHandler {
    link: Arc<AgentLink>,
}

impl TransportHandler for LinkHandler {
    fn on_connect(&self) {
        // 同步段先上门闩（必修 2b）：spawn 的 async on_connect 里再置位太晚——
        // spawn 调度延迟窗口内 on_frame 的 command 可先走 send_frame，result
        // 先于 hello 到达服务端。此处同步置 pending=true，spawn 返回即已 armed。
        // try_lock 失败仅意味着并发方持锁（send_frame 入缓冲/on_connect 开闩），
        // 门已生效，跳过即可。
        if let Ok(mut g) = self.link.gate.try_lock() {
            g.pending = true;
        }
        let link = self.link.clone();
        tokio::spawn(async move {
            link.on_connect().await;
        });
    }
    fn on_frame(&self, frame: Value) {
        match frame["kind"].as_str() {
            Some("command") => {
                let link = self.link.clone();
                tokio::spawn(async move {
                    link.on_command(&frame).await;
                });
            }
            // ack 清箱（spec §3.2）：服务端确认收到的事件出箱
            Some("ack") => {
                let link = self.link.clone();
                tokio::spawn(async move {
                    link.on_ack(&frame).await;
                });
            }
            _ => {}
        }
    }
    fn on_disconnect(&self, reason: String) {
        // reason 并入 message 正文——GUI 运行日志只渲染 message 字段，
        // 结构化字段在前端不可见（生产 4001 排障时 reason 全被吞）
        tracing::warn!("WS 断开: {reason}");
        // GUI 桥：连接态复位 + 补发 status（前端「服务器连接」灯数据源；
        // 补发的是 event_sink 侧的 status 事件——断开后 WS 不可达，只走桥）
        self.link.ws_connected.store(false, Ordering::Release);
        // 依赖先 clone 成 'static 再进 spawn（&self 借用不得逃逸出方法体）
        let sink = read_sink(&self.link.event_sink).unwrap_or_default();
        let session = self.link.session.clone();
        let listeners = self.link.listeners.clone();
        tokio::spawn(async move {
            let health = session.probe_health().await;
            let n = listeners.list().await.len();
            if let Some(sink) = sink.as_ref() {
                sink(inbound::status_event(
                    health.is_online(),
                    health.as_str(),
                    n,
                    true,
                ));
            }
        });
    }
}

#[cfg(test)]
mod alert_tests {
    use super::*;
    use crate::alert::AlertClient;
    use crate::sidecar::SidecarHandle;
    use crate::wx::WxHealth;
    use std::sync::Arc;

    /// set_alert_sink 注入后可读回（RwLock 中毒恢复路径同 event_sink）
    #[tokio::test]
    async fn test_alert_sink_roundtrip() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let handle = SidecarHandle::spawn_with_python("pass\n", &[])
            .await
            .expect("spawn 失败");
        let session = Arc::new(WxSession::new(Arc::new(Mutex::new(handle))));
        let listeners = Arc::new(ListenerRegistry::new(session.clone()));
        let link = AgentLink::new(session, listeners, format!("ws://{addr}/"));
        let client = Arc::new(AlertClient::new(String::new(), String::new(), "t".into()));
        link.set_alert_sink(client.clone());
        assert!(
            link.alert_sink
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some(),
            "注入后应可读回"
        );
        // 空 URL send 全链不炸（no-op）；三态各走一遍
        link.on_health_changed(&WxHealth::Offline).await;
        link.on_health_changed(&WxHealth::Unreachable).await;
        link.on_health_changed(&WxHealth::Online).await;
    }

    /// P1 三态告警文案：卡死（Unreachable）与掉线（Offline）分开——
    /// 掉线指引用户看微信，卡死指引自愈路径（watchdog 重启）
    #[test]
    fn test_health_alert_text_three_states() {
        let (t1, d1) = health_alert_text(&WxHealth::Online);
        assert_eq!(t1, "微信已恢复");
        assert!(d1.contains("恢复在线"));

        let (t2, d2) = health_alert_text(&WxHealth::Offline);
        assert_eq!(t2, "微信掉线");
        assert!(d2.contains("微信关闭"), "掉线文案应指向微信侧原因");

        let (t3, d3) = health_alert_text(&WxHealth::Unreachable);
        assert_eq!(t3, "sidecar 无响应");
        assert!(d3.contains("卡死"), "卡死文案应区分于掉线");
        assert!(d3.contains("重启"), "卡死文案应指引自愈路径");
    }
}
