//! 下行指令执行跟踪：requestId 幂等去重（spec §5.1 统一原则）
//!
//! 服务端可能在超时重发时重复下发同 requestId 的 command；
//! 在途期间重复到达直接忽略，finish 之后同 id 允许重新执行。
use std::collections::HashSet;

use tokio::sync::Mutex;

pub struct CommandTracker {
    running: Mutex<HashSet<String>>,
}

impl CommandTracker {
    pub fn new() -> Self {
        Self {
            running: Mutex::new(HashSet::new()),
        }
    }

    /// 尝试开始执行；同 id 在途时返回 false（重复下发去重）
    pub async fn begin(&self, request_id: String) -> bool {
        self.running.lock().await.insert(request_id)
    }

    /// 执行完成（允许后续同 id 重试）
    pub async fn finish(&self, request_id: &str) {
        self.running.lock().await.remove(request_id);
    }
}

impl Default for CommandTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// finish 一个不存在的 id 应是安全无操作
    #[tokio::test]
    async fn test_finish_unknown_id_is_noop() {
        let t = CommandTracker::new();
        t.finish("nope").await; // 不 panic 即可
        assert!(t.begin("nope".into()).await);
    }
}
