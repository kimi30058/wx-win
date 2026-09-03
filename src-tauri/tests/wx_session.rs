//! WxSession 能力域集成测试（TDD Step 1：先写测试再实现）
//! 策略：spawn 一个 python 内联 echo sidecar（回显 method），
//! 验证 16 action 映射正确、并发调用经串行队列不死锁、拟人间隙生效。

use serde_json::json;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use wxauto_desktop::sidecar::SidecarHandle;
use wxauto_desktop::wx::listener::ListenerRegistry;
use wxauto_desktop::wx::WxSession;

/// echo sidecar 脚本：把请求的 method 原样放进 result 回显
const ECHO_SCRIPT: &str = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    print(json.dumps({"jsonrpc":"2.0","id":req["id"],"result":{"method":req["method"]}}), flush=True)
"#;

/// spawn 一个 echo sidecar 并包成 WxSession（测试公用构造）
async fn make_session() -> WxSession {
    let handle = SidecarHandle::spawn_with_python(ECHO_SCRIPT, &[])
        .await
        .expect("spawn 失败（需本机有 python3）");
    WxSession::new(Arc::new(Mutex::new(handle)))
}

/// 并发发 3 条不同 action，验证全部成功且映射正确（串行队列不死锁）
#[tokio::test]
async fn test_action_mapping_and_serial() {
    let session = make_session().await;

    let r1 = session.execute("send_message", json!({"who": "张三", "text": "a"}));
    let r2 = session.execute("get_my_info", json!({}));
    let r3 = session.execute("get_moments", json!({"count": 5}));
    let (a, b, c) = tokio::join!(r1, r2, r3);
    assert_eq!(a.expect("send_message 应成功")["method"], "msg.send");
    assert_eq!(b.expect("get_my_info 应成功")["method"], "wx.get_my_info");
    assert_eq!(c.expect("get_moments 应成功")["method"], "moments.get");
}

/// 串行 + 拟人间隙：3 条并发操作总时长应 ≥ 3×500ms（每条放行前 sleep ≥0.5s）
#[tokio::test]
async fn test_humanization_gap_enforced() {
    let session = make_session().await;
    let start = Instant::now();
    let (a, b, c) = tokio::join!(
        session.execute("send_message", json!({"who": "a", "text": "1"})),
        session.execute("send_message", json!({"who": "b", "text": "2"})),
        session.execute("send_message", json!({"who": "c", "text": "3"})),
    );
    assert!(a.is_ok() && b.is_ok() && c.is_ok(), "并发 3 条都应成功");
    assert!(
        start.elapsed() >= std::time::Duration::from_millis(1500),
        "3 条串行操作总时长应 ≥1.5s（拟人间隙），实际 {:?}",
        start.elapsed()
    );
}

/// 未知 action 应报错且不触碰 sidecar
#[tokio::test]
async fn test_unknown_action_rejected() {
    let session = make_session().await;
    let r = session.execute("no_such_action", json!({})).await;
    assert!(r.is_err(), "未知 action 应报错");
    let msg = r.expect_err("刚确认过是 Err").to_string();
    assert!(
        msg.contains("no_such_action"),
        "错误信息应含 action 名: {msg}"
    );
}

/// 16 个 action 全量映射回归（防止后续改动漏改映射表）
#[tokio::test]
async fn test_all_16_actions_mapped() {
    let session = make_session().await;
    let cases: &[(&str, &str, serde_json::Value)] = &[
        ("send_message", "msg.send", json!({"who": "a", "text": "t"})),
        (
            "send_file",
            "file.send",
            json!({"who": "a", "filepath": "C:/x.png"}),
        ),
        (
            "quote_reply",
            "msg.quote",
            json!({"who": "a", "quoteContent": "q", "text": "t"}),
        ),
        (
            "forward_message",
            "msg.forward",
            json!({"target": "a", "sourceWho": "b", "match": "m"}),
        ),
        ("get_my_info", "wx.get_my_info", json!({})),
        ("get_friend_requests", "friends.new_requests", json!({})),
        (
            "accept_friend",
            "friend.accept",
            json!({"nickname": "n", "remark": "r"}),
        ),
        ("add_listen_chat", "listen.add", json!({"nickname": "n"})),
        (
            "remove_listen_chat",
            "listen.remove",
            json!({"nickname": "n"}),
        ),
        ("list_listen_chats", "listen.list", json!({})),
        ("search_chat", "chat.search", json!({"keyword": "k"})),
        ("get_chat_history", "chat.history", json!({"who": "a"})),
        ("get_moments", "moments.get", json!({})),
        ("publish_moment", "moments.publish", json!({"text": "t"})),
        ("download_media", "media.download", json!({"msgId": "m1"})),
        ("voice_to_text", "voice.to_text", json!({"msgId": "m1"})),
    ];
    assert_eq!(cases.len(), 16, "映射用例应为 16 个");
    for (action, method, params) in cases {
        let r = session.execute(action, params.clone()).await;
        let v = r.unwrap_or_else(|e| panic!("{action} 应成功，实际: {e}"));
        assert_eq!(
            &v["method"].as_str().unwrap_or(""),
            method,
            "action {action} 映射错误"
        );
    }
}

/// ListenerRegistry：add/list/remove 走 session 且本地名单同步维护
#[tokio::test]
async fn test_listener_registry_add_list_remove() {
    let session = Arc::new(make_session().await);
    let reg = ListenerRegistry::new(session.clone());

    reg.add("客户群".into()).await.expect("add 应成功");
    reg.add("张三".into()).await.expect("add 应成功");
    // BTreeSet 按 UTF-8 字节序：「客」(U+5BA2) < 「张」(U+5F20)
    assert_eq!(
        reg.list().await,
        vec!["客户群".to_string(), "张三".to_string()]
    );

    reg.remove("张三").await.expect("remove 应成功");
    assert_eq!(reg.list().await, vec!["客户群".to_string()]);
}

/// resync：对已注册名单逐个重放 add_listen_chat，成功时返回空失败列表
#[tokio::test]
async fn test_listener_registry_resync() {
    let session = Arc::new(make_session().await);
    let reg = ListenerRegistry::new(session);
    reg.add("客户群".into()).await.expect("add 应成功");
    reg.add("张三".into()).await.expect("add 应成功");

    let failed = reg.resync().await;
    assert!(
        failed.is_empty(),
        "echo sidecar 下 resync 应全部成功，失败: {failed:?}"
    );
    // resync 后名单不变（BTreeSet 字节序）
    assert_eq!(
        reg.list().await,
        vec!["客户群".to_string(), "张三".to_string()]
    );
}
