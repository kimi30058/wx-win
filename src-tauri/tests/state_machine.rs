//! Task 6 集成测试（TDD Step 1：先红后绿）
//! 覆盖：状态机六态迁移 / backoff 序列 / 配置持久化 / keyring 优雅降级 /
//! sidecar 退出探测 / WxSession 句柄热替换 / Supervisor 崩溃重启闭环。
//!
//! python 相关用例复用 Task 1 的假 sidecar 策略（本机 Linux 无 wxautox4）。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::sync::Mutex;

use wxauto_desktop::config::{
    default_config_path, keyring_get_token, keyring_set_token, load_config, save_config, Config,
};
use wxauto_desktop::sidecar::SidecarHandle;
use wxauto_desktop::state::{AppState, AppStateMachine, Supervisor, RESTART_BACKOFF};
use wxauto_desktop::wx::listener::ListenerRegistry;
use wxauto_desktop::wx::WxSession;

/// 立即退出的假 sidecar（0.3s 后 exit(1)，不回任何 RPC）
const CRASH_SCRIPT: &str = r#"
import sys, time
time.sleep(0.3)
sys.exit(1)
"#;

/// 稳定假 sidecar：wx.init 回 licensed mock，其余方法回 {"ok": true}
const STABLE_SCRIPT: &str = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    if 'id' in req:
        if req.get("method") == "wx.init":
            print(json.dumps({"jsonrpc":"2.0","id":req["id"],"result":{"licensed":True,"wxid":"t_wxid","nickname":"测试"}}), flush=True)
        else:
            print(json.dumps({"jsonrpc":"2.0","id":req["id"],"result":{"ok":True}}), flush=True)
"#;

// ── 状态机迁移（简报 Step 1 两用例） ──────────────────────────

#[tokio::test]
async fn test_state_transitions() {
    let m = AppStateMachine::new();
    assert!(matches!(m.state().await, AppState::SidecarBooting));
    m.mark_wx_init(true).await; // license ok
    assert!(matches!(m.state().await, AppState::WxInit));
    m.mark_ready().await;
    assert!(matches!(m.state().await, AppState::Ready));
    m.mark_busy().await;
    assert!(matches!(m.state().await, AppState::Busy));
    m.mark_idle().await;
    assert!(matches!(m.state().await, AppState::Ready));
    m.mark_sidecar_died().await;
    assert!(matches!(m.state().await, AppState::SidecarDead));
}

#[tokio::test]
async fn test_license_fail_stays_booting() {
    let m = AppStateMachine::new();
    m.mark_wx_init(false).await;
    assert!(matches!(m.state().await, AppState::SidecarBooting)); // 未授权不进 WxInit
}

// ── 状态机守卫迁移（补充覆盖） ────────────────────────────────

/// mark_busy 只从 Ready 进：Booting 下 mark_busy 无效、Busy 下不重复
#[tokio::test]
async fn test_mark_busy_only_from_ready() {
    let m = AppStateMachine::new();
    m.mark_busy().await; // Booting 下试图置 Busy
    assert!(
        matches!(m.state().await, AppState::SidecarBooting),
        "Booting 下 mark_busy 应无效"
    );
    m.mark_wx_init(true).await;
    m.mark_ready().await;
    m.mark_busy().await;
    m.mark_busy().await; // 已是 Busy，幂等
    assert!(matches!(m.state().await, AppState::Busy));
}

/// mark_idle 只从 Busy 回：Ready 下 mark_idle 无效
#[tokio::test]
async fn test_mark_idle_only_from_busy() {
    let m = AppStateMachine::new();
    m.mark_wx_init(true).await;
    m.mark_ready().await;
    m.mark_idle().await; // Ready 下试图置 Idle
    assert!(
        matches!(m.state().await, AppState::Ready),
        "Ready 下 mark_idle 应无效"
    );
}

/// 心跳探测驱动的 Ready↔Degraded 切换（微信离线降级 / 恢复上线还原）
#[tokio::test]
async fn test_mark_online_degraded_toggle() {
    let m = AppStateMachine::new();
    m.mark_wx_init(true).await;
    m.mark_ready().await;
    m.mark_online(false).await; // 微信离线
    assert!(matches!(m.state().await, AppState::Degraded));
    m.mark_online(true).await; // 恢复上线
    assert!(matches!(m.state().await, AppState::Ready));
    // 非 Ready/Degraded 状态下探测结果不强行迁移
    m.mark_sidecar_died().await;
    m.mark_online(true).await;
    assert!(matches!(m.state().await, AppState::SidecarDead));
}

/// 同步查询 state_sync 与 async state() 一致（前端低开销查询路径）
#[tokio::test]
async fn test_state_sync_consistent() {
    let m = AppStateMachine::new();
    assert!(matches!(m.state_sync(), AppState::SidecarBooting));
    m.mark_wx_init(true).await;
    assert!(matches!(m.state_sync(), AppState::WxInit));
    m.mark_ready().await;
    m.mark_busy().await;
    assert!(matches!(m.state_sync(), AppState::Busy));
}

/// 审查 I1 回归：SidecarDead 终态不可被任何迟到写入覆盖。
/// 顺序版：心跳 mark_online(false) 在终态之后到达。
#[tokio::test]
async fn test_sidecar_dead_terminal_not_overridden() {
    let m = AppStateMachine::new();
    m.mark_wx_init(true).await;
    m.mark_ready().await;
    m.mark_sidecar_died().await; // Supervisor 退避耗尽写终态
                                 // 各类迟到写入逐一轰击，终态纹丝不动
    m.mark_online(false).await;
    assert!(
        matches!(m.state().await, AppState::SidecarDead),
        "心跳不得覆盖终态"
    );
    m.mark_online(true).await;
    assert!(matches!(m.state().await, AppState::SidecarDead));
    m.mark_ready().await;
    assert!(
        matches!(m.state().await, AppState::SidecarDead),
        "mark_ready 不得复活终态"
    );
    m.mark_busy().await;
    assert!(matches!(m.state().await, AppState::SidecarDead));
    m.mark_wx_init(true).await;
    assert!(
        matches!(m.state().await, AppState::SidecarDead),
        "mark_wx_init 不得复活终态"
    );
    m.mark_degraded().await;
    assert!(
        matches!(m.state().await, AppState::SidecarDead),
        "mark_degraded 不得覆盖终态"
    );
}

/// 审查 I1 回归（并发版）：心跳与 mark_sidecar_died 并发竞态——
/// 心跳在终态写入前后任意交错，最终态必须是 SidecarDead（不得是 Degraded）。
/// 高并发循环压测 check-and-set 原子性（写锁内守卫消除 check-then-act 窗口）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_sidecar_dead_race_with_heartbeat() {
    for _round in 0..200 {
        let m = Arc::new(AppStateMachine::new());
        m.mark_wx_init(true).await;
        m.mark_ready().await;
        // 心跳泵（持续打 mark_online）与 Supervisor 终态写入并发
        let hb = m.clone();
        let heartbeat = tokio::spawn(async move {
            for _ in 0..50 {
                hb.mark_online(false).await;
            }
        });
        let sv = m.clone();
        let supervisor = tokio::spawn(async move {
            sv.mark_sidecar_died().await;
        });
        let _ = tokio::join!(heartbeat, supervisor);
        // 无论交错如何，终态必须是 SidecarDead——若曾出现 Degraded 覆盖即失败
        assert!(
            matches!(m.state().await, AppState::SidecarDead),
            "第 {_round} 轮竞态后终态应为 SidecarDead，实际 {:?}（心跳覆盖了终态）",
            m.state().await
        );
    }
}

// ── backoff 序列 ─────────────────────────────────────────────
// （backoff_for 辅助函数已随死代码删除——run 主循环直接索引 backoff_table；
//  档位边界由下方常量测试覆盖）

#[test]
fn test_restart_backoff_constants() {
    assert_eq!(RESTART_BACKOFF.len(), 5, "退避序列应为 5 档");
    assert_eq!(RESTART_BACKOFF[0], Duration::from_secs(1));
    assert_eq!(RESTART_BACKOFF[4], Duration::from_secs(16));
}

// ── 配置持久化 ───────────────────────────────────────────────

#[test]
fn test_config_roundtrip() {
    let dir = tempfile::tempdir().expect("临时目录创建失败");
    let path = dir.path().join("config.json");
    let cfg = Config {
        server_url: "ws://127.0.0.1:60021/channel-1".into(),
        channel_id: "ch-1".into(),
        auto_connect: true,
        listen_names: vec!["张三".into(), "李四".into()],
        delay_min_ms: 300,
        delay_max_ms: 800,
        webhook_url: "https://open.feishu.cn/hook/abc".into(),
        webhook_template: "{\"text\":\"{title}\"}".into(),
    };
    save_config(&path, &cfg).expect("保存配置失败");
    let loaded = load_config(&path).expect("加载配置失败");
    assert_eq!(loaded.server_url, "ws://127.0.0.1:60021/channel-1");
    assert_eq!(loaded.channel_id, "ch-1");
    assert!(loaded.auto_connect);
    assert_eq!(
        loaded.listen_names,
        vec!["张三".to_string(), "李四".to_string()]
    );
    assert_eq!(loaded.delay_min_ms, 300);
    assert_eq!(loaded.delay_max_ms, 800);
    assert_eq!(loaded.webhook_url, "https://open.feishu.cn/hook/abc");
    assert_eq!(loaded.webhook_template, "{\"text\":\"{title}\"}");
}

/// 配置文件缺失 → 返回默认值（首启场景不报错）
#[test]
fn test_load_config_missing_file_returns_default() {
    let dir = tempfile::tempdir().expect("临时目录创建失败");
    let path = dir.path().join("not-exist.json");
    let cfg = load_config(&path).expect("缺文件应回默认而非报错");
    assert_eq!(cfg.delay_min_ms, 500, "默认拟人延时下限 500ms");
    assert_eq!(cfg.delay_max_ms, 1000, "默认拟人延时上限 1000ms");
    assert!(!cfg.auto_connect, "默认不自动连接");
    assert!(cfg.listen_names.is_empty());
    assert!(!cfg.server_url.is_empty(), "默认 server_url 非空");
}

/// 配置文件损坏 → 显式报错（不静默吞坏配置）
#[test]
fn test_load_config_invalid_json_errors() {
    let dir = tempfile::tempdir().expect("临时目录创建失败");
    let path = dir.path().join("broken.json");
    std::fs::write(&path, "{ not json !!!").expect("写入失败");
    assert!(load_config(&path).is_err(), "坏 JSON 应报错");
}

/// 默认配置路径形如 .../wxauto-desktop/.../config.json
#[test]
fn test_default_config_path_shape() {
    let p = default_config_path();
    let s = p.to_string_lossy();
    assert!(s.contains("wxauto-desktop"), "路径应含应用目录: {s}");
    assert!(s.ends_with("config.json"), "文件名应为 config.json: {s}");
}

/// keyring 跨实例写读一致（生产 4001 根因回归锁）：
/// keyring_set_token/keyring_get_token 各自 Entry::new 新实例——mock 后端
/// （EntryOnly 持久化）下 set 写进实例内存、新实例 get 必 NoEntry，表现为
/// 「设置 token 成功但连接恒 4001（token=<空>）」。真平台后端
/// （windows-native）必须跨实例读回同值。
/// 环境确无凭据服务时（极端 headless）两者都 Err 可接受，但「set Ok +
/// get Err」组合 = 后端是 mock/坏，必须红。
/// 仅 Windows 断言（产品只发 Windows；非 Windows 无平台后端回落 mock，
/// cfg 守卫跳过——若误删 windows-native feature，Windows 构建/CI 必红）。
#[cfg(target_os = "windows")]
#[test]
fn test_keyring_roundtrip_or_graceful_err() {
    let set_res = keyring_set_token("tok-test-123");
    let get_res = keyring_get_token();
    match (set_res, get_res) {
        (Ok(()), Ok(v)) => assert_eq!(v, "tok-test-123", "跨实例读回值应与写入一致"),
        (Ok(()), Err(get_e)) => panic!(
            "set 成功但跨实例 get 失败——keyring 后端无跨实例持久化（mock 特征），\
             生产表现为设置 token 后恒 4001: {get_e}"
        ),
        (Err(set_e), get) => {
            assert!(!set_e.is_empty(), "错误信息应非空");
            // set 失败时 get 可能 Err（无后端）也可能 Ok（旧值残留），只排除「假装刚写入成功」
            if let Ok(v) = get {
                assert_ne!(v, "tok-test-123", "set 已失败，get 不应取回本次未写入的值");
            }
        }
    }
}

// ── sidecar 退出探测 + 句柄热替换（Task 6 关键集成点） ─────────

#[tokio::test]
async fn test_sidecar_exit_watcher() {
    let handle = SidecarHandle::spawn_with_python(CRASH_SCRIPT, &[])
        .await
        .expect("spawn 失败");
    let mut rx = handle.exit_watcher();
    let status = tokio::time::timeout(Duration::from_secs(5), SidecarHandle::await_exit(&mut rx))
        .await
        .expect("等待退出超时");
    assert!(status.is_some(), "退出状态应可观测");
}

#[tokio::test]
async fn test_wx_session_replace_sidecar() {
    // 初代 sidecar：0.3s 后崩溃
    let dying = SidecarHandle::spawn_with_python(CRASH_SCRIPT, &[])
        .await
        .expect("spawn 失败");
    let session = WxSession::new(Arc::new(Mutex::new(dying)));
    let mut rx = session.sidecar_exit_watcher().await;
    assert!(
        tokio::time::timeout(Duration::from_secs(5), SidecarHandle::await_exit(&mut rx))
            .await
            .is_ok(),
        "初代应退出"
    );
    // 崩溃后调 execute 应报错（Closed）——证明旧句柄确实不可用
    assert!(session.execute("get_my_info", json!({})).await.is_err());

    // 热替换新句柄后恢复可用（唯一入口语义不变）
    let fresh = SidecarHandle::spawn_with_python(STABLE_SCRIPT, &[])
        .await
        .expect("spawn 失败");
    session.replace_sidecar(fresh).await;
    let r = session
        .execute("get_my_info", json!({}))
        .await
        .expect("替换后应恢复可用");
    assert_eq!(r["ok"], true);
}

// ── Supervisor 崩溃重启闭环 ──────────────────────────────────

/// 初代崩溃 → 1s 退避 → spawner 重启稳定 sidecar → wx.init licensed → WxInit
#[tokio::test]
async fn test_supervisor_restarts_after_crash() {
    let first = SidecarHandle::spawn_with_python(CRASH_SCRIPT, &[])
        .await
        .expect("spawn 失败");
    let session = Arc::new(WxSession::new(Arc::new(Mutex::new(first))));
    let listeners = Arc::new(ListenerRegistry::new(session.clone()));
    let state = Arc::new(AppStateMachine::new());

    // 重启 spawner：始终给稳定 sidecar（本用例只验证一次重启）
    let spawner: wxauto_desktop::state::SidecarSpawner =
        Arc::new(|| Box::pin(SidecarHandle::spawn_with_python(STABLE_SCRIPT, &[])));

    let supervisor =
        Supervisor::new(state.clone(), session.clone(), listeners).with_spawner(spawner);
    let sup = Arc::new(supervisor);
    let runner = sup.clone();
    tokio::spawn(async move {
        runner.run().await;
    });

    // 时间线：init(Closed/报错) ≈0.3s → 退避 1s → 重启 init licensed → resync → Ready ≈2~3s。
    // 等 Ready 而非 WxInit：WxInit 是瞬时中间态（init 后立即 resync+mark_ready），
    // WxInit/Ready 迁移正确性已由状态机单元测试覆盖。
    let ok = wait_for_state(&state, AppState::Ready, Duration::from_secs(30)).await;
    assert!(ok, "重启后应达 Ready，实际: {:?}", state.state().await);

    // 重启后的会话确实可用（新句柄已生效）
    let r = session
        .execute("list_listen_chats", json!({}))
        .await
        .expect("重启后 execute 应可用");
    assert_eq!(r["ok"], true);
}

/// backoff 耗尽 → SidecarDead：spawner 恒失败 + 毫秒级注入退避加速，
/// 验证 5 次 spawn 失败后不再重试、状态进 SidecarDead。
#[tokio::test]
async fn test_supervisor_dead_after_spawn_failures() {
    let first = SidecarHandle::spawn_with_python(CRASH_SCRIPT, &[])
        .await
        .expect("spawn 失败");
    let session = Arc::new(WxSession::new(Arc::new(Mutex::new(first))));
    let listeners = Arc::new(ListenerRegistry::new(session.clone()));
    let state = Arc::new(AppStateMachine::new());

    let calls = Arc::new(AtomicUsize::new(0));
    let c2 = calls.clone();
    let spawner: wxauto_desktop::state::SidecarSpawner = Arc::new(move || {
        c2.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err("模拟 spawn 失败".into()) })
    });

    let supervisor = Supervisor::new(state.clone(), session.clone(), listeners)
        .with_spawner(spawner)
        .with_backoff([Duration::from_millis(10); 5]);
    let runner = Arc::new(supervisor);
    let r2 = runner.clone();
    tokio::spawn(async move {
        r2.run().await;
    });

    // 初代 0.3s 崩溃 → init 报错 → 5 轮 10ms 退避全失败 → SidecarDead
    let ok = wait_for_state(&state, AppState::SidecarDead, Duration::from_secs(30)).await;
    assert!(
        ok,
        "5 次 spawn 失败后应进 SidecarDead，实际: {:?}",
        state.state().await
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        5,
        "恰好 5 次 spawner 调用后放弃（不再第 6 次）"
    );
}

/// 等待状态机到达目标态（50ms 轮询；超时返 false）
async fn wait_for_state(m: &AppStateMachine, want: AppState, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if m.state().await == want {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}
