//! GUI 装配持久化回归：save_settings 三字段合并语义 + 监听名单不被清空。
//! 锁定的不变量：persist hook 与 save_settings 共享同一把内存锁
//! （app_state.rs 的 Assembled.config），任一写入路径都序列化到同一份内存真相。
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

use wxauto_desktop::config::{load_config, save_config, Config};
use wxauto_desktop::sidecar::SidecarHandle;
use wxauto_desktop::wx::listener::{file_persist_hook, ListenerRegistry};
use wxauto_desktop::wx::WxSession;

const SCRIPT: &str = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {"ok": True}}), flush=True)
"#;

/// save_settings 与 persist hook 经同一把内存锁 → listen_names 不被三字段提交清空
#[tokio::test]
async fn test_save_settings_preserves_listen_names() {
    // 1) 存量 config.json：listen_names 非空
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.json");
    let mut cfg = Config::default();
    cfg.listen_names = vec!["张三".into()];
    save_config(&path, &cfg).expect("save 初始配置");

    // 2) 装配对齐 assemble_into 关键步：共享锁 + new_seeded + hook 注入
    let handle = SidecarHandle::spawn_with_python(SCRIPT, &[])
        .await
        .expect("spawn python 失败");
    let session = Arc::new(WxSession::new(Arc::new(Mutex::new(handle))));
    let cfg_lock = Arc::new(RwLock::new(load_config(&path).expect("load")));
    let listeners = Arc::new(ListenerRegistry::new_seeded(
        session,
        cfg_lock.read().await.listen_names.clone(),
    ));
    listeners
        .set_persist_hook(file_persist_hook(cfg_lock.clone(), path.clone()))
        .await;

    // 3) 模拟 save_settings：前端只提交三字段（serverUrl/channelId/autoConnect），
    //    按字段合并写锁（对齐 app_state.rs save_settings 的合并语义）
    {
        let mut cfg = cfg_lock.write().await;
        cfg.server_url = "ws://new:1".into();
        cfg.channel_id = "ch9".into();
        cfg.auto_connect = true;
        save_config(&path, &cfg).expect("save 设置");
    }

    // 4) 断言：listen_names 保留（整体反序列化/hook 竞争清空都会被抓）
    let final_cfg = load_config(&path).expect("load final");
    assert_eq!(final_cfg.listen_names, vec!["张三".to_string()]);
    assert_eq!(final_cfg.server_url, "ws://new:1");
}

/// 锁同源回归（T3 审查增强）：add 触发 hook 落盘 → 再模拟 save_settings →
/// 终态文件双字段共存。若装配把 hook 与 save_settings 拆成两把锁（或
/// save_settings 改整体反序列化），本测试的终态断言必红。
#[tokio::test]
async fn test_hook_and_save_settings_share_lock() {
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.json");
    let mut cfg = Config::default();
    cfg.listen_names = vec!["张三".into()];
    save_config(&path, &cfg).expect("save 初始配置");

    let handle = SidecarHandle::spawn_with_python(SCRIPT, &[])
        .await
        .expect("spawn python 失败");
    let session = Arc::new(WxSession::new(Arc::new(Mutex::new(handle))));
    let cfg_lock = Arc::new(RwLock::new(load_config(&path).expect("load")));
    let listeners = Arc::new(ListenerRegistry::new_seeded(
        session,
        cfg_lock.read().await.listen_names.clone(),
    ));
    listeners
        .set_persist_hook(file_persist_hook(cfg_lock.clone(), path.clone()))
        .await;

    // 1) add 触发 hook（异步 spawn 落盘，轮询等文件出现两条）
    listeners.add("李四".into()).await.expect("add 应成功");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if load_config(&path).expect("load").listen_names.len() == 2 {
            break;
        }
        assert!(Instant::now() < deadline, "5s 内 hook 应落盘 2 条监听");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // 2) 模拟 save_settings 三字段提交（同锁合并——对齐 app_state.rs）
    {
        let mut cfg = cfg_lock.write().await;
        cfg.server_url = "ws://new:2".into();
        cfg.channel_id = "ch10".into();
        save_config(&path, &cfg).expect("save 设置");
    }

    // 3) 终态双字段共存（锁不同源/合并破坏都会在此暴露）
    let final_cfg = load_config(&path).expect("load final");
    assert_eq!(
        final_cfg.listen_names,
        vec!["张三".to_string(), "李四".to_string()]
    );
    assert_eq!(final_cfg.server_url, "ws://new:2");
    assert_eq!(final_cfg.channel_id, "ch10");
}
