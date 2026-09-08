# desktop 激活页实现计划（wxautox4 内核授权）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** desktop/ 新增激活视图，打通 wxautox4 授权的检测 → 激活 → 初始化重试闭环，init 失败三态化（licensed / wechat_missing / 其他）精准引导。

**Architecture:** 四层薄改与既有架构同构——sidecar 新增 `wx.activate`（闭源库 `authenticate(code)` 校验）+ `wx.init` 返回 `failReason`；Supervisor `init_sequence` 解析原因并经 event_sink 发 `init_fail` 帧桥转 `wxauto://init-fail` 事件，新增公开 `retry_init()`；Rust 命令 `activate_license`（30s 超时直连 + 成功即重试 init 一次）与 `retry_init`；前端 Activation.vue + 顶栏横幅 + 一次性自动跳转 + 概览授权卡。激活状态权威在 wxautox4 `check_license()` 现查，desktop 零新增存储。

**Tech Stack:** Tauri 2（Rust）/ Vue 3 + TS + TDesign + pinia / Python sidecar（JSON-RPC over stdio）/ pytest + cargo test + vitest

**Spec:** `docs/superpowers/specs/2026-09-08-license-activation-page-design.md`（本计划与 spec 同仓 desktop/）

## Global Constraints

- 中文注释（代码注释含中文说明）——承继仓库既有约定
- Rust 禁 `unwrap()`/`expect()`（`#[cfg(test)]` 测试代码内允许，与既有测试一致）
- TS 禁 `any` / `as any`，用 `unknown` + 类型守卫
- CLI `--cli` 模式零回归（CLI 无 event_sink，emit 走丢弃分支；init 序列语义不变）
- TDD：每任务先写测试看红，再实现看绿，然后 commit
- 测试命令（在 desktop/ 下）：
  - pytest：`cd sidecar-python && python3 -m pytest test_methods.py -k <关键词> -v`
  - Rust：`cd src-tauri && cargo test --test <文件名>`（单文件）或 `cargo test`（全量）
  - 前端：`pnpm vitest run <文件路径>`；类型检查 `pnpm typecheck`
- t-form 必绑 `:data`（TDesign FormItem 校验取值是 form.data[name] 而非 v-model——2026-09-08 事故，见 Settings.spec.ts）

## 文件结构（改动全景）

```
sidecar-python/
  methods.py                 改：_activate 新方法 + _init failReason 三态 + mock 表加 wx.activate
  test_methods.py            改：激活/init 三态测试 + mock 覆盖清单加 wx.activate
src-tauri/src/
  sidecar/spec.rs            改：ACTIVATE 常量 + InitResult.failReason 字段
  state.rs                   改：init_sequence 三态解析 + init_fail 帧发射 + retry_init 公开方法
  ui_events.rs               改：EVENT_INIT_FAIL 常量 + forward_event_frame 转发 arm
  commands.rs                改：activate_license / retry_init 两命令
  gui.rs                     改：generate_handler! 注册两命令
src-tauri/tests/
  license_init.rs            新：Supervisor init_fail / retry_init 集成测试
src/
  stores/app.ts              改：activeView/initFailReason 状态 + 三 getter + switchView/
                               activateLicense/retryInit actions + init-fail 监听
  stores/app.spec.ts         改：getter 推导 + 监听落 state 测试
  views/Activation.vue       新：激活视图
  views/Activation.spec.ts   新：组件测试
  App.vue                    改：菜单项/横幅/自动跳转/view 迁 store
  App.spec.ts                新：自动跳转回归
  views/Overview.vue         改：授权卡
WINDOWS-安装打包流程.txt      改：装机接线补激活步骤
```

视图切换归 store（`activeView`）——Overview「去激活」与 Activation「跳回概览」都要跨视图导航，App.vue 本地 ref 不可达；这是对 spec「App.vue view.value=...」的实现细化，语义不变。

---

### Task 1: sidecar `wx.activate` 方法

**Files:**
- Modify: `sidecar-python/methods.py`（新增 `_activate`；`_METHODS` 表注册；`_mock_dispatch` 表加条目）
- Test: `sidecar-python/test_methods.py`

**Interfaces:**
- Consumes: wxautox4 `wxautox4.utils.useful.authenticate(code)` / `check_license()`（闭源库，测试经 sys.modules 伪造）
- Produces: RPC 方法 `wx.activate`，params `{"code": string}`，返回 `{"ok": bool, "message": string}`；mock 模式下 `code == "MOCK-ACTIVATION"` 即成功（Task 5 Rust 命令、Task 7 store 依赖此契约）

- [ ] **Step 1: 写失败测试**

在 `test_methods.py` 末尾（`test_mock_init_followed_by_other_methods` 之后）追加：

```python
# ══════════ wx.activate（激活码认证）══════════


def _install_fake_wxautox4(monkeypatch, licensed=True, authenticate_result=True):
    """伪造 wxautox4 包：sys.modules 预置三模块 + 顶层属性挂接。

    返回 calls 字典记录 authenticate 实参（断言激活码透传）。
    """
    import types

    calls = {"authenticate": []}

    def _authenticate(code):
        calls["authenticate"].append(code)
        return authenticate_result

    useful = types.ModuleType("wxautox4.utils.useful")
    useful.check_license = lambda: licensed
    useful.authenticate = _authenticate
    utils = types.ModuleType("wxautox4.utils")
    utils.useful = useful
    top = types.ModuleType("wxautox4")
    top.utils = utils
    top.WeChat = object  # wx.activate 不触 WeChat，占位即可
    top.WxParam = types.SimpleNamespace()

    monkeypatch.setitem(sys.modules, "wxautox4", top)
    monkeypatch.setitem(sys.modules, "wxautox4.utils", utils)
    monkeypatch.setitem(sys.modules, "wxautox4.utils.useful", useful)
    return calls


def test_activate_empty_code_short_circuits():
    """空码前置拦截：不触 wxautox4 导入即返回失败"""
    r = _dispatch("wx.activate", {"code": "  "}, None)
    assert r == {"ok": False, "message": "激活码不能为空"}


def test_activate_success_roundtrip(monkeypatch):
    calls = _install_fake_wxautox4(monkeypatch, licensed=True, authenticate_result=True)
    r = _dispatch("wx.activate", {"code": " ABC-123 "}, None)
    assert r["ok"] is True
    assert calls["authenticate"] == ["ABC-123"]  # strip 后透传
    # 激活成功 → 立即回查 check_license 确认
    assert r["message"] == "激活成功"


def test_activate_invalid_code(monkeypatch):
    _install_fake_wxautox4(monkeypatch, licensed=False, authenticate_result=False)
    r = _dispatch("wx.activate", {"code": "BAD"}, None)
    assert r["ok"] is False
    assert "无效" in r["message"]


def test_activate_accepted_but_not_effective(monkeypatch):
    """authenticate 过但回查仍 false：不静默吞（spec 错误表边界条）"""
    _install_fake_wxautox4(monkeypatch, licensed=False, authenticate_result=True)
    r = _dispatch("wx.activate", {"code": "X"}, None)
    assert r["ok"] is False
    assert "重启应用" in r["message"]


def test_activate_mock_mode():
    """mock 表：MOCK-ACTIVATION 成功、其余失败（Linux CI 全链路依赖）"""
    ok = _dispatch("wx.activate", {"code": "MOCK-ACTIVATION"}, None, mock=True)
    assert ok["ok"] is True
    bad = _dispatch("wx.activate", {"code": "WRONG"}, None, mock=True)
    assert bad["ok"] is False
```

同文件顶部确认已有 `import sys`（若无需补：`test_methods.py` 第 20 行附近已有 `import` 区，补 `import sys`）。

再改 `test_mock_covers_all_non_init_methods`：`methods_list` 列表加 `"wx.activate"`，`params` 字典加 `"code": "MOCK-ACTIVATION"`。

- [ ] **Step 2: 跑测试确认红**

```
cd sidecar-python && python3 -m pytest test_methods.py -k activate -v
```
预期：`test_activate_empty_code_short_circuits` 等以 `SidecarError: 未知或未实现的方法: wx.activate` 失败。

- [ ] **Step 3: 最小实现**

`methods.py` 的 `_init` 定义之后新增：

```python
def _activate(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """wx.activate：authenticate(code) 激活 → 立即 check_license() 回查确认

    激活状态由 wxautox4 自持久化（一机一码、跨重启有效），本方法无副作用存储。
    """
    code = (params.get("code") or "").strip()
    if not code:
        return {"ok": False, "message": "激活码不能为空"}
    # 真实导入（仅 Windows + 已 pip install wxautox4 时可达）
    from wxautox4.utils.useful import authenticate  # noqa: PLC0415 — 延迟导入是硬要求

    if not authenticate(code):
        return {"ok": False, "message": "激活失败：激活码无效或已过期"}
    # 回查确认：authenticate 过但状态未生效的极端情况不静默吞
    from wxautox4.utils.useful import check_license  # noqa: PLC0415

    if not check_license():
        return {"ok": False, "message": "激活码已接受但授权状态未生效，请重启应用"}
    return {"ok": True, "message": "激活成功"}
```

`_mock_dispatch` 的 `table` 字典加一行（表内 params 可用，沿用 `chat.search` 的写法）：

```python
        "wx.activate": {
            "ok": params.get("code") == "MOCK-ACTIVATION",
            "message": "模拟激活成功" if params.get("code") == "MOCK-ACTIVATION" else "激活失败：激活码无效或已过期",
        },
```

`_METHODS` 字典加：`"wx.activate": _activate,`（紧跟 `"wx.init": _init,` 之后）。

- [ ] **Step 4: 跑测试确认绿**

```
cd sidecar-python && python3 -m pytest test_methods.py -v
```
预期：全绿（含既有 800 行存量）。

- [ ] **Step 5: Commit**

```
git add sidecar-python/methods.py sidecar-python/test_methods.py
git commit -m "feat(sidecar): wx.activate 方法——authenticate 激活+回查确认+mock 三态"
```

---

### Task 2: sidecar `_init` 失败三态化（failReason）

**Files:**
- Modify: `sidecar-python/methods.py`（`_init` 真实分支改造）
- Test: `sidecar-python/test_methods.py`

**Interfaces:**
- Consumes: Task 1 的 `_install_fake_wxautox4` helper（本任务扩展 `wechat_ok` 参数）
- Produces: `wx.init` 返回值新增可选字段 `failReason: "licensed" | "wechat_missing"`——缺省（成功）不带该字段；Task 3 Rust 契约与 Task 4 Supervisor 解析依赖。优先级：未授权恒 `licensed`（可行动根因），授权过但微信没开才是 `wechat_missing`

- [ ] **Step 1: 写失败测试**

`test_methods.py` 的 `test_wx_init_mock_mode` 之后追加（复用 Task 1 helper，先给它加 `wechat_ok=True` 形参——见 Step 3）：

```python
# ══════════ wx.init failReason 三态 ═════════


@pytest.fixture(autouse=True)
def _reset_init_globals(monkeypatch):
    """_init 写模块级 _instance/_license_ok——用例间隔离，防跨测试泄漏"""
    monkeypatch.setattr(methods, "_instance", None)
    monkeypatch.setattr(methods, "_license_ok", False)
    yield


def test_init_real_path_licensed_ok(monkeypatch):
    """授权过 + 微信在：无 failReason 字段"""
    _install_fake_wxautox4(monkeypatch, licensed=True, wechat_ok=True)
    r = _dispatch("wx.init", {}, None)
    assert r["licensed"] is True
    assert "failReason" not in r


def test_init_real_path_unlicensed(monkeypatch):
    """未授权：failReason=licensed（WeChat 同失败也不改口径——授权是可行动根因）"""
    _install_fake_wxautox4(monkeypatch, licensed=False, wechat_ok=False)
    r = _dispatch("wx.init", {}, None)
    assert r["licensed"] is False
    assert r["failReason"] == "licensed"


def test_init_real_path_wechat_missing(monkeypatch):
    """授权过 + 微信未开（中英两版本兜底都抛）：failReason=wechat_missing"""
    _install_fake_wxautox4(monkeypatch, licensed=True, wechat_ok=False)
    r = _dispatch("wx.init", {}, None)
    assert r["licensed"] is True
    assert r["failReason"] == "wechat_missing"
```

注意：`_install_fake_wxautox4` 需 `import methods` 在测试文件可用（文件已有 `import methods` 用法，见 `test_mock_unknown_raises`）。

- [ ] **Step 2: 跑测试确认红**

```
cd sidecar-python && python3 -m pytest test_methods.py -k "init_real_path or wx_init" -v
```
预期：三个新用例失败——`_install_fake_wxautox4` 收到意外 kwarg `wechat_ok`（TypeError）。

- [ ] **Step 3: 最小实现**

Task 1 的 `_install_fake_wxautox4` 签名加 `wechat_ok=True`，`top.WeChat = object` 替换为：

```python
    class _FakeWeChat:
        def __init__(self, version=None):
            if not wechat_ok:
                raise RuntimeError("微信窗口未找到")

    top.WeChat = _FakeWeChat
```

`methods.py` `_init` 真实分支改造（mock 分支不动）：

```python
    # 全局参数（spec 附录 A 已验证配置）
    WxParam.MESSAGE_HASH = True
    WxParam.FORCE_MESSAGE_XBIAS = True
    WxParam.CHAT_WINDOW_SIZE = (1500, 6000)
    WxParam.DEFAULT_MESSAGE_YBIAS = 40

    _license_ok = bool(check_license())
    # 失败三态：未授权恒 licensed（可行动根因优先）；授权过但微信未开才是
    # wechat_missing——旧版两版本兜底都抛会整体 RPC 报错，现降为带原因返回
    fail_reason = None if _license_ok else "licensed"
    try:
        _instance = WeChat(version="微信")
    except Exception:  # noqa: BLE001 — 国际版微信兜底（参考项目验证的双版本尝试）
        try:
            _instance = WeChat(version="WeChat")
        except Exception:  # noqa: BLE001
            if _license_ok:
                fail_reason = "wechat_missing"
    result = {
        "licensed": _license_ok,
        "wxid": getattr(_instance, "wxid", ""),
        "nickname": getattr(_instance, "nickname", ""),
    }
    if fail_reason:
        result["failReason"] = fail_reason
    return result
```

（`_instance` 为 None 时 `getattr(None, "wxid", "")` 得 `""`——wechat_missing 场景 wxid 留空，Rust 侧不消费。）

- [ ] **Step 4: 跑测试确认绿**

```
cd sidecar-python && python3 -m pytest test_methods.py -v
```
预期：全绿。

- [ ] **Step 5: Commit**

```
git add sidecar-python/methods.py sidecar-python/test_methods.py
git commit -m "feat(sidecar): wx.init 失败三态化——failReason licensed/wechat_missing"
```

---

### Task 3: Rust spec 契约 + Supervisor init_fail 帧 / retry_init

**Files:**
- Modify: `src-tauri/src/sidecar/spec.rs`（ACTIVATE 常量 + InitResult.failReason）
- Modify: `src-tauri/src/state.rs`（init_sequence 三态解析 + retry_init）
- Test: `src-tauri/tests/license_init.rs`（新）

**Interfaces:**
- Consumes: Task 2 的 `wx.init` failReason 契约、Task 1 的 `wx.activate` 返回 `{ok, message}`
- Produces:
  - `spec.rs`：`methods::ACTIVATE = "wx.activate"`；`InitResult { licensed: bool, wxid: String, nickname: String, failReason: Option<String> }`（serde default None 向后兼容旧 sidecar）
  - `state.rs`：`Supervisor::retry_init(&self)`（pub async，重跑 init 序列；Task 5 命令层调用）
  - 事件帧：init 不就绪时经 event_sink 发 `{"kind":"event","type":"init_fail","data":{"reason": <string>}}`（Task 4 桥转前端）

- [ ] **Step 1: 写失败测试**

`src-tauri/tests/license_init.rs`（新文件）：

```rust
//! 激活链路集成测试：init 失败三态 → init_fail 帧；retry_init 闭环。
//! 复用 state_machine.rs 的剧本 sidecar 策略（Linux 无 wxautox4）。

use std::sync::{Arc, Mutex as StdMutex};

use serde_json::Value;
use tokio::sync::Mutex;

use wxauto_desktop::sidecar::SidecarHandle;
use wxauto_desktop::state::{AppState, AppStateMachine, Supervisor};
use wxauto_desktop::wx::listener::ListenerRegistry;
use wxauto_desktop::wx::WxSession;

/// 未授权剧本：wx.init 回 licensed=false + failReason=licensed
const UNLICENSED_SCRIPT: &str = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    if req.get("method") == "wx.init":
        out = {"licensed": False, "wxid": "", "nickname": "", "failReason": "licensed"}
    elif req.get("method") == "wx.is_online":
        out = {"online": True}
    else:
        out = {"ok": True}
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": out}), flush=True)
"#;

/// 微信未开剧本：licensed=true + failReason=wechat_missing
const WECHAT_MISSING_SCRIPT: &str = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    if req.get("method") == "wx.init":
        out = {"licensed": True, "wxid": "", "nickname": "", "failReason": "wechat_missing"}
    elif req.get("method") == "wx.is_online":
        out = {"online": True}
    else:
        out = {"ok": True}
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": out}), flush=True)
"#;

/// 就绪剧本：wx.init 回 licensed=true（无 failReason）
const READY_SCRIPT: &str = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    if req.get("method") == "wx.init":
        out = {"licensed": True, "wxid": "wxid_t", "nickname": "测试"}
    elif req.get("method") == "wx.is_online":
        out = {"online": True}
    else:
        out = {"ok": True}
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": out}), flush=True)
"#;

/// 装配剧本 sidecar + Supervisor（event_sink 收集帧；不跑 run 循环，
/// 直接驱动 retry_init——确定性无退避时序干扰）
async fn make_supervisor(script: &str) -> (Arc<Supervisor>, Arc<AppStateMachine>, Arc<StdMutex<Vec<Value>>>) {
    let handle = SidecarHandle::spawn_with_python(script, &[])
        .await
        .expect("spawn 失败");
    let session = Arc::new(WxSession::new(Arc::new(Mutex::new(handle))));
    let listeners = Arc::new(ListenerRegistry::new(session.clone()));
    let state = Arc::new(AppStateMachine::new().await);
    let events: Arc<StdMutex<Vec<Value>>> = Arc::new(StdMutex::new(Vec::new()));
    let ev = events.clone();
    let sup = Supervisor::new(state.clone(), session, listeners)
        .with_event_sink(Box::new(move |v: Value| {
            ev.lock().unwrap().push(v);
        }));
    (Arc::new(sup), state, events)
}

#[tokio::test]
async fn unlicensed_init_stays_booting_and_emits_init_fail() {
    let (sup, state, events) = make_supervisor(UNLICENSED_SCRIPT).await;
    sup.retry_init().await;
    assert_eq!(state.state().await, AppState::SidecarBooting, "未授权必须留 Booting");
    let frames = events.lock().unwrap();
    let hit = frames
        .iter()
        .any(|f| f["type"] == "init_fail" && f["data"]["reason"] == "licensed");
    assert!(hit, "应收 init_fail 帧 reason=licensed，实收: {frames:?}");
}

#[tokio::test]
async fn wechat_missing_init_stays_booting_with_reason() {
    let (sup, state, events) = make_supervisor(WECHAT_MISSING_SCRIPT).await;
    sup.retry_init().await;
    // licensed=true 但微信未开：同样不可进 WxInit
    assert_eq!(state.state().await, AppState::SidecarBooting);
    let frames = events.lock().unwrap();
    let hit = frames
        .iter()
        .any(|f| f["type"] == "init_fail" && f["data"]["reason"] == "wechat_missing");
    assert!(hit, "应收 init_fail 帧 reason=wechat_missing，实收: {frames:?}");
}

#[tokio::test]
async fn ready_script_retry_init_reaches_ready() {
    let (sup, state, _events) = make_supervisor(READY_SCRIPT).await;
    sup.retry_init().await;
    assert_eq!(state.state().await, AppState::Ready, "授权+微信在 → Ready");
}
```

（若 `AppStateMachine::new()` 无 `.await`——按编译器提示去掉；它是同步构造。）

- [ ] **Step 2: 跑测试确认红**

```
cd src-tauri && cargo test --test license_init
```
预期：编译失败——`retry_init` 方法不存在。

- [ ] **Step 3: 最小实现**

**spec.rs**：`methods` 常量块 `INIT` 之后加：

```rust
    pub const ACTIVATE: &str = "wx.activate";
```

`InitResult` 加字段：

```rust
pub struct InitResult {
    pub licensed: bool,
    pub wxid: String,
    pub nickname: String,
    /// 失败三态（sidecar Task 2 契约）：licensed=未授权 / wechat_missing=微信未开；
    /// 旧 sidecar 二进制无此字段——default None 向后兼容
    #[serde(default)]
    pub fail_reason: Option<String>,
}
```

（文件头注释「20 个」改「21 个」。`test_method_constants` 加 `assert_eq!(methods::ACTIVATE, "wx.activate");`。）

**state.rs**：`init_sequence`（345 行起）替换前半段——`licensed` 判定改为三态：

```rust
    /// init 序列：wx.init（licensed 且无 failReason → WxInit）→ resync 监听 → Ready + status 事件。
    /// 未授权 / 微信未开 / init 报错：留在 Booting；Ok 且带原因时发 init_fail 帧
    /// 引导前端（RPC 失败不发——那是 sidecar 死亡，归 Supervisor 重启域）。
    async fn init_sequence(&self) {
        let init = self.direct_wx_init().await.ok();
        let licensed = init
            .as_ref()
            .and_then(|v| v["licensed"].as_bool())
            .unwrap_or(false);
        let fail_reason = init
            .as_ref()
            .and_then(|v| v["failReason"].as_str())
            .map(str::to_string);
        if !licensed || fail_reason.is_some() {
            // 未授权缺 failReason（旧 sidecar）也归一为 licensed——引导口径一致
            let reason = fail_reason.unwrap_or_else(|| "licensed".to_string());
            tracing::warn!(%reason, "wx.init 未就绪，保持 Booting（等待授权/微信引导）");
            if init.is_some() {
                self.emit(serde_json::json!({
                    "kind": "event", "type": "init_fail",
                    "data": {"reason": reason},
                }))
                .await;
            }
            return;
        }
        self.state.mark_wx_init(true).await;
```

（后半段 resync/mark_ready/status 与现状一致，不动。）

`init_sequence` 方法之后新增公开方法：

```rust
    /// 手动重跑 init 序列（激活成功后 activate_license 命令 / 前端「重新初始化」
    /// 按钮入口）。与 run 循环内 init_sequence 同一段逻辑，幂等可重入；
    /// 不借道崩溃重启循环——init 失败时 sidecar 进程还活着。
    pub async fn retry_init(&self) {
        self.init_sequence().await;
    }
```

- [ ] **Step 4: 跑测试确认绿**

```
cd src-tauri && cargo test --test license_init && cargo test
```
预期：新测试 3 例绿；全量（含 state_machine 等存量）零回归。

- [ ] **Step 5: Commit**

```
git add src-tauri/src/sidecar/spec.rs src-tauri/src/state.rs src-tauri/tests/license_init.rs
git commit -m "feat(state): init失败三态解析+init_fail帧+retry_init公开方法"
```

---

### Task 4: ui_events 桥转 `wxauto://init-fail`

**Files:**
- Modify: `src-tauri/src/ui_events.rs`（EVENT_INIT_FAIL 常量 + forward arm）
- Test: `src-tauri/src/ui_events.rs`（tests mod 内）

**Interfaces:**
- Consumes: Task 3 的 init_fail 帧（GUI 装配下经 app_state.rs 的 event_sink 注入 wsConnected 后进 `forward_event_frame`——额外字段无害）
- Produces: 前端事件 `wxauto://init-fail`，payload `{"reason": "licensed" | "wechat_missing"}`（Task 6 store 监听契约）

- [ ] **Step 1: 写失败测试**

`ui_events.rs` tests mod 追加：

```rust
    /// init_fail 帧路由：forward 后按 EVENT_INIT_FAIL 发射（payload 即前端契约；
    /// 未 attach 时 emit 走丢弃分支——发射验证由 license_init.rs 帧级测试承担，
    /// 此处覆盖路由代码路径与常量契约）
    #[tokio::test]
    async fn test_forward_init_fail_routes_to_event() {
        let bridge = UiEventBridge::new();
        assert_eq!(EVENT_INIT_FAIL, "wxauto://init-fail");
        bridge
            .forward_event_frame(&serde_json::json!({
                "kind": "event", "type": "init_fail",
                "data": {"reason": "licensed"}, "wsConnected": false,
            }))
            .await;
    }
```

- [ ] **Step 2: 跑测试确认红**

```
cd src-tauri && cargo test --lib
```
预期：编译失败——`EVENT_INIT_FAIL` 未定义。

- [ ] **Step 3: 最小实现**

常量区（`EVENT_STATE` 附近）加：

```rust
pub const EVENT_INIT_FAIL: &str = "wxauto://init-fail";
```

文件头注释事件清单加一行：`//! - wxauto://init-fail payload { reason: string }（init 未就绪原因）`。

`forward_event_frame` 的 match 加 arm（`"status"` 之后）：

```rust
            "init_fail" => {
                // init 未就绪原因（licensed / wechat_missing）——激活页与横幅数据源
                let payload = serde_json::json!({
                    "reason": frame["data"]["reason"].as_str().unwrap_or_default(),
                });
                self.emit(EVENT_INIT_FAIL, &payload).await;
            }
```

- [ ] **Step 4: 跑测试确认绿**

```
cd src-tauri && cargo test --lib
```
预期：绿。

- [ ] **Step 5: Commit**

```
git add src-tauri/src/ui_events.rs
git commit -m "feat(ui-events): wxauto://init-fail 事件桥转发"
```

---

### Task 5: Rust 命令 `activate_license` / `retry_init` + 注册

**Files:**
- Modify: `src-tauri/src/commands.rs`（两命令）
- Modify: `src-tauri/src/gui.rs`（generate_handler! 注册）

**Interfaces:**
- Consumes: Task 3 的 `methods::ACTIVATE` 常量与 `Supervisor::retry_init`；`WxSession::direct_call_with_timeout(&self, method: &str, params: Value, timeout: Duration)`（wx/mod.rs:114，既有）
- Produces: invoke 命令 `activate_license(code: string) -> {ok: boolean, message: string}`（Ok resolve / Err 字符串 reject）；`retry_init() -> null`——Task 7 store 依赖。激活成功即内联重试 init 一次（失败即止不循环）

- [ ] **Step 1: 写实现（命令层是装配薄层，逻辑已由 Task 3 测试覆盖；本任务验证=编译+全量测试+CLI 零回归）**

`commands.rs` 头部 use 区加：

```rust
use std::time::Duration;

use serde_json::json;
use wxauto_desktop::sidecar::spec::methods;
```

文件常量区（`clear_logs` 之前或文件头注释后）加：

```rust
/// 激活超时：authenticate 可能走网络校验，比 init 的 10s 宽松
const ACTIVATE_TIMEOUT: Duration = Duration::from_secs(30);
```

文件末尾追加两命令：

```rust
/// 激活 wxautox4（直连 sidecar wx.activate；绕过 16-action 白名单——
/// 激活是编排层动作，与 wx.init 同类）。成功即内联重试 init 一次
/// （失败即止：微信未开场景每次白耗 UIA 扫描，由用户择机 retry_init）。
#[tauri::command]
pub async fn activate_license(ctx: State<'_, AppStateCtx>, code: String) -> Result<Value, String> {
    let code = code.trim().to_string();
    if code.is_empty() {
        return Err("激活码不能为空".to_string());
    }
    let assembled = ctx.ready().await?;
    let result = assembled
        .session
        .direct_call_with_timeout(methods::ACTIVATE, json!({ "code": code }), ACTIVATE_TIMEOUT)
        .await
        .map_err(|e| format!("激活请求失败：{e}"))?;
    if result["ok"].as_bool().unwrap_or(false) {
        assembled.supervisor.retry_init().await;
    }
    Ok(result)
}

/// 手动重跑 init 序列（激活页「重新初始化」按钮——sidecar 活着时
/// Supervisor 不会自动重跑 init，必须有显式入口）
#[tauri::command]
pub async fn retry_init(ctx: State<'_, AppStateCtx>) -> Result<(), String> {
    let assembled = ctx.ready().await?;
    assembled.supervisor.retry_init().await;
    Ok(())
}
```

`gui.rs` `generate_handler![...]` 列表末尾（`clear_logs,` 之后）加两行：

```rust
            activate_license,
            retry_init,
```

- [ ] **Step 2: 编译 + 全量测试**

```
cd src-tauri && cargo build && cargo test
```
预期：编译零警告零错误；全量测试绿（CLI 路径不触两命令——tauri command 仅 GUI 注册，零回归）。

- [ ] **Step 3: 冒烟（CLI 模式回归 + mock 链路）**

```
bash scripts/smoke_cli.sh
```
预期：PASS（CLI 装配不受 init_sequence 改造影响——licensed=false 行为同旧版留 Booting）。

- [ ] **Step 4: Commit**

```
git add src-tauri/src/commands.rs src-tauri/src/gui.rs
git commit -m "feat(commands): activate_license 30s直连+成功即重试init / retry_init 命令"
```

---

### Task 6: 前端 store 扩展（initFailReason / activeView / actions）

**Files:**
- Modify: `src/stores/app.ts`
- Test: `src/stores/app.spec.ts`

**Interfaces:**
- Consumes: Task 4 事件 `wxauto://init-fail` payload `{reason}`；Task 5 invoke 命令 `activate_license`/`retry_init`
- Produces（Task 7/8/9 依赖）:
  - state：`activeView: string`（'overview' 起）、`initFailReason: '' | 'licensed' | 'wechat_missing'`
  - getters：`needsActivation` / `wechatMissing` / `licensePassed`
  - actions：`switchView(v: string): void`、`activateLicense(code: string): Promise<ActivationOutcome>`、`retryInit(): Promise<void>`
  - 类型：`export interface ActivationOutcome { ok: boolean; message: string }`

- [ ] **Step 1: 写失败测试**

`app.spec.ts` 顶部 mock 区（现有 import 之后、`import { useAppStore... }` 之前——模块级 vi.mock 会提升，位置无碍但需在文件顶部区）加：

```ts
/* tauri mock：init() 订阅测试需捕获 listen 回调（纯逻辑用例不受影响） */
const listenMock = vi.fn<(evt: string, cb: (e: { payload: unknown }) => void) => Promise<() => void>>();
const invokeMock = vi.fn<(cmd: string, args?: unknown) => Promise<unknown>>();
vi.mock('@tauri-apps/api/event', () => ({
  listen: (evt: string, cb: (e: { payload: unknown }) => void) => listenMock(evt, cb),
}));
vi.mock('@tauri-apps/api/core', () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));
```

describe 块之后追加新 describe：

```ts
describe('激活状态推导（initFailReason × appState）', () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    invokeMock.mockReset();
    invokeMock.mockImplementation(() => Promise.resolve(null));
    listenMock.mockReset();
    listenMock.mockImplementation(async () => () => undefined);
  });

  it('getter 推导表：needsActivation / wechatMissing / licensePassed', () => {
    const store = useAppStore();
    store.appState = 'SidecarBooting';
    store.initFailReason = 'licensed';
    expect(store.needsActivation).toBe(true);
    expect(store.wechatMissing).toBe(false);
    expect(store.licensePassed).toBe(false);

    store.initFailReason = 'wechat_missing';
    expect(store.needsActivation).toBe(false);
    expect(store.wechatMissing).toBe(true);

    store.appState = 'Ready';
    expect(store.licensePassed).toBe(true);
  });

  it('wxauto://init-fail 事件落 initFailReason（未知 reason 守卫忽略）', async () => {
    const store = useAppStore();
    await store.init();
    const reg = listenMock.mock.calls.find((c) => c[0] === 'wxauto://init-fail');
    expect(reg).toBeTruthy();
    if (!reg) return;
    const cb = reg[1];
    cb({ payload: { reason: 'licensed' } });
    expect(store.initFailReason).toBe('licensed');
    cb({ payload: { reason: 'something_odd' } });
    expect(store.initFailReason).toBe('licensed', '未知 reason 不覆盖');
  });

  it('状态进入 WxInit 及之后清 initFailReason', async () => {
    const store = useAppStore();
    await store.init();
    store.initFailReason = 'licensed';
    const reg = listenMock.mock.calls.find((c) => c[0] === 'wxauto://state');
    expect(reg).toBeTruthy();
    if (!reg) return;
    reg[1]({ payload: 'WxInit' });
    expect(store.appState).toBe('WxInit');
    expect(store.initFailReason).toBe('');
  });

  it('activateLicense：invoke 透传 + 判别联合返回', async () => {
    const store = useAppStore();
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === 'activate_license') return Promise.resolve({ ok: true, message: '激活成功' });
      return Promise.resolve(null);
    });
    const r = await store.activateLicense('ABC');
    expect(invokeMock).toHaveBeenCalledWith('activate_license', { code: 'ABC' });
    expect(r).toEqual({ ok: true, message: '激活成功' });

    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === 'activate_license') return Promise.reject(new Error('激活请求失败：超时'));
      return Promise.resolve(null);
    });
    const bad = await store.activateLicense('X');
    expect(bad.ok).toBe(false);
    expect(bad.message).toContain('超时');
  });

  it('switchView 切 activeView', () => {
    const store = useAppStore();
    expect(store.activeView).toBe('overview');
    store.switchView('activation');
    expect(store.activeView).toBe('activation');
  });
});
```

（文件头注释补一句测试范围。）

- [ ] **Step 2: 跑测试确认红**

```
pnpm vitest run src/stores/app.spec.ts
```
预期：新用例红——`initFailReason`/`needsActivation`/`switchView` 等不存在（TS 编译期即报）。

- [ ] **Step 3: 最小实现**

`app.ts`：

(a) `AppStateName` 类型定义之后加：

```ts
/** init 未就绪原因（Rust wxauto://init-fail 载荷；''=无） */
export type InitFailReasonName = '' | 'licensed' | 'wechat_missing';

/** 已过授权判据态（进入即视为 license 通过） */
const LICENSE_PASSED_STATES: AppStateName[] = ['WxInit', 'Ready', 'Busy', 'Degraded'];

/** 已知 init 失败原因（未知值守卫忽略——防 sidecar 异常值污染 UI） */
const KNOWN_INIT_FAIL_REASONS: string[] = ['licensed', 'wechat_missing'];

/** 激活结果判别联合（Rust activate_license 的 resolve/reject 归一） */
export interface ActivationOutcome {
  ok: boolean;
  message: string;
}

/** init-fail 载荷守卫 */
function isInitFailPayload(v: unknown): v is { reason: string } {
  if (typeof v !== 'object' || v === null) return false;
  const reason = (v as { reason?: unknown }).reason;
  return typeof reason === 'string';
}
```

(b) state 加两项（`appState` 之后）：

```ts
    /** 当前视图（跨视图导航归 store：Overview 去激活/Activation 回概览） */
    activeView: 'overview',
    /** init 未就绪原因（wxauto://init-fail 最后值；''=无） */
    initFailReason: '' as InitFailReasonName,
```

(c) getters 加：

```ts
    /** 需要激活：Booting 且未授权（横幅+自动跳激活页判据） */
    needsActivation(state): boolean {
      return state.appState === 'SidecarBooting' && state.initFailReason === 'licensed';
    },
    /** 已激活但微信未开（重新初始化按钮判据；不自动跳激活页） */
    wechatMissing(state): boolean {
      return state.appState === 'SidecarBooting' && state.initFailReason === 'wechat_missing';
    },
    /** 授权已通过（状态进入 WxInit 及之后） */
    licensePassed(state): boolean {
      return LICENSE_PASSED_STATES.includes(state.appState);
    },
```

(d) `init()` 内 state 监听回调扩为：

```ts
          await listen<unknown>('wxauto://state', (e) => {
            if (isAppStateName(e.payload)) {
              this.appState = e.payload;
              if (LICENSE_PASSED_STATES.includes(e.payload)) this.initFailReason = '';
            }
          }),
```

紧随其后新增监听（status 之前）：

```ts
        unlisteners.push(
          await listen<unknown>('wxauto://init-fail', (e) => {
            if (isInitFailPayload(e.payload) && KNOWN_INIT_FAIL_REASONS.includes(e.payload.reason)) {
              this.initFailReason = e.payload.reason as InitFailReasonName;
            }
          }),
        );
```

(e) actions 加（`disconnect` 之后）：

```ts
    /** 跨视图导航（Overview 去激活 / Activation 回概览） */
    switchView(v: string) {
      this.activeView = v;
    },
    /** 激活 wxautox4（Rust 成功即内联重试 init；结果经 state/init-fail 事件回流 UI） */
    async activateLicense(code: string): Promise<ActivationOutcome> {
      try {
        const r = await invoke<unknown>('activate_license', { code });
        const ok = (r as { ok?: unknown } | null)?.ok === true;
        const message = typeof (r as { message?: unknown } | null)?.message === 'string'
          ? (r as { message: string }).message
          : '';
        return { ok, message };
      } catch (err) {
        return { ok: false, message: err instanceof Error ? err.message : String(err) };
      }
    },
    /** 手动重跑 init 序列（激活页「重新初始化」按钮） */
    async retryInit() {
      await invoke('retry_init');
    },
```

文件头注释的事件契约清单补：``wxauto://init-fail` payload { reason: 'licensed'|'wechat_missing' }``；invoke 契约补 `activate_license / retry_init`。

- [ ] **Step 4: 跑测试确认绿 + 类型检查**

```
pnpm vitest run src/stores/app.spec.ts && pnpm typecheck
```
预期：全绿、零类型错误。

- [ ] **Step 5: Commit**

```
git add src/stores/app.ts src/stores/app.spec.ts
git commit -m "feat(store): initFailReason/activeView 状态+三getter+激活actions+init-fail订阅"
```

---

### Task 7: Activation.vue 激活视图

**Files:**
- Create: `src/views/Activation.vue`
- Test: `src/views/Activation.spec.ts`

**Interfaces:**
- Consumes: store `needsActivation`/`wechatMissing`/`licensePassed`/`activateLicense`/`retryInit`/`switchView`（Task 6）
- Produces: 激活视图组件（Task 8 App.vue 挂载，`view === 'activation'`）

- [ ] **Step 1: 写失败测试**

`src/views/Activation.spec.ts`（新文件，模式沿用 Settings.spec.ts）：

```ts
/**
 * Activation 视图测试：状态卡三态 + 激活表单提交 + 重新初始化入口。
 * mock tauri invoke（activate_license / retry_init 按用例注入）。
 */
// @vitest-environment happy-dom
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { mount, flushPromises } from '@vue/test-utils';
import { createPinia, setActivePinia } from 'pinia';
import TDesign from 'tdesign-vue-next';

const invokeMock = vi.fn<(cmd: string, args?: unknown) => Promise<unknown>>();
vi.mock('@tauri-apps/api/core', () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));
vi.mock('@tauri-apps/api/event', () => ({
  listen: async () => () => undefined,
}));

import Activation from './Activation.vue';

function mountActivation() {
  return mount(Activation, { global: { plugins: [TDesign, createPinia()] } });
}

/** 未激活现场：Booting + licensed（视图各分支的前提态） */
async function setUnlicensed() {
  const { useAppStore } = await import('../stores/app');
  const store = useAppStore();
  store.appState = 'SidecarBooting';
  store.initFailReason = 'licensed';
  await flushPromises();
  return store;
}

describe('Activation 视图', () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    invokeMock.mockReset();
    invokeMock.mockImplementation(() => Promise.resolve(null));
  });

  it('未激活：状态卡显示未激活文案 + 联系运营引导', async () => {
    const wrapper = mountActivation();
    await setUnlicensed();
    expect(wrapper.text()).toContain('未激活');
    expect(wrapper.text()).toContain('服务管理员');
  });

  it('已通过：状态卡显示授权正常且禁用激活按钮', async () => {
    const wrapper = mountActivation();
    const { useAppStore } = await import('../stores/app');
    useAppStore().appState = 'Ready';
    await flushPromises();
    expect(wrapper.text()).toContain('授权正常');
    const btn = wrapper.find('button[type="submit"]');
    expect((btn.element as HTMLButtonElement).disabled).toBe(true);
  });

  it('填码提交 → activate_license invoke 透传 + 成功提示', async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === 'activate_license') return Promise.resolve({ ok: true, message: '激活成功' });
      return Promise.resolve(null);
    });
    const wrapper = mountActivation();
    await setUnlicensed();
    const input = wrapper.find('input');
    await input.setValue('ABC-123');
    await wrapper.find('form').trigger('submit');
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith('activate_license', { code: 'ABC-123' });
    expect(wrapper.text()).toContain('激活成功');
  });

  it('激活失败（invoke reject）→ 红字错误不白屏', async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === 'activate_license') return Promise.reject(new Error('激活请求失败：超时'));
      return Promise.resolve(null);
    });
    const wrapper = mountActivation();
    await setUnlicensed();
    await wrapper.find('input').setValue('BAD');
    await wrapper.find('form').trigger('submit');
    await flushPromises();
    expect(wrapper.text()).toContain('激活请求失败：超时');
  });

  it('wechat_missing：显示重新初始化按钮 → 点击调 retry_init', async () => {
    const wrapper = mountActivation();
    const { useAppStore } = await import('../stores/app');
    const store = useAppStore();
    store.appState = 'SidecarBooting';
    store.initFailReason = 'wechat_missing';
    await flushPromises();
    const btn = wrapper.findAll('button').find((b) => b.text().includes('重新初始化'));
    expect(btn).toBeTruthy();
    if (!btn) return;
    await btn.trigger('click');
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith('retry_init');
  });
});
```

- [ ] **Step 2: 跑测试确认红**

```
pnpm vitest run src/views/Activation.spec.ts
```
预期：模块解析失败——`./Activation.vue` 不存在。

- [ ] **Step 3: 最小实现**

`src/views/Activation.vue`（新文件）：

```vue
<template>
  <div class="activation">
    <!-- 授权状态卡：未激活红 / 已通过绿 / 微信未开橙 -->
    <t-card title="授权状态" :bordered="false">
      <div class="status-line">
        <span class="lamp" :class="lampClass" />
        <span>{{ statusText }}</span>
      </div>
      <!-- 已激活但微信未开：显式重新初始化入口（sidecar 活着 Supervisor 不自动重跑 init） -->
      <t-button
        v-if="store.wechatMissing"
        theme="warning"
        variant="outline"
        :loading="retrying"
        class="block-inline"
        @click="onRetryInit"
      >
        重新初始化
      </t-button>
    </t-card>

    <!-- 激活表单（已通过后隐藏） -->
    <t-card v-if="!store.licensePassed" title="激活" :bordered="false" class="block">
      <!-- :data 必绑：TDesign FormItem 校验取值是 form.data[name] 而非 v-model（Settings 事故同款） -->
      <t-form :data="form" label-width="80px" @submit="onActivate">
        <t-form-item label="激活码" name="code" :rules="[{ required: true, message: '激活码必填' }]">
          <t-input v-model="form.code" placeholder="请输入 wxautox4 激活码" clearable />
        </t-form-item>
        <t-form-item>
          <t-button type="submit" theme="primary" :loading="activating">立即激活</t-button>
        </t-form-item>
      </t-form>
      <t-alert v-if="resultMessage" class="block-inline" :theme="resultOk ? 'success' : 'error'" :message="resultMessage" />
    </t-card>

    <!-- 获取激活码引导（企业统一采购发码模式） -->
    <t-card title="获取激活码" :bordered="false" class="block contact">
      {{ ACTIVATION_CONTACT }}
    </t-card>
  </div>
</template>

<script setup lang="ts">
/**
 * 激活视图：wxautox4 内核授权（spec 2026-09-08）。
 * 状态卡消费 store 三 getter；激活走 activateLicense（Rust 成功即内联
 * 重试 init）；闭环由 state/init-fail 事件回流驱动，本视图只呈现。
 */
import { onUnmounted, reactive, ref, watch } from 'vue';
import { useAppStore } from '../stores/app';

/** 运营联系方式（改文案随发版即可——YAGNI 不做配置项） */
const ACTIVATION_CONTACT = 'wxautox4 激活码为按设备授权（一机一码，激活后永久有效），请联系您的服务管理员获取。';

const store = useAppStore();
const form = reactive({ code: '' });
const activating = ref(false);
const retrying = ref(false);
const resultMessage = ref('');
const resultOk = ref(false);
let jumpTimer: ReturnType<typeof setTimeout> | null = null;
onUnmounted(() => {
  if (jumpTimer) clearTimeout(jumpTimer);
});

/** 状态卡文案（三态 + 检测中兜底） */
const statusText = ref('正在检测授权状态…');
function refreshStatusText() {
  if (store.licensePassed) statusText.value = 'wxautox4 授权正常';
  else if (store.needsActivation) statusText.value = 'wxautox4 未激活：微信自动化核心功能不可用';
  else if (store.wechatMissing) statusText.value = '已激活，但未检测到微信客户端——请打开微信 PC 客户端后点击「重新初始化」';
  else statusText.value = '正在检测授权状态…';
}
const lampClass = ref('lamp--gray');
function refreshLampClass() {
  if (store.licensePassed) lampClass.value = 'lamp--green';
  else if (store.needsActivation) lampClass.value = 'lamp--red';
  else if (store.wechatMissing) lampClass.value = 'lamp--yellow';
  else lampClass.value = 'lamp--gray';
}
watch(
  () => [store.licensePassed, store.needsActivation, store.wechatMissing],
  () => {
    refreshStatusText();
    refreshLampClass();
  },
  { immediate: true },
);

/** 激活提交（TDesign form submit 回调携 validateResult） */
async function onActivate({ validateResult }: { validateResult: boolean }) {
  if (validateResult !== true || activating.value) return;
  activating.value = true;
  resultMessage.value = '';
  const r = await store.activateLicense(form.code.trim());
  resultOk.value = r.ok;
  resultMessage.value = r.message || (r.ok ? '激活成功' : '激活失败');
  activating.value = false;
}

/** 手动重新初始化（微信打开后） */
async function onRetryInit() {
  retrying.value = true;
  try {
    await store.retryInit();
  } finally {
    retrying.value = false;
  }
}

/** 激活闭环：状态离开 Booting（WxInit 及之后）→ 成功提示 + 2s 后回概览 */
watch(
  () => store.licensePassed,
  (passed) => {
    if (!passed) return;
    resultOk.value = true;
    resultMessage.value = '激活成功，正在初始化';
    if (jumpTimer) clearTimeout(jumpTimer);
    jumpTimer = setTimeout(() => store.switchView('overview'), 2000);
  },
);
</script>

<style scoped>
.status-line {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 14px;
}
.lamp {
  width: 12px;
  height: 12px;
  border-radius: 50%;
  display: inline-block;
}
.lamp--green {
  background: var(--td-success-color);
  box-shadow: 0 0 6px var(--td-success-color);
}
.lamp--yellow {
  background: var(--td-warning-color);
  box-shadow: 0 0 6px var(--td-warning-color);
}
.lamp--red {
  background: var(--td-error-color);
  box-shadow: 0 0 6px var(--td-error-color);
}
.lamp--gray {
  background: var(--td-gray-color-6);
}
.block {
  margin-top: 16px;
}
.block-inline {
  margin-top: 12px;
}
.contact {
  color: var(--td-text-color-secondary);
  font-size: 13px;
}
</style>
```

- [ ] **Step 4: 跑测试确认绿 + 类型检查**

```
pnpm vitest run src/views/Activation.spec.ts && pnpm typecheck
```
预期：5 用例全绿。若 TDesign form submit 的 `validateResult` 未按预期回调（happy-dom 校验时序），对照 Settings.spec.ts 的提交驱动方式修正测试触发器，**不得**改实现迁就。

- [ ] **Step 5: Commit**

```
git add src/views/Activation.vue src/views/Activation.spec.ts
git commit -m "feat(views): 激活视图——状态卡三态+激活表单+重新初始化+运营引导"
```

---

### Task 8: App.vue 菜单 / 横幅 / 自动跳转 / view 迁 store

**Files:**
- Modify: `src/App.vue`
- Test: `src/App.spec.ts`（新）

**Interfaces:**
- Consumes: store `activeView`/`switchView`/`needsActivation`/`wechatMissing`（Task 6）、`Activation` 组件（Task 7）
- Produces: 完整壳集成——菜单「激活」项、顶栏未激活红横幅（点击去激活）、微信未开橙提示、未激活一次性自动跳转

- [ ] **Step 1: 写失败测试**

`src/App.spec.ts`（新文件）：

```ts
/**
 * App 壳激活集成测试：未激活自动跳转（一次性）+ 横幅 + 菜单项。
 */
// @vitest-environment happy-dom
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { mount, flushPromises } from '@vue/test-utils';
import { createPinia, setActivePinia } from 'pinia';
import TDesign from 'tdesign-vue-next';

const invokeMock = vi.fn<(cmd: string, args?: unknown) => Promise<unknown>>();
vi.mock('@tauri-apps/api/core', () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));
vi.mock('@tauri-apps/api/event', () => ({
  listen: async () => () => undefined,
}));

import App from './App.vue';

function mountApp() {
  return mount(App, { global: { plugins: [TDesign, createPinia()] } });
}

describe('App 壳激活集成', () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    invokeMock.mockReset();
    invokeMock.mockImplementation(() => Promise.resolve(null));
  });

  it('未激活（licensed）→ 自动跳激活页 + 顶栏横幅 + 菜单项存在', async () => {
    const wrapper = mountApp();
    const { useAppStore } = await import('./stores/app');
    const store = useAppStore();
    await flushPromises();
    store.appState = 'SidecarBooting';
    store.initFailReason = 'licensed';
    await flushPromises();
    expect(store.activeView).toBe('activation', '未激活应自动跳激活页');
    expect(wrapper.text()).toContain('点击去激活');
    expect(wrapper.text()).toContain('激活');
  });

  it('自动跳转一次性：跳回概览后再次未激活不重跳', async () => {
    mountApp();
    const { useAppStore } = await import('./stores/app');
    const store = useAppStore();
    await flushPromises();
    store.appState = 'SidecarBooting';
    store.initFailReason = 'licensed';
    await flushPromises();
    expect(store.activeView).toBe('activation');
    store.switchView('overview');
    // 模拟状态反复（sidecar 重启再 init 失败）
    store.initFailReason = '';
    await flushPromises();
    store.initFailReason = 'licensed';
    await flushPromises();
    expect(store.activeView).toBe('overview', '二次未激活不再抢跳');
  });

  it('wechat_missing → 不跳激活页，横幅提示打开微信', async () => {
    mountApp();
    const { useAppStore } = await import('./stores/app');
    const store = useAppStore();
    await flushPromises();
    store.appState = 'SidecarBooting';
    store.initFailReason = 'wechat_missing';
    await flushPromises();
    expect(store.activeView).toBe('overview');
  });
});
```

- [ ] **Step 2: 跑测试确认红**

```
pnpm vitest run src/App.spec.ts
```
预期：`store.activeView` 断言失败——App.vue 尚无跳转逻辑（activeView 停在 overview 且无横幅）。

- [ ] **Step 3: 最小实现**

`App.vue` 全量更新：

template 侧——菜单加激活项（`moments` 之后、`settings` 之前）：

```html
        <t-menu-item value="activation">
          <template #icon><t-icon name="secured" /></template>激活
        </t-menu-item>
```

`<t-menu>` 绑定改 store：`:value="store.activeView"` + `@change="(v: unknown) => store.switchView(typeof v === 'string' ? v : 'overview')"`。

顶栏 `initError` tag 之后加两条横幅 tag：

```html
        <t-tag
          v-if="store.needsActivation"
          theme="danger"
          variant="light"
          style="cursor: pointer"
          @click="store.switchView('activation')"
        >
          wxautox4 未激活，点击去激活
        </t-tag>
        <t-tag v-else-if="store.wechatMissing" theme="warning" variant="light">
          微信客户端未打开，请在激活页重新初始化
        </t-tag>
```

content 区 `Settings` 分支前加：

```html
        <Activation v-else-if="view === 'activation'" />
```

（`view` 为下方计算别名，保持模板简洁。）

script 侧全量替换：

```ts
<script setup lang="ts">
/**
 * 桌面面板壳：TDesign 侧边菜单 + activeView 切换（router-less——
 * 单窗口桌面 App 不需要 vue-router，视图切换即组件 v-if 分支）。
 * 视图归 store.activeView（Overview 去激活/Activation 回概览跨视图导航）。
 * 未激活（licensed）一次性自动跳激活页；wechat_missing 只横幅提示不抢跳。
 */
import { computed, onMounted, ref, watch } from 'vue';
import { useAppStore, APP_STATE_LABELS } from './stores/app';
import Overview from './views/Overview.vue';
import Messages from './views/Messages.vue';
import ListenView from './views/Listen.vue';
import CommandLog from './views/CommandLog.vue';
import AppLogView from './views/AppLog.vue';
import Moments from './views/Moments.vue';
import Activation from './views/Activation.vue';
import Settings from './views/Settings.vue';

const store = useAppStore();
/** 模板短别名（真值在 store.activeView） */
const view = computed(() => store.activeView);
/** 自动跳转一次性标记（每次 App 运行至多抢跳一回） */
const autoJumped = ref(false);

onMounted(() => void store.init());

watch(
  () => store.needsActivation,
  (needs) => {
    if (needs && !autoJumped.value) {
      autoJumped.value = true;
      store.switchView('activation');
    }
  },
);
</script>
```

style 区不动。

- [ ] **Step 4: 跑测试确认绿 + 类型检查**

```
pnpm vitest run src/App.spec.ts && pnpm typecheck
```
预期：3 用例绿。

- [ ] **Step 5: Commit**

```
git add src/App.vue src/App.spec.ts
git commit -m "feat(app): 激活菜单项+未激活横幅+一次性自动跳转+view迁store"
```

---

### Task 9: Overview 授权卡 + 全量回归 + 文档

**Files:**
- Modify: `src/views/Overview.vue`
- Modify: `WINDOWS-安装打包流程.txt`

**Interfaces:**
- Consumes: store `licensePassed`/`needsActivation`/`wechatMissing`/`switchView`（Task 6）
- Produces: 概览第四张指示灯卡（运营一眼定位授权态）；装机文档激活步骤

- [ ] **Step 1: 实现授权卡**

`Overview.vue` 三灯 `<t-row>` 内追加第四列（微信卡 `<t-col>` 之后）：

```html
      <t-col :span="4">
        <t-card title="授权" :bordered="false">
          <div class="lamp-row">
            <span
              class="lamp"
              :class="store.licensePassed ? 'lamp--green' : store.needsActivation ? 'lamp--red' : store.wechatMissing ? 'lamp--yellow' : 'lamp--gray'"
            />
            <span>{{
              store.licensePassed ? '正常' : store.needsActivation ? '未激活' : store.wechatMissing ? '已激活·微信未开' : '检测中'
            }}</span>
          </div>
          <t-button
            v-if="store.needsActivation"
            size="small"
            theme="danger"
            variant="outline"
            style="margin-top: 8px"
            @click="store.switchView('activation')"
          >
            去激活
          </t-button>
        </t-card>
      </t-col>
```

（三灯行 4×span4=12 恰满栅格；lamp 样式类文件内已有。）

- [ ] **Step 2: 类型检查 + 前端全量**

```
pnpm typecheck && pnpm test
```
预期：全绿（含存量 Settings/Moments/AppLog spec）。

- [ ] **Step 3: 三端全量回归**

```
cd sidecar-python && python3 -m pytest test_methods.py -v
cd ../src-tauri && cargo test
cd .. && bash scripts/smoke_cli.sh
```
预期：pytest 全绿；cargo 全量零回归；CLI 冒烟 PASS。

- [ ] **Step 4: 装机文档补激活节**

`WINDOWS-安装打包流程.txt` §6「装机接线」步骤清单中，设置页说明之后插入一行（沿用该文件既有编号与文风）：

```
6.x 激活：首次启动若提示「wxautox4 未激活」，在左侧「激活」页输入运营发放的
     激活码（一机一码）→ 立即激活；激活成功自动初始化，若提示微信未开，
     打开微信 PC 客户端后在同页点「重新初始化」。
```

§10 故障速查表补一行：`激活失败：激活码无效或已过期 → 核对码与设备（一机一码）；仍失败看运行日志 tab 的 sidecar 行`。

- [ ] **Step 5: Commit**

```
git add src/views/Overview.vue WINDOWS-安装打包流程.txt
git commit -m "feat(overview): 授权状态卡+去激活入口; docs: 装机流程补激活步骤"
```

---

## 收尾验收（实现完成后）

1. **mock 全链路**（Linux 可跑）：`WXAUTO_MOCK=1` 下 sidecar mock `wx.init` 恒 licensed=true，激活分支不触发——激活链路的自动化验证即上述 pytest/vitest/cargo 三层；真机激活闭环（真实激活码 + 微信 4.1.x）随 Windows 发版流程验收，参照 `WINDOWS-安装打包流程.txt`。
2. **真机验收清单**（记入发版 runbook）：未激活冷启动自动跳激活页；错码红字；正确码 → 状态进「微信初始化中」→「正常服务」；杀微信进程重启 App → 横幅「微信客户端未打开」+ 重新初始化按钮生效。
3. 差距报告 P1 两项（自动更新、托盘常驻+开机自启）不在本计划——各自走独立拷问→spec→计划循环。
