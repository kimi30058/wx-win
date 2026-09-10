//! outbox 单测：入箱/清除/重开恢复/容量修剪/过期修剪/坏行容忍/禁用形态
use serde_json::json;
use wxauto_desktop::outbox::{Outbox, EXPIRY_SECS, MAX_ENTRIES};

fn temp_outbox() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("outbox.jsonl");
    (dir, path)
}

fn msg_frame(id: &str) -> serde_json::Value {
    json!({
        "kind": "event", "type": "message", "eventId": id,
        "data": { "chatName": "张三", "content": "hi" }, "ts": 100
    })
}

/// enqueue → pending 保序；ack 按 eventId 清除
#[test]
fn test_enqueue_ack_roundtrip() {
    let (_dir, path) = temp_outbox();
    let ob = Outbox::open(&path).expect("open");
    assert_eq!(ob.len(), 0);
    assert!(ob.enqueue(&msg_frame("e1")));
    assert!(ob.enqueue(&msg_frame("e2")));
    assert_eq!(ob.len(), 2);
    let p = ob.pending();
    assert_eq!(p[0]["eventId"], "e1");
    assert_eq!(p[1]["eventId"], "e2");
    assert!(ob.ack("e1"));
    assert_eq!(ob.len(), 1);
    assert_eq!(ob.pending()[0]["eventId"], "e2");
    // ack 不存在的 id：返回 false，no-op
    assert!(!ob.ack("ghost"));
}

/// 重开恢复：进程重启语义（文件留存 → pending 保留 → ack 可清除）
#[test]
fn test_reopen_restores_pending() {
    let (_dir, path) = temp_outbox();
    {
        let ob = Outbox::open(&path).expect("open");
        ob.enqueue(&msg_frame("r1"));
        ob.enqueue(&msg_frame("r2"));
    }
    let ob2 = Outbox::open(&path).expect("reopen");
    assert_eq!(ob2.len(), 2);
    assert!(ob2.ack("r1"));
    assert_eq!(ob2.len(), 1);
    assert_eq!(ob2.pending()[0]["eventId"], "r2");
}

/// 容量修剪：超 MAX_ENTRIES 丢最旧
#[test]
fn test_capacity_evicts_oldest() {
    let (_dir, path) = temp_outbox();
    let ob = Outbox::open(&path).expect("open");
    for i in 0..(MAX_ENTRIES + 10) {
        assert!(ob.enqueue(&msg_frame(&format!("e{i}"))));
    }
    assert_eq!(ob.len(), MAX_ENTRIES);
    assert_eq!(ob.pending()[0]["eventId"], "e10"); // e0~e9 被淘汰
}

/// 过期修剪：open 时淘汰 ts 距今超 7 天的条目（绕过 enqueue 手工写旧行）
#[test]
fn test_expiry_trims_on_open() {
    let (_dir, path) = temp_outbox();
    let old_ms: u64 = 1_000; // epoch+1s，距今远超 7 天
    let entry = json!({ "event_id": "old1", "ts": old_ms, "frame": msg_frame("old1") });
    std::fs::write(&path, format!("{}\n", entry)).expect("write");
    let ob = Outbox::open(&path).expect("open");
    assert_eq!(ob.len(), 0);
    assert!(EXPIRY_SECS >= 7 * 24 * 3600); // 常量语义锁
}

/// 坏行容忍：非 JSON 行 / 缺字段行 → 跳过不炸
#[test]
fn test_corrupt_line_tolerated() {
    let (_dir, path) = temp_outbox();
    std::fs::write(&path, "not json\n{\"eventId\":\"x\"}\n").expect("write");
    let ob = Outbox::open(&path).expect("open 应容忍坏行");
    assert_eq!(ob.len(), 0);
}

/// 禁用形态：全 no-op，len 恒 0
#[test]
fn test_disabled_noop() {
    let ob = Outbox::disabled();
    assert_eq!(ob.len(), 0);
    assert!(ob.pending().is_empty());
    assert!(!ob.ack("any"));
    // enqueue 对 Disabled 返回 false（不入箱），也不 panic
    assert!(!ob.enqueue(&msg_frame("d1")));
}

/// 非入箱帧过滤：status 类型 / 无 eventId 的帧不入箱
#[test]
fn test_filters_non_outbox_frames() {
    let (_dir, path) = temp_outbox();
    let ob = Outbox::open(&path).expect("open");
    let status = json!({ "kind": "event", "type": "status", "eventId": "s1", "data": {}, "ts": 1 });
    let no_id = json!({ "kind": "event", "type": "message", "data": {}, "ts": 1 });
    assert!(!ob.enqueue(&status));
    assert!(!ob.enqueue(&no_id));
    assert_eq!(ob.len(), 0);
}
