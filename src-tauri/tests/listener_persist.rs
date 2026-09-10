//! 监听名单持久化集成测试：种子恢复 + add/remove 落盘
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};

use wxauto_desktop::config::{load_config, save_config, Config};
use wxauto_desktop::sidecar::SidecarHandle;
use wxauto_desktop::wx::listener::{file_persist_hook, ListenerRegistry};
use wxauto_desktop::wx::WxSession;

/// 剧本 sidecar：所有方法回 ok（add/remove_listen_chat 穿透成功）
const SCRIPT: &str = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {"ok": True}}), flush=True)
"#;

/// 装配：剧本 sidecar + 种子名单 + persist hook 落盘到临时 config.json。
/// TempDir 由调用方持有（drop 即清目录，须活得比 registry 长）。
async fn make_registry(
    initial: Vec<String>,
) -> (Arc<ListenerRegistry>, tempfile::TempDir, std::path::PathBuf) {
    let handle = SidecarHandle::spawn_with_python(SCRIPT, &[])
        .await
        .expect("spawn python 失败");
    let session = Arc::new(WxSession::new(Arc::new(Mutex::new(handle))));
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.json");
    let mut cfg = Config::default();
    cfg.listen_names = initial;
    save_config(&path, &cfg).expect("save 初始配置");
    let cfg_lock = Arc::new(RwLock::new(cfg));
    let reg = Arc::new(ListenerRegistry::new_seeded(
        session,
        cfg_lock.read().await.listen_names.clone(),
    ));
    reg.set_persist_hook(file_persist_hook(cfg_lock, path.clone()))
        .await;
    (reg, dir, path)
}

/// 轮询等文件达到期望条数（hook 内 spawn 异步落盘，非同步可见）
async fn wait_for_names(path: &std::path::Path, expect: usize) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let names = load_config(path).expect("load").listen_names;
        if names.len() == expect {
            return names;
        }
        assert!(
            Instant::now() < deadline,
            "5s 内应落盘 {expect} 条，实际: {names:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 种子恢复：构造时名单非空 → list() 即含种子
#[tokio::test]
async fn test_seed_restores_names() {
    let (reg, _dir, _path) = make_registry(vec!["客户群".into(), "张三".into()]).await;
    let names = reg.list().await;
    assert_eq!(names, vec!["客户群".to_string(), "张三".to_string()]); // BTreeSet 有序
}

/// add/remove 成功后落盘：load_config 回读 listen_names 一致
#[tokio::test]
async fn test_add_remove_persisted_to_config() {
    let (reg, _dir, path) = make_registry(vec!["张三".into()]).await;
    reg.add("李四".into()).await.expect("add 应成功");
    let names = wait_for_names(&path, 2).await;
    assert!(names.contains(&"张三".to_string()) && names.contains(&"李四".to_string()));

    reg.remove("张三").await.expect("remove 应成功");
    let names = wait_for_names(&path, 1).await;
    assert_eq!(names, vec!["李四".to_string()]);
}
