//! webhook 告警客户端（2026-09-10 P1：旁路观察者）
//!
//! 哲学与 event_sink 同款：告警绝不阻塞主链路——send 内部 tokio::spawn
//! 独立任务发送（5s 超时），失败/超时仅 tracing::warn，无重试队列
//! （告警丢失可接受，风暴不可接受）。
//! 模板机制借鉴 SiverWXbot：先 serde_json 解析模板为 JSON 值，再递归
//! 替换所有字符串占位符——detail 含引号/换行（traceback）不破坏 JSON。
//! core 不依赖 tauri（reqwest + tokio 而已）。

use serde_json::{json, Value};

/// 发送超时（spec：5s）
const SEND_TIMEOUT_SECS: u64 = 5;

/// 主机名（Windows COMPUTERNAME 优先，兼容 HOSTNAME；取不到为空）。
/// 自 agent_link 迁来（Task 8）：GUI 装配侧组告警 device 字段需跨 crate
/// 取用，agent_link 内 hello 帧构造改经 `crate::alert::hostname()`。
pub fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_default()
}

/// webhook URL 脱敏（同 cli::mask_token 口径）：保留 scheme+host 与尾 4 字符——
/// 排障可定位配置、不泄 hook 凭证（URL 即飞书系 bot 的 bearer）
fn mask_webhook_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(u) => {
            let tail: String = url.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
            format!("{}://{}...{}", u.scheme(), u.host_str().unwrap_or("?"), tail)
        }
        Err(_) => "<invalid-url>".to_string(),
    }
}

pub struct AlertClient {
    url: String,
    template: String,
    device: String,
}

impl AlertClient {
    pub fn new(webhook_url: String, webhook_template: String, device: String) -> Self {
        Self {
            url: webhook_url,
            template: webhook_template,
            device,
        }
    }

    /// fire-and-forget：空 URL no-op；否则 spawn 异步任务（不阻塞调用方）
    pub fn send(&self, title: &str, detail: &str) {
        if self.url.trim().is_empty() {
            return; // 未配置 webhook = 禁用
        }
        let client = std::sync::Arc::new(AlertClient {
            url: self.url.clone(),
            template: self.template.clone(),
            device: self.device.clone(),
        });
        let title = title.to_string();
        let detail = detail.to_string();
        tokio::spawn(async move {
            client.do_send(title, detail).await;
        });
    }

    /// 实际发送：5s 超时；HTTP 或应用层失败仅 warn（告警丢失可接受）
    async fn do_send(self: std::sync::Arc<Self>, title: String, detail: String) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let payload = render_payload(&self.template, &title, &detail, ts, &self.device);
        let http = match reqwest::Client::builder().build() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("告警 HTTP 客户端构建失败: {e}");
                return;
            }
        };
        match tokio::time::timeout(
            std::time::Duration::from_secs(SEND_TIMEOUT_SECS),
            http.post(&self.url).json(&payload).send(),
        )
        .await
        {
            Err(_) => {
                tracing::warn!(url = %mask_webhook_url(&self.url), title = %title, "告警发送超时({SEND_TIMEOUT_SECS}s)丢弃")
            }
            Ok(Err(e)) => tracing::warn!(url = %mask_webhook_url(&self.url), title = %title, "告警发送失败: {e}"),
            Ok(Ok(resp)) => {
                // 应用层拒绝识别（飞书等 200 但 code≠0）：读 body 判 code
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                if !status.is_success() {
                    tracing::warn!(%status, %body, "告警被网关拒绝(HTTP)");
                    return;
                }
                if let Ok(v) = serde_json::from_str::<Value>(&body) {
                    if let Some(code) = v["code"].as_i64() {
                        if code != 0 {
                            tracing::warn!(code, "告警被应用层拒绝(code≠0): {body}");
                        }
                    }
                }
            }
        }
    }
}

/// 渲染载荷：模板合法 → 解析后递归占位符替换；否则降级通用 JSON
pub fn render_payload(template: &str, title: &str, detail: &str, ts: u64, device: &str) -> Value {
    let t = template.trim();
    if !t.is_empty() {
        match serde_json::from_str::<Value>(t) {
            Ok(mut v) => {
                if !v.is_object() {
                    tracing::warn!("告警模板非 JSON 对象（裸标量/数组），降级通用格式");
                    return json!({ "title": title, "detail": detail, "ts": ts, "device": device });
                }
                apply_placeholders(&mut v, title, detail, ts, device);
                return v;
            }
            Err(e) => {
                tracing::warn!("告警模板非法 JSON，降级通用格式: {e}");
            }
        }
    }
    json!({ "title": title, "detail": detail, "ts": ts, "device": device })
}

/// 递归替换所有字符串值的占位符（含对象值/数组元素）
pub fn apply_placeholders(v: &mut Value, title: &str, detail: &str, ts: u64, device: &str) {
    match v {
        Value::String(s) => {
            if s.contains("{title}")
                || s.contains("{detail}")
                || s.contains("{ts}")
                || s.contains("{device}")
            {
                *s = s
                    .replace("{title}", title)
                    .replace("{detail}", detail)
                    .replace("{ts}", &ts.to_string())
                    .replace("{device}", device);
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                apply_placeholders(item, title, detail, ts, device);
            }
        }
        Value::Object(map) => {
            for (_k, val) in map.iter_mut() {
                apply_placeholders(val, title, detail, ts, device);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 飞书模板：msg_type/text/content 嵌套结构，占位符在深层字符串里
    #[test]
    fn test_render_payload_feishu_template() {
        let tpl = r#"{"msg_type":"text","content":{"text":"【{title}】{device}\n{detail}@{ts}"}}"#;
        let v = render_payload(tpl, "微信掉线", "设备掉线", 1700000000, "wx-rig-01");
        assert_eq!(v["msg_type"], "text");
        assert_eq!(v["content"]["text"], "【微信掉线】wx-rig-01\n设备掉线@1700000000");
        assert!(v["content"]["text"].as_str().unwrap().contains("1700000000"), "ts 占位符必须被替换");
    }

    /// detail 含引号+换行的 traceback 样本：渲染结果仍是合法 JSON 且值完整
    #[test]
    fn test_render_payload_traceback_detail_does_not_break_json() {
        let tpl = r#"{"text":"{detail}"}"#;
        let detail =
            "Traceback (most recent call last):\n  File \"x.py\", line 1\nRuntimeError: 'boom'";
        let v = render_payload(tpl, "sidecar 崩溃", detail, 1, "dev-1");
        let s = serde_json::to_string(&v).expect("渲染结果必须是合法 JSON");
        assert!(s.contains("Traceback (most recent call last):"));
        // 原样往返无损（占位符替换不破坏结构）
        let back: Value = serde_json::from_str(&s).expect("往返解析失败");
        assert_eq!(back["text"], format!("{detail}"));
    }

    /// 模板非法 JSON → 降级通用 JSON（含全部四字段）
    #[test]
    fn test_render_payload_invalid_template_falls_back_to_generic() {
        let v = render_payload("not a {json", "t", "d", 42, "dev");
        assert_eq!(v["title"], "t");
        assert_eq!(v["detail"], "d");
        assert_eq!(v["ts"], 42);
        assert_eq!(v["device"], "dev");
    }

    /// 占位符出现在数组元素与嵌套对象值里
    #[test]
    fn test_apply_placeholders_nested_and_array() {
        let mut v = json!({
            "rows": ["{title}", {"inner": "{device}"}],
            "n": 5
        });
        apply_placeholders(&mut v, "T", "D", 9, "DEV");
        assert_eq!(v["rows"][0], "T");
        assert_eq!(v["rows"][1]["inner"], "DEV");
        assert_eq!(v["n"], 5, "数字不受影响");
    }

    /// 未出现占位符的字符串原样保留
    #[test]
    fn test_apply_placeholders_leaves_plain_strings() {
        let mut v = json!({"msg_type": "text"});
        apply_placeholders(&mut v, "T", "D", 9, "DEV");
        assert_eq!(v["msg_type"], "text");
    }

    /// 空 URL no-op（不 panic、不 spawn）
    #[test]
    fn test_send_noop_on_empty_url() {
        let c = AlertClient::new(String::new(), String::new(), "dev".into());
        c.send("t", "d"); // 不应 panic
    }

    /// webhook URL 脱敏：host 可见、凭证中段截断（I2）
    #[test]
    fn test_mask_webhook_url() {
        let m = mask_webhook_url("https://open.feishu.cn/open-apis/bot/v2/hook/abcdef123456");
        assert!(m.starts_with("https://open.feishu.cn..."));
        assert!(m.ends_with("3456"));
        assert!(!m.contains("abcdef12"), "中段凭证不得泄露");
        assert_eq!(mask_webhook_url("not a url"), "<invalid-url>");
    }

    /// 模板是合法 JSON 但非对象（数组）→ 降级通用 JSON（T5 守卫）
    #[test]
    fn test_render_payload_non_object_template_falls_back() {
        let v = render_payload("[\"{title}\"]", "t", "d", 1, "dev");
        assert_eq!(v["title"], "t");
        assert_eq!(v["detail"], "d");
    }
}
