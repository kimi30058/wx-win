//! AlertClient HTTP 真路径集成测试：tokio TcpListener 起本地 HTTP 服务，
//! 断言 POST 到达、body 含渲染值、200+code≠0 判应用层失败（仅 warn 不炸）。
use std::time::Duration;
use wxauto_desktop::alert::AlertClient;

/// body 长度从 Content-Length 头解析（读满 body 才算完整请求）
async fn spawn_http_responder_full(
    respond_with: String,
) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = vec![0u8; 65536];
        let mut total = String::new();
        loop {
            let n = stream.read(&mut buf).await.unwrap();
            if n == 0 { break; }
            total.push_str(&String::from_utf8_lossy(&buf[..n]));
            // 头尾分离后按 Content-Length 判读完
            if let Some(pos) = total.find("\r\n\r\n") {
                let headers = &total[..pos];
                let cl = headers
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                    .and_then(|l| l.split(':').nth(1))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if total.len() - pos - 4 >= cl {
                    break;
                }
            }
        }
        let _ = tx.send(total);
        stream.write_all(respond_with.as_bytes()).await.unwrap();
    });
    (format!("http://{addr}/hook"), rx)
}

#[tokio::test]
async fn test_send_posts_rendered_payload_to_url() {
    let (url, mut rx) = spawn_http_responder_full(
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}".to_string(),
    )
    .await;
    let tpl = r#"{"msg_type":"text","content":{"text":"{title} @ {device}"}}"#.to_string();
    let client = AlertClient::new(url, tpl, "dev-1".to_string());
    client.send("微信掉线", "心跳探测失败");
    let req = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("超时：告警未发出")
        .expect("通道关闭");
    assert!(req.starts_with("POST /hook"), "应为 POST 请求: {}", &req[..20.min(req.len())]);
    let body = req.split("\r\n\r\n").nth(1).unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(body).expect("body 应为合法 JSON");
    assert_eq!(v["content"]["text"], "微信掉线 @ dev-1");
}

/// 200 + code≠0（飞书应用层拒绝）——只 warn 不 panic（守护不炸语义即可）
#[tokio::test]
async fn test_send_app_level_rejection_only_warns() {
    let (url, mut rx) = spawn_http_responder_full(
        "HTTP/1.1 200 OK\r\nContent-Length: 19\r\n\r\n{\"code\": 19021, \"msg\": \"sign error\"}"
            .to_string(),
    )
    .await;
    let client = AlertClient::new(url, String::new(), "dev-1".to_string());
    client.send("sidecar 终态", "重启 5 次仍失败");
    let req = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("超时：告警未发出")
        .expect("通道关闭");
    assert!(req.contains("sidecar 终态"), "通用 JSON 降级也应携带 title");
}

/// URL 不可达（端口无服务）：仅丢弃（10s 超时内不 panic）
#[tokio::test]
async fn test_send_to_unreachable_url_does_not_panic() {
    // 绑定后立即 drop 掉 listener——端口有势无服务
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let client = AlertClient::new(format!("http://{addr}/hook"), String::new(), "d".into());
    client.send("t", "d");
    // 给 spawn 的任务留执行窗口（不可达 → 连接拒绝 → warn → 正常结束）
    tokio::time::sleep(Duration::from_millis(300)).await;
}
