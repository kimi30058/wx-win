//! GUI 全局状态聚合（Task 9）：tauri manage 的 AppStateCtx。
//!
//! 聚合 core 全部运行件 + GUI 装配件：
//! - config：RwLock<Config>（save_config 按字段合并，不清空 listenNames 等非提交字段）
//! - listeners / session：与 CLI 共用的 core 件
//! - state_machine：六态机（get_app_state 数据源）
//! - sidecar：首代句柄（退出回收用；Supervisor 管 spawn 后的重启）
//! - supervisor：崩溃退避重启 + init 序列
//! - link：当前 AgentLink（connect 重建 / disconnect halt；RwLock 支持换新）
//! - bridge：UiEventBridge（事件出口）
//! - url 组装所需 keyring 读取（connect 时取 token）
//!
//! 铁律：运行时路径禁 unwrap；keyring 同步 IO 一律 spawn_blocking 包裹
//! （Task 6 审查 M3 遗留要求——keyring 走 dbus/凭据管理器是阻塞 IO，
//! 直接在 async 上下文调用会卡 runtime worker）。

use std::sync::Arc;

use serde_json::Value;

#[cfg(test)]
use serde_json::json;
use tokio::sync::{Mutex, RwLock};

use wxauto_desktop::agent_link::AgentLink;
use wxauto_desktop::cli::build_ws_url;
use wxauto_desktop::config::{self, Config};
use wxauto_desktop::outbox::Outbox;
use wxauto_desktop::sidecar::SidecarHandle;
use wxauto_desktop::state::{AppStateMachine, Supervisor};
use wxauto_desktop::wx::listener::ListenerRegistry;
use wxauto_desktop::wx::WxSession;

use crate::ui_events::UiEventBridge;

/// GUI 侧保存设置的三字段载荷（前端 Settings.vue 提交口径）
pub struct SettingsPatch {
    pub server_url: String,
    pub channel_id: String,
    pub auto_connect: bool,
    pub webhook_url: String,
    pub webhook_template: String,
}

/// 从前端提交的 config JSON 按字段提取（Task 8 审查硬性输入 #3：
/// 勿整体反序列化 Config——前端只提交三个字段，整体反序列化会把
/// listenNames/delayMinMs/delayMaxMs 置默认值清空存量）。
pub fn extract_settings_patch(config: &Value) -> SettingsPatch {
    SettingsPatch {
        server_url: config["serverUrl"].as_str().unwrap_or_default().to_string(),
        channel_id: config["channelId"].as_str().unwrap_or_default().to_string(),
        auto_connect: config["autoConnect"].as_bool().unwrap_or(false),
        webhook_url: config["webhookUrl"].as_str().unwrap_or_default().to_string(),
        webhook_template: config["webhookTemplate"].as_str().unwrap_or_default().to_string(),
    }
}

/// keyring 写 token（同步阻塞 IO → spawn_blocking 包裹；
/// Task 6 审查 M3 遗留要求）。None = 前端未提交 token（保留已存值）。
async fn keyring_set_token_async(token: String) -> Result<(), String> {
    tokio::task::spawn_blocking(move || config::keyring_set_token(&token))
        .await
        .map_err(|e| format!("keyring 任务 Join 失败: {e}"))?
}

/// tauri manage 的全局状态（main.rs 构造一次，commands 经 State<'_, AppStateCtx> 取）。
///
/// 装配两段式（消除 setup 里 spawn 的 manage 时序竞态——前端 webview 加载
/// 可能先于 async 装配完成发起 invoke，报「state not managed」）：
/// 1. `shell()`：同步构造（setup 里立即 manage——invoke 从此永远找得到状态）
/// 2. `assemble_into()`：async 装配（spawn sidecar 等）回填 OnceCell；
///    commands 经 `ready().await` 等装配完成（或拿到装配错误）。
pub struct AppStateCtx {
    /// 装配结果槽：None=装配中；Some(Err)=装配失败（前端 invoke 报原因）
    inner: std::sync::Arc<tokio::sync::OnceCell<Result<Arc<Assembled>, String>>>,
    /// 事件桥（装配前即可用——shell 阶段注入 AppHandle，先到的事件不丢）
    pub bridge: UiEventBridge,
    /// 运行日志环形缓冲（「运行日志」tab 数据源：tracing Layer + sidecar
    /// stderr sink 双入口写；get_recent_logs/clear_logs 命令读写）
    pub log_ring: crate::ui_log::LogRing,
    /// 实时日志通道（I-1：sidecar stderr 行也走 mpsc → 桥 emit
    /// `wxauto://app-log`；None = CLI/测试装配，只落 ring）
    pub log_tx: Option<tokio::sync::mpsc::Sender<crate::ui_log::AppLogEntry>>,
    /// 配置文件路径（装配读 + 保存写共用）
    config_path: std::path::PathBuf,
}

/// Clone 是浅克隆：三个槽位全 Arc/PathBuf 快照，克隆共享同一 OnceCell 与
/// bridge（setup 的 manage 一份、装配任务持一份——C1 修复后 manage 裸值，
/// 装配任务不能 move 走它，只能持克隆）。
impl Clone for AppStateCtx {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            bridge: self.bridge.clone(),
            log_ring: self.log_ring.clone(),
            log_tx: self.log_tx.clone(),
            config_path: self.config_path.clone(),
        }
    }
}

/// 当前 AgentLink 槽（connect 写入 / disconnect 清空）。
/// std 同步 RwLock：sink 闭包（同步上下文）也要读它取 ws_connected 快照
/// （I2：Supervisor 的 status 帧同样要注入 wsConnected——读锁持有纳秒级，
/// 无 await 点，不违反 tokio 锁纪律）。
pub type LinkSlot = Arc<std::sync::RwLock<Option<Arc<AgentLink>>>>;

/// LinkSlot 读守卫中毒恢复（必修 3）：panic 不级联——数据结构本身完好
fn slot_read(slot: &LinkSlot) -> std::sync::RwLockReadGuard<'_, Option<Arc<AgentLink>>> {
    slot.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// LinkSlot 写守卫中毒恢复（同上）
fn slot_write(slot: &LinkSlot) -> std::sync::RwLockWriteGuard<'_, Option<Arc<AgentLink>>> {
    slot.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 装配完成后的运行件集合（AppStateCtx.inner 的 Ok 载荷）
pub struct Assembled {
    /// 当前配置（GUI 保存按字段合并；connect 时读 serverUrl）。
    /// 共享锁（Task 3）：persist hook 与 save_settings 经同一把内存锁
    /// 串行化——监听名单落盘与设置保存共享同一份内存真相，任一写入
    /// 路径都不会用旧快照覆盖对方（spec §4.1 不变量）
    pub config: Arc<RwLock<Config>>,
    /// 监听名单注册表（Listen 页 CRUD 入口）
    pub listeners: Arc<ListenerRegistry>,
    /// 微信会话（manual_execute 与 agent 指令共用串行队列）
    pub session: Arc<WxSession>,
    /// 六态机（get_app_state 数据源；状态变化经 bridge emit wxauto://state）
    pub state_machine: Arc<AppStateMachine>,
    /// 首代 sidecar 句柄（GUI 退出时 graceful 回收）
    pub sidecar: Arc<Mutex<SidecarHandle>>,
    /// Supervisor（sidecar 崩溃退避重启 + init 序列；run 任务在 main spawn）
    pub supervisor: Arc<Supervisor>,
    /// Supervisor run 任务句柄（GUI 退出先 abort——I3：防退出回收窗口内
    /// 按退避表再拉起孤儿 sidecar，对齐 CLI 的 supervisor_task.abort()）
    pub supervisor_task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// 当前 AgentLink（connect 重建换新 / disconnect halt）
    pub link: LinkSlot,
    /// webhook 告警客户端（SidecarDead/掉线两挂点共用）。Arc<RwLock<..>>
    /// （LinkSlot 同 idiom）：外层 Arc 供状态机 on_change 闭包与结构体共享
    /// 同一把锁；内层 RwLock 是热更新槽——save_settings 合并配置后重建注入，
    /// 每次告警读当前值，用户改 webhookUrl 保存后即刻生效（Task 7 review
    /// 硬性输入：装配时固化 cfg 快照会让告警永远发旧地址）。
    pub alert: Arc<RwLock<Arc<wxauto_desktop::alert::AlertClient>>>,
    /// 连接管理互斥（必修 4）：start_link/stop_link 全程持锁串行化——
    /// 并发双 connect 时旧「先写者」link 从未被 halt（幽灵重连+心跳假亮）。
    /// 槽锁只保护单次读写，管理操作（halt 旧+建新+写槽）须整体原子。
    link_ops: tokio::sync::Mutex<()>,
    /// 前端事件桥（start_link 注 sink 用）
    pub bridge: UiEventBridge,
    /// 事件发件箱（spec §4.2：事件先落盘后发送；ack 清除 + 重连补发）。
    /// start_link 注入 link——GUI 每次 connect 重建 link 都重新接线
    pub outbox: Arc<Outbox>,
}

impl Assembled {
    /// 测试构造：剧本 sidecar（python 脚本回固定形状）+ 空配 URL，
    /// 免走 keyring/env——专测 link 管理逻辑（必修 4/5 回归）。
    #[cfg(test)]
    pub async fn for_test(url: String) -> Result<Arc<Assembled>, String> {
        let script = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    m = req["method"]
    if m == "wx.is_online":
        out = {"online": True}
    elif m == "wx.get_my_info":
        out = {"licensed": True, "wxid": "wxid_t", "nickname": "测试", "online": True}
    else:
        out = {"ok": True}
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": out}), flush=True)
"#;
        let handle = wxauto_desktop::sidecar::SidecarHandle::spawn_with_python(script, &[])
            .await
            .map_err(|e| format!("测试 sidecar 启动失败: {e}"))?;
        let sidecar = Arc::new(Mutex::new(handle));
        let session = Arc::new(WxSession::new(sidecar.clone()));
        let listeners = Arc::new(ListenerRegistry::new(session.clone()));
        let state_machine = Arc::new(AppStateMachine::new());
        let supervisor = Arc::new(Supervisor::new(
            state_machine.clone(),
            session.clone(),
            listeners.clone(),
        ));
        let link: LinkSlot = Arc::new(std::sync::RwLock::new(None));
        let alert: Arc<RwLock<Arc<wxauto_desktop::alert::AlertClient>>> = Arc::new(RwLock::new(
            Arc::new(wxauto_desktop::alert::AlertClient::new(
                String::new(),
                String::new(),
                wxauto_desktop::alert::hostname(),
            )),
        ));
        Ok(Arc::new(Assembled {
            config: Arc::new(RwLock::new(Config {
                server_url: url,
                ..Config::default()
            })),
            listeners,
            session,
            state_machine,
            sidecar,
            supervisor,
            supervisor_task: std::sync::Mutex::new(None),
            link,
            alert,
            link_ops: tokio::sync::Mutex::new(()),
            bridge: UiEventBridge::new(),
            // 测试装配不落盘——outbox 全 no-op（Disabled 形态）
            outbox: Arc::new(Outbox::disabled()),
        }))
    }

    /// 组装 WS URL：serverUrl（env 覆盖 > 配置）+ keyring token（env 覆盖）。
    /// keyring 读取走 spawn_blocking（同步 dbus IO）。
    /// 读失败/无 token 必留痕（warn）——静默空 token 连接会恒 4001，
    /// 排障时日志里必须能看到「为什么没带 token」（生产 4001 事故教训）。
    pub async fn build_url(&self) -> String {
        let cfg_server = self.config.read().await.server_url.clone();
        let server = std::env::var("WXAUTO_SERVER_URL").unwrap_or(cfg_server);
        let token = match std::env::var("WXAUTO_DEVICE_TOKEN") {
            Ok(t) => {
                tracing::warn!("token 来自环境变量 WXAUTO_DEVICE_TOKEN（覆盖 keyring）");
                t
            }
            Err(_) => match tokio::task::spawn_blocking(config::keyring_get_token).await {
                Ok(Ok(t)) if !t.is_empty() => t,
                Ok(Ok(_)) => {
                    tracing::warn!("keyring 中 token 为空——将以无鉴权方式连接（预期服务端 4001，请先在设置页填入设备 token）");
                    String::new()
                }
                Ok(Err(e)) => {
                    tracing::warn!("keyring 读取 token 失败，将以无鉴权方式连接（预期 4001）: {e}");
                    String::new()
                }
                Err(e) => {
                    tracing::warn!("keyring 任务 Join 失败，将以无鉴权方式连接（预期 4001）: {e}");
                    String::new()
                }
            },
        };
        // 组装结果留痕（token 遮罩——日志会被截图分享，不得泄漏可用整串；
        // 头 12 字符足以辨认「token= 前缀污染/空值/截断」等形状问题）。
        // 关键值并入 message 正文：GUI 运行日志只渲染 message 字段
        let masked = wxauto_desktop::cli::mask_token(&token);
        tracing::info!("WS 连接参数组装: server={server} token={masked}");
        build_ws_url(&server, &token)
    }

    /// connect：组装 URL → 新建 AgentLink（注入双 sink）→ 换新 → run。
    /// 旧 link（若有）先 halt（幂等；halt 后的实例不再复用）。
    /// 全程持 link_ops 锁（必修 4）：与并发 stop_link/另一 start_link 串行化，
    /// 消除「先写者 link 未 halt」的孤儿窗口。
    pub async fn start_link(&self) -> Result<(), String> {
        let _ops = self.link_ops.lock().await;
        let old = slot_read(&self.link).clone();
        if let Some(old) = old {
            old.halt();
        }
        let url = self.build_url().await;
        let link = Arc::new(AgentLink::new_with_event_sink(
            self.session.clone(),
            self.listeners.clone(),
            url,
            None,
        ));
        // P2 任务 8c：hello 帧带 channelId（配置快照注入；重连复用同值）
        link.set_channel_id(self.config.read().await.channel_id.clone());
        // outbox 注入（spec §4.2：事件先落盘后发送 + ack 清除 + 重连补发；
        // 每次 connect 重建 link 都重新接线——发件箱本体跨连接复用）
        link.set_outbox(self.outbox.clone());
        // 双 sink：event 帧（status 组装 wsConnected）+ 指令日志
        {
            let bridge = self.bridge.clone();
            let link_for_sink = link.clone();
            link.set_event_sink(Box::new(move |frame: Value| {
                let bridge = bridge.clone();
                let frame = with_ws_connected_snapshot(&frame, &link_for_sink);
                tokio::spawn(async move {
                    bridge.forward_event_frame(&frame).await;
                });
            }));
        }
        {
            let bridge = self.bridge.clone();
            link.set_command_sink(Box::new(move |entry: Value| {
                let bridge = bridge.clone();
                tokio::spawn(async move {
                    bridge.forward_command_log(&entry).await;
                });
            }));
        }
        // webhook 告警（心跳掉线/恢复挂点）：与状态机挂点同源——每次连接
        // 从 Assembled.alert 读当前客户端（配置热更新后重连即生效）
        link.set_alert_sink(self.alert.read().await.clone());
        *slot_write(&self.link) = Some(link.clone());
        tokio::spawn({
            let l = link.clone();
            async move { l.run().await }
        });
        tracing::info!("AgentLink 已启动（GUI connect）");
        Ok(())
    }

    /// disconnect：halt 当前 link（transport 循环 + 三泵协作退出）。
    /// 持 link_ops 锁（必修 4）：与并发 start_link 串行化。
    /// 主动断开不经 on_disconnect 回调（halt 直接退出 transport run），
    /// 桥侧补发一帧 wsConnected=false 的 status（必修 5：前端「服务器连接」
    /// 灯不得停在亮态——与 on_disconnect 补发口径一致）。
    pub async fn stop_link(&self) -> Result<(), String> {
        let _ops = self.link_ops.lock().await;
        let current = slot_read(&self.link).clone();
        match current {
            Some(link) => {
                link.halt();
                *slot_write(&self.link) = None;
                tracing::info!("AgentLink 已停止（GUI disconnect）");
                // ws_connected 已由 halt 复位 false——现场探测组装必得 false 帧Payload；
                // wxOnline 走 sidecar 现场探测（微信本身可能仍在线，两灯独立）
                let online = self.session.health_probe().await;
                let listeners = self.listeners.list().await.len();
                let status = crate::ui_events::with_ws_connected(
                    &wxauto_desktop::agent_link::inbound::status_event(online, listeners, true),
                    link.ws_connected(),
                );
                self.bridge.forward_event_frame(&status).await;
                Ok(())
            }
            None => Err("当前未连接".to_string()),
        }
    }

    /// 六态快照名（get_app_state 命令数据源；同步读——低开销查询）
    pub fn state_snapshot(&self) -> String {
        crate::ui_events::app_state_name(&self.state_machine.state_sync()).to_string()
    }

    /// 设置热重载（生产 4001 恒失败根因修复）：WsTransport 构造时固化
    /// URL，重连循环永不重读 keyring/config——保存设置（token/serverUrl）
    /// 后若不重建 link，设备永远拿旧 URL 重试。此处 link 运行中时以
    /// 新配置重建（复用 start_link：halt 旧 + build_url 重读 + 换新）。
    /// 槽空（未连接）= no-op 返回 false——尊重用户未连接的显式状态。
    pub async fn reload_link_if_running(&self) -> Result<bool, String> {
        if slot_read(&self.link).is_none() {
            return Ok(false);
        }
        self.start_link().await?;
        Ok(true)
    }
}

impl AppStateCtx {
    /// 同步壳构造（setup 里 manage——invoke 时刻必有状态可寻）
    pub fn shell(
        config_path: std::path::PathBuf,
        bridge: UiEventBridge,
        log_ring: crate::ui_log::LogRing,
        log_tx: Option<tokio::sync::mpsc::Sender<crate::ui_log::AppLogEntry>>,
    ) -> Self {
        Self {
            inner: std::sync::Arc::new(tokio::sync::OnceCell::new()),
            bridge,
            log_ring,
            log_tx,
            config_path,
        }
    }

    /// async 装配回填（setup spawn 的任务里调用；重复调用返回首次结果）。
    /// 构造副作用 = spawn 首代 sidecar + 组装全部运行件。
    pub async fn assemble_into(&self) -> Result<Arc<Assembled>, String> {
        self.inner
            .get_or_init(|| async {
                let bridge = self.bridge.clone();
                let config_path = self.config_path.clone();
                let log_ring = self.log_ring.clone();
                let cfg =
                    config::load_config(&config_path).map_err(|e| format!("配置读取失败: {e}"))?;

                // 1. 首代 sidecar（失败即装配失败——GUI 起不来要有明确报错）。
                //    stderr 注入 ring sink：sidecar 日志进「运行日志」tab
                //    （spec §4 source=sidecar；CLI 不走本装配，零回归）。
                //    I-1：同时接 mpsc——前端实时流只消费 mpsc 转发出的
                //    `wxauto://app-log` 事件，只落 ring 时日志 tab 看不到
                //    sidecar 行。必须走 spawn_default_sunk（含
                //    CREATE_NO_WINDOW 防闪窗，Task 3 审查者提示：勿退回
                //    spawn_with_python_sunk）
                let log_tx = self.log_tx.clone();
                let stderr_sink: std::sync::Arc<dyn wxauto_desktop::sidecar::StderrSink> =
                    std::sync::Arc::new(crate::ui_log::RingStderrSink::new(log_ring, log_tx));
                let sidecar = SidecarHandle::spawn_default_sunk(Some(stderr_sink.clone()))
                    .await
                    .map_err(|e| format!("sidecar 启动失败: {e}"))?;
                let sidecar = Arc::new(Mutex::new(sidecar));
                // 2. session + listeners（P2 任务 8b：delayMinMs/MaxMs 接线——
                // 配置的拟人间隙经 sanitize 后生效，非法值回退默认 500~1000ms）。
                // 共享配置锁（Task 3）：persist hook 与 save_settings 经同一把锁
                // 串行化（spec §4.1 不变量）——listeners 种子恢复 + hook 注入
                // 都走 cfg_lock，Assembled.config 即 cfg_lock 本体
                let session = Arc::new(WxSession::with_gaps(
                    sidecar.clone(),
                    cfg.delay_min_ms,
                    cfg.delay_max_ms,
                ));
                let cfg_lock = Arc::new(RwLock::new(cfg));
                let listeners = Arc::new(ListenerRegistry::new_seeded(
                    session.clone(),
                    cfg_lock.read().await.listen_names.clone(),
                ));
                listeners
                    .set_persist_hook(wxauto_desktop::wx::listener::file_persist_hook(
                        cfg_lock.clone(),
                        config_path.clone(),
                    ))
                    .await;
                // 3'. webhook 告警客户端（配置驱动；空 URL = 禁用 no-op）。
                //     RwLock 包裹供 save_settings 热更新（见 Assembled.alert 注释）。
                //     webhook 字段从 cfg_lock 现读（cfg 已 move 进锁——P0 共享锁改造）
                let (webhook_url, webhook_template) = {
                    let g = cfg_lock.read().await;
                    (g.webhook_url.clone(), g.webhook_template.clone())
                };
                let alert = Arc::new(RwLock::new(Arc::new(
                    wxauto_desktop::alert::AlertClient::new(
                        webhook_url,
                        webhook_template,
                        wxauto_desktop::alert::hostname(),
                    ),
                )));
                // outbox 装配（spec §4.2）：事件先落盘后发送。打开失败降级
                // disabled（不阻断启动——发件箱故障只损失补发能力，
                // 不损失实时上报）。路径与 config.json 同目录
                let outbox = match Outbox::open(&config_path.with_file_name("outbox.jsonl")) {
                    Ok(ob) => Arc::new(ob),
                    Err(e) => {
                        tracing::error!("outbox 打开失败，事件不落盘（不阻断启动）: {e}");
                        Arc::new(Outbox::disabled())
                    }
                };
                // 3. 状态机（注入 on_change：六态变化 → wxauto://state。
                //    bridge 已在 shell 阶段 attach——首事件不丢，问题 B 消除。
                //    alert 捕获进闭包：SidecarDead 终态发 webhook——告警时刻
                //    从 RwLock 读当前客户端，配置热更新即刻生效）
                let state_machine = {
                    let bridge = bridge.clone();
                    let alert = alert.clone();
                    Arc::new(
                        AppStateMachine::new()
                            .with_on_change(Box::new(move |_old, new| {
                                let bridge = bridge.clone();
                                let alert = alert.clone();
                                let new = new.clone();
                                tokio::spawn(async move {
                                    bridge.emit_state(&new).await;
                                    if new == wxauto_desktop::state::AppState::SidecarDead {
                                        let client = alert.read().await.clone();
                                        client.send(
                                            "sidecar 瘫痪",
                                            "退避重启耗尽进入终态——设备需人工介入（重启 App 或检查微信环境）",
                                        );
                                    }
                                });
                            }))
                            .await,
                    )
                };
                // 4. Supervisor（event_sink：status 帧 → 注入 ws 快照 → 桥转发。
                //    I2：Supervisor 的 status 帧无 wsConnected 字段，直接转发
                //    前端 WS 灯恒灰——经 link 槽读快照注入，与 AgentLink sink 同口径。
                //    I-A（终审）：注入带 sink 的 spawner——重启代 sidecar 的 stderr
                //    也接管进同一 ring（spec §5「多代 sidecar 各代读循环写同一
                //    ring」）。若保持 default_spawner，新代 stderr 回 inherit，
                //    release GUI 无控制台 = 崩溃代日志彻底丢失——恰是最需要
                //    看日志的场景。sink 在闭包外构造一次、克隆捕获，与首代
                //    共用同一个 RingStderrSink；CLI（main.rs）不注入，零回归）
                let link: LinkSlot = Arc::new(std::sync::RwLock::new(None));
                let supervisor = {
                    let bridge = bridge.clone();
                    let link_slot = link.clone();
                    let spawner_sink = stderr_sink.clone();
                    let spawner: wxauto_desktop::state::SidecarSpawner = Arc::new(move || {
                        let sink = spawner_sink.clone();
                        Box::pin(async move { SidecarHandle::spawn_default_sunk(Some(sink)).await })
                    });
                    let sup =
                        Supervisor::new(state_machine.clone(), session.clone(), listeners.clone())
                            .with_spawner(spawner)
                            .with_event_sink(Box::new(move |frame: Value| {
                                let bridge = bridge.clone();
                                // ws 快照在 sink（同步上下文）就地读取注入
                                let frame = with_ws_connected_from_slot(&frame, &link_slot);
                                tokio::spawn(async move {
                                    bridge.forward_event_frame(&frame).await;
                                });
                            }));
                    Arc::new(sup)
                };

                Ok(Arc::new(Assembled {
                    config: cfg_lock,
                    listeners,
                    session,
                    state_machine,
                    sidecar,
                    supervisor,
                    supervisor_task: std::sync::Mutex::new(None),
                    link,
                    alert,
                    link_ops: tokio::sync::Mutex::new(()),
                    bridge,
                    outbox,
                }))
            })
            .await
            .clone()
    }

    /// 等装配完成（commands 入口统一经此取运行件）。
    /// 装配失败返回 Err（前端 invoke reject 拿到原因）。
    pub async fn ready(&self) -> Result<Arc<Assembled>, String> {
        match self.inner.get() {
            Some(r) => r.clone(),
            None => self.assemble_into().await,
        }
    }

    /// 已装配时取运行件（退出回收路径：同步上下文，未装配返回 None）
    pub fn try_assembled(&self) -> Option<Arc<Assembled>> {
        self.inner.get().and_then(|r| r.clone().ok())
    }

    /// 保存设置：按字段合并 → 落盘（token 非空先写 keyring）→ link 运行中
    /// 则热重载（WsTransport URL 构造时固化，不重建则新 token 永不生效——
    /// 生产 4001 恒失败根因）。合并而非替换——listenNames/delayMinMs/delayMaxMs
    /// 保持存量。热重载失败不让保存整体报错（配置已落盘，重连靠用户手点/
    /// 下次启动），只留错误日志。
    pub async fn save_settings(
        &self,
        patch: SettingsPatch,
        token: Option<String>,
    ) -> Result<(), String> {
        let assembled = self.ready().await?;
        if let Some(tok) = token {
            if !tok.is_empty() {
                // 保存详情留痕（遮罩同 build_url）：排障时对齐「用户到底存了什么」
                tracing::info!(
                    "保存设置：写入设备 token 到 keyring: {}",
                    wxauto_desktop::cli::mask_token(&tok)
                );
                keyring_set_token_async(tok).await?;
            }
        }
        {
            let mut cfg = assembled.config.write().await;
            cfg.server_url = patch.server_url;
            cfg.channel_id = patch.channel_id;
            cfg.auto_connect = patch.auto_connect;
            cfg.webhook_url = patch.webhook_url;
            cfg.webhook_template = patch.webhook_template;
            config::save_config(&self.config_path, &cfg)?;
            // webhook 热更新（Task 7 review 硬性输入）：合并后以新配置重建
            // alert 客户端——状态机闭包与心跳挂点在告警时刻读 RwLock 当前值，
            // 用户改 webhookUrl 保存后下一次告警即发新地址（不重建则固化
            // 装配时快照，告警永远发旧地址）。在 cfg 写锁内重建：读 cfg 字段
            // 与写 alert 槽同临界区，杜绝「读到半新半旧配置」的交错。
            let new_alert = Arc::new(wxauto_desktop::alert::AlertClient::new(
                cfg.webhook_url.clone(),
                cfg.webhook_template.clone(),
                wxauto_desktop::alert::hostname(),
            ));
            *assembled.alert.write().await = new_alert;
        }
        // 落盘成功后热重载（原配置读写锁在 start_link 的 build_url 里还要读，
        // 先释放写锁防死锁）
        if let Err(e) = assembled.reload_link_if_running().await {
            tracing::error!("设置已保存但热重载连接失败（下次连接/重启生效）: {e}");
        }
        Ok(())
    }
}

/// event_sink 转发前的 status 帧组装：wsConnected 取 link 现场快照注入帧
/// （core 帧无此字段，Task 8 审查硬性输入 #1）。
fn with_ws_connected_snapshot(frame: &Value, link: &Arc<AgentLink>) -> Value {
    if frame["type"] == "status" {
        crate::ui_events::with_ws_connected(frame, link.ws_connected())
    } else {
        frame.clone()
    }
}

/// Supervisor sink 侧的同口径注入（I2）：从 link 槽取当前 AgentLink 的
/// ws_connected 快照（槽空 = 未连接 → false）。读锁纳秒级、无 await 点。
fn with_ws_connected_from_slot(frame: &Value, slot: &LinkSlot) -> Value {
    if frame["type"] == "status" {
        let connected = slot_read(slot)
            .as_ref()
            .map(|l| l.ws_connected())
            .unwrap_or(false);
        crate::ui_events::with_ws_connected(frame, connected)
    } else {
        frame.clone()
    }
}

/// get_config 命令体：Config 序列化（token 不在其中——keyring 侧只写
/// 不读给前端；delayMinMs 等面板暂无 UI 但保留透出）
pub fn config_to_json(cfg: &Config) -> Result<Value, String> {
    serde_json::to_value(cfg).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按字段提取：三个提交字段全取；缺字段兜底（空串/false）
    #[test]
    fn test_extract_settings_patch_fields() {
        let v = json!({
            "serverUrl": "ws://x:1", "channelId": "ch1", "autoConnect": true,
            "token": "secret", "多余字段": "忽略"
        });
        let p = extract_settings_patch(&v);
        assert_eq!(p.server_url, "ws://x:1");
        assert_eq!(p.channel_id, "ch1");
        assert!(p.auto_connect);

        // 缺字段兜底（前端漏传不 panic）
        let empty = extract_settings_patch(&json!({}));
        assert_eq!(empty.server_url, "");
        assert!(!empty.auto_connect);

        // webhook 两字段提取（2026-09-10 P1）
        let patch3 = extract_settings_patch(&serde_json::json!({
            "serverUrl": "s", "channelId": "c", "autoConnect": true,
            "webhookUrl": "https://open.feishu.cn/hook/x", "webhookTemplate": "{\"text\":\"{title}\"}"
        }));
        assert_eq!(patch3.webhook_url, "https://open.feishu.cn/hook/x");
        assert_eq!(patch3.webhook_template, "{\"text\":\"{title}\"}");
        // 缺字段兜底空串（不 panic）
        let patch4 = extract_settings_patch(&serde_json::json!({
            "serverUrl": "s", "channelId": "c"
        }));
        assert_eq!(patch4.webhook_url, "");
    }

    /// config 序列化：camelCase 字段名（前端 parseAppConfig 守卫对齐）
    #[test]
    fn test_config_to_json_camel_case() {
        let v = config_to_json(&Config::default()).expect("序列化失败");
        assert!(v["serverUrl"].is_string());
        assert!(v["channelId"].is_string());
        assert!(v["autoConnect"].is_boolean());
        assert!(v["listenNames"].is_array(), "listenNames 保留透出");
    }

    /// I2 防回归：Supervisor 路径的 status 帧经 link 槽注入 wsConnected
    /// （槽空 = 未连接 → false；修复前恒 false 且不论连接态——sidecar 重启
    /// 后前端 WS 灯误红最长 30s）。非 status 帧不动。
    #[test]
    fn test_with_ws_connected_from_slot() {
        let slot: LinkSlot = Arc::new(std::sync::RwLock::new(None));
        let status = json!({
            "kind": "event", "type": "status",
            "data": { "wxOnline": true, "listeners": 1, "sidecarAlive": true }
        });
        // 槽空（未连接）→ wsConnected=false（而非缺字段）
        let out = with_ws_connected_from_slot(&status, &slot);
        assert_eq!(out["wsConnected"], false, "未连接时应显式 false");
        // 非 status 帧透传不动
        let msg = json!({ "kind": "event", "type": "message", "data": {} });
        let out2 = with_ws_connected_from_slot(&msg, &slot);
        assert!(
            out2.get("wsConnected").is_none(),
            "非 status 帧不应注入字段"
        );
    }

    /// 必修 4 回归：并发 start_link 竞争——link_ops 串行化后，
    /// 后入者必 halt 先写者：终态槽内只有一个 link，且它未被 halt
    /// （败者 halt 标志 true、胜者 false）。修复前双 start_link 交错
    /// （A 读槽空 → B 读槽空 → A 写槽 → B 写槽）时 A 从未被 halt——
    /// A 的 transport 幽灵重连 + 心跳泵假亮。
    #[tokio::test]
    async fn test_concurrent_start_link_no_orphan() {
        // 不可达地址即可：transport 连接失败走 on_disconnect 退避循环，
        // link 管理不变量的断言不依赖连接成功
        let a = Assembled::for_test("ws://127.0.0.1:1".into())
            .await
            .expect("测试装配失败");

        // 并发双 connect（同一 Assembled 实例上竞争——生产 invoke 同源）
        let (r1, r2) = tokio::join!(a.start_link(), a.start_link());
        assert!(r1.is_ok() && r2.is_ok(), "串行化后两次 connect 均应成功");

        // 终态不变量：两次串行化的 start_link 后，槽内恰一个 link 且
        // halted=false——先入者被后入者 halt（双写交错则无人 halt 旧者，
        // 或终态 link 被误 halt，均能被本断言抓到）。
        {
            let guard = slot_read(&a.link);
            let link = guard.as_ref().expect("connect 后槽内必有 link");
            assert!(
                !link.is_halted_for_test(),
                "终态 link 不应处于 halted（胜者被误 halt 即回归）"
            );
        }

        // stop 后槽清空（管理不变量）
        a.stop_link().await.expect("disconnect 应成功");
        assert!(
            slot_read(&a.link).is_none(),
            "disconnect 后槽应清空（孤儿 link 不残留）"
        );
    }

    /// 设置热生效回归（生产 4001 恒失败根因）：WsTransport 构造时固化
    /// URL，重连循环永不重读 keyring/config——保存设置后若不重建 link，
    /// 设备永远拿旧 URL（无 token）重试。reload_link_if_running 在
    /// link 运行中时必须以新配置重建：新连接打到新地址。
    #[tokio::test]
    async fn test_reload_link_if_running_rebuilds_with_new_url() {
        use tokio_tungstenite::tungstenite::Message;
        use futures_util::{SinkExt, StreamExt};
        use std::time::Duration;

        // 双 mock server：各收一条连接（hello）即记录并关闭
        async fn spawn_echo_server() -> (std::net::SocketAddr, tokio::sync::mpsc::Receiver<()>) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            tokio::spawn(async move {
                if let Ok((stream, _)) = listener.accept().await {
                    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                    let _ = ws.next().await; // hello
                    let _ = ws.send(Message::Close(None)).await;
                    let _ = tx.send(()).await;
                }
            });
            (addr, rx)
        }

        let (addr1, mut rx1) = spawn_echo_server().await;
        let (addr2, mut rx2) = spawn_echo_server().await;

        let a = Assembled::for_test(format!("ws://{addr1}"))
            .await
            .expect("测试装配失败");
        a.start_link().await.expect("首次 connect 应成功");
        tokio::time::timeout(Duration::from_secs(10), rx1.recv())
            .await
            .expect("server1 应收到首连")
            .expect("server1 通道不应关闭");

        // 模拟保存设置：改 server_url 后热重载
        a.config.write().await.server_url = format!("ws://{addr2}");
        let reloaded = a.reload_link_if_running().await.expect("热重载应成功");
        assert!(reloaded, "link 运行中应触发重建");

        // 新连接必须打到 server2（修复前：旧 transport 持旧 URL 死循环重连 server1）
        tokio::time::timeout(Duration::from_secs(10), rx2.recv())
            .await
            .expect("热重载后新连接应打到新地址（当前实现未重建——URL 固化根因）")
            .expect("server2 通道不应关闭");

        // 未连接时热重载是 no-op（不应报错、不应建连）
        a.stop_link().await.expect("disconnect 应成功");
        let reloaded2 = a.reload_link_if_running().await.expect("未连接时应 Ok");
        assert!(!reloaded2, "未连接时不应触发重建");
    }

    /// 必修 5 回归：主动 disconnect（stop_link）后，桥应转发一帧
    /// wsConnected=false 的 status——halt 不经 on_disconnect 回调，
    /// 不补发则前端「服务器连接」灯恒亮。
    /// mock tauri runtime 监听 wxauto://status 事件验证帧内容。
    #[tokio::test]
    async fn test_stop_link_emits_ws_disconnected_status() {
        // mock server：接受连接后保持（transport 在跑，ws_connected 曾为 true）
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use futures_util::StreamExt;
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let ws = tokio_tungstenite::accept_async(stream).await;
                    let mut ws = match ws {
                        Ok(w) => w,
                        Err(_) => return,
                    };
                    // 持续读（保连接；忽略帧）
                    while let Some(Ok(_)) = ws.next().await {}
                });
            }
        });

        // mock tauri runtime + 事件桥 attach + 监听 status 事件
        let app = tauri::test::mock_builder()
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .expect("mock app 构建失败");
        let statuses: std::sync::Arc<std::sync::Mutex<Vec<Value>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let target = statuses.clone();
        let _sid = tauri::Listener::listen(
            &app,
            crate::ui_events::EVENT_STATUS,
            move |event: tauri::Event| {
                if let Ok(v) = serde_json::from_str::<Value>(event.payload()) {
                    target.lock().unwrap_or_else(|e| e.into_inner()).push(v);
                }
            },
        );

        let a = Assembled::for_test(format!("ws://{addr}"))
            .await
            .expect("测试装配失败");
        a.bridge.attach(app.handle().clone()).await;
        a.start_link().await.expect("connect 应成功");

        // 等连接建立（ws_connected=true——确保断开前灯确曾亮起）
        let current = slot_read(&a.link).clone().expect("槽内应有 link");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !current.ws_connected() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(current.ws_connected(), "10s 内应完成连接（断开前灯须曾亮）");

        // 断开 → 桥应补发 wsConnected=false 的 status
        a.stop_link().await.expect("disconnect 应成功");
        assert!(!current.ws_connected(), "halt 后 ws_connected 应复位 false");
        assert!(slot_read(&a.link).is_none(), "断开后槽应清空");

        // 轮询等补发帧到达监听器（终审必修 5：断开路径前端灯灭的数据源）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let got = statuses
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|v| v["wsConnected"] == false);
            if got {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "5s 内应收到 wsConnected=false 的 status 帧（断开灯灭补发），实际: {:?}",
                statuses.lock().unwrap_or_else(|e| e.into_inner())
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        server.abort();
    }
}
