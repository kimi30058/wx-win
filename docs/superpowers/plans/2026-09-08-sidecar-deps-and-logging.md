# sidecar 传递依赖补齐 + 三通道日志系统 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复真机激活事故（冻结包缺 wxautox4 传递依赖 requests 等 9 个）并建立三通道日志系统（内存 ring + 按天落盘 + Python 侧主动日志），让装机现场可诊断。

**Architecture:** PyInstaller + externalBin 打包链保留不动；spec hiddenimports 显式补齐 PyPI 声明的全部传递依赖；CI 冒烟从 MOCK 单条升级为真路径三条；Rust init_sequence 把 RPC 错误帧（sidecar 活着）从静默吞掉改为写 last_init_fail + 发 init_fail 帧；新增 file_log.rs 在 ring 写入侧并联文件 sink；Python 侧新增 sidecar_log.py 走 stderr（stdout 是 JSON-RPC 专用通道，铁律不得混入）；启动探测链结果写 bootstrap.log。

**Tech Stack:** Rust（tauri 2 / tracing / tokio）、Python 3（stdlib only，sidecar 无第三方依赖）、Vue 3 + Pinia + TDesign、PyInstaller、vitest / cargo test / pytest

**Spec:** `docs/superpowers/specs/2026-09-08-sidecar-deps-and-logging-design.md`（本计划与 spec 同仓，`desktop/` 独立 wx-win 仓）

## Global Constraints

- **工作仓库**：`/home/working/lyagent/desktop`（独立 git 仓 wx-win，分支 main）。所有路径相对该根；commit 就地做。
- **stdout 铁律**：sidecar.py 的 stdout 是 JSON-RPC 帧专用通道，任何日志只准写 stderr——混入即协议损坏。
- **TypeScript 禁 `any`/`as any`**：用 `unknown` + 类型守卫（主仓铁律，本仓同遵）。
- **Rust**：生产路径禁 unwrap；`cargo fmt` 过后才能 commit。
- **测试三线全绿后才 commit**：cargo（`cargo test`，在 `src-tauri/` 下）、pytest（`pytest sidecar-python/`，在 desktop/ 下）、vitest（`pnpm vitest run`，在 desktop/ 下）。
- **Python sidecar 零第三方依赖**：sidecar_log.py 只用 stdlib（time/sys/threading/os）。
- **日期处理不引 chrono**：Rust 侧无 chrono 依赖，文件名日期用 epoch 秒 → UTC 天的纯整数换算（file_log.rs 自带 helper，见 Task 6）；Python 侧用 `time.strftime`。
- **日志永不阻断业务**：FileSink/写盘任何失败只 tracing::warn 一条后降级，不传播错误。
- 体积预期：冻结 exe 24.7MB → ~35MB（验收参考值，非硬门槛）。

---

### Task 1: spec hiddenimports 补齐 9 个传递依赖

**Files:**
- Modify: `sidecar-python/wxauto-sidecar.spec`（hiddenimports 列表段）

**Interfaces:**
- Consumes: 无（独立任务）
- Produces: 冻结 exe 内含 requests/pillow/colorama/psutil/pyperclip/pywin32/sounddevice/tenacity/comtypes——Task 2 的真路径冒烟依赖此事实

背景：wxautox4 41.1.1.post1 在 PyPI 声明 `requires_dist = ['colorama','comtypes','pillow','psutil','pyperclip','pywin32','requests','sounddevice','tenacity']`。它是 cp312-win_amd64 编译型 wheel，这些 import 藏在 .pyd 二进制里，PyInstaller 静态分析看不见。2026-09-08 真机事故：冻结包缺 requests → wx.activate 抛 `ModuleNotFoundError: No module named 'requests'`（-32603）。

- [ ] **Step 1: 修改 hiddenimports**

`sidecar-python/wxauto-sidecar.spec` 的 `hiddenimports` 从：

```python
    hiddenimports=[
        "wxautox4",
        "wxautox4.utils.useful",
        "pythoncom",
        "win32com",
        "comtypes",
    ],
```

改为（保持原有 5 项顺序不动，追加 8 项 + 注释；comtypes 已存在不重复列）：

```python
    # wxautox4 传递依赖显式补齐（2026-09-08 真机事故：wxautox4 是编译型
    # wheel，.pyd 内部的 import 静态分析看不见，冻结包缺 requests 导致
    # activate 抛 ModuleNotFoundError）。清单来源 = PyPI wxautox4 41.1.1.post1
    # 的 requires_dist：colorama/comtypes/pillow/psutil/pyperclip/pywin32/
    # requests/sounddevice/tenacity（comtypes 原清单已有）。
    # ⚠ wxautox4 升级后必须对照 PyPI requires_dist 重新核对此清单！
    hiddenimports=[
        "wxautox4",
        "wxautox4.utils.useful",
        "pythoncom",
        "win32com",
        "comtypes",
        "colorama",
        "pillow",
        "psutil",
        "pyperclip",
        "pywin32",
        "requests",
        "sounddevice",
        "tenacity",
    ],
```

- [ ] **Step 2: 语法验证（Linux 上无法跑 PyInstaller 冻结，验证 spec 是合法 Python）**

```bash
python3 -c "import ast; ast.parse(open('sidecar-python/wxauto-sidecar.spec').read()); print('spec 语法 OK')"
```

Expected: `spec 语法 OK`

- [ ] **Step 3: Commit**

```bash
git add sidecar-python/wxauto-sidecar.spec
git commit -m "fix(sidecar): 冻结包补齐 wxautox4 全部 9 个传递依赖——真机 activate ModuleNotFoundError 事故根因"
```

---

### Task 2: CI 冒烟升级为真路径三条

**Files:**
- Modify: `scripts/build_sidecar_windows.ps1`（冒烟段，约 26-35 行）

**Interfaces:**
- Consumes: Task 1 补齐后的冻结 exe（真路径冒烟才会过）
- Produces: CI 出的安装包经过三条冒烟验证（MOCK / wx.init 真路径 / wx.activate 假码真路径）

断言原理：**业务错 ≠ 打包错**。CI 机器无微信无授权——wx.init 真路径返回结果帧（`licensed:false`），wx.activate 假码返回错误帧或 `ok:false` 结果帧（授权服务器拒绝），这些是业务错；只有 `ModuleNotFoundError` 是打包错。帧形态判断：有 `"error"` 键 = 错误帧，有 `"result"` 键 = 结果帧。

- [ ] **Step 1: 重写冒烟段**

把 `scripts/build_sidecar_windows.ps1` 末段（`# 冒烟：MOCK 模式发一条...` 到 `Write-Host "OK: ..."` 整段）替换为：

```powershell
# ── 冒烟三条 ──────────────────────────────────────────────
# 1) MOCK：mock 数据回来（原有，不触 wxautox4）
# 2) 真路径 wx.init：CI 无授权 → 应收「结果帧」（licensed:false）；
#    若收「错误帧」即打包缺陷（缺 pythoncom/comtypes 等导入级依赖）
# 3) 真路径 wx.activate 假码：授权服务器拒绝是业务错，可接受；
#    ModuleNotFoundError 是打包错——本次事故的直接复现路径
$Exe = "$OutDir/wxauto-sidecar-$Triple.exe"

$env:WXAUTO_MOCK = "1"
$resp = '{"id": 1, "method": "wx.get_my_info", "params": {}}' | & $Exe | Select-Object -First 1
Write-Host "smoke[1/3] mock wxid: $resp"
if (-not ($resp -match '"wxid"')) { throw "sidecar exe MOCK 冒烟失败: $resp" }

Remove-Item Env:WXAUTO_MOCK -ErrorAction SilentlyContinue
$resp2 = '{"id": 2, "method": "wx.init", "params": {}}' | & $Exe | Select-Object -First 1
Write-Host "smoke[2/3] init: $resp2"
if (-not ($resp2 -match '"result"')) { throw "真路径 wx.init 应返回结果帧, 实得: $resp2" }
if ($resp2 -match '"error"') { throw "真路径 wx.init 返回错误帧(疑似缺依赖): $resp2" }

$resp3 = '{"id": 3, "method": "wx.activate", "params": {"code": "CI-SMOKE-FAKE"}}' | & $Exe | Select-Object -First 1
Write-Host "smoke[3/3] activate: $resp3"
if ($resp3 -match 'ModuleNotFoundError') { throw "真路径 wx.activate 缺依赖(本次事故形态): $resp3" }
if (-not (($resp3 -match '"error"') -or ($resp3 -match '"result"'))) { throw "activate 应返回 JSON-RPC 帧, 实得: $resp3" }

Write-Host "OK: $Exe"
```

注意：第 2/3 条跑真 wxautox4 导入链，onefile 自解压 + 导入每条 +5~15s，属预期。

- [ ] **Step 2: 本机 Linux 上无法跑 ps1 冻结，做语法级验证**

```bash
pwsh -NoProfile -Command '[System.Management.Automation.Language.Parser]::ParseFile("scripts/build_sidecar_windows.ps1", [ref]$null, [ref]$errs) > $null; if ($errs.Count -gt 0) { $errs; exit 1 }; "ps1 语法 OK"' 2>/dev/null || echo "本机无 pwsh，跳过语法验证（CI 会跑）"
```

Expected: `ps1 语法 OK` 或跳过提示（CI 首跑兜底）

- [ ] **Step 3: Commit**

```bash
git add scripts/build_sidecar_windows.ps1
git commit -m "ci(sidecar): 冒烟升级真路径三条——wx.init 结果帧断言+wx.activate 假码缺依赖断言(事故复现路径)"
```

---

### Task 3: init_sequence 把 RPC 错误帧归入 init_fail

**Files:**
- Modify: `src-tauri/src/state.rs:359-385`（init_sequence 开头段）
- Test: `src-tauri/tests/license_init.rs`（追加剧本 + 用例）

**Interfaces:**
- Consumes: `RpcError` 枚举（`src-tauri/src/sidecar/protocol.rs:52-61`，四变体 `Timeout`/`Sidecar(String)`/`Io(String)`/`Closed`）；现有测试基建 `make_supervisor(script)`（license_init.rs:71-92）
- Produces: `last_init_fail` 快照对 RPC 错误帧也写入（格式 `"初始化失败：{msg}"`，msg 即 RpcError::Sidecar 的 Display `sidecar 错误: {0}`）；init_fail 帧 `data.reason` 同值。Task 5 前端透传依赖此格式。

设计语义（ADR-0011）：`Err(RpcError::Sidecar(msg))` = sidecar 活着但 wx 层报错（依赖缺失/导入失败）→ 写快照 + 发帧 + warn；`Err(Timeout | Io | Closed)` = transport 级（sidecar 死/卡）→ 维持原行为（不写不报，归 Supervisor 重启域）。

- [ ] **Step 1: 写失败测试**

在 `src-tauri/tests/license_init.rs` 追加（文件顶部剧本区加一个，测试区加两个）：

```rust
/// RPC 错误帧剧本：wx.init 回 -32603（复现 wxautox4 依赖缺失形态）
const INIT_ERROR_SCRIPT: &str = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    if req.get("method") == "wx.init":
        err = {"code": -32603, "message": "ModuleNotFoundError: No module named 'requests'"}
        print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "error": err}), flush=True)
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {"ok": True}}), flush=True)
"#

/// RPC 错误帧（sidecar 活着、wx 层报错）必须进 init_fail 引导链路：
/// 写 last_init_fail 快照 + 发 init_fail 帧——修复前被 .ok() 静默吞掉，
/// 前端永远 Booting 无原因（ADR-0011）
#[tokio::test]
async fn rpc_error_frame_writes_snapshot_and_emits_init_fail() {
    let (sup, state, events) = make_supervisor(INIT_ERROR_SCRIPT).await;
    sup.retry_init().await;
    assert_eq!(
        state.state().await,
        AppState::SidecarBooting,
        "init 报错保持 Booting"
    );
    let snap = sup.last_init_fail().await;
    assert!(
        snap.as_deref().unwrap_or("").contains("ModuleNotFoundError"),
        "快照应含真实错误, 实得: {snap:?}"
    );
    assert!(snap.as_deref().unwrap_or("").starts_with("初始化失败："), "格式约定前缀");
    let frames = events.lock().unwrap();
    let hit = frames.iter().any(|f| {
        f["type"] == "init_fail"
            && f["data"]["reason"]
                .as_str()
                .unwrap_or("")
                .contains("ModuleNotFoundError")
    });
    assert!(hit, "应收 init_fail 帧含错误详情, 实收: {frames:?}");
}

/// transport 级失败（进程死/Closed）不写快照——不谎报授权失败，归重启域
#[tokio::test]
async fn transport_failure_keeps_snapshot_empty() {
    // 起一个秒退的 sidecar：spawn 成功即 exit → RPC 走 Closed
    let script = r#"
import sys
sys.stdin.readline()
"#;
    let handle = SidecarHandle::spawn_with_python(script, &[])
        .await
        .expect("spawn 失败");
    let session = Arc::new(WxSession::new(Arc::new(Mutex::new(handle))));
    let listeners = Arc::new(ListenerRegistry::new(session.clone()));
    let state = Arc::new(AppStateMachine::new());
    let sup = Supervisor::new(state.clone(), session, listeners);
    sup.retry_init().await;
    assert_eq!(
        sup.last_init_fail().await,
        None,
        "transport 失败不得写授权失败快照"
    );
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd src-tauri && cargo test --test license_init rpc_error_frame
```

Expected: FAIL——`snap` 为 None（现状 `.ok()` 吞掉）

- [ ] **Step 3: 实现**

`src-tauri/src/state.rs` init_sequence 开头段，把：

```rust
        let init = self.direct_wx_init().await.ok();
```

改为（后续 `licensed`/`fail_reason` 的 `init.as_ref()` 链保持编译——用 Option 两段式）：

```rust
        let init = match self.direct_wx_init().await {
            Ok(v) => Some(v),
            Err(RpcError::Sidecar(msg)) => {
                // ADR-0011：RPC 错误帧 = sidecar 活着、wx 层报错（依赖缺失/
                // 导入失败），是可诊断问题——进 init_fail 引导链路，激活页
                // 显示真实原因。区别于 transport 级失败（下分支）。
                let reason = format!("初始化失败：sidecar 错误: {msg}");
                tracing::warn!(%reason, "wx.init RPC 错误帧");
                *self.last_init_fail.write().await = Some(reason.clone());
                self.emit(serde_json::json!({
                    "kind": "event", "type": "init_fail",
                    "data": {"reason": reason},
                }))
                .await;
                return;
            }
            Err(_) => None, // Timeout/Io/Closed = transport 级（sidecar 死/卡），
                           // 归 Supervisor 重启域——不写快照不谎报授权失败
        };
```

同时更新 `last_init_fail` 字段文档注释（state.rs:231-233）：把「RPC 失败不写（sidecar 死亡归重启域，不谎报授权失败）」改为「RPC 错误帧（Sidecar 变体）也写——sidecar 活着、wx 层可诊断（ADR-0011）；transport 级失败不写」。需要 `use` 检查：`RpcError` 若未在 state.rs 作用域，加 `use crate::sidecar::protocol::RpcError;`（先 grep 现有 import 路径，`wxauto_desktop::sidecar` 里 protocol 模块 pub）。

- [ ] **Step 4: 跑测试确认通过 + 全量**

```bash
cd src-tauri && cargo test --test license_init && cargo test
```

Expected: 全 PASS（新增 2 + 原有全绿）

- [ ] **Step 5: cargo fmt + commit**

```bash
cd src-tauri && cargo fmt
git add src-tauri/src/state.rs src-tauri/tests/license_init.rs
git commit -m "fix(state): init RPC 错误帧归入 init_fail 引导——不再静默吞掉,激活页可见真实原因(ADR-0011)"
```

---

### Task 4: Python 侧日志（sidecar_log.py + 埋点）

**Files:**
- Create: `sidecar-python/sidecar_log.py`
- Create: `sidecar-python/test_sidecar_log.py`
- Modify: `sidecar-python/sidecar.py`（主循环兜底 except 加一行日志）
- Modify: `sidecar-python/methods.py`（`_init`/`_activate` 埋点 + 3 处 print 改 log）

**Interfaces:**
- Consumes: 无
- Produces: `sidecar_log.log(level: str, msg: str) -> None`（全模块唯一入口，stderr 输出格式 `[YYYY-MM-DD HH:MM:SS] [SIDECAR] [LEVEL] msg`）。Rust 侧 `RingStderrSink` 现有关键字升级逻辑（含 "ERROR" 升 error）自动命中 `[SIDECAR] [ERROR]` 行，无需 Rust 改动。

- [ ] **Step 1: 写失败测试**

`sidecar-python/test_sidecar_log.py`：

```python
"""sidecar_log 单元测试：格式 / 线程锁 / 异常吞没（spec §4）"""
import io
import sys
import threading

sys.path.insert(0, os.path.dirname(__file__)) if False else None
import os
sys.path.insert(0, os.path.dirname(__file__))

import sidecar_log  # noqa: E402


def _capture(func):
    """临时替换 stderr 捕获输出"""
    buf = io.StringIO()
    old = sys.stderr
    sys.stderr = buf
    try:
        func()
    finally:
        sys.stderr = old
    return buf.getvalue()


def test_format_contains_timestamp_tag_level():
    out = _capture(lambda: sidecar_log.log("ERROR", "boom"))
    assert "[SIDECAR]" in out
    assert "[ERROR]" in out
    assert "boom" in out
    # 时间戳形态 YYYY-MM-DD HH:MM:SS
    parts = out.split("] [")
    assert len(parts[0]) == 21  # "[YYYY-MM-DD HH:MM:SS" = 1+19+1


def test_info_level_passthrough():
    out = _capture(lambda: sidecar_log.log("INFO", "hello"))
    assert "[INFO]" in out and "hello" in out


def test_write_failure_silent(capsys):
    """stderr 写失败必须静默——日志永不阻断业务（spec 铁律）"""
    class Boom:
        def write(self, *_):
            raise OSError("disk full")

        def flush(self):
            pass

    old = sys.stderr
    sys.stderr = Boom()
    try:
        sidecar_log.log("ERROR", "x")  # 不抛即通过
    finally:
        sys.stderr = old


def test_thread_safety_no_interleave():
    """并发 50 线程各写一行——输出行数完整无交错（锁的意义）"""
    buf = io.StringIO()
    old = sys.stderr
    sys.stderr = buf
    try:
        threads = [threading.Thread(target=lambda i=i: sidecar_log.log("INFO", f"line-{i}")) for i in range(50)]
        [t.start() for t in threads]
        [t.join() for t in threads]
    finally:
        sys.stderr = old
    lines = [l for l in buf.getvalue().splitlines() if l.strip()]
    assert len(lines) == 50
    assert sum(1 for l in lines if "line-" in l) == 50
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd /home/working/lyagent/desktop && python3 -m pytest sidecar-python/test_sidecar_log.py -v
```

Expected: FAIL——`ModuleNotFoundError: No module named 'sidecar_log'`

- [ ] **Step 3: 实现 sidecar_log.py**

```python
"""sidecar 日志：stderr 单通道（stdout 是 JSON-RPC 帧专用——铁律不得混入）。

Rust 侧 RingStderrSink 逐行消费本输出进内存 ring + 前端日志 tab + 落盘
（[SIDECAR] [ERROR] 行被 ERROR 关键字升 error）。写失败静默——日志
永不阻断业务。参考原型 SiverWXbot logger.py 精简（无文件通道：落盘由
Rust 侧 FileSink 统一收口，Python 不重复写文件）。
"""
import sys
import threading
import time

_lock = threading.Lock()


def log(level: str, msg: str) -> None:
    """写一条日志到 stderr。level: "INFO"|"WARN"|"ERROR"。永不抛异常。"""
    try:
        line = time.strftime("[%Y-%m-%d %H:%M:%S]") + f" [SIDECAR] [{level}] {msg}\n"
        with _lock:
            sys.stderr.write(line)
            sys.stderr.flush()
    except Exception:  # noqa: BLE001 — 日志永不阻断业务
        pass
```

- [ ] **Step 4: 跑测试确认通过**

```bash
python3 -m pytest sidecar-python/test_sidecar_log.py -v
```

Expected: 4 PASS

- [ ] **Step 5: 埋点（sidecar.py + methods.py）**

`sidecar-python/sidecar.py`：
1. 顶部 `import methods` 之后加 `import sidecar_log  # noqa: E402 — 同目录；日志走 stderr（stdout 铁律专用 JSON-RPC）`
2. 主循环兜底 except（现 `except Exception as e:` 转 -32603 处）改为：

```python
        except Exception as e:  # noqa: BLE001 — sidecar 边界统一转 error 帧
            sidecar_log.log("ERROR", f"dispatch {req.get('method', '?')} 失败: {type(e).__name__}: {e}")
            _error(req["id"], -32603, f"{type(e).__name__}: {e}")
```

`sidecar-python/methods.py`：
1. 文件顶部 import 区加 `import sidecar_log`
2. `_init`（真实导入前、`# 真实导入` 注释行后）加：

```python
    sidecar_log.log("INFO", "wx.init 开始（真实模式）")
```

`_init` 的 `return result` 前加：

```python
    sidecar_log.log("INFO", f"wx.init 完成: licensed={_license_ok} failReason={fail_reason}")
```

3. `_activate` 真实导入段前加 `sidecar_log.log("INFO", "wx.activate 开始（真实模式）")`；三个 return 点前各加一条：

```python
    sidecar_log.log("WARN", "wx.activate 失败：激活码无效或已过期")
    return {"ok": False, "message": "激活失败：激活码无效或已过期"}
```

```python
    sidecar_log.log("WARN", "wx.activate：码已接受但授权未生效")
    return {"ok": False, "message": "激活码已接受但授权状态未生效，请重启应用"}
```

```python
    sidecar_log.log("INFO", "wx.activate 成功")
    return {"ok": True, "message": "激活成功"}
```

4. 三处现有 print 改 log（280 / 391 / 511 行附近）：

```python
# 280: print(f"[sidecar] quote 失败({e})，降级普通发送", file=sys.stderr)
sidecar_log.log("WARN", f"quote 失败({e})，降级普通发送")
# 391: print(f"[sidecar] message.received 回调异常: {e}", file=sys.stderr)
sidecar_log.log("ERROR", f"message.received 回调异常: {e}")
# 511: print(f"[sidecar] 语音转写失败: {e}", file=sys.stderr)
sidecar_log.log("WARN", f"语音转写失败: {e}")
```

- [ ] **Step 6: 全量 pytest（mock 路径不触新日志的断言不破坏）**

```bash
python3 -m pytest sidecar-python/ -v
```

Expected: 全 PASS（78+4）

- [ ] **Step 7: Commit**

```bash
git add sidecar-python/sidecar_log.py sidecar-python/test_sidecar_log.py sidecar-python/sidecar.py sidecar-python/methods.py
git commit -m "feat(sidecar): Python 侧日志——sidecar_log stderr 单通道+dispatch/init/activate 埋点(事故现场取证)"
```

---

### Task 5: 前端 initFailReason 透传 + 激活页第 4 态

**Files:**
- Modify: `src/stores/app.ts`（类型 + KNOWN 白名单逻辑 + 事件/快照两处赋值）
- Modify: `src/views/Activation.vue`（状态卡第 4 态）
- Test: `src/stores/app.spec.ts`（追加用例）

**Interfaces:**
- Consumes: Rust `init_fail` 帧 `data.reason`（Task 3 起可为 `"初始化失败：sidecar 错误: …"` 任意串）；`get_init_fail_reason` 命令返回 `Option<String>`
- Produces: `InitFailReasonName` 类型放宽为 `string`；store getter 现有 `needsActivation`/`wechatMissing` 语义不变（仍按已知两值判等）；新增无 getter（第 4 态由视图直接判 `initFailReason !== '' && !needsActivation && !wechatMissing && sidecarBooting`）

- [ ] **Step 1: 写失败测试**

`src/stores/app.spec.ts` 追加 describe 块：

```typescript
describe('initFailReason 未知值透传（P0-3）', () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    listenMock.mockReset();
    invokeMock.mockReset();
    invokeMock.mockImplementation(() => Promise.resolve(null));
  });

  function bootStore() {
    const store = useAppStore();
    store.appState = 'SidecarBooting';
    return store;
  }

  it('init_fail 事件携带未知 reason 时原样存储', () => {
    const store = bootStore();
    // 直接模拟事件回调路径：store 内部 handler 逻辑（订阅外不可达，测状态字段容错）
    store.initFailReason = '初始化失败：sidecar 错误: [-32603] ModuleNotFoundError' as never;
    expect(store.needsActivation).toBe(false);
    expect(store.wechatMissing).toBe(false);
  });

  it('快照兜底不再丢弃未知值', () => {
    const store = useAppStore();
    store.appState = 'SidecarBooting';
    // get_init_fail_reason 返回未知串（Task 3 起 Rust 会写）
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === 'get_init_fail_reason') return Promise.resolve('初始化失败：sidecar 错误: [-32603] X');
      if (cmd === 'get_app_state') return Promise.resolve('SidecarBooting');
      if (cmd === 'get_recent_logs') return Promise.resolve([]);
      return Promise.resolve(null);
    });
    // init() 走快照兜底分支（listen mock 默认 resolve 空清理函数）
    void store.init();
    // 断言放 flushPromises 后不可行（未引 @vue/test-utils）——用微任务等待
    return Promise.resolve().then(() => {
      // init 是 async：链式微任务两跳后快照分支已跑
      return Promise.resolve();
    }).then(() => {
      expect(store.initFailReason).toContain('初始化失败');
    });
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

```bash
pnpm vitest run src/stores/app.spec.ts
```

Expected: FAIL——`initFailReason` 类型/守卫拒绝未知值（现状 `KNOWN_INIT_FAIL_REASONS.includes` 守卫丢弃）

- [ ] **Step 3: 实现 store 放宽**

`src/stores/app.ts`：

1. 类型（原 38-40 行区域）：

```typescript
/** init 未就绪原因（Rust wxauto://init-fail 载荷；''=无）。
 * 已知值：'licensed' | 'wechat_missing'；P0-3 起还可能是任意错误串
 * （Rust init RPC 错误帧透传，如「初始化失败：sidecar 错误: …」）。 */
export type InitFailReasonName = string;
```

2. `KNOWN_INIT_FAIL_REASONS` 常量保留（getter 判等仍用），注释改为：

```typescript
/** 已知 init 失败原因（getter 判等用；未知错误串不进 getter 但原样透传展示） */
const KNOWN_INIT_FAIL_REASONS: string[] = ['licensed', 'wechat_missing'];
```

3. 事件侧赋值（原 292 行 `this.initFailReason = e.payload.reason as InitFailReasonName;` 不变——类型放宽后天然合法）。确认事件 payload 守卫（`isInitFailPayload` 或等价守卫函数）没有按已知值过滤；若有，放宽为只验 `reason` 是 string。

4. 快照兜底（原 333-340 行）把 `KNOWN_INIT_FAIL_REASONS.includes(failSnap)` 守卫改为非空即收：

```typescript
        const failSnap = await invoke<unknown>('get_init_fail_reason');
        if (
          this.initFailReason === '' &&
          typeof failSnap === 'string' &&
          failSnap !== ''
        ) {
          this.initFailReason = failSnap;
        }
```

（`KNOWN_INIT_FAIL_REASONS` 若因此不再被引用则删除常量；若 getter 仍引用则保留——以编译器为准。）

- [ ] **Step 4: 跑测试确认通过**

```bash
pnpm vitest run src/stores/app.spec.ts
```

Expected: PASS

- [ ] **Step 5: Activation.vue 第 4 态**

`src/views/Activation.vue`：

1. `refreshStatusText` / `refreshLampClass`（66-80 行区域）在 wechatMissing 分支后插入初始化错误分支：

```typescript
/** 初始化错误（P0-3 第 4 态）：RPC 错误帧透传的原始串 */
const initErrorText = computed(() =>
  store.sidecarBooting && !store.needsActivation && !store.wechatMissing && store.initFailReason !== ''
    ? store.initFailReason
    : ''
);
```

`refreshStatusText` 的 else 兜底前加：

```typescript
  else if (initErrorText.value) {
    statusText.value = `初始化错误：${initErrorText.value}——程序文件可能不完整，请重新安装或联系管理员`;
  }
```

`refreshLampClass` 同位加：

```typescript
  else if (initErrorText.value) lampClass.value = 'lamp--orange';
```

2. 若样式表无 `lamp--orange`，检查 `src/views/` 里灯色 class 定义（Overview.vue / Activation.vue 的 style 段），补一条与 `lamp--yellow` 同形态的 orange（如无橙色变量用 `#e37318` 系）。灯色 class 若是全局公共样式，在定义处补。

3. 需要的 import 补齐（`computed` 若未引入从 vue 引）。refreshStatusText/refreshLampClass 的调用点（watch/挂载处）确认覆盖 initFailReason 变化——若现有 watch 源只盯 getter，补 `() => store.initFailReason` watch 或直接在现有 watch 数组加项（看现有实现形态）。

- [ ] **Step 6: Activation.spec 追加第 4 态用例**

`src/views/Activation.spec.ts` 追加（仿 setUnlicensed 模式）：

```typescript
/** 初始化错误现场：Booting + 未知错误串（P0-3 第 4 态） */
async function setInitError() {
  const { useAppStore } = await import('../stores/app');
  const store = useAppStore();
  store.appState = 'SidecarBooting';
  store.initFailReason = '初始化失败：sidecar 错误: [-32603] ModuleNotFoundError: No module named \'requests\'';
  await flushPromises();
  return store;
}

it('初始化错误态显示原始错误串与重装引导', async () => {
  mountActivation();
  await setInitError();
  await flushPromises();
  const text = document.body.innerText;
  expect(text).toContain('ModuleNotFoundError');
  expect(text).toContain('重新安装');
});
```

- [ ] **Step 7: vitest 全量 + vue-tsc**

```bash
pnpm vitest run && pnpm vue-tsc --noEmit -p tsconfig.json
```

Expected: 全 PASS、零类型错

- [ ] **Step 8: Commit**

```bash
git add src/stores/app.ts src/stores/app.spec.ts src/views/Activation.vue src/views/Activation.spec.ts
git commit -m "feat(gui): initFailReason 未知值透传+激活页初始化错误第4态——RPC 错误帧可见(spec P0-3)"
```

---

### Task 6: FileSink 落盘（file_log.rs + 双模式装配）

**Files:**
- Create: `src-tauri/src/file_log.rs`
- Modify: `src-tauri/src/main.rs`（CLI 装配：fmt 层旁挂文件层）
- Modify: `src-tauri/src/gui.rs`（GUI 装配：registry 加文件层）
- Modify: `src-tauri/src/lib.rs`（`pub mod file_log;`——先 grep 现有 mod 声明位置）

**Interfaces:**
- Consumes: `AppLogEntry`/`AppLogLevel`/`AppLogSource`（ui_log.rs；`level.as_str()`/`source.as_str()` 已存在且 `#[allow(dead_code)]` 注释可移除——现在有消费方）
- Produces: `pub fn file_log_layer(dir: Option<PathBuf>) -> Option<FileLogLayer>`（返回 tracing Layer；dir=None 或初始化失败返回 None，调用方不装配——日志系统永不阻断启动）；`pub fn cleanup_old_logs(dir: &Path, keep_days: u64)`（启动清理，幂等，失败静默）

设计：FileSink 走 **tracing Layer** 形态（与 UiLogLayer 平级）——这样 Rust tracing 事件一次 emit 自动三fan-out（fmt stderr / UI ring / 文件），而 **sidecar stderr 行**经 RingStderrSink.consume 时需手动调 file_log 的写入函数（并联 push）。目录 `~/.wxauto-desktop/logs/`，文件名 `app-YYYYMMDD.log`（UTC 天，epoch 秒整除 86400——不引 chrono），utf-8-sig 首写 BOM，追加模式。保留 7 天。

- [ ] **Step 1: 写失败测试**

`src-tauri/src/file_log.rs` 底部 `#[cfg(test)] mod tests`（先建骨架文件再补测试；测试用 tempfile——dev-dependencies 已有）：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// epoch 秒 → UTC YYYYMMDD（不引 chrono 的纯整数换算）
    #[test]
    fn test_epoch_day_filename() {
        assert_eq!(day_stamp(0), "19700101");
        assert_eq!(day_stamp(1), "19700101");
        assert_eq!(day_stamp(86399), "19700101");
        assert_eq!(day_stamp(86400), "19700102");
    }

    /// 一条 entry 落盘：行格式含时间戳/来源/级别/消息；文件名按天
    #[test]
    fn test_format_line_writes_to_daily_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut sink = FileSink::new(dir.path().to_path_buf());
        sink.write(crate::ui_log::AppLogEntry {
            ts: 1000,
            level: crate::ui_log::AppLogLevel::Error,
            source: crate::ui_log::AppLogSource::Sidecar,
            message: "boom 测试".into(),
        });
        let name = format!("app-{}.log", day_stamp(1000 / 1000));
        let content = std::fs::read_to_string(dir.path().join(name)).expect("应写入");
        assert!(content.contains('\u{feff}'), "utf-8-sig BOM 首写");
        assert!(content.contains("[sidecar]"), "来源标记");
        assert!(content.contains("[error]"), "级别小写");
        assert!(content.contains("boom 测试"), "消息保留");
    }

    /// 清理：只删 app-*.log 且只删超龄的
    #[test]
    fn test_cleanup_removes_only_stale_app_logs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let old = dir.path().join(format!("app-{}.log", day_stamp(0)));
        let now_name = format!("app-{}.log", day_stamp(now_secs()));
        std::fs::write(&old, "x").unwrap();
        std::fs::write(dir.path().join(&now_name), "x").unwrap();
        std::fs::write(dir.path().join("config.json"), "x").unwrap();
        cleanup_old_logs(dir.path(), 7);
        assert!(!old.exists(), "超龄删除");
        assert!(dir.path().join(&now_name).exists(), "保留期内不删");
        assert!(dir.path().join("config.json").exists(), "非 app-*.log 不动");
    }

    /// 降级：目录不可写（只读父目录下不存在路径）→ write 静默不抛
    #[test]
    fn test_write_failure_silent() {
        let mut sink = FileSink::new(PathBuf::from("/nonexistent-root/x/y"));
        sink.write(crate::ui_log::AppLogEntry {
            ts: 1,
            level: crate::ui_log::AppLogLevel::Info,
            source: crate::ui_log::AppLogSource::Rust,
            message: "x".into(),
        }); // 不 panic 即通过
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd src-tauri && cargo test file_log
```

Expected: 编译 FAIL——模块不存在

- [ ] **Step 3: 实现 file_log.rs**

```rust
//! 文件日志通道（spec §5 三通道之二）：ring 写入侧并联文件 sink。
//!
//! 设计：
//! - FileSink：`~/.wxauto-desktop/logs/app-YYYYMMDD.log` 追加写，utf-8-sig
//!   （BOM 便于记事本），按天分文件；日期 = epoch 秒 → UTC 天纯整数换算
//!   （不引 chrono）
//! - FileLogLayer：tracing Layer 形态，与 UiLogLayer 平级——Rust 事件
//!   一次 emit 三 fan-out（fmt stderr / UI ring / 文件）
//! - sidecar stderr 行：RingStderrSink.consume 并联调用 FileSink::write
//!   （经全局 OnceLock 句柄——见 install_sidecar_file_sink）
//! - 失败降级：目录创建/写失败 → tracing::warn 一条后降级内存（本模块
//!   任何错误不阻断启动，spec 铁律）
//! - 保留 7 天：启动时 cleanup_old_logs 删 mtime 超龄的 app-*.log

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::ui_log::{AppLogEntry, AppLogLevel, AppLogSource};

/// 日志保留天数（spec §5：7 天）
pub const LOG_KEEP_DAYS: u64 = 7;

/// 当前 epoch 秒（集中一处便于测试注入）
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// epoch 秒 → UTC 天序号（civil-from-days 算法，Howard Hinnant 经典式）
fn day_number(epoch_secs: u64) -> i64 {
    (epoch_secs as i64) / 86400
}

/// UTC 天序号 → "YYYYMMDD"
fn day_stamp_from_number(z: i64) -> String {
    // days → civil（Hinnant 算法）
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}")
}

/// 毫秒时间戳 → "YYYYMMDD"（AppLogEntry.ts 是 ms）
fn day_stamp(ts_ms: u64) -> String {
    day_stamp_from_number(day_number(ts_ms / 1000))
}

/// ms → "HH:MM:SS"（当日秒）
fn time_of_day(ts_ms: u64) -> String {
    let s = (ts_ms / 1000) % 86400;
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// 单行格式（与日志 tab 展示同构）：[UTC 时间] [来源] [级别] 消息
fn format_line(e: &AppLogEntry) -> String {
    format!(
        "[{} {}] [{}] [{}] {}\n",
        day_stamp(e.ts),
        time_of_day(e.ts),
        e.source.as_str(),
        e.level.as_str(),
        e.message
    )
}

/// 文件 sink：目录惰性创建；跨天自动切文件；写失败静默（降级语义）
pub struct FileSink {
    dir: PathBuf,
    state: Mutex<Option<(String, std::fs::File)>>, // (当前天, 句柄)
}

impl FileSink {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            state: Mutex::new(None),
        }
    }

    /// 追加一条。任何 IO 失败静默吞（日志永不阻断业务）+ 一次性 warn。
    pub fn write(&mut self, entry: AppLogEntry) {
        let day = day_stamp(entry.ts);
        let mut guard = match self.state.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let need_open = match guard.as_ref() {
            Some((d, _)) => *d != day,
            None => true,
        };
        if need_open {
            if let Err(e) = std::fs::create_dir_all(&self.dir) {
                tracing::warn!(%e, dir = %self.dir.display(), "日志目录创建失败，本条落盘跳过");
                *guard = None;
                return;
            }
            let path = self.dir.join(format!("app-{day}.log"));
            match OpenOptions::new().create(true).append(true).open(&path) {
                Ok(f) => {
                    // utf-8-sig：新文件首写 BOM（已存在文件长度 0 时也补）
                    let need_bom = f.metadata().map(|m| m.len() == 0).unwrap_or(false);
                    let f = if need_bom {
                        let mut f = f;
                        let _ = f.write_all("\u{feff}".as_bytes());
                        f
                    } else {
                        f
                    };
                    *guard = Some((day.clone(), f));
                }
                Err(e) => {
                    tracing::warn!(%e, path = %path.display(), "日志文件打开失败，本条落盘跳过");
                    *guard = None;
                    return;
                }
            }
        }
        if let Some((_, f)) = guard.as_mut() {
            let _ = f.write_all(format_line(&entry).as_bytes());
            let _ = f.flush();
        }
    }
}

/// 全局文件 sink 句柄（RingStderrSink 并联写入口；None = 未装配/降级关闭）
static SIDECAR_FILE_SINK: OnceLock<Mutex<Option<FileSink>>> = OnceLock::new();

/// 装配 sidecar 行文件通道（GUI/CLI 启动时调用；None 即不装配）
pub fn install_sidecar_file_sink(dir: Option<PathBuf>) {
    if let Some(dir) = dir {
        let _ = SIDECAR_FILE_SINK.set(Mutex::new(Some(FileSink::new(dir))));
    }
}

/// sidecar stderr 行 → 落盘（RingStderrSink.consume 并联调用；未装配 no-op）
pub fn write_sidecar_entry(entry: &AppLogEntry) {
    let Some(cell) = SIDECAR_FILE_SINK.get() else { return };
    let mut guard = match cell.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if let Some(sink) = guard.as_mut() {
        sink.write(entry.clone());
    }
}

/// 清理超龄日志（启动调用；只匹配 app-*.log；失败静默）
pub fn cleanup_old_logs(dir: &Path, keep_days: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let cutoff_days = day_number(now_secs()) - keep_days as i64;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("app-") || !name.ends_with(".log") {
            continue;
        }
        // 文件名里的天序号优先（比 mtime 稳定）；解析失败跳过
        let day = name
            .trim_start_matches("app-")
            .trim_end_matches(".log");
        if day.len() == 8 && day.chars().all(|c| c.is_ascii_digit()) {
            let y: i64 = day[..4].parse().unwrap_or(0);
            let m: i64 = day[4..6].parse().unwrap_or(0);
            let d: i64 = day[6..8].parse().unwrap_or(0);
            if y > 0 && m > 0 {
                // civil → days 逆变换（Hinnant）：先估 era 级
                let yy = if m <= 2 { y - 1 } else { y };
                let mm = if m > 2 { m - 3 } else { m + 9 };
                let dd = d - 1;
                let days = 365 * yy + yy / 4 - yy / 100 + yy / 400 + (153 * mm + 2) / 5 + dd - 719468;
                if days < cutoff_days {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}

/// 默认日志目录：~/.wxauto-desktop/logs（与 config.json 同根，spec §5）
pub fn default_log_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".wxauto-desktop").join("logs"))
}

// ── tracing Layer 形态（Rust 事件三 fan-out 之一）──

/// tracing Layer：事件 → FileSink（内部独立 FileSink，不经全局句柄——
/// Rust 事件与 sidecar 行共用目录，各持句柄跨天各自切文件）
pub struct FileLogLayer {
    sink: Mutex<FileSink>,
}

impl<S> tracing_subscriber::Layer<S> for FileLogLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let level = match *event.metadata().level() {
            tracing::Level::ERROR => AppLogLevel::Error,
            tracing::Level::WARN => AppLogLevel::Warn,
            tracing::Level::INFO => AppLogLevel::Info,
            tracing::Level::DEBUG => AppLogLevel::Debug,
            tracing::Level::TRACE => AppLogLevel::Trace,
        };
        let mut visitor = crate::ui_log::MessageFieldVisitor {
            message: String::new(),
        };
        event.record(&mut visitor);
        let mut guard = match self.sink.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.write(AppLogEntry {
            ts: crate::ui_log::now_ms(),
            level,
            source: AppLogSource::Rust,
            message: visitor.message,
        });
    }
}

/// 装配文件 Layer + 启动清理（GUI/CLI 共用；目录拿不到返回 None 不装配）
pub fn file_log_layer() -> Option<FileLogLayer> {
    let dir = default_log_dir()?;
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(%e, "日志目录创建失败，文件日志降级关闭");
        return None;
    }
    cleanup_old_logs(&dir, LOG_KEEP_DAYS);
    Some(FileLogLayer {
        sink: Mutex::new(FileSink::new(dir)),
    })
}
```

注意：`MessageFieldVisitor` 当前是 ui_log.rs 私有 struct——需在 ui_log.rs 把 `struct MessageFieldVisitor` 改 `pub struct`（字段 `message` 也 `pub`），或提供 `pub fn extract_message(event) -> String` 帮助函数（推荐后者，一处封装）。实现时二选一，全仓 grep 确认无重名冲突。`AppLogLevel::as_str`/`AppLogSource::as_str` 的 `#[allow(dead_code)]` 可移除（本模块成为消费方）。

- [ ] **Step 4: 跑 file_log 测试**

```bash
cd src-tauri && cargo test file_log
```

Expected: 4 PASS

- [ ] **Step 5: 装配（main.rs CLI + gui.rs GUI + RingStderrSink 并联）**

1. `src-tauri/src/lib.rs`：mod 声明区加 `pub mod file_log;`

2. `src-tauri/src/main.rs` run_cli：fmt().init 改 registry 组合（对齐 gui.rs 形态）：

```rust
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let mut registry = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(std::io::stderr().is_terminal())
                .with_writer(std::io::stderr),
        )
        .with(env_filter);
    if let Some(layer) = wxauto_desktop::file_log::file_log_layer() {
        registry = registry.with(layer);
    }
    registry.init();
    // sidecar stderr 行落盘通道（CLI 的 sidecar stderr 是 inherit 不进 ring，
    // 但文件通道仍要装——诊断价值对 CLI 等价，spec §5）
    wxauto_desktop::file_log::install_sidecar_file_sink(wxauto_desktop::file_log::default_log_dir());
```

（注意 registry 类型是泛型叠加，`if let Some` 分支加层后类型不同——用如下两段式避免类型漂移，实现时按编译器提示调整：

```rust
    let file_layer = wxauto_desktop::file_log::file_log_layer();
    // 无文件层：仅 fmt+filter
    match file_layer {
        Some(layer) => tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_ansi(std::io::stderr().is_terminal()).with_writer(std::io::stderr))
            .with(layer)
            .with(env_filter)
            .init(),
        None => tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_ansi(std::io::stderr().is_terminal()).with_writer(std::io::stderr))
            .with(env_filter)
            .init(),
    }
```

3. `src-tauri/src/gui.rs` run_gui：registry 链（54-62 行）加一层（同样两段式或按编译器）：

```rust
    let file_layer = crate::file_log::file_log_layer();
    // ... with(UiLogLayer::new(...)) 之后：
    //     .with(file_layer)（Some 时）
```

并在 setup 段（ring 建立后）加：

```rust
    crate::file_log::install_sidecar_file_sink(crate::file_log::default_log_dir());
```

4. `src-tauri/src/ui_log.rs` RingStderrSink::consume（205-222 行）：push ring 与 try_send 之间加：

```rust
        crate::file_log::write_sidecar_entry(&entry);
```

（consume 在 bin 目标 gui.rs 里，路径按实际 crate 名——ui_log.rs 属于 bin crate 时用 `crate::file_log`；若 ui_log 在 lib 里用 `crate::file_log` 同样成立，lib.rs 声明 pub mod 后自洽。）

- [ ] **Step 6: 全量 cargo + fmt**

```bash
cd src-tauri && cargo test && cargo fmt
```

Expected: 全 PASS（103+4+新增装配零破坏）；smoke 冒烟本机可跑则跑：

```bash
bash scripts/smoke_cli.sh 2>&1 | tail -5
```

Expected: PASS（CLI 路径现在含文件日志装配，验证不破坏启动）

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/file_log.rs src-tauri/src/lib.rs src-tauri/src/main.rs src-tauri/src/gui.rs src-tauri/src/ui_log.rs
git commit -m "feat(log): FileSink 落盘通道——~/.wxauto-desktop/logs 按天分文件+7天清理+GUI/CLI 双装配+sidecar 行并联(spec P1-2)"
```

---

### Task 7: bootstrap.log 自检报告 + invoke 失败补日志 + 文档收尾

**Files:**
- Modify: `src-tauri/src/sidecar/mod.rs`（spawn_default_sunk 探测链写报告）
- Modify: `src-tauri/src/commands.rs`（activate_license 失败路径 tracing::error）
- Modify: `WINDOWS-安装打包流程.txt`（§10 日志口径兑现 + §9 follow-up 留档）
- Test: `src-tauri/tests/sidecar_stderr.rs` 或新 `src-tauri/tests/bootstrap_log.rs`

**Interfaces:**
- Consumes: `crate::cli::{parse_sidecar_cmd, find_bundled_sidecar, resolve_sidecar_workdir, BUNDLED_SIDECAR_NAMES}`（cli.rs:15-96）；Task 6 的 `file_log::default_log_dir`
- Produces: `~/.wxauto-desktop/logs/bootstrap.log`（覆盖写，一次启动一份）；探测行格式 `[YYYY-MM-DD HH:MM:SS] [probe] …`——运维排障读这个文件

- [ ] **Step 1: 写失败测试**

新文件 `src-tauri/tests/bootstrap_log.rs`：

```rust
//! bootstrap.log 探测报告测试（spec §6）：spawn_default_sunk 的探测链
//! 每一级结果写入 logs/bootstrap.log（覆盖写）。用 env WXAUTO_SIDECAR_CMD
//! 注入可命中路径驱动「env 命中」分支；报告写入函数本身直测。

use wxauto_desktop::bootstrap;

/// 报告函数直测：给定目录，写一行 → 覆盖语义（第二次启动只剩第二行）
#[test]
fn test_bootstrap_report_overwrites_per_startup() {
    let dir = tempfile::tempdir().expect("tempdir");
    bootstrap::start_report(&dir.path().join("bootstrap.log"));
    bootstrap::probe_line(&dir.path().join("bootstrap.log"), "env WXAUTO_SIDECAR_CMD: 未设置");
    bootstrap::probe_line(&dir.path().join("bootstrap.log"), "bundled: 未命中");
    let content = std::fs::read_to_string(dir.path().join("bootstrap.log")).expect("应写入");
    assert!(content.contains("[probe]"), "探测行前缀");
    assert!(content.contains("env WXAUTO_SIDECAR_CMD: 未设置"));
    assert!(content.contains("bundled: 未命中"));
    // 二次启动覆盖
    bootstrap::start_report(&dir.path().join("bootstrap.log"));
    let content2 = std::fs::read_to_string(dir.path().join("bootstrap.log")).expect("应写入");
    assert!(content2.contains("[probe]") && !content2.contains("bundled: 未命中") || content2.lines().count() <= 2,
        "覆盖语义：旧启动内容不残留（新文件只剩 start 行）");
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd src-tauri && cargo test --test bootstrap_log
```

Expected: 编译 FAIL——`wxauto_desktop::bootstrap` 不存在

- [ ] **Step 3: 实现 bootstrap 模块**

新文件 `src-tauri/src/bootstrap.rs`：

```rust
//! bootstrap 自检报告（spec §6）：每次启动把 sidecar 探测链各级结果
//! 覆盖写 `~/.wxauto-desktop/logs/bootstrap.log`——装机远程排障
//! 「报一个文件」即可。

use std::io::Write;
use std::path::Path;

/// 启动报告头（覆盖写 = 一次启动一份）
pub fn start_report(path: &Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let ts = crate::ui_log::now_ms();
    let _ = std::fs::write(path, format!("[{}] [probe] ===== 启动 =====\n", stamp(ts)));
}

/// 追加一行探测结果
pub fn probe_line(path: &Path, msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(path) {
        let _ = writeln!(f, "[{}] [probe] {}", stamp(crate::ui_log::now_ms()), msg);
    }
}

/// ms → "YYYY-MM-DD HH:MM:SS"（UTC；与 file_log 同算法族）
fn stamp(ts_ms: u64) -> String {
    let day = ts_ms / 1000 / 86400;
    let s = (ts_ms / 1000) % 86400;
    // civil-from-days（与 file_log.rs 同式；此处内联避免跨模块私有依赖）
    let z = day as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        s / 3600,
        (s % 3600) / 60,
        s % 60
    )
}
```

`lib.rs` 加 `pub mod bootstrap;`。

`sidecar/mod.rs` spawn_default_sunk（107-129 行区域）改（探测链每分支写报告）：

```rust
    pub async fn spawn_default_sunk(
        stderr_sink: Option<Arc<dyn StderrSink>>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // bootstrap 自检报告（spec §6）：探测链各级结果留痕
        let report = crate::file_log::default_log_dir()
            .map(|d| d.join("bootstrap.log"));
        if let Some(p) = &report {
            crate::bootstrap::start_report(p);
        }
        let probe = |msg: String| {
            if let Some(p) = &report {
                crate::bootstrap::probe_line(p, &msg);
            }
        };
        let mut cmd = if let Ok(cmd_str) = std::env::var("WXAUTO_SIDECAR_CMD") {
            let (program, args) = crate::cli::parse_sidecar_cmd(&cmd_str);
            probe(format!("env WXAUTO_SIDECAR_CMD 命中: {program}"));
            let mut c = Command::new(program);
            c.args(args);
            c
        } else if let Some(bundled) = crate::cli::find_bundled_sidecar() {
            probe(format!("bundled 命中: {bundled}"));
            let c = Command::new(&bundled);
            tracing::info!(path = %bundled, "使用内置 sidecar（安装态）");
            c
        } else {
            probe(format!(
                "⚠ 回退 python3——bundled 候选 {:?} 均不存在, 安装可能不完整",
                crate::cli::BUNDLED_SIDECAR_NAMES
            ));
            let mut c = Command::new("python3");
            c.arg("sidecar-python/sidecar.py");
            if let Some(root) = crate::cli::resolve_sidecar_workdir() {
                c.current_dir(root);
            }
            c
        };
```

（do_spawn 内 `let pid = child.id().unwrap_or(0);` 之后可加 `probe(format!("spawn 完成 PID={pid}"));`——probe 闭包需移进或重复；实现时以借用检查为准，必要时把 report/probe 提为自由函数调用。）

- [ ] **Step 4: 跑 bootstrap 测试**

```bash
cd src-tauri && cargo test --test bootstrap_log
```

Expected: PASS

- [ ] **Step 5: activate_license 失败补日志**

`src-tauri/src/commands.rs` activate_license（144 行 map_err 处）：

```rust
        .await
        .map_err(|e| {
            let msg = format!("激活请求失败：{e}");
            tracing::error!(%msg, "activate_license 命令失败");
            msg
        })?;
```

（retry_init 失败路径同理检查一遍——`ctx.ready().await?` 的 Err 已有装配层 tracing，无需重复。）

- [ ] **Step 6: 全量三线 + fmt**

```bash
cd src-tauri && cargo test && cargo fmt
cd .. && python3 -m pytest sidecar-python/ -q && pnpm vitest run
```

Expected: 全绿

- [ ] **Step 7: 文档收尾**

`WINDOWS-安装打包流程.txt`：
1. §10 故障速查表「看 %USERPROFILE%\.wxauto-desktop\ 日志」条目兑现——改为明确路径清单：`logs\app-YYYYMMDD.log`（运行日志，7 天）/ `logs\bootstrap.log`（启动探测链，最近一次）；
2. §9 follow-up 追加一条：「2026-09-09 真机日志 07:05:12『GUI 装配完成/使用内置 sidecar』双行重复——快照重叠既有 follow-up（AppLog 快照与事件短暂重叠），另行处置」。

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/bootstrap.rs src-tauri/src/lib.rs src-tauri/src/sidecar/mod.rs src-tauri/src/commands.rs WINDOWS-安装打包流程.txt src-tauri/tests/bootstrap_log.rs
git commit -m "feat(diag): bootstrap.log 启动探测链自检报告+activate 失败补日志+运维文档日志口径兑现(spec P1-3)"
```

---

### Task 8: CI 验证与真机验收清单（收尾）

**Files:**
- Modify: 无代码（验证任务）
- Produce: CI run 绿 + 验收记录

**Interfaces:**
- Consumes: Task 1-7 全部
- Produces: 可发版的安装包

- [ ] **Step 1: 本机全量三线终验**

```bash
cd /home/working/lyagent/desktop
cd src-tauri && cargo test && cargo fmt --check && cargo clippy -- -D warnings 2>&1 | tail -3
cd .. && python3 -m pytest sidecar-python/ -q
pnpm vitest run
pnpm vue-tsc --noEmit -p tsconfig.json
```

Expected: 全绿（clippy 若有存量告警与本次无关可豁免，记录即可）

- [ ] **Step 2: push 触发 CI**

```bash
git push origin main
```

Expected: GitHub Actions windows build 绿——冻结步骤日志可见三条冒烟输出（`smoke[1/3]`/`smoke[2/3]`/`smoke[3/3]`）+ 体积打印；artifact 体积 ~35MB。

（push 凭据遇 `VSCODE_GIT_IPC_HANDLE` 死 socket 坑：`ls -t /run/user/1000/vscode-git-*.sock` 探活 + askpass 验证后带 env push——memory 既有解法。）

- [ ] **Step 3: 更新运维文档验收状态**

CI 绿后在 `WINDOWS-安装打包流程.txt` §3 检查清单勾选新条目（若文档结构需要）；memory 待办记录真机验收项：

- 装机（无 Python）安装 → 激活页输码 → 授权卡转绿（本次事故回归路径）
- `~/.wxauto-desktop/logs/` 三个文件齐（app-*.log / bootstrap.log）
- 故意改坏 env WXAUTO_SIDECAR_CMD → bootstrap.log 有回退告警行

- [ ] **Step 4: Commit（如有文档改动）+ 总结**

```bash
git status --short
# 有改动则：
git add -A && git commit -m "docs: 打包可诊断性收尾——CI 冒烟记录与真机验收清单"
```

## Self-Review 记录

- **Spec 覆盖**：§1→Task 1；§2→Task 2；§3→Task 3+5；§4→Task 4；§5→Task 6；§6→Task 6(装配)+Task 7；§7 不做清单→未立任务（正确）。日志 tab 重复行修复在 spec §7 明确不做——无任务，正确。
- **占位符扫描**：无 TBD/TODO；所有代码块完整可执行；Task 6 Step 5 的 registry 两段式给了具体形态并注明按编译器调整（这是 Rust 泛型叠加的现实约束，非占位）。
- **类型一致性**：`sidecar_log.log(level, msg)` Python 侧唯一入口；Rust `write_sidecar_entry(&AppLogEntry)` / `install_sidecar_file_sink(Option<PathBuf>)` / `file_log_layer() -> Option<FileLogLayer>` / `bootstrap::{start_report, probe_line}` 在 Task 6/7 定义且消费一致；前端 `InitFailReasonName = string` 放宽后 292 行 `as InitFailReasonName` 天然合法。`MessageFieldVisitor` 私有→Task 6 Step 3 注明二选一处理（推荐 extract helper），Task 6 内自洽。
- **跨任务依赖方向**：Task 2 冒烟依赖 Task 1（同一 PR 内顺序执行无问题）；Task 5 依赖 Task 3 的 reason 格式（`初始化失败：` 前缀）——两边都写死了同一格式串。
