//! 全局状态机（spec §3.3）+ Supervisor 崩溃退避重启（spec §5.1）
//!
//! 六态：
//! - SidecarDead：退避 5 轮耗尽（1/2/4/8/16s），需人工介入重试
//! - SidecarBooting：sidecar 已起、wx.init 未完成（含 license 未过——面板显示授权引导）
//! - WxInit：wx.init 且 licensed → 等监听注册完成
//! - Ready：监听注册完成，正常服务
//! - Busy：有指令在执行（仅 Ready 可进；串行队列保证同一时刻一条）
//! - Degraded：心跳探测 wx.is_online=false（微信掉线但 sidecar 存活）；
//!   恢复上线自动回 Ready
//!
//! Supervisor 职责（spec §5.1）：
//! 等子进程退出 → 退避 → 重新 spawn（经注入的 spawner）→ 热替换
//! WxSession 内句柄 → wx.init → 监听 resync → 发 status 事件。
//! WS 侧（hello 重放 / 事件上报）由 AgentLink 独立处理，这里只管 sidecar 域。
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::sidecar::protocol::RpcError;
use crate::sidecar::spec::methods;
use crate::sidecar::SidecarHandle;
use crate::wx::listener::ListenerRegistry;
use crate::wx::WxSession;

/// sidecar 崩溃退避序列（spec §5.1：1/2/4/8/16s，5 次后 SidecarDead）
pub const RESTART_BACKOFF: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
];

/// wx.init 专属超时：比 RPC_TIMEOUT(30s) 短——init 无响应即视 sidecar
/// 病态，尽快进入崩溃处理循环（见 direct_wx_init 注释）
const INIT_TIMEOUT: Duration = Duration::from_secs(10);

/// 「活够久」阈值：sidecar 存活超过该时长后崩溃,失败计数才在重启时清零。
/// spawn 成功但立即退出（Windows 9009 找不到命令 / 杀软删 exe）不清零——
/// 否则 1s 退避无限循环,永不进 SidecarDead（2026-09-03 真机日志定位）。
const SIDECAR_MIN_UPTIME: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppState {
    SidecarDead,
    SidecarBooting,
    WxInit,
    Ready,
    Busy,
    Degraded,
}

/// 状态变化回调（GUI 模式 Task 9 注入 tauri emit；CLI/测试为 None）。
/// 参数为 (旧态, 新态)；仅变化时触发。
pub type StateChangeCallback = Box<dyn Fn(&AppState, &AppState) + Send + Sync>;

pub struct AppStateMachine {
    state: RwLock<AppState>,
    /// 变化回调（可选；GUI 事件桥）。构造后由 with_on_change 注入。
    on_change: RwLock<Option<Arc<StateChangeCallback>>>,
}

impl AppStateMachine {
    pub fn new() -> Self {
        Self {
            state: RwLock::new(AppState::SidecarBooting),
            on_change: RwLock::new(None),
        }
    }

    /// 注入状态变化回调（GUI 事件桥；Builder 风格返回自身便于链式）
    pub async fn with_on_change(self, cb: StateChangeCallback) -> Self {
        *self.on_change.write().await = Some(Arc::new(cb));
        self
    }

    /// 当前状态（异步读；主路径）
    pub async fn state(&self) -> AppState {
        self.state.read().await.clone()
    }

    /// 同步快照（前端低开销查询；读锁被占的瞬间回退 Degraded——
    /// 不阻塞不 panic，调用方下次查询自然拿到真实值）
    pub fn state_sync(&self) -> AppState {
        self.state
            .try_read()
            .map(|s| s.clone())
            .unwrap_or(AppState::Degraded)
    }

    /// 无条件迁移（仅 mark_sidecar_died 用——终态写入是唯一豁免）。
    /// 其余入口一律走 `transition_if`（写锁内守卫，见其注释）。
    async fn transition(&self, next: AppState) {
        self.transition_if(next, |_, _| true).await;
    }

    /// 统一迁移入口：守卫谓词在【写锁内】判定（check-and-set 原子）。
    /// 审查 I1：此前 mark_busy/mark_idle/mark_online 是「读锁取态 → 放锁 →
    /// 写锁写入」三段，读锁释放到写锁 acquire 的窗口内状态可被并发改写
    /// （危害场景：心跳读到 Ready 算出 Degraded → Supervisor 恰在此刻写入
    /// SidecarDead 终态并退出 → 心跳的迟到写入覆盖终态，App 永显 Degraded
    /// 无人纠正）。守卫移进写锁后该窗口不复存在。
    /// 附带不变量：SidecarDead 是终态，除 mark_sidecar_died 外任何入口
    /// 不得写入/离开（复活终态 = 谎报 sidecar 已死）。
    /// 同值或守卫拒绝不触发回调；写锁释放后再回调（回调内查询不死锁）。
    async fn transition_if(
        &self,
        next: AppState,
        guard: impl FnOnce(&AppState, &AppState) -> bool,
    ) {
        let (old, next) = {
            let mut s = self.state.write().await;
            if *s == next || !guard(&s, &next) {
                return;
            }
            let old = s.clone();
            *s = next.clone();
            (old, next)
        };
        if let Some(cb) = self.on_change.read().await.clone() {
            cb(&old, &next);
        }
    }

    /// wx.init 结果落状态：licensed 进 WxInit；未授权保持 Booting（面板显示授权引导）
    pub async fn mark_wx_init(&self, licensed: bool) {
        if licensed {
            self.transition_if(AppState::WxInit, |cur, _| {
                *cur != AppState::SidecarDead // 终态不可复活
            })
            .await;
        }
        // 未授权：不迁移（Booting 态即「等待授权」语义）
    }

    /// 监听注册完成 → Ready
    pub async fn mark_ready(&self) {
        self.transition_if(AppState::Ready, |cur, _| {
            *cur != AppState::SidecarDead // 终态不可复活
        })
        .await;
    }

    /// 指令开始执行：仅 Ready 可进（Busy 表示串行队列占用）。
    /// 守卫在写锁内判定（审查 I1：消除 check-then-act 窗口）。
    pub async fn mark_busy(&self) {
        self.transition_if(AppState::Busy, |cur, _| *cur == AppState::Ready)
            .await;
    }

    /// 指令执行完毕：Busy → Ready。守卫在写锁内判定（审查 I1）。
    pub async fn mark_idle(&self) {
        self.transition_if(AppState::Ready, |cur, _| *cur == AppState::Busy)
            .await;
    }

    /// 心跳探测结果（spec：微信离线 → Degraded；恢复 → Ready）。
    /// 仅在 Ready/Degraded 两态间切换，不打断 Booting/WxInit/Busy/SidecarDead
    /// （那些态有自己的推进条件，心跳无权改写）。
    /// 守卫在写锁内判定（审查 I1：SidecarDead 终态不可被迟到的心跳覆盖）。
    pub async fn mark_online(&self, online: bool) {
        let next = if online {
            AppState::Ready
        } else {
            AppState::Degraded
        };
        self.transition_if(next, |cur, next| {
            matches!(
                (cur, next),
                (AppState::Ready, AppState::Degraded) | (AppState::Degraded, AppState::Ready)
            )
        })
        .await;
    }

    /// sidecar 退避耗尽 → SidecarDead（终态，需人工重试）。
    /// 唯一允许写入终态的入口（无条件迁移）。
    pub async fn mark_sidecar_died(&self) {
        self.transition(AppState::SidecarDead).await;
    }

    /// 显式置 Degraded（Supervisor 重启失败的中间态 / 未来扩展）。
    /// 终态不可覆盖（审查 I1 不变量）。
    pub async fn mark_degraded(&self) {
        self.transition_if(AppState::Degraded, |cur, _| *cur != AppState::SidecarDead)
            .await;
    }
}

impl Default for AppStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

/// 重启用的 sidecar 生成器：返回新句柄的 future（生产 = spawn_default，
/// 测试 = spawn_with_python 假 sidecar）。
pub type SidecarSpawner = Arc<
    dyn Fn() -> futures_util::future::BoxFuture<'static, Result<SidecarHandle, SpawnerError>>
        + Send
        + Sync,
>;

/// spawner 失败的错误类型（Box<dyn Error> 的薄包装，便于测试构造）
pub type SpawnerError = Box<dyn std::error::Error + Send + Sync>;

/// Supervisor：监控 sidecar 子进程退出 → 退避重启 → wx.init → resync。
///
/// 装配（CLI main / Task 9 GUI）：
/// ```ignore
/// let sup = Arc::new(Supervisor::new(state, session, listeners).with_spawner(prod_spawner()));
/// tokio::spawn(async move { sup.run().await });
/// ```
/// 首代 sidecar 由装配方 spawn 后交给 WxSession（Supervisor 不负责首启），
/// 此后所有重启都经 spawner 产新句柄、`replace_sidecar` 热替换进 WxSession——
/// 「所有操作唯一入口」语义保持不变。
pub struct Supervisor {
    pub state: Arc<AppStateMachine>,
    session: Arc<WxSession>,
    listeners: Arc<ListenerRegistry>,
    /// sidecar 重启生成器（默认走 SidecarHandle::spawn_default）
    spawner: SidecarSpawner,
    /// 退避表（生产 RESTART_BACKOFF；测试注入毫秒级加速）
    backoff_table: Arc<Vec<Duration>>,
    /// 事件出口（GUI 事件桥；None 时只进 WS 由 AgentLink 上报，这里跳过）
    event_sink: Option<Box<dyn Fn(Value) + Send + Sync>>,
    /// 诊断：当前所处阶段（测试 / 排障插桩用）
    phase: RwLock<&'static str>,
}

impl Supervisor {
    pub fn new(
        state: Arc<AppStateMachine>,
        session: Arc<WxSession>,
        listeners: Arc<ListenerRegistry>,
    ) -> Self {
        Self {
            state,
            session,
            listeners,
            spawner: default_spawner(),
            backoff_table: Arc::new(RESTART_BACKOFF.to_vec()),
            event_sink: None,
            phase: RwLock::new("constructed"),
        }
    }

    /// 当前阶段名（诊断插桩；测试观察 Supervisor 推进位置）
    pub async fn current_phase(&self) -> String {
        self.phase.read().await.to_string()
    }

    /// 注入自定义 spawner（测试用假 sidecar；生产默认 spawn_default）
    pub fn with_spawner(mut self, spawner: SidecarSpawner) -> Self {
        self.spawner = spawner;
        self
    }

    /// 注入自定义退避表（测试加速；长度即最大重试次数）
    pub fn with_backoff(mut self, backoff: impl Into<Vec<Duration>>) -> Self {
        self.backoff_table = Arc::new(backoff.into());
        self
    }

    /// 注入事件出口（Task 9 GUI 桥：状态/重启事件透传前端）
    pub fn with_event_sink(mut self, sink: Box<dyn Fn(Value) + Send + Sync>) -> Self {
        self.event_sink = Some(sink);
        self
    }

    /// 主循环：init 序列 → 等子进程退出 → 退避 → 重启 → 重复。
    /// 由 CLI main / tauri runtime spawn，永不出错返回（错误全在循环内消化）。
    pub async fn run(&self) {
        let mut failures: usize = 0;
        // 当前 sidecar 的 spawn 时刻：退出时判「活够久」（SIDECAR_MIN_UPTIME）
        // 决定失败计数是否清零。首代 sidecar 由装配方先 spawn——Supervisor
        // 启动时它已在跑,取当前时刻为近似出生点（偏差只会让清零更保守）。
        let mut spawned_at = tokio::time::Instant::now();
        loop {
            // 1. init 序列（wx.init + 监听 resync + 状态推进 + status 事件）
            *self.phase.write().await = "init_sequence";
            self.init_sequence().await;

            // 2. 等待当前 sidecar 退出（Booting/WxInit/Ready/Degraded 任一态下都可能发生）
            *self.phase.write().await = "await_exit";
            let mut exit_rx = self.session.sidecar_exit_watcher().await;
            let uptime = tokio::time::Instant::now() - spawned_at;
            let lived_long_enough = uptime >= SIDECAR_MIN_UPTIME;
            match SidecarHandle::await_exit(&mut exit_rx).await {
                Some(status) => tracing::warn!(?status, failures, ?uptime, "sidecar 退出，准备退避重启"),
                None => {
                    // 观察通道意外关闭（理论上 reaper 不死；防御性退出避免忙转）
                    tracing::error!("sidecar 退出观察通道关闭，Supervisor 停止");
                    self.state.mark_sidecar_died().await;
                    return;
                }
            }

            // 3. 退避 → 重启。失败计数：sidecar 没活够久（立即死——Windows
            //    9009 找不到命令/杀软删 exe）不清零,让退避逐级升到
            //    SidecarDead 终态;活够久才清零（正常偶发崩溃重新计数）。
            *self.phase.write().await = "backoff";
            failures += 1;
            match self.backoff_table.get(failures.wrapping_sub(1)).copied() {
                Some(wait) => {
                    tracing::warn!(failures, ?wait, "退避后重启 sidecar");
                    tokio::time::sleep(wait).await;
                }
                None => {
                    // 退避表耗尽：SidecarDead 终态（人工介入），循环退出
                    tracing::error!(
                        failures,
                        "sidecar 重启 {} 次仍失败，进入 SidecarDead（不再自动重启）",
                        self.backoff_table.len()
                    );
                    self.state.mark_sidecar_died().await;
                    return;
                }
            }

            *self.phase.write().await = "spawn";
            match (self.spawner)().await {
                Ok(new_handle) => {
                    self.session.replace_sidecar(new_handle).await;
                    spawned_at = tokio::time::Instant::now();
                    if lived_long_enough {
                        failures = 0; // 活够久的偶发崩溃：重新计数
                    }
                }
                Err(e) => {
                    tracing::error!(%e, failures, "sidecar 重启 spawn 失败");
                    // spawn 失败与崩溃同罪：计数已 +1（已计入本轮退避）。
                    // 注意 init_sequence 不改状态（对死句柄 init 报错→留在原态），
                    // 窗口期若先前已达 Ready/Degraded 则停留原态，由心跳泵的
                    // mark_online(false)（closed 快速失败→false）降级兜底；
                    // 随后 await_exit 因句柄已死立即返回，快速进入下一轮退避
                }
            }
        }
    }

    /// init 序列：wx.init（licensed 且无 failReason → WxInit）→ resync 监听 → Ready + status 事件。
    /// 未授权 / 微信未开 / init 报错：留在 Booting；Ok 且带原因时发 init_fail 帧
    /// 引导前端（RPC 失败不发——那是 sidecar 死亡，归 Supervisor 重启域）。
    async fn init_sequence(&self) {
        let init = self.direct_wx_init().await.ok();
        let licensed = init
            .as_ref()
            .and_then(|v| v["licensed"].as_bool())
            .unwrap_or(false);
        let fail_reason = init
            .as_ref()
            .and_then(|v| v["failReason"].as_str())
            .map(str::to_string);
        if !licensed || fail_reason.is_some() {
            // 未授权缺 failReason（旧 sidecar）也归一为 licensed——引导口径一致
            let reason = fail_reason.unwrap_or_else(|| "licensed".to_string());
            tracing::warn!(%reason, "wx.init 未就绪，保持 Booting（等待授权/微信引导）");
            if init.is_some() {
                self.emit(serde_json::json!({
                    "kind": "event", "type": "init_fail",
                    "data": {"reason": reason},
                }))
                .await;
            }
            return;
        }
        self.state.mark_wx_init(true).await;

        // 监听 resync：sidecar 可能刚重启，本地名单重放
        let failed = self.listeners.resync().await;
        if !failed.is_empty() {
            tracing::warn!(?failed, "部分监听重注册失败");
        }
        self.state.mark_ready().await;

        // status 事件（wxOnline 现场探测 + 监听数 + sidecar 存活）
        let online = self.session.health_probe().await;
        let n = self.listeners.list().await.len();
        self.emit(crate::agent_link::inbound::status_event(online, n, true))
            .await;
    }

    /// 手动重跑 init 序列（激活成功后 activate_license 命令 / 前端「重新初始化」
    /// 按钮入口）。与 run 循环内 init_sequence 同一段逻辑，幂等可重入；
    /// 不借道崩溃重启循环——init 失败时 sidecar 进程还活着。
    pub async fn retry_init(&self) {
        self.init_sequence().await;
    }

    /// 直连 sidecar 调 wx.init（绕过 session 串行队列与 16-action 白名单——
    /// init 是编排层动作而非 WS action；仍复用 sidecar 的 RPC 协议实现）。
    /// 超时收紧到 10s：默认 30s 是给慢 UIA 操作的，init 若 10s 内无响应
    /// 说明 sidecar 已死/卡死，挂满 30s 只会拖慢 Supervisor 重启节奏
    /// （sidecar 死后 closed 标志快速失败，此处兜底「活着但不回」的病态）。
    async fn direct_wx_init(&self) -> Result<Value, RpcError> {
        self.session
            .direct_call_with_timeout(methods::INIT, json!({}), INIT_TIMEOUT)
            .await
    }

    /// 事件出口：event_sink（若有）。WS 侧 status 上报由 AgentLink 心跳承担。
    async fn emit(&self, frame: Value) {
        if let Some(sink) = self.event_sink.as_ref() {
            sink(frame);
        }
    }
}

/// 生产默认 spawner：`SidecarHandle::spawn_default()`（env WXAUTO_SIDECAR_CMD 可覆盖）
fn default_spawner() -> SidecarSpawner {
    Arc::new(|| Box::pin(SidecarHandle::spawn_default()))
}
