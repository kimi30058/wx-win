//! sidecar 协议层集成测试（TDD Step 2：先写测试再实现）
//! 策略：spawn 一个 python 内联脚本做假 sidecar，验证 JSON-RPC over stdio 往返。
//! 本机是 Linux，无 wxautox4，测试脚本不 import wxautox4。

use serde_json::json;
use std::time::Duration;
use wxauto_desktop::sidecar::SidecarHandle;

/// 假 sidecar：请求原样回 result（echo method）；无 id 的输入行回一条通知。
/// 用于验证 RPC 往返 + 帧编解码。
#[tokio::test]
async fn test_rpc_roundtrip_with_echo_sidecar() {
    let script = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    if 'id' in req:
        print(json.dumps({"jsonrpc":"2.0","id":req["id"],"result":{"echo":req.get("method")}}), flush=True)
    else:
        print(json.dumps({"jsonrpc":"2.0","method":"message.received","params":{"mock":True}}), flush=True)
"#;
    let mut handle = SidecarHandle::spawn_with_python(script, &[])
        .await
        .expect("spawn 失败（需本机有 python3）");
    let r = handle.call("wx.init", json!({})).await.expect("RPC 失败");
    assert_eq!(r["echo"], "wx.init");
    // 二次调用验证 id 递增与 pending 清理
    let r2 = handle
        .call("wx.get_my_info", json!({"foo": 1}))
        .await
        .expect("第二次 RPC 失败");
    assert_eq!(r2["echo"], "wx.get_my_info");
    handle.shutdown().await;
}

/// 假 sidecar：收到请求睡眠不回 → 应在 1s 短超时变体上报 RpcError::Timeout
#[tokio::test]
async fn test_rpc_timeout() {
    let script = r#"
import sys, time
for line in sys.stdin: time.sleep(60)
"#;
    let mut handle = SidecarHandle::spawn_with_python(script, &[])
        .await
        .expect("spawn 失败");
    let r = handle
        .call_with_timeout("wx.init", json!({}), Duration::from_secs(1))
        .await;
    assert!(r.is_err(), "应超时报错");
    assert!(
        matches!(
            r.err(),
            Some(wxauto_desktop::sidecar::protocol::RpcError::Timeout)
        ),
        "错误类型应为 Timeout"
    );
    handle.shutdown().await;
}

/// 通知路径：假 sidecar 主动推送无 id 帧 → broadcast 订阅者应收到 message.received
#[tokio::test]
async fn test_notification_broadcast() {
    let script = r#"
import sys, json, time
# 启动即推一条通知，然后进入 echo 循环
print(json.dumps({"jsonrpc":"2.0","method":"message.received","params":{"text":"hello"}}), flush=True)
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    if 'id' in req:
        print(json.dumps({"jsonrpc":"2.0","id":req["id"],"result":{"ok":True}}), flush=True)
"#;
    let mut handle = SidecarHandle::spawn_with_python(script, &[])
        .await
        .expect("spawn 失败");
    // 先订阅再触发 RPC（RPC 往返确保通知已写入 stdout）
    let mut sub1 = handle.subscribe_notifications();
    let mut sub2 = handle.subscribe_notifications();
    let _r = handle.call("wx.init", json!({})).await.expect("RPC 失败");

    // 两个订阅者都应收到同一条通知（broadcast 多订阅者语义）
    let n1 = tokio::time::timeout(Duration::from_secs(5), sub1.recv())
        .await
        .expect("订阅者1超时")
        .expect("订阅者1通道关闭");
    assert_eq!(n1["method"], "message.received");
    assert_eq!(n1["params"]["text"], "hello");

    let n2 = tokio::time::timeout(Duration::from_secs(5), sub2.recv())
        .await
        .expect("订阅者2超时")
        .expect("订阅者2通道关闭");
    assert_eq!(n2["method"], "message.received");
    handle.shutdown().await;
}

/// 错误帧路径：假 sidecar 回 error 对象 → call 应返回 RpcError::Sidecar
#[tokio::test]
async fn test_rpc_error_frame() {
    let script = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    if 'id' in req:
        print(json.dumps({"jsonrpc":"2.0","id":req["id"],"error":{"code":-32000,"message":"微信未登录"}}), flush=True)
"#;
    let mut handle = SidecarHandle::spawn_with_python(script, &[])
        .await
        .expect("spawn 失败");
    let r = handle.call("wx.init", json!({})).await;
    match r {
        Err(wxauto_desktop::sidecar::protocol::RpcError::Sidecar(msg)) => {
            assert!(msg.contains("微信未登录"), "错误信息应透传，实际: {msg}");
        }
        other => panic!("应返回 Sidecar 错误，实际: {other:?}"),
    }
    handle.shutdown().await;
}
