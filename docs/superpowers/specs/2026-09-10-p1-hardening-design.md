# P1 加固设计：is_at 透传 · 消息去重 · webhook 告警 · forward 验证清单

> 2026-09-10 · 源自与 SiverWXbot_plus（原生参考项目）的横向评比结论 P1 档。
> 评分上下文：desktop 是 Tauri v2 (Rust) + Vue + Python sidecar 的微信自动化设备端，
> 架构已领先（进程自愈/事件总线/测试覆盖），本批吸收对方在「语义正确 + 可运维」上的
> 四项工程补偿。

## 背景与目标

评比发现的 P1 缺口：

| # | 缺口 | 现状 |
|---|------|------|
| 1 | is_at 语义缺失 | `agent_link/inbound.rs:32` 硬编码 `"isAt": false`，sidecar 侧从未读取 wxautox `msg.is_at` |
| 2 | 消息无法幂等 | `methods.py:525` msg_id 是随机 uuid；Rust 上行纯转发无去重 |
| 3 | 零外发告警 | sidecar 崩溃终态/微信掉线只能等用户自己看面板 |
| 4 | msg.forward 上下文坑 | 已有 ChatWith 预切换缓解但未验证正确性（`_chat_who` 取了弃用） |

目标：前三项全量开发（TDD），第四项交付真机验证清单文档。

## 任务 1：is_at 透传

打通 wxautox `msg.is_at` → sidecar 通知帧 → Rust `RawMessage` → 上行 WS 帧 `isAt` 全链。

**改动点：**

1. `sidecar-python/methods.py` `_raw_message()`（line 188）：dict 加
   `"is_at": bool(getattr(msg, "is_at", False))`——getattr 默认值兜底，旧版
   wxautox4 无此属性时安全降级 false。
2. `src-tauri/src/sidecar/spec.rs` `RawMessage`（line 52）：加
   `#[serde(default)] pub is_at: bool`——serde default 兼容旧 sidecar 帧。
3. `src-tauri/src/agent_link/inbound.rs:32`：`"isAt": false` 改为
   `n["is_at"].as_bool().unwrap_or(false)`。
4. 前端 `src/views/Messages.vue` columns 加「@我」列（读 `isAt`），
   `src/stores/app.ts` `parseMessageItem` 透传该字段。

**测试：**

- `test_methods.py`：FakeMsg 加 `is_at` 属性（附录 A 形状同步扩），
  `test_listen_callback_fires_notification_with_msg_id`（line 631）补通知帧断言；
  新增「无 is_at 属性降级 false」用例。
- Rust `src/sidecar/spec.rs` 单测：RawMessage roundtrip 含 is_at；
  旧帧（无 is_at 键）反序列化 default false。
- Rust `tests/agent_link.rs` `test_message_event_from_notification_camel`：
  补 isAt 断言。

## 任务 2：消息上行去重（原生 id 透传 + 滑动窗双保险）

### 层 1 — 原生 id 透传（幂等键本质修复）

`methods.py` `on_msg` 回调（line 518-527）：

```python
mid = str(getattr(msg, "id", "")).strip() or uuid.uuid4().hex[:12]
```

- 优先取 wxautox 原生 msg.id（确定性，Server 可按 msg_id 幂等去重）；
  取不到（旧版库）退回随机 uuid，行为不劣于现状。
- msg 池键、`download_media`/`voice_to_text` 的 msgId 参数随之自动获得
  确定性（同一条消息重复回调时命中同一池键，二次 RPC 不会重复触发）。

### 层 2 — Python 滑动窗（吞 UIA 重复回调）

`on_msg` 入口处新增内容指纹去重：

- 模块级 `OrderedDict` + `threading.RLock`（wxautox 回调线程安全）。
- 键：`hashlib.md5(f"{chat_who}|{sender}|{content}|{msg_type}".encode()).hexdigest()`。
- 窗口 5s；命中即整条丢弃（不 `_pool_put`、不 `notify`）。
- 容量上限 1024 条，超限淘汰最旧（防长期运行内存膨胀）。
- 时间源：`time.time()`，但去重判定函数接受 `now` 参数注入（测试可控时序）。

**边界与不做：**

- `chat.history` 路径（`methods.py:485-496`）走 `getattr(m, "id")`，与此路径
  不同源，去重键不混用、history 不去重（语义就是拉全量）。
- MESSAGE_HASH=True（wxautox 库层，line 255 已开）保持不动——它挡库内重复
  采集，本层挡回调层重复，互补。

**测试（test_methods.py）：**

- 幂等键：FakeMsg 带 id → 通知帧 msg_id == 原生 id；不带 id → uuid 形态（12 hex）。
- 滑动窗：同指纹 5s 内第二次回调被吞（notify 调用数不变）；
  窗口过期后同指纹放行（注入 now 推进）；
  不同指纹互不影响；容量上限淘汰最旧后老指纹可重新放行。
- RLock 并发冒烟（多线程同指纹并发回调只过一条）。

## 任务 3：webhook 告警通道

### 架构

新建 `src-tauri/src/alert.rs` —— `AlertClient`「旁路观察者」（与 event_sink
同款哲学：绝不阻塞主链路）：

- `send(title, detail)`：内部 `tokio::spawn` + 5s 超时；失败/超时仅
  `tracing::warn!`，无重试队列（YAGNI，告警丢失可接受，风暴不可接受）。
- `webhook_url` 空串 = 禁用（默认），send 直接 no-op。
- 依赖 `reqwest = { version = "0.13", features = ["json"] }`——Cargo.lock
  已有（tauri 传递依赖），零新增编译单元。
- lib.rs core 不依赖 tauri 的约束不破坏（alert.rs 只需 reqwest + tokio）。

### 模板机制（借鉴 SiverWXbot 两处巧思）

- 默认通用 JSON：`{"title", "detail", "ts", "device"}`。
- 用户自定义模板：**先 `serde_json::from_str` 解析成 JSON 值，再递归遍历
  所有字符串做占位符替换**（`{title}`/`{detail}`/`{ts}`/`{device}`），
  替换后序列化发送——detail 含引号/换行（traceback）不会破坏 JSON 结构。
- 模板非法 JSON 或渲染失败：降级通用 JSON + tracing warn。
- 应用层拒绝识别：HTTP 200 后解析响应体 `code` 字段（飞书等平台 200 但
  code≠0 = 真实失败），判失败记 warn。

### 挂点（已拍板两项场景）

1. **SidecarDead 终态**：`AppStateMachine::with_on_change` 回调 match 新态
   == SidecarDead 发告警。回调在写锁释放后触发、AlertClient 内部 spawn，
   不阻塞状态机。GUI 侧 `app_state.rs:371-384` 已注入 on_change 转发
   wxauto://state 事件，告警挂点在其旁（Assembled 持 `Arc<AlertClient>`）。
2. **wxOnline 翻转**：`agent_link/mod.rs` `heartbeat_loop` 的
   `last_online != Some(online)` 变化点（line 430 附近）——true→false 发
   「微信掉线」告警，false→true 发「已恢复」通知；变化才发天然防风暴。
   通过给 `AgentLink` 加 `alert_sink`（模式照抄 event_sink 的
   `RwLock<Option<...>>` 注入式设计，mod.rs:92-97）。

已知事实：Degraded 态是死代码（`mark_online` 无生产调用方），本任务不补
状态机驱动（超范围），wxOnline 告警只挂心跳变化点。

### 配置链（5 处落点）

Config 加 `webhook_url: String` + `webhook_template: String`（serde camelCase
→ `webhookUrl`/`webhookTemplate`，Default 均空串）：

1. `src-tauri/src/config.rs` Config + Default；
2. `src-tauri/src/app_state.rs` `SettingsPatch`（line 36）+
   `extract_settings_patch`（line 45）+ `save_settings` 合并段（line 470）；
3. `src/views/Settings.vue` form + t-form-item（webhookUrl input +
   webhookTemplate textarea，均可选）+ onSave；
4. `src/stores/app.ts` `AppConfig`（line 132）+ `parseAppConfig`（line 208）；
5. 测试同步：`tests/state_machine.rs` config roundtrip、
   `Settings.spec.ts` FAKE_CONFIG、`app_state.rs` patch 单测。

### 测试

- `alert.rs` 单测：URL 空 no-op；模板解析+占位符替换（含 detail 带
  引号/换行的 traceback 样本）；模板非法降级；通用 JSON 形状。
- mock HTTP server（或 `httpmock`/手写 tokio TcpListener）断言：POST
  到达、body 含渲染值、200+code≠0 判失败。
- 挂点：`tests/state_machine.rs` 补「进 SidecarDead 触发一次告警」；
  `tests/agent_link.rs` 补「心跳 online 翻转触发掉线/恢复两条」。

## 任务 4：msg.forward 验证清单（文档交付）

**不动代码**（缓解已存在，盲改有风险）。交付
`docs/forward-context-verification.md` 真机验收清单：

- 链路 A「池直取 forward」：监听注册态下收到消息 → 下发 forward_message
  （带 msgId）→ 观察是否成功。重点裁决：`ChatWith(target)` 预切换是否
  等价于参考项目的「主窗口上下文」要求（docstring 自述与实际语义有偏差）。
- 链路 B「locate forward」：无 msgId 走 `_locate_msg` → forward，观察
  GetAllMessage 遍历期间监听回调扰动是否使控件引用失效。
- 每链路含：前置条件/操作步骤/预期结果/失败时日志特征。
- 记录修复钩子：若链路失败，`_msg_forward`（methods.py:431）弃用的
  `_chat_who` 是现成修复点（ChatWith 切源会话而非 target）；
  `_msg_quote`（line 391）无预切换是同族隐患，一并列入观察项。

## 工作区与集成策略

- 主 checkout 有另一主题遗留未提交改动（UIA 失败诊断化，362 行，
  methods.py/test_methods.py，94 测试绿）——**本批全部在独立 git worktree
  开发**，两边改动区域不重叠（遗留改动不加 `_raw_message`/`on_msg` 字段）。
- worktree 自 HEAD（9aa94e6）分支，完成后 `git merge --ff-only` 合回 main。
- 全程 TDD：每任务先红后绿；Python `pytest test_methods.py`、Rust
  `cargo test`、前端 `pnpm test`（vitest）。

## 非目标（明确不做）

- WS 断线超阈值告警、授权失效告警（用户拍板排除，后续可加）。
- 告警重试队列/持久化（YAGNI）。
- Degraded 状态机驱动补全（`mark_online` 死代码治理，超范围）。
- P0 项（监听名单持久化、消息持久化+断线补发）另批实施。
- SiverWXbot 的远程面板/AI 本地处理/防封拟人策略（架构上明确不抄）。
