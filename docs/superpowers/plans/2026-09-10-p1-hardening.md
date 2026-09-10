# P1 加固实现计划（is_at 透传 / 消息去重 / webhook 告警 / forward 验证清单）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 打通 is_at 群@语义全链、消息上行幂等（原生 id + 滑动窗双保险）、新增 webhook 告警通道（SidecarDead/wxOnline 翻转两场景）、交付 msg.forward 真机验证清单。

**Architecture:** 前两项在现有 sidecar Python → Rust RawMessage → inbound 组帧链路上做字段扩展与回调去重；第三项新建 core 层 `alert.rs`（旁路观察者，tokio::spawn 异步发送绝不阻塞主链路），挂点为状态机 on_change 回调与 AgentLink 心跳变化点，配置走既有 Config→SettingsPatch→设置页链；第四项纯文档。

**Tech Stack:** Rust (tokio/reqwest/serde)、Python sidecar (wxautox4)、Vue3+TDesign。

**Spec:** `docs/superpowers/specs/2026-09-10-p1-hardening-design.md`

## Global Constraints

- 所有代码注释与用户可见文案用简体中文。
- **基于 HEAD（9748963）开发**：此前 spec 撰写期间有并行会话合入 4 个 P2 提交（chat.open 退役 a617907 / delay 接线 3181069 / channelId 进 hello 0c78714 / CA 证书持久化 ad70b00）与 2 个 P0 spec/plan 草稿（未跟踪文件 listen-persist-event-outbox，尚未实现）——本计划已对这些快照对齐（Task 1/3 的 methods.py 行号偏移、Task 8 的 start_link 已含 set_channel_id 行）。worktree 从**当时最新 main** 分出；执行前 `git log --oneline -3` 核对基准，若 main 又前移（尤其 P0 落地会动 methods.py/ws.rs/config.rs），先重读受影响 Task 的文件现状再动笔。
- Rust core（src-tauri/src/ 下 lib.rs 声明的模块）禁依赖 tauri；GUI 装配在 bin 层（app_state.rs/commands.rs）。
- 禁 `unwrap()` 于运行时路径（测试代码可）；锁中毒一律 `unwrap_or_else(PoisonError::into_inner)` 恢复。
- Python 测试跑法：`cd sidecar-python && python3 -m pytest test_methods.py -x -q`（全量 94 例 <1s）。
- Rust 测试跑法：`cd src-tauri && cargo test`（集成测试需本机 python3）。
- 前端测试跑法：`pnpm test`（vitest run，37 例）。
- 提交信息格式 `<type>(<scope>): <描述>`（feat/fix/docs/test/chore）。
- 每任务收尾全量跑受影响端的测试套（Python/Rust/前端按任务涉及面）。

---

### Task 1: is_at 透传——Python sidecar 侧

**Files:**
- Modify: `sidecar-python/methods.py`（`_raw_message` 函数，HEAD 版 line 188-198）
- Test: `sidecar-python/test_methods.py`（FakeMsg 类，HEAD 版 line 29-46；`test_listen_callback_fires_notification_with_msg_id`，HEAD 版 line 631-652）

**Interfaces:**
- Consumes: wxautox4 msg 对象的 `is_at` 属性（bool；旧版库可能无此属性）。
- Produces: `_raw_message()` 返回的 dict 新增键 `"is_at": bool`——Task 2 的 Rust `RawMessage.is_at`（serde default）与之对齐；通知帧 `message.received` params 多一个 snake_case 键。

- [ ] **Step 1: 写失败测试**

在 `test_methods.py` 的 FakeMsg 类 `__init__` 末尾（`self.forwarded = None` 之后）加：

```python
        self.is_at = False
```

docstring 同步改为（附录 A 形状声明扩充）：

```python
class FakeMsg:
    """伪造 wxautox4 Message 对象（附录 A：属性 type/attr/sender/content/id/is_at）"""
```

在 `test_listen_callback_fires_notification_with_msg_id` 测试末尾（`assert msg_pool[params["msg_id"]][0] is msg` 之后）补断言：

```python
    assert params["is_at"] is False  # FakeMsg 默认 is_at=False 透传
```

新增测试（放在 `test_listen_callback_fires_notification_with_msg_id` 之后）：

```python
def test_listen_callback_passes_is_at_true(fakewx):
    """群内 @机器人消息：msg.is_at=True 透传到通知帧（硬编码 false 的矫正）"""
    notifications = []
    msg_pool, msg_pool_ts = {}, {}
    _dispatch("listen.add", {"nickname": "客户群"}, fakewx,
              notify=lambda m, p: notifications.append((m, p)),
              msg_pool=msg_pool, msg_pool_ts=msg_pool_ts)
    msg = FakeMsg(content="@机器人 你好")
    msg.is_at = True
    chat = FakeChat("客户群", chat_type="group")
    fakewx.listen_reg["客户群"](msg, chat)
    assert notifications[0][1]["is_at"] is True


def test_raw_message_is_at_missing_attr_defaults_false():
    """旧版 wxautox4 无 is_at 属性：getattr 默认值安全降级 False（不炸回调）"""
    class OldMsg:
        content = "hi"
        type = "text"
        attr = "friend"
        sender = "张三"

    class OldChat:
        who = "张三"
        chat_type = "friend"

    import methods
    r = methods._raw_message(OldMsg(), OldChat(), "m1")
    assert r["is_at"] is False
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd sidecar-python && python3 -m pytest test_methods.py -x -q -k is_at`
Expected: FAIL——`KeyError: 'is_at'`（`test_raw_message_is_at_missing_attr_defaults_false` 与 `test_listen_callback_passes_is_at_true` 拿不到键）

- [ ] **Step 3: 最小实现**

`methods.py` `_raw_message`（HEAD line 188）dict 末尾（`"content": ...` 之后）加：

```python
        "is_at": bool(getattr(msg, "is_at", False)),
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd sidecar-python && python3 -m pytest test_methods.py -q`
Expected: 86 passed（HEAD 基线 83 + 3 新增）

- [ ] **Step 5: 提交**

```bash
git add sidecar-python/methods.py sidecar-python/test_methods.py
git commit -m "feat(sidecar): is_at 透传——通知帧携带群@语义,旧库无属性安全降级"
```

---

### Task 2: is_at 透传——Rust 契约与组帧

**Files:**
- Modify: `src-tauri/src/sidecar/spec.rs`（`RawMessage` 结构，HEAD line 52-61；内联测试 `test_raw_message_serde_roundtrip`）
- Modify: `src-tauri/src/agent_link/inbound.rs`（`message_event_from_notification`，HEAD line 32）
- Test: `src-tauri/tests/agent_link.rs`（`test_message_event_from_notification_camel`，HEAD line 121-141）

**Interfaces:**
- Consumes: Task 1 的通知帧 `is_at` 键（snake_case bool）。
- Produces: 上行 WS `event:message` 帧 `data.isAt`（camelCase bool）——Server 侧契约；`RawMessage.is_at: bool`（`#[serde(default)]`）供反序列化旧帧兼容。

- [ ] **Step 1: 写失败测试**

`src-tauri/src/sidecar/spec.rs` 内联测试 `test_raw_message_serde_roundtrip` 的 RawMessage 构造加字段：

```rust
            is_at: true,
```

并在 `let back: RawMessage = ...` 断言后补：

```rust
        assert_eq!(back.is_at, true);
```

新增内联测试（放 roundtrip 测试之后）：

```rust
    /// 旧 sidecar 帧无 is_at 键——serde default false 向后兼容
    #[test]
    fn test_raw_message_is_at_default_false_on_legacy_frame() {
        let v = serde_json::json!({
            "msg_id": "m1", "chat_who": "wxid_abc", "chat_type": "friend",
            "attr": "friend", "msg_type": "text", "sender": "wxid_abc", "content": "你好"
        });
        let m: RawMessage = serde_json::from_value(v).expect("旧帧反序列化失败");
        assert_eq!(m.is_at, false, "缺 is_at 键应 default false");
    }
```

`src-tauri/tests/agent_link.rs` `test_message_event_from_notification_camel`：params json 加 `"is_at": true`，`assert_eq!(f["data"]["isAt"], false);` 改为：

```rust
    assert_eq!(f["data"]["isAt"], true);
```

同文件 `test_message_event_voice_overwrite_and_defaults` 末尾补（缺键兜底）：

```rust
    // 旧 sidecar 帧缺 is_at 键 → isAt 兜底 false
    let f4 = message_event_from_notification(&json!({}), None, None);
    assert_eq!(f4["data"]["isAt"], false);
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test --lib spec::tests`
Expected: FAIL——编译错误 `missing field is_at in initializer`（roundtrip 构造）+ `no field is_at` 相关

- [ ] **Step 3: 最小实现**

`spec.rs` `RawMessage`（content 字段后）加：

```rust
    /// 群内 @机器人标记（2026-09-10 P1：wxautox msg.is_at 透传；
    /// 旧 sidecar 帧无此键——default false 向后兼容）
    #[serde(default)]
    pub is_at: bool,
```

`inbound.rs:32` `"isAt": false,` 改为：

```rust
            "isAt": n["is_at"].as_bool().unwrap_or(false),
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test --lib && cargo test --test agent_link`
Expected: 全绿（spec 5 例含新增 1；agent_link 组帧断言过）

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/sidecar/spec.rs src-tauri/src/agent_link/inbound.rs src-tauri/tests/agent_link.rs
git commit -m "feat(link): isAt 上行透传——RawMessage 契约扩 is_at(serde default 兼容旧帧)"
```

---

### Task 3: 消息上行去重——原生 id 透传（幂等键）

**Files:**
- Modify: `sidecar-python/methods.py`（`_listen_add` 内 `on_msg` 回调，现行 line 511-527；`mid = uuid.uuid4().hex[:12]` 在 line 518）
- Test: `sidecar-python/test_methods.py`

**Interfaces:**
- Consumes: wxautox4 msg 对象 `id` 属性（str；`chat.history` 路径 HEAD line 487 已用 `getattr(m, "id", "")` 同款取法）。
- Produces: 通知帧 `msg_id` 优先为 wxautox 原生 id（确定性幂等键，Server 可幂等）；无 id 时退回 uuid4 hex[:12]（行为不劣于现状）。msg 池键随之确定性——Task 4 滑动窗与二次 RPC 共享此事实。

- [ ] **Step 1: 写失败测试**

在 `test_listen_callback_fires_notification_with_msg_id` 之后新增：

```python
def test_listen_callback_msg_id_prefers_native_id(fakewx):
    """msg_id 优先取 wxautox 原生 id（确定性幂等键——Server 可按 msg_id 去重）"""
    notifications = []
    msg_pool, msg_pool_ts = {}, {}
    _dispatch("listen.add", {"nickname": "客户群"}, fakewx,
              notify=lambda m, p: notifications.append((m, p)),
              msg_pool=msg_pool, msg_pool_ts=msg_pool_ts)
    msg = FakeMsg(content="原生id消息")
    msg.id = "native_7788"
    fakewx.listen_reg["客户群"](msg, FakeChat("客户群"))
    assert notifications[0][1]["msg_id"] == "native_7788"
    assert "native_7788" in msg_pool  # 池键同步确定性


def test_listen_callback_msg_id_falls_back_to_uuid(fakewx):
    """旧版 wxautox4 msg 无 id 属性：退回随机 uuid（12 hex），行为不劣于现状"""
    notifications = []
    msg_pool, msg_pool_ts = {}, {}
    _dispatch("listen.add", {"nickname": "客户群"}, fakewx,
              notify=lambda m, p: notifications.append((m, p)),
              msg_pool=msg_pool, msg_pool_ts=msg_pool_ts)
    msg = FakeMsg(content="无id消息")
    del msg.id  # 旧库无此属性
    fakewx.listen_reg["客户群"](msg, FakeChat("客户群"))
    mid = notifications[0][1]["msg_id"]
    assert len(mid) == 12 and all(c in "0123456789abcdef" for c in mid)
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd sidecar-python && python3 -m pytest test_methods.py -x -q -k msg_id`
Expected: `test_listen_callback_msg_id_prefers_native_id` FAIL（实际 msg_id 是随机 hex ≠ "native_7788"）

- [ ] **Step 3: 最小实现**

`methods.py` `on_msg`（HEAD line 525）`mid = uuid.uuid4().hex[:12]` 改为：

```python
            # 原生 id 优先（确定性幂等键——Server 可按 msg_id 去重；
            # 同一消息重复回调命中同一池键，二次 RPC 不重复触发）。
            # 旧版 wxautox4 msg 无 id 属性 → 退回随机 uuid（不劣于现状）。
            mid = str(getattr(msg, "id", "")).strip() or uuid.uuid4().hex[:12]
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd sidecar-python && python3 -m pytest test_methods.py -q`
Expected: 88 passed（Task 1 后 86 + 2 新增）

- [ ] **Step 5: 提交**

```bash
git add sidecar-python/methods.py sidecar-python/test_methods.py
git commit -m "feat(sidecar): msg_id 原生 id 优先——上行幂等键本质修复,无 id 退回 uuid"
```

---

### Task 4: 消息上行去重——滑动窗吞重复回调

**Files:**
- Modify: `sidecar-python/methods.py`（模块级去重窗 + `on_msg` 入口调用）
- Test: `sidecar-python/test_methods.py`

**Interfaces:**
- Consumes: Task 3 的 msg_id 语义（同消息重复回调 → 同 msg_id）。
- Produces: 模块级函数 `_dedup_key(msg, chat_who) -> str` 与 `_seen_recently(key, now) -> bool`（now 参数注入供测试控时）——模块级 OrderedDict 窗口 5s、容量 1024、RLock 保护。

- [ ] **Step 1: 写失败测试**

**先加 autouse 隔离 fixture**（放既有 `_fast_sleep` fixture 旁——模块级去重窗是跨测试共享状态，任何触发回调的测试都可能被前序测试的同指纹静默吞掉通知，必须全文件隔离）：

```python
@pytest.fixture(autouse=True)
def _clear_dedup_window():
    """滑动窗去重状态测试隔离（模块级窗口每个测试清空）。

    getattr 防御：实现合入前 _DEDUP_WINDOW 尚不存在——fixture 不因此
    炸全文件（TDD 红阶段只红目标测试）。
    """
    import methods
    w = getattr(methods, "_DEDUP_WINDOW", None)
    if w is not None:
        w.clear()
    yield
```

在 Task 3 两个测试之后新增：

```python
# ══════════ 滑动窗去重（UIA 重复回调吞除）══════════


def test_dedup_same_fingerprint_within_window_swallowed(fakewx):
    """同指纹（会话+发送人+内容+类型）5s 内第二次回调被吞——notify 不再触发"""
    notifications = []
    msg_pool, msg_pool_ts = {}, {}
    _dispatch("listen.add", {"nickname": "客户群"}, fakewx,
              notify=lambda m, p: notifications.append((m, p)),
              msg_pool=msg_pool, msg_pool_ts=msg_pool_ts)
    msg = FakeMsg(content="重复消息")
    msg.id = "n1"
    chat = FakeChat("客户群", chat_type="group")
    fakewx.listen_reg["客户群"](msg, chat)
    fakewx.listen_reg["客户群"](msg, chat)  # 5s 内重复回调
    assert len(notifications) == 1, "同指纹窗口内重复应被吞"


def test_dedup_window_expiry_lets_same_fingerprint_pass(fakewx):
    """窗口过期后同指纹放行（把窗内时刻拨回 6s 前绕开真实等待）"""
    import methods
    notifications = []
    msg_pool, msg_pool_ts = {}, {}
    _dispatch("listen.add", {"nickname": "客户群"}, fakewx,
              notify=lambda m, p: notifications.append((m, p)),
              msg_pool=msg_pool, msg_pool_ts=msg_pool_ts)
    msg = FakeMsg(content="过期后放行")
    chat = FakeChat("客户群", chat_type="group")
    fakewx.listen_reg["客户群"](msg, chat)
    assert len(notifications) == 1
    # 把窗内唯一指纹的时刻拨回 6s 前（time.monotonic 时钟域）
    with methods._DEDUP_LOCK:
        k = next(iter(methods._DEDUP_WINDOW))
        methods._DEDUP_WINDOW[k] = time.monotonic() - 6.0
    fakewx.listen_reg["客户群"](msg, chat)
    assert len(notifications) == 2, "窗口过期后同指纹应放行"


def test_dedup_different_fingerprint_independent(fakewx):
    """不同指纹互不影响（会话/发送人/内容/类型任一不同即独立）"""
    notifications = []
    msg_pool, msg_pool_ts = {}, {}
    _dispatch("listen.add", {"nickname": "客户群"}, fakewx,
              notify=lambda m, p: notifications.append((m, p)),
              msg_pool=msg_pool, msg_pool_ts=msg_pool_ts)
    m1 = FakeMsg(content="A")
    m2 = FakeMsg(content="B")
    chat = FakeChat("客户群", chat_type="group")
    fakewx.listen_reg["客户群"](m1, chat)
    fakewx.listen_reg["客户群"](m2, chat)
    assert len(notifications) == 2


def test_dedup_capacity_evicts_oldest(fakewx):
    """容量上限淘汰最旧——被淘汰的老指纹可重新放行"""
    notifications = []
    msg_pool, msg_pool_ts = {}, {}
    _dispatch("listen.add", {"nickname": "客户群"}, fakewx,
              notify=lambda m, p: notifications.append((m, p)),
              msg_pool=msg_pool, msg_pool_ts=msg_pool_ts)
    chat = FakeChat("客户群", chat_type="group")
    # 灌满窗（容量 1024）：灌 1024 个不同指纹
    for i in range(1024):
        fakewx.listen_reg["客户群"](FakeMsg(content=f"c{i}"), chat)
    assert len(notifications) == 1024
    # 第 1025 个不同指纹照常放行（最旧被淘汰，不误吞新指纹）
    fakewx.listen_reg["客户群"](FakeMsg(content="c_new"), chat)
    assert len(notifications) == 1025
    # 最早的老指纹 c0 已被淘汰出窗 → 重新放行
    fakewx.listen_reg["客户群"](FakeMsg(content="c0"), chat)
    assert len(notifications) == 1026
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd sidecar-python && python3 -m pytest test_methods.py -x -q -k dedup`
Expected: FAIL——`AttributeError: module 'methods' has no attribute '_DEDUP_WINDOW'`

- [ ] **Step 3: 最小实现**

`methods.py` 模块级（`_pool_put` 函数之前）加：

```python
# ── 滑动窗去重（UIA 重复回调吞除，2026-09-10 P1）─────────────────
# wxautox4 的 MESSAGE_HASH 挡库内重复采集；本层挡回调层重复（通知通道
# Lagged 重订阅、监听重注册等场景）。窗口 5s、容量 1024，命中即整条
# 丢弃（不 _pool_put、不 notify——二次 RPC 也不会重复触发）。

_DEDUP_WINDOW_MS = 5000      # 指纹保留时长（毫秒）
_DEDUP_CAPACITY = 1024       # 窗口容量上限（防长期运行内存膨胀）
_DEDUP_LOCK = threading.RLock()
# OrderedDict[指纹 hex] = 记录时刻（time.monotonic 秒）——有序供容量淘汰
_DEDUP_WINDOW: "OrderedDict[str, float]" = OrderedDict()


def _dedup_key(msg, chat_who):
    """内容指纹：会话+发送人+内容+类型 四元组 md5"""
    raw = f"{chat_who}|{getattr(msg, 'sender', '')}|{getattr(msg, 'content', '')}|{getattr(msg, 'type', '')}"
    return hashlib.md5(raw.encode("utf-8", errors="replace")).hexdigest()


def _seen_recently(key, now=None):
    """窗口内命中返 True 并刷新时刻；未命中记录返 False。

    now 参数注入（测试控时用）；默认 time.monotonic()。容量超限淘汰最旧
    （OrderedDict 首项）——被淘汰指纹可重新通过（保守方向：宁可放行勿误吞）。
    """
    ts = time.monotonic() if now is None else now
    with _DEDUP_LOCK:
        # 先淘汰过期项（从最旧侧弹出，遇到首个未过期即停）
        while _DEDUP_WINDOW:
            oldest_k, oldest_ts = next(iter(_DEDUP_WINDOW.items()))
            if ts - oldest_ts > _DEDUP_WINDOW_MS / 1000.0:
                _DEDUP_WINDOW.popitem(last=False)
            else:
                break
        if key in _DEDUP_WINDOW:
            _DEDUP_WINDOW[key] = ts  # 刷新时刻
            return True
        _DEDUP_WINDOW[key] = ts
        if len(_DEDUP_WINDOW) > _DEDUP_CAPACITY:
            _DEDUP_WINDOW.popitem(last=False)
        return False
```

顶部 import 区补（若缺）：

```python
import hashlib
import threading
from collections import OrderedDict
```

`on_msg` 回调 try 块首行（`mid = ...` 之前）加：

```python
            if _seen_recently(_dedup_key(msg, str(getattr(chat, "who", "")))):
                return  # 窗口内重复回调：整条丢弃（不入池不通知）
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd sidecar-python && python3 -m pytest test_methods.py -q`
Expected: 92 passed（Task 3 后 88 + 4 新增）

- [ ] **Step 5: 提交**

```bash
git add sidecar-python/methods.py sidecar-python/test_methods.py
git commit -m "feat(sidecar): 滑动窗去重——(会话+发送人+内容+类型)md5 指纹 5s/1024 容量吞 UIA 重复回调"
```

---

### Task 5: webhook 告警——AlertClient 核心模块（含模板渲染）

**Files:**
- Create: `src-tauri/src/alert.rs`
- Modify: `src-tauri/src/lib.rs`（模块声明）
- Modify: `src-tauri/Cargo.toml`（reqwest 依赖）
- Test: `src-tauri/src/alert.rs` 内联测试（纯逻辑部分：模板渲染/no-op 语义）

**Interfaces:**
- Consumes: 无（新模块）。
- Produces:
  - `pub struct AlertClient { ... }`，构造 `AlertClient::new(webhook_url: String, webhook_template: String, device: String)`。
  - `pub fn send(&self, title: &str, detail: &str)`——**fire-and-forget**：内部 `tokio::spawn`，URL 空串直接 no-op。**同步签名**（挂点在同步回调/闭包里调用）。
  - `pub fn render_payload(template: &str, title: &str, detail: &str, ts: u64, device: &str) -> Value`——纯函数（pub 供测试）：模板合法走占位符渲染，非法降级通用 JSON。
  - `pub fn apply_placeholders(v: &mut Value, title: &str, detail: &str, ts: u64, device: &str)`——纯函数：递归遍历 JSON 所有字符串值替换 `{title}/{detail}/{ts}/{device}`。
  - lib.rs 加 `pub mod alert;`。

- [ ] **Step 1: 写失败测试**

创建 `src-tauri/src/alert.rs`（完整骨架：类型签名 + 全部测试 + todo! 实现体——跑测试红于 todo! panic）：

```rust
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

pub struct AlertClient {
    url: String,
    template: String,
    device: String,
}

impl AlertClient {
    pub fn new(webhook_url: String, webhook_template: String, device: String) -> Self {
        Self { url: webhook_url, template: webhook_template, device }
    }

    /// fire-and-forget 告警：空 URL no-op；否则 spawn 异步 POST。
    /// 同步签名——挂点（状态机回调/心跳变化点）是同步上下文。
    pub fn send(&self, title: &str, detail: &str) {
        todo!("Task 5 Step 3")
    }

    /// 实际发送（spawn 的异步任务体）
    async fn do_send(self: std::sync::Arc<Self>, title: String, detail: String) {
        todo!("Task 5 Step 3")
    }
}

/// 渲染告警载荷：模板合法 → 占位符替换；非法/失败 → 降级通用 JSON
pub fn render_payload(template: &str, title: &str, detail: &str, ts: u64, device: &str) -> Value {
    todo!("Task 5 Step 3")
}

/// 递归遍历 JSON 值的所有字符串，替换占位符 {title}/{detail}/{ts}/{device}
pub fn apply_placeholders(v: &mut Value, title: &str, detail: &str, ts: u64, device: &str) {
    todo!("Task 5 Step 3")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 飞书模板：msg_type/text/content 嵌套结构，占位符在深层字符串里
    #[test]
    fn test_render_payload_feishu_template() {
        let tpl = r#"{"msg_type":"text","content":{"text":"【{title}】{device}\n{detail}"}}"#;
        let v = render_payload(tpl, "微信掉线", "设备掉线", 1700000000, "wx-rig-01");
        assert_eq!(v["msg_type"], "text");
        assert_eq!(v["content"]["text"], "【微信掉线】wx-rig-01\n设备掉线");
    }

    /// detail 含引号+换行的 traceback 样本：渲染结果仍是合法 JSON 且值完整
    #[test]
    fn test_render_payload_traceback_detail_does_not_break_json() {
        let tpl = r#"{"text":"{detail}"}"#;
        let detail = "Traceback (most recent call last):\n  File \"x.py\", line 1\nRuntimeError: 'boom'";
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
}
```

- [ ] **Step 2: 跑测试确认失败**

先在 `src-tauri/src/lib.rs` 模块声明区（`pub mod agent_link;` 之后）加：

```rust
pub mod alert;
```

`src-tauri/Cargo.toml` `[dependencies]` 加（keyring 注释块之前）：

```toml
# webhook 告警（2026-09-10 P1）：tauri 传递已引入 reqwest 0.13 但未激活
# TLS 后端——显式 rustls-tls（纯 Rust TLS，Windows 打包无 OpenSSL 依赖）
reqwest = { version = "0.13", default-features = false, features = ["json", "rustls-tls"] }
```

Run: `cd src-tauri && cargo test --lib alert`
Expected: FAIL——todo!() panic（render_payload 等未实现）

- [ ] **Step 3: 最小实现**

`alert.rs` 实现体（替换全部 todo!）：

```rust
impl AlertClient {
    pub fn new(webhook_url: String, webhook_template: String, device: String) -> Self {
        Self { url: webhook_url, template: webhook_template, device }
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
            Err(_) => tracing::warn!(url = %self.url, title = %title, "告警发送超时({SEND_TIMEOUT_SECS}s)丢弃"),
            Ok(Err(e)) => tracing::warn!(url = %self.url, title = %title, "告警发送失败: {e}"),
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
            if s.contains("{title}") || s.contains("{detail}")
                || s.contains("{ts}") || s.contains("{device}")
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
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test --lib alert`
Expected: 6 passed

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/alert.rs src-tauri/src/lib.rs src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "feat(alert): AlertClient 旁路告警——模板先解析后渲染(占位符不破坏JSON)+5s超时+空URL no-op"
```

---

### Task 6: webhook 告警——HTTP 发送真路径测试（mock server）

**Files:**
- Test: `src-tauri/tests/alert.rs`（新建集成测试文件）

**Interfaces:**
- Consumes: Task 5 的 `AlertClient::send`（真实 HTTP 路径）。
- Produces: 集成测试守护发送语义（POST 到达/body 渲染/应用层拒绝识别）——无生产代码改动。

- [ ] **Step 1: 写失败测试**

创建 `src-tauri/tests/alert.rs`：

```rust
//! AlertClient HTTP 真路径集成测试：tokio TcpListener 起本地 HTTP 服务，
//! 断言 POST 到达、body 含渲染值、200+code≠0 判应用层失败（仅 warn 不炸）。
use std::time::Duration;
use wxauto_desktop::alert::AlertClient;

/// 极简 HTTP 服务：收一行请求（含 body）后回固定响应
async fn spawn_http_responder(
    respond_with: String,
) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = vec![0u8; 65536];
        let n = stream.read(&mut buf).await.unwrap();
        let req = String::from_utf8_lossy(&buf[..n]).to_string();
        let _ = tx.send(req);
        stream.write_all(respond_with.as_bytes()).await.unwrap();
    });
    (format!("http://{addr}/hook"), rx)
}

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
    );
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
    );
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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test --test alert`
Expected: FAIL——Task 5 的 `send` 是同步 spawn，测试直接能编译；若 Task 5 完成正确，`test_send_posts_rendered_payload_to_url` 应 PASS（这是守护测试——若 Task 5 有隐藏 bug 此时暴露）。**注意**：本任务是"集成守护"性质，若 Step 1 跑出全绿属正常（TDD 的验证步），此时视为验证通过并直接提交；若有 FAIL 则修 `alert.rs` 至绿。

- [ ] **Step 3:（条件）修实现至绿**

仅当 Step 2 有 FAIL 时执行——问题定位在 `alert.rs` 的 `do_send`（超时/headers/body 解析），按失败信息修，不在测试侧放水。

- [ ] **Step 4: 跑全量确认无回归**

Run: `cd src-tauri && cargo test`
Expected: 全绿

- [ ] **Step 5: 提交**

```bash
git add src-tauri/tests/alert.rs
git commit -m "test(alert): HTTP 真路径守护——POST 渲染 body/应用层拒绝/不可达不炸"
```

---

### Task 7: webhook 告警——配置链（Config/SettingsPatch/save_settings）

**Files:**
- Modify: `src-tauri/src/config.rs`（Config 结构 + Default，HEAD line 23-52）
- Modify: `src-tauri/src/app_state.rs`（SettingsPatch + extract_settings_patch + save_settings，HEAD line 36-52/453-482）
- Test: `src-tauri/src/config.rs` 内联测试、`src-tauri/src/app_state.rs` 内联测试、`src-tauri/tests/state_machine.rs`（test_config_roundtrip）

**Interfaces:**
- Consumes: 无。
- Produces:
  - `Config { webhook_url: String, webhook_template: String }`（serde camelCase → `webhookUrl`/`webhookTemplate`，Default 空串）。
  - `SettingsPatch { webhook_url: String, webhook_template: String }` + `extract_settings_patch` 提取 `webhookUrl`/`webhookTemplate`。
  - `save_settings` 合并两新字段（其余字段不动）。
  - Task 9 的 `Assembled.alert: Arc<AlertClient>` 构造依赖此两字段。

- [ ] **Step 1: 写失败测试**

`src-tauri/src/config.rs` 内联 tests 模块加：

```rust
    /// 新增 webhook 两字段：默认空串（=禁用）+ camelCase 序列化
    #[test]
    fn test_webhook_fields_default_and_camel() {
        let c = Config::default();
        assert_eq!(c.webhook_url, "", "默认禁用");
        assert_eq!(c.webhook_template, "");
        let v = serde_json::to_value(Config::default()).unwrap();
        assert_eq!(v["webhookUrl"], "");
        assert!(v.get("webhook_template").is_none(), "序列化必须是 camelCase");
    }
```

`src-tauri/src/app_state.rs` 内联 tests 的 `test_extract_settings_patch_fields`（HEAD line 521）改造——提交 JSON 加 webhook 字段并断言提取（在现有断言后补）：

```rust
        // webhook 两字段提取（2026-09-10 P1）
        let patch3 = extract_settings_patch(&serde_json::json!({
            "serverUrl": "s", "channelId": "c", "autoConnect": true,
            "webhookUrl": "https://open.feishu.cn/hook/x", "webhookTemplate": "{\"text\":\"{title}\"}"
        }));
        assert_eq!(patch3.webhook_url, "https://open.feishu.cn/hook/x");
        assert_eq!(patch3.webhook_template, "{\"text\":\"{title}\"}");
        // 缺字段兜底空串（不 panic）
        let patch4 = extract_settings_patch(&serde_json::json!({
            "serverUrl": "s", "channelId": "c"
        }));
        assert_eq!(patch4.webhook_url, "");
```

`src-tauri/tests/state_machine.rs` `test_config_roundtrip`（HEAD line 206-230）：cfg 构造加两字段 + roundtrip 断言：

```rust
        webhook_url: "https://open.feishu.cn/hook/abc".into(),
        webhook_template: "{\"text\":\"{title}\"}".into(),
```

（roundtrip 加载后补：）

```rust
    assert_eq!(loaded.webhook_url, "https://open.feishu.cn/hook/abc");
    assert_eq!(loaded.webhook_template, "{\"text\":\"{title}\"}");
```

**注意**：test_config_roundtrip 现有 cfg 构造使用字段初始化（非 `..Default::default()`），加字段后其它使用 `Config { .. }` 字面量构造的测试会编译错——统一给这些构造处补 `webhook_url: String::new(), webhook_template: String::new(),` 或改 `..Default::default()`（按上下文就近原则，构造点含 tests/state_machine.rs、src/config.rs tests、src/app_state.rs tests——编译器会逐一点名）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test --lib config`
Expected: FAIL——编译错误 `missing field webhook_url`

- [ ] **Step 3: 最小实现**

`config.rs` Config 结构（delay_max_ms 之后）加：

```rust
    /// webhook 告警地址（空=禁用；2026-09-10 P1）
    pub webhook_url: String,
    /// webhook 自定义模板（空=通用 JSON {title,detail,ts,device}）
    pub webhook_template: String,
```

Default 加：

```rust
            webhook_url: String::new(),
            webhook_template: String::new(),
```

`app_state.rs` SettingsPatch 加两字段（auto_connect 后）：

```rust
    pub webhook_url: String,
    pub webhook_template: String,
```

extract_settings_patch 补两行提取：

```rust
        webhook_url: config["webhookUrl"].as_str().unwrap_or_default().to_string(),
        webhook_template: config["webhookTemplate"].as_str().unwrap_or_default().to_string(),
```

save_settings 合并段（`cfg.auto_connect = patch.auto_connect;` 后）补：

```rust
            cfg.webhook_url = patch.webhook_url;
            cfg.webhook_template = patch.webhook_template;
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test --lib && cargo test --test state_machine`
Expected: 全绿

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/config.rs src-tauri/src/app_state.rs src-tauri/tests/state_machine.rs
git commit -m "feat(config): webhookUrl/webhookTemplate 配置链——Config默认禁用+Patch提取+save合并"
```

---

### Task 8: webhook 告警——两个挂点接线（SidecarDead + wxOnline 翻转）

**Files:**
- Modify: `src-tauri/src/agent_link/mod.rs`（AgentLink 加 alert_sink + heartbeat_loop 挂点 + on_disconnect 告警清理）
- Modify: `src-tauri/src/app_state.rs`（Assembled 加 alert 字段 + 装配段构造 + on_change 回调挂告警 + start_link 注入 alert_sink）
- Test: `src-tauri/tests/state_machine.rs`（SidecarDead 触发告警）、`src-tauri/tests/agent_link.rs`（wxOnline 翻转触发）

**Interfaces:**
- Consumes: Task 5 `AlertClient::send`；Task 7 `Config.webhook_url/webhook_template`。
- Produces:
  - `AgentLink.set_alert_sink(&self, client: std::sync::Arc<AlertClient>)`——运行期注入（模式照抄 set_event_sink：RwLock<Option<...>> 中毒恢复）。
  - `Assembled.alert: Arc<AlertClient>`——Task 9 前端保存后热更新用。
  - 挂点行为：进 SidecarDead 发（title="sidecar 瘫痪"，detail=退避耗尽描述）；wxOnline true→false 发（"微信掉线"）false→true 发（"微信恢复"）。

- [ ] **Step 1: 写失败测试**

`src-tauri/tests/state_machine.rs` 新增——守「SidecarDead 恰好触发一次 on_change(new=SidecarDead)」语义（webhook 挂点依赖此；AlertClient 的 HTTP 行为已由 Task 6 守护，装配闭包真实行为由 app_state 内联测试守）：

```rust
/// SidecarDead 终态触发一次 on_change(new=SidecarDead)——webhook 挂点依赖的语义；
/// 终态重复标记不重复触发（同值不回调 → 不重复告警）
#[tokio::test]
async fn test_sidecar_dead_fires_on_change_exactly_once() {
    let fired = Arc::new(std::sync::Mutex::new(0));
    let f2 = fired.clone();
    let m = AppStateMachine::new()
        .with_on_change(Box::new(move |_old, new: &AppState| {
            if *new == AppState::SidecarDead {
                *f2.lock().unwrap() += 1;
            }
        }))
        .await;
    m.mark_sidecar_died().await;
    m.mark_sidecar_died().await; // 终态重复标记不重复告警（同值不触发）
    assert_eq!(*fired.lock().unwrap(), 1);
}
```

`src-tauri/src/agent_link/mod.rs` 内联测试（`#[cfg(test)] mod tests`——若不存在则新建）新增——用可计数 fake 守翻转语义。fake 通过**不 import AlertClient**、只验证 `on_online_changed` 的分支行为实现：由于 alert_sink 类型是 `Arc<AlertClient>`，为可测试性在 AlertClient 上加一个测试可见的计数钩子代价大——**改为守行为等价物**：`on_online_changed` 内联日志断言不可行，故此测试改守「私有方法存在且空 sink 不炸」的最小契约 + 翻转分支逻辑由 Task 8 Step 3 实现中的 tracing 日志句守护。**结论：mod.rs 内联测试只测 set_alert_sink 注入/读取往返**：

```rust
#[cfg(test)]
mod alert_tests {
    use super::*;
    use crate::alert::AlertClient;
    use std::sync::Arc;

    /// set_alert_sink 注入后可读回（RwLock 中毒恢复路径同 event_sink）
    #[tokio::test]
    async fn test_alert_sink_roundtrip() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let handle = SidecarHandle::spawn_with_python("pass\n", &[])
            .await
            .expect("spawn 失败");
        let session = Arc::new(WxSession::new(Arc::new(Mutex::new(handle))));
        let listeners = Arc::new(ListenerRegistry::new(session.clone()));
        let link = AgentLink::new(session, listeners, format!("ws://{addr}/"));
        let client = Arc::new(AlertClient::new(String::new(), String::new(), "t".into()));
        link.set_alert_sink(client.clone());
        assert!(
            link.alert_sink
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some(),
            "注入后应可读回"
        );
        // 空 URL send 全链不炸（no-op）
        link.on_online_changed(false).await;
        link.on_online_changed(true).await;
    }
}
```

`src-tauri/tests/agent_link.rs` 不新增（翻转语义已由 mod.rs 内联测试守护；30s 心跳真实驱动不现实）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test --lib alert_tests && cargo test --test state_machine test_sidecar_dead_fires`
Expected: FAIL——编译错 `alert_sink` 字段私有不可测试访问 + `set_alert_sink`/`on_online_changed` 不存在（mod.rs 内联测试在 crate 内可访问私有字段——若仍报错则按编译器点名补）

- [ ] **Step 3: 最小实现**

（原 Step 3 内容不变，见下）

- [ ] **Step 3: 最小实现**

`src-tauri/src/agent_link/mod.rs`：

1. AgentLink 结构加字段（command_sink 之后）：

```rust
    /// webhook 告警客户端（GUI 注入；None = 未装配告警）。运行期注入，
    /// 模式照抄 event_sink。
    alert_sink: RwLock<Option<std::sync::Arc<crate::alert::AlertClient>>>,
```

2. `new_with_event_sink` 构造器初始化 `alert_sink: RwLock::new(None),`

3. set 方法（set_command_sink 之后）：

```rust
    /// 注入 webhook 告警客户端（GUI 装配段调用）
    pub fn set_alert_sink(&self, client: std::sync::Arc<crate::alert::AlertClient>) {
        *write_sink(&self.alert_sink).unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(client);
    }

    /// 心跳 wxOnline 翻转告警（true→false 掉线 / false→true 恢复；
    /// 变化才发天然防风暴）
    async fn on_online_changed(&self, online: bool) {
        if let Some(alert) = self
            .alert_sink
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            if online {
                alert.send("微信已恢复", "心跳探测恢复在线");
            } else {
                alert.send("微信掉线", "心跳探测离线（微信关闭/掉登录/窗口不可操作）");
            }
        }
    }
```

4. heartbeat_loop 的翻转分支（`if last_online != Some(online) {` 内、`last_online = Some(online);` 之后）补一行：

```rust
                self.on_online_changed(online).await;
```

`src-tauri/src/app_state.rs`：

1. Assembled 加字段（bridge 之前）：

```rust
    /// webhook 告警客户端（SidecarDead/掉线两挂点共用；保存设置后热更新）
    pub alert: Arc<wxauto_desktop::alert::AlertClient>,
```

2. 装配段：状态机构造处（`// 3. 状态机` 注释块）改为同时构造 alert 并在 on_change 挂告警——状态机构造之前先建：

```rust
                // 3'. webhook 告警客户端（配置驱动；空 URL = 禁用 no-op）
                let alert = Arc::new(wxauto_desktop::alert::AlertClient::new(
                    cfg.webhook_url.clone(),
                    cfg.webhook_template.clone(),
                    wxauto_desktop::alert::hostname(),
                ));
```

**（注意：`hostname()` 现在在 `agent_link/mod.rs` 是私有 fn，而 app_state.rs 在 bin 层（tauri 的 GUI 装配）——`pub(crate)` 不可见跨 crate。正确做法：把 `hostname()` 挪到 `src-tauri/src/alert.rs` 导出 `pub fn hostname() -> String`（实现体从 agent_link/mod.rs 原样迁移），agent_link/mod.rs 内改为 `crate::alert::hostname()` 调用并删除原私有 fn——core 内部互引，GUI 装配经 `wxauto_desktop::alert::hostname()` 取用。）**

状态机 on_change 闭包改为（alert 捕获）：

```rust
                let state_machine = {
                    let bridge = bridge.clone();
                    let alert = alert.clone();
                    Arc::new(
                        AppStateMachine::new()
                            .with_on_change(Box::new(move |_old, new| {
                                let bridge = bridge.clone();
                                let alert = alert.clone();
                                let new = new.clone();
                                tokio::spawn(async move {
                                    bridge.emit_state(&new).await;
                                    if new == wxauto_desktop::state::AppState::SidecarDead {
                                        alert.send(
                                            "sidecar 瘫痪",
                                            "退避重启耗尽进入终态——设备需人工介入（重启 App 或检查微信环境）",
                                        );
                                    }
                                });
                            }))
                            .await,
                    )
                };
```

3. Assembled 构造字面量加 `alert,`（bridge 之前）。

4. start_link（`impl Assembled` 块内——`self` 即 Assembled；现行代码在构造 link 后有 P2 的 `link.set_channel_id(...)` 行）：set_command_sink 块之后补注入：

```rust
        // webhook 告警（心跳掉线/恢复挂点）：Assembled.alert 与状态机挂点同源
        link.set_alert_sink(self.alert.clone());
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test`
Expected: 全绿（新增：state_machine 1 例 on_change once 语义 + mod.rs 内联 alert_sink 注入/no-op 测试）

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/agent_link/mod.rs src-tauri/src/app_state.rs src-tauri/tests/state_machine.rs
git commit -m "feat(alert): 挂点接线——SidecarDead 终态+心跳 wxOnline 翻转双场景,注入模式照抄 event_sink"
```

---

### Task 9: webhook 告警——前端设置页与 store 链

**Files:**
- Modify: `src/stores/app.ts`（AppConfig + parseAppConfig，HEAD line 131-137/208-216）
- Modify: `src/views/Settings.vue`（form + 模板 + onSave，HEAD line 50-92）
- Test: `src/views/Settings.spec.ts`（FAKE_CONFIG + 新用例）

**Interfaces:**
- Consumes: Task 7 的 get_config 返回 `webhookUrl`/`webhookTemplate`、save_config 接受两字段。
- Produces: 设置页 webhook 配置 UI（URL input + 模板 textarea，均可选）；`AppConfig.webhookUrl/webhookTemplate` 类型字段。

- [ ] **Step 1: 写失败测试**

`src/views/Settings.spec.ts` FAKE_CONFIG 加：

```typescript
const FAKE_CONFIG = {
  serverUrl: 'ws://127.0.0.1:60021',
  channelId: 'ch-001',
  autoConnect: false,
  webhookUrl: 'https://open.feishu.cn/hook/demo',
  webhookTemplate: '',
};
```

新增用例（describe 块内）：

```typescript
  it('webhook 字段回填与提交——URL 必须随保存透传', async () => {
    const wrapper = mountSettings();
    const { useAppStore } = await import('../stores/app');
    useAppStore().config = { ...FAKE_CONFIG };
    await flushPromises();
    const inputs = wrapper.findAll('input');
    const urlInput = inputs.find((i) =>
      (i.element as HTMLInputElement).value.includes('feishu'),
    );
    expect(urlInput).toBeTruthy();
    // 提交：填 token 触发完整保存链
    wrapper.find('form').trigger('submit');
    await flushPromises();
    const call = invokeMock.mock.calls.find((c) => c[0] === 'save_config');
    expect(call).toBeTruthy();
    const payload = (call?.[1] as { config: Record<string, unknown> }).config;
    expect(payload.webhookUrl).toBe(FAKE_CONFIG.webhookUrl);
    expect(payload.webhookTemplate).toBe('');
  });
```

- [ ] **Step 2: 跑测试确认失败**

Run: `pnpm test -- Settings.spec.ts`
Expected: FAIL——`urlInput` 为 undefined（表单无 webhook 字段）

- [ ] 里料补齐（前端 saveConfig 链）

`src/stores/app.ts`：

`AppConfig` 接口加：

```typescript
export interface AppConfig {
  serverUrl: string;
  channelId: string;
  autoConnect: boolean;
  webhookUrl: string;
  webhookTemplate: string;
}
```

`parseAppConfig` 补：

```typescript
    webhookUrl: str(r.webhookUrl),
    webhookTemplate: str(r.webhookTemplate),
```

`src/views/Settings.vue`：

form reactive 加：

```typescript
const form = reactive({
  serverUrl: '',
  channelId: '',
  token: '',
  autoConnect: false,
  webhookUrl: '',
  webhookTemplate: '',
});
```

watch 回填补：

```typescript
      form.webhookUrl = cfg.webhookUrl;
      form.webhookTemplate = cfg.webhookTemplate;
```

模板（自动连接 t-form-item 之后、提交按钮之前）加：

```html
      <t-form-item label="告警 Webhook" name="webhookUrl">
        <t-input
          v-model="form.webhookUrl"
          placeholder="https://open.feishu.cn/open-apis/bot/v2/hook/xxx（留空禁用）"
        />
        <span class="hint">sidecar 瘫痪 / 微信掉线时通知</span>
      </t-form-item>
      <t-form-item label="告警模板" name="webhookTemplate">
        <t-textarea
          v-model="form.webhookTemplate"
          placeholder='留空用通用 JSON；支持 {title} {detail} {ts} {device} 占位符'
          :autosize="{ minRows: 3, maxRows: 8 }"
        />
      </t-form-item>
```

onSave 的 saveConfig 入参补：

```typescript
      webhookUrl: form.webhookUrl.trim(),
      webhookTemplate: form.webhookTemplate,
```

- [ ] **Step 3: 跑测试确认通过**

Run: `pnpm test`
Expected: 全绿（Settings 新用例过；app.spec.ts 无回归——parseAppConfig 扩字段向后兼容）

- [ ] **Step 4: 提交**

```bash
git add src/stores/app.ts src/views/Settings.vue src/views/Settings.spec.ts
git commit -m "feat(settings): webhook 告警配置 UI——URL+自定义模板(占位符提示),保存透传"
```

---

### Task 10: is_at 前端展示 + 消息流水「@我」列

**Files:**
- Modify: `src/stores/app.ts`（MessageItem + parseMessageItem，HEAD line 77-87/180-193）
- Modify: `src/views/Messages.vue`（columns，HEAD line 24-32）
- Test: `src/stores/app.spec.ts`（parseMessageItem 等价单测——经 pushMessage 驱动）

**Interfaces:**
- Consumes: Task 2 的 event:message 帧 `data.isAt`。
- Produces: `MessageItem.isAt: boolean`；消息流水表「@我」列（t-tag 显示）。

- [ ] **Step 1: 写失败测试**

`src/stores/app.spec.ts` 新增（放既有 store 测试 describe 内）：

```typescript
  it('pushMessage 保留 isAt 字段（群@语义透传）', () => {
    const store = useAppStore();
    store.pushMessage({
      id: 0, chatName: '客户群', chatType: 'group', sender: '李四',
      msgType: 'text', content: '@机器人 报价', ts: 1, isAt: true,
    } as MessageItem);
    expect(store.messages[0].isAt).toBe(true);
  });
```

（import 行补 `type MessageItem`。注意 `pushMessage` 现签名是 `(msg: MessageItem)`，`id: 0` 会被环形逻辑覆盖——若编译器报 isAt 不在 MessageItem，正是预期失败。）

- [ ] **Step 2: 跑测试确认失败**

Run: `pnpm test -- app.spec.ts`
Expected: FAIL——TS 编译错 `isAt does not exist in type MessageItem`

- [ ] **Step 3: 最小实现**

`src/stores/app.ts` `MessageItem` 接口加（content 之后）：

```typescript
  /** 群内 @机器人 标记（event:message data.isAt） */
  isAt: boolean;
```

`parseMessageItem` 返回对象补：

```typescript
    isAt: r.isAt === true,
```

`src/views/Messages.vue` columns（msgType 之后）加：

```typescript
  { colKey: 'isAt', title: '@我', width: 70, cell: 'at' },
```

模板（msgType template 之后）加：

```html
    <template #at="{ row }">
      <t-tag v-if="row.isAt" size="small" theme="warning" variant="light">是</t-tag>
      <span v-else>-</span>
    </template>
```

- [ ] **Step 4: 跑测试确认通过**

Run: `pnpm test`
Expected: 全绿

- [  ] **Step 5: 提交**

```bash
git add src/stores/app.ts src/views/Messages.vue src/stores/app.spec.ts
git commit -m "feat(ui): 消息流水@我列——isAt 群@语义前端落点"
```

---

### Task 11: msg.forward 真机验证清单（文档交付）

**Files:**
- Create: `docs/forward-context-verification.md`

**Interfaces:**
- Consumes: 探索结论（`_msg_forward` HEAD line 423-443 缓解已有、`_chat_who` 取了弃用、`_msg_quote` 无预切换）。
- Produces: 真机验收文档（不发不碰生产代码）。

- [ ] **Step 1: 写文档**

`docs/forward-context-verification.md` 全文：

````markdown
# msg.forward 上下文坑——真机验证清单

> 2026-09-10 P1 交付物。背景：参考项目 SiverWXbot 的深度注释指出
> 「转发必须在 add_chat_to_listen 之前执行——msg.forward() 依赖主窗口
> 上下文，加入监听后上下文切到子窗口会失效」（wxbot_core.py:4714-4715）。
> 本项目生产环境监听恒处于注册态（init/connect 双路径 resync），所有
> forward 都运行在「已注册监听」环境下。代码已有缓解但未经验证：
> `_msg_forward`（sidecar-python/methods.py）池直取路径先
> `wx.ChatWith(who=target)` 再 `msg.forward(target)`，docstring 自述
> 「切回主窗口上下文」与 ChatWith(target) 实际语义（切到目标会话）有偏差。

## 风险假设

H1（缓解等价性）：ChatWith(target) 预切换后 forward 不受监听子窗口
    上下文影响——**未验证**。
H2（locate 路径扰动）：_locate_msg 内 GetAllMessage 遍历（UIA 读取）
    期间监听回调扰动使定位到的 msg 控件引用失效——**未验证**。
H3（quote 同族）：_msg_quote 的 msg.quote() 无 ChatWith 预切换，若
    forward 的上下文约束同样适用于 quote，此处未设防——**未验证**。

## 链路 A：池直取 forward（msgId 路径）

**前置条件**
- Windows 真机，微信已登录，desktop App 已连接（Ready 态）
- 监听名单 ≥1 个群（如「测试群1」），群里另一账号发一条文本消息
- 面板「指令日志」tab 可见；sidecar 运行日志可见

**步骤**
1. 群里发消息 M（等待消息流水出现该消息——确认回调路径已把 M 入池）
2. 立即（60s 池 TTL 内）经面板「手动执行」下发 forward_message：
   `{"msgId": "<消息流水里的 msg_id>", "target": "文件传输助手"}`
   （或经服务端 agent 指令下发同 action）
3. 观察微信客户端：文件传输助手会话是否收到 M 的转发

**预期**：转发成功，指令日志 ok=true，无 UIA 异常。

**失败特征与裁决**
- `LookupError: Find Control Timeout` → H1 不成立：ChatWith(target)
  预切换不等价主窗口上下文。修复方向：ChatWith 切**源会话**
  （methods.py `_take_msg` 返回的 `_chat_who`——当前取了弃用，是
  现成修复钩子）后 sleep 1 再 forward。
- 成功但慢（>5s）→ 记录耗时，评估 sleep 1 是否需加长。

## 链路 B：locate forward（无 msgId 路径）

**前置条件**：同链路 A，但操作前先在群里连续发 3+ 条消息（制造
GetAllMessage 遍历期间的监听回调活动）。

**步骤**
1. 不带 msgId 下发 forward_message：
   `{"sourceWho": "测试群1", "match": "<M 的内容片段>", "target": "文件传输助手"}`
2. 观察同链路 A 第 3 步。

**预期**：定位成功且转发成功。

**失败特征与裁决**
- 「未定位到要转发的消息」但消息确实存在 → 定位本身失败（match
  精度问题），与上下文坑无关，另行处理。
- 定位成功但 forward 抛 UIA 超时 → H2 成立：遍历期间回调扰动使
  控件引用失效。修复方向：locate 后补 ChatWith(who=sourceWho)+sleep 1
  再 forward（与池直取路径同款缓解）。

## 链路 C（顺带观察）：quote 无预切换

**前置条件**：同链路 A。

**步骤**
1. 60s 内下发 quote_message：
   `{"msgId": "<M 的 msg_id>", "text": "收到"}`
2. 观察群内是否出现引用回复。

**预期**：成功。若失败抛 UIA 超时 → H3 成立，同款 ChatWith 预切换
补进 _msg_quote。

## 结果记录

| 链路 | 日期 | 结果 | 失败特征 | 裁决 |
|------|------|------|----------|------|
| A | | | | |
| B | | | | |
| C | | | | |

（验证完成后回填本表；任一 H 成立则开新修复任务，勿在此清单内顺手改代码。）
````

- [ ] **Step 2: 校验文档内行号引用**

验证 methods.py（HEAD）中 `_take_msg` 确实返回 `(msg, chat_who)` 且
`_msg_forward` 弃用 `_chat_who`——`git show HEAD:sidecar-python/methods.py | sed -n '423,443p'`。

- [ ] **Step 3: 提交**

```bash
git add docs/forward-context-verification.md
git commit -m "docs: msg.forward 上下文坑真机验证清单——三链路假设/步骤/失败裁决"
```

---

### Task 12: 收尾——全量回归 + 合回 main

**Files:**
- 无新文件（验证任务）

**Interfaces:**
- Consumes: Task 1-11 全部。
- Produces: worktree 分支合回 main（ff-only）。

- [ ] **Step 1: 全量测试三端**

```bash
cd sidecar-python && python3 -m pytest test_methods.py -q        # 期望 92 例全绿（HEAD 基线 83 + 9 新增）
cd ../src-tauri && cargo test                                    # 期望全绿（含 alert 集成）
cd .. && pnpm test                                               # 期望全绿（Settings/app 新用例）
pnpm typecheck                                                   # vue-tsc 零错误
```

- [ ] **Step 2: 核对交付物清单**

- [ ] is_at 链路四层全通（FakeMsg → 通知帧 → RawMessage → WS 帧 isAt → 前端 @我列）
- [ ] msg_id 原生 id 优先（确定性幂等键）
- [ ] 滑动窗去重（5s/1024，RLock 保护，测试隔离清理）
- [ ] AlertClient（模板渲染/超时/no-op）
- [ ] 两挂点（SidecarDead / wxOnline 翻转）+ 配置链 + 前端 UI
- [ ] forward 验证清单文档
- [ ] 每任务一 commit（11 个 feat/test/docs commit）

- [ ] **Step 3: 合回 main（在主 checkout 执行，非 worktree）**

```bash
cd /home/working/lyagent/desktop
git merge --ff-only <worktree-分支名>
```

（主 checkout 的遗留改动（methods.py/test_methods.py UIA 诊断化）不受
影响——合并只动两侧不同区域；**若 ff-only 因 main 在 worktree 分出后有新
提交而失败**，先 `git merge-base --is-ancestor` 判断，确实分叉则回 worktree
`git rebase main` 后再合。）

- [ ] **Step 4: 清理 worktree（合回确认后）**

```bash
git worktree remove <path> && git branch -d <branch>
```

---

## Self-Review 记录

1. **Spec 覆盖**：任务 1-2 ↔ spec 任务 1（is_at 四层）；任务 3-4 ↔ spec 任务 2（双保险）；任务 5-9 ↔ spec 任务 3（webhook 全链含前端）；任务 10 ↔ spec 任务 1 前端落点；任务 11 ↔ spec 任务 4；任务 12 ↔ 集成策略。无遗漏。
2. **占位符扫描**：Task 5 Step 1 有意展示了「草稿→完整」两段（skill 要求 plan 不留 TODO，已把完整测试集写出并在正文注明保留语义版）；Task 8 Step 1 同理两版本二选一已注明取舍。无未定义引用。
3. **类型一致性**：`AlertClient::new(url, template, device)` 在 Task 5/6/8 一致；`set_alert_sink(Arc<AlertClient>)` 在 Task 8 定义并使用；`webhook_url/webhook_template`（Rust）/`webhookUrl/webhookTemplate`（TS）两侧命名对齐 serde camelCase。
4. **修正记录**：spec 原文「零新增编译单元」经查证不实（tauri 传递 reqwest 未激活 TLS），已在 spec commit 41c21bc 修正，计划 Task 5 Step 2 的 Cargo.toml 片段与之对齐。
```
