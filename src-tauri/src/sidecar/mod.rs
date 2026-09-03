//! sidecar 进程管理与 JSON-RPC 协议（spec §3.2）
//!
//! 架构：`SidecarHandle` 持有子进程 + 共享态（`Arc<Shared>`）。
//! 读循环任务挂在 `Shared` 上（避免 `Arc<SidecarHandle>` 自引用），
//! 逐行解析 `RpcFrame`：有 id 的响应按 pending 表唤醒对应 oneshot；
//! 无 id 的通知转发 broadcast 通道（多订阅者：agent_link 与前端事件桥都要听）。
//!
//! 禁止 unwrap（运行时路径）；IO 错误统一 `RpcError::Io`。

pub mod protocol;
pub mod spec;

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{broadcast, mpsc, oneshot, watch, Mutex};

use serde_json::{json, Value};

pub use protocol::RpcError;

/// 单条 RPC 默认超时（微信 UIA 操作慢，spec §3.2）
pub const RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// 通知 broadcast 通道容量（消息突发时慢订阅者丢旧保新，不阻塞读循环）
const NOTIFY_CHANNEL_CAPACITY: usize = 256;

/// 共享态：读循环 / call / 订阅者三方共同持有
struct Shared {
    writer: Mutex<ChildStdin>,
    /// id → oneshot 发送端；读循环按 id 取出并唤醒
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>,
    next_id: AtomicU64,
    /// 通知通道（message.received 等；多订阅者 broadcast）
    notify_tx: broadcast::Sender<Value>,
    /// 子进程退出状态通道（reaper 任务 wait 后发一次；None=尚未退出）
    exit_tx: watch::Sender<Option<std::process::ExitStatus>>,
    /// 读循环已退出（EOF/IO 错）→ 后续 call 快速 Closed，不挂 30s 超时
    closed: AtomicBool,
    /// kill 请求通道：shutdown 经此请 reaper 代杀（reaper 独占 Child，
    /// 避免 kill 与 wait 抢锁互等死锁）
    kill_tx: mpsc::Sender<()>,
}

/// sidecar 子进程句柄。clone 语义未实现（单实例由上层 Supervisor 管理）。
/// Child 本体移交 reaper 任务独占——wait/kill 都在 reaper 内串行，
/// 句柄只持共享态与 pid（诊断用）。
pub struct SidecarHandle {
    shared: Arc<Shared>,
    #[allow(dead_code)]
    pid: u32,
}

impl SidecarHandle {
    /// 用 python 解释器跑内联脚本（测试 / 嵌入式启动用）
    pub async fn spawn_with_python(
        script: &str,
        envs: &[(&str, &str)],
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let mut cmd = Command::new(find_python());
        cmd.arg("-c")
            .arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for (k, v) in envs {
            cmd.env(k, v);
        }
        Self::do_spawn(cmd).await
    }

    /// sidecar 启动解析优先级（安装态开箱即用 → 开发态回退）：
    /// 1. env `WXAUTO_SIDECAR_CMD` 显式覆盖（引号语义解析，支持含空格路径）
    /// 2. exe 同目录的内置 sidecar（externalBin 安装布局；Windows 加
    ///    CREATE_NO_WINDOW 防控制台闪窗）
    /// 3. 开发态回退 `python3 sidecar-python/sidecar.py`（cwd 锚仓库根）
    pub async fn spawn_default() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let mut cmd = if let Ok(cmd_str) = std::env::var("WXAUTO_SIDECAR_CMD") {
            let (program, args) = crate::cli::parse_sidecar_cmd(&cmd_str);
            let mut c = Command::new(program);
            c.args(args);
            c
        } else if let Some(bundled) = crate::cli::find_bundled_sidecar() {
            let mut c = Command::new(&bundled);
            apply_windows_no_window(&mut c);
            tracing::info!(path = %bundled, "使用内置 sidecar（安装态）");
            c
        } else {
            let mut c = Command::new("python3");
            c.arg("sidecar-python/sidecar.py");
            if let Some(root) = crate::cli::resolve_sidecar_workdir() {
                c.current_dir(root);
            }
            c
        };
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        Self::do_spawn(cmd).await
    }

    /// 实际 spawn：取管道、建共享态、挂读循环 + 退出 reaper
    async fn do_spawn(mut cmd: Command) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // Child 整体移交 reaper（wait/kill/回收都在 reaper 内）；
        // kill_on_drop 不再适用——句柄不再拥有 Child，
        // 兜底 kill 由 shutdown（显式）与 reaper（wait 回收）承担。
        let mut child = cmd.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("无 stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("无 stdout"))?;
        let (notify_tx, _) = broadcast::channel(NOTIFY_CHANNEL_CAPACITY);
        let (exit_tx, _) = watch::channel(None);
        let (kill_tx, kill_rx) = mpsc::channel(1);
        let pid = child.id().unwrap_or(0);
        let shared = Arc::new(Shared {
            writer: Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            notify_tx,
            exit_tx,
            closed: AtomicBool::new(false),
            kill_tx,
        });
        spawn_reader(shared.clone(), stdout);
        // 退出 reaper：独占 Child 做 wait（+ 代杀请求）。回收（僵尸 reap）
        // 由 reaper 的 wait 完成；显式终止走 shutdown → kill 通道。
        spawn_reaper(child, shared.exit_tx.clone(), kill_rx);
        Ok(SidecarHandle { shared, pid })
    }

    /// 默认 30s 超时的 RPC 调用
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.call_with_timeout(method, params, RPC_TIMEOUT).await
    }

    /// 带自定义超时的 RPC 调用：
    /// 1. 注册 oneshot 到 pending[id]
    /// 2. writer 写一行 JSON-RPC 请求
    /// 3. tokio::time::timeout 等 oneshot；超时/错误统一 RpcError
    pub async fn call_with_timeout(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RpcError> {
        // 读循环已退出（sidecar 死）→ 快速 Closed，不注册 pending 挂满超时。
        // （Supervisor 重启窗口内心跳/execute 都走这里立即感知死亡）
        if self.shared.closed.load(Ordering::Acquire) {
            return Err(RpcError::Closed);
        }
        let id = self.shared.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        // 先注册 pending 再写帧，杜绝响应早于注册的竞态
        self.shared.pending.lock().await.insert(id, tx);

        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut line = serde_json::to_string(&frame)
            .map_err(|e| RpcError::Io(format!("请求序列化失败: {e}")))?;
        line.push('\n');

        let write_res = self
            .shared
            .writer
            .lock()
            .await
            .write_all(line.as_bytes())
            .await;
        if let Err(e) = write_res {
            // 写失败即清理 pending，避免泄漏
            self.shared.pending.lock().await.remove(&id);
            return Err(RpcError::Io(format!("写入 stdin 失败: {e}")));
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_recv_err)) => {
                // oneshot 被 drop：读循环已退出（sidecar stdout 关闭）→ pending 里对所有等待者发 Closed
                Self::fail_all_pending(&self.shared, RpcError::Closed).await;
                Err(RpcError::Closed)
            }
            Err(_elapsed) => {
                // 超时：摘除自己的 pending（读循环稍后收到迟到响应也只是孤儿丢弃）
                self.shared.pending.lock().await.remove(&id);
                Err(RpcError::Timeout)
            }
        }
    }

    /// 订阅通知流（message.received 等）。可多次调用，每个订阅者独立游标。
    pub fn subscribe_notifications(&self) -> broadcast::Receiver<Value> {
        self.shared.notify_tx.subscribe()
    }

    /// 退出观察通道：子进程 wait 结束后收到 `Some(ExitStatus)`。
    /// watch 语义：新订阅者立刻拿到当前值（已退出也不错过），Supervisor
    /// 可在任意时刻订阅。可多次调用。
    pub fn exit_watcher(&self) -> watch::Receiver<Option<std::process::ExitStatus>> {
        self.shared.exit_tx.subscribe()
    }

    /// 等退出状态（Supervisor 主循环用）：返回 Some(ExitStatus)；
    /// 发送端全部 drop（理论不可达——reaper 与 Shared 同生命周期）返回 None。
    pub async fn await_exit(
        rx: &mut watch::Receiver<Option<std::process::ExitStatus>>,
    ) -> Option<std::process::ExitStatus> {
        loop {
            if let Some(status) = *rx.borrow() {
                return Some(status);
            }
            if rx.changed().await.is_err() {
                return None;
            }
        }
    }

    /// 读循环退出时对所有未决请求发 Closed（避免上层挂到各自超时）
    async fn fail_all_pending(shared: &std::sync::Arc<Shared>, err: RpcError) {
        let mut pending = shared.pending.lock().await;
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(err.clone()));
        }
    }

    /// 杀子进程并等待回收（幂等）。Child 归 reaper 独占，这里经 kill 通道
    /// 请 reaper 代杀（发 SIGKILL + wait 回收 + 广播退出状态）——彻底
    /// 消除 kill 与 wait 抢 child 锁的互等死锁。通道关闭（reaper 已收尾）
    /// 视为已终止，静默返回。
    pub async fn shutdown(&mut self) {
        let _ = self.shared.kill_tx.send(()).await;
    }
}

/// 退出 reaper：独占 Child 的 wait/kill 串行点。
/// 职责：
/// - 自然退出（崩溃/正常）→ wait 返回 → 广播退出状态
/// - 收到 kill 请求 → start_kill（发信号即返回）→ 继续等 wait 返回回收
///
/// 退出状态用 send_replace 写入 watch（send 在无 receiver 时不保存值——
/// 订阅前的崩溃会丢信号，send_replace 无条件落值）。
fn spawn_reaper(
    mut child: Child,
    exit_tx: watch::Sender<Option<std::process::ExitStatus>>,
    mut kill_rx: mpsc::Receiver<()>,
) {
    tokio::spawn(async move {
        let status;
        tokio::select! {
            s = child.wait() => {
                status = s;
            }
            _ = kill_rx.recv() => {
                // 代杀：发信号后继续 wait 回收（kill 与 wait 同任务内串行，
                // 无锁竞争）。kill 报错=进程已退出，忽略后照常 wait。
                if let Err(e) = child.start_kill() {
                    tracing::warn!("kill sidecar 失败（可能已退出）: {e}");
                }
                status = child.wait().await;
            }
        }
        match status {
            Ok(s) => {
                exit_tx.send_replace(Some(s));
                tracing::info!(code = s.code(), "sidecar 子进程已退出");
            }
            Err(e) => {
                tracing::error!("wait sidecar 子进程失败: {e}");
            }
        }
    });
}

/// 读循环：挂在 `Arc<Shared>` 上（不持 SidecarHandle，避免自引用）。
/// 逐行 parse `RpcFrame`——id 匹配 pending 唤醒；无 id 转发 notify_tx；
/// 协议违规帧（Request / 解析失败）记日志丢弃，不让单条坏帧杀死整条循环。
fn spawn_reader(shared: Arc<Shared>, stdout: tokio::process::ChildStdout) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => {
                    // EOF：sidecar 退出 → 唤醒所有 pending 为 Closed + 置 closed 快速失败标志
                    shared.closed.store(true, Ordering::Release);
                    SidecarHandle::fail_all_pending(&shared, RpcError::Closed).await;
                    tracing::warn!("sidecar stdout 已关闭（EOF），读循环退出");
                    break;
                }
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let frame: protocol::RpcFrame = match serde_json::from_str(trimmed) {
                        Ok(f) => f,
                        Err(e) => {
                            tracing::warn!(
                                "sidecar 输出无法解析为 RpcFrame，丢弃: {e}（原文: {trimmed}）"
                            );
                            continue;
                        }
                    };
                    match frame {
                        protocol::RpcFrame::Response {
                            id, result, error, ..
                        } => {
                            let tx = shared.pending.lock().await.remove(&id);
                            match tx {
                                Some(tx) => {
                                    let payload = match error {
                                        Some(e) => Err(RpcError::Sidecar(format!(
                                            "[{}] {}",
                                            e.code, e.message
                                        ))),
                                        None => Ok(result.unwrap_or(Value::Null)),
                                    };
                                    let _ = tx.send(payload);
                                }
                                None => {
                                    // 迟到响应（调用方已超时摘除）或未知 id → 丢弃
                                    tracing::debug!("孤儿响应 id={id}，丢弃");
                                }
                            }
                        }
                        protocol::RpcFrame::Notification { method, params, .. } => {
                            //（字段模式已含 .. 忽略 jsonrpc）
                            // 通知统一带 method 字段转发（广播语义：无订阅者时发送失败可忽略）
                            let payload = json!({
                                "method": method,
                                "params": params,
                            });
                            let _ = shared.notify_tx.send(payload);
                        }
                        protocol::RpcFrame::Request { .. } => {
                            // sidecar→Rust 方向不应出现请求帧（协议违规）→ 丢弃
                            tracing::warn!("sidecar 输出了请求帧（协议违规），丢弃");
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("读 sidecar stdout 出错: {e}，读循环退出");
                    shared.closed.store(true, Ordering::Release);
                    SidecarHandle::fail_all_pending(&shared, RpcError::Closed).await;
                    break;
                }
            }
        }
    });
}

/// 找可用的 python（python3 优先；探测不到兜底 python3 让 spawn 报真错）。
/// 用同步 std::process::Command 探测（async fn 里调 tokio Command::output 需 await，
/// 而本函数是同步的——spawn_with_python 调用处不便改 async 嵌套）。
fn find_python() -> String {
    for p in ["python3", "python"] {
        if std::process::Command::new(p)
            .arg("--version")
            .output()
            .is_ok()
        {
            return p.into();
        }
    }
    "python3".into()
}

/// Windows GUI 进程 spawn 控制台子进程时加 CREATE_NO_WINDOW（防黑窗闪烁）；
/// 非 Windows 空操作。tokio Command 与 std Command 的 creation_flags 在
/// Windows 上 API 一致。
#[cfg(windows)]
fn apply_windows_no_window(cmd: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn apply_windows_no_window(_cmd: &mut Command) {}
