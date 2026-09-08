# desktop 激活页设计（wxautox4 内核授权）

> 2026-09-08 grill-brainstorm 产物。范围：desktop/（独立仓 wx-win）新增激活视图，
> 打通 wxautox4 内核库授权的检测、激活、初始化闭环；附功能差距报告。
> 本轮**不含**差距报告 P1 两项（自动更新、托盘常驻）的实现——各自走独立循环。

## 背景与定位

desktop/（Tauri 2 + Vue 3 + Python sidecar 的 WxAuto 微信自动化桌面 App）依赖
wxautox4 商业库，未激活时核心功能不可用。现状缺陷：`licensed=false` 时状态机停留
SidecarBooting（`state.rs` 注释"面板显示授权引导"），**但引导 UI 从未实现**——
客户面对永远「启动中」的状态无从下手。

原型参考 `/home/working/SiverWXbot_plus-main` 有两套激活体系：
- 体系 A（wxautox4 内核授权，红卡+激活弹窗，`authenticate(code)` 本地闭源校验）
- 体系 B（SiverPanel 远程访问服务，远程服务器注册/绑定/续期）

**本设计只做体系 A 同款**：激活码交给 wxautox4 闭源库 `authenticate(code)` 校验，
状态由 wxautox4 自持久化（一机一码、跨重启有效）。desktop 不新建签发/绑定/续期
体系（与既有 device token 准入机制重叠，已在拷问中否决）。

关键事实（已核实）：

- wxautox4 官方激活方式为 `authenticate(code)` / `check_license()`
  （原型 `web_server.py:1308-1326` 的 `/activate` 路由同款实现）
- 激活状态由 wxautox4 自行持久化，无需 desktop 存储或重试体系
- 微信 PC 客户端未打开与未激活当前都导致停在 Booting，无法区分——本次三态化

## 已定决策一览（拷问结论）

| 决策点 | 结论 |
|---|---|
| 激活对象 | wxautox4 内核授权（非应用自身授权体系） |
| 未激活形态 | 专用激活视图 + 自动跳转 + 顶栏横幅；**无门禁**，其余视图可切换 |
| 获取码引导 | 联系运营（企业统一采购发码模式），不放官方购买链接 |
| 概览页 | 加授权状态卡（未激活红/已激活绿） |
| 激活成功闭环 | 自动重试 init（一次为限，不循环）；微信未开时手动「重新初始化」 |
| 失败区分 | init 失败三态：licensed / wechat_missing / 其他 |
| 机器码展示 | 不展示（channelId 已可定位设备） |
| 技术方案 | 方案 A：sidecar 直连（与 manual_execute 直连模式同构） |
| 状态权威 | wxautox4 `check_license()` 现查，desktop 不存激活状态 |

术语定义见主仓 `CONTEXT.md`「desktop 激活页」节。

## 架构

四层薄改，各司其职：

```
┌────────────────────────────────────────────────┐
│ Vue 前端                                        │
│  views/Activation.vue（新）                      │
│   状态卡（红/绿）+ 激活码表单 + 联系运营引导      │
│  App.vue 加菜单项 + 未激活横幅 + 自动跳转         │
│  Overview.vue 加授权状态卡                       │
│  stores/app.ts 加 licensed/initFailReason 状态   │
├────────────────────────────────────────────────┤
│ Rust 命令层（commands.rs / gui.rs）              │
│  activate_license(code) → 直连 wx.activate       │
│  成功后内联重发 wx.init（一次为限）                │
├────────────────────────────────────────────────┤
│ Rust 状态机（state.rs）                          │
│  init 失败原因三态化 → 事件新字段透出前端          │
├────────────────────────────────────────────────┤
│ Python sidecar（methods.py / spec.rs）           │
│  wx.activate：authenticate(code) + 立即回查       │
│  wx.init 失败时返回 failReason 而非笼统 false     │
└────────────────────────────────────────────────┘
```

核心闭环：用户输入激活码 → `invoke('activate_license')` → sidecar `wx.activate`
调 `authenticate(code)` → wxautox4 持久化 → Rust 重发 `wx.init` → `licensed=true`
→ 状态机 Booting→WxInit→Ready → 前端收 `wxauto://state` 事件离开激活页。

## 组件设计

### ① sidecar 层（sidecar-python/methods.py + src-tauri/src/sidecar/spec.rs）

**wx.activate 新方法**：

```python
def _activate(params, wx, msg_pool, msg_pool_ts, notify, mock):
    """wx.activate：authenticate(code) → 立即 check_license() 回查确认
    返回 {ok: bool, message: str}——message 带回 wxautox4 的失败原因"""
```

- 真实路径：`from wxautox4.utils.useful import authenticate` → `authenticate(code)`
  → 成功后 `check_license()` 回查（**回查仍 false 的极端情况不静默**：返回
  `{ok: false, message: "激活码已接受但授权状态未生效，请重启应用"}`）
- mock 路径：`ok = (code == "MOCK-ACTIVATION")`（可测确定性）
- spec.rs `methods` 常量表加 `wx.activate`；两侧契约测试同步

**wx.init 失败三态化**（_init 改造）：

| 情形 | 返回 |
|---|---|
| `check_license()` false | `{licensed: false, failReason: "licensed"}` |
| `WeChat()` 中英两版本兜底均抛异常 | `{licensed: <真实值>, failReason: "wechat_missing"}` |
| 成功 | `{licensed: true, wxid, nickname}`（无 failReason） |

spec.rs `InitResult` 加 `failReason: Option<String>`（serde default None，
向后兼容旧 sidecar 二进制）。

### ② Rust 状态机（src-tauri/src/state.rs）

- `init_sequence` 解析 `failReason`；licensed=false 时日志带原因
- UiEventBridge 发 `wxauto://state` 事件时**新增 `initFailReason` 字段**
  （state=sidecar_booting 且 failReason=licensed / wechat_missing）
- **不改六态结构**——wechat_missing 仍停留 Booting（微信没开不该进 WxInit），
  但前端可区分文案

### ③ Rust 命令层（src-tauri/src/commands.rs / gui.rs）

```rust
#[tauri::command]
async fn activate_license(code: String) -> Result<ActivationResult, String>
```

- 直连 `wx.activate`（复用 direct_call 模式，**30s 超时**——authenticate 可能走网络）
- 成功（ok=true）→ **内联重发 wx.init 一次**（复用 direct_wx_init 短超时模式；
  不借道 Supervisor 崩溃循环——init 失败时 sidecar 进程还活着）
- init 重试失败即止，不自动循环（微信没开场景每次白耗 UIA 扫描）

```rust
#[tauri::command]
async fn retry_init() -> Result<(), String>
```

- 独立的重新初始化命令：仅重发 wx.init 一次（供激活页「重新初始化」按钮用——
  激活成功但微信未开的场景，用户打开微信后手动触发；sidecar 活着时 Supervisor
  不会重跑 init 序列，必须有显式入口）
- **不新增 get_license_status 命令**——前端从既有 state 事件 + get_app_state
  快照推导 licensed，避免两套真相
- gui.rs `generate_handler!` 注册两个新命令

### ④ 前端（src/）

**views/Activation.vue（新，约 150 行）**：

- 上半：授权状态卡——未激活红色「微信自动化核心功能未授权」/已激活绿色「授权正常」；
  激活成功但微信未开时显示「激活成功 ✓ 请打开微信 PC 客户端」+「重新初始化」按钮
- 中部：激活码输入框 + 「立即激活」按钮（loading 态防重复提交；失败红字提示）
- 底部：灰底引导卡「获取激活码请联系运营」+ 运营联系方式——文案为前端常量
  `ACTIVATION_CONTACT`（如「联系运营 QQ/微信：xxx」，具体号码实现时向运营取，
  取不到先留「请联系您的服务管理员」通用文案，**不做配置项**——YAGNI，改文案
  随发版即可）
- 监听 state 事件：状态离开 Booting（进 WxInit）→ 显示成功态，2s 后跳回概览

**App.vue**：

- 菜单加「激活」项（TDesign `secured` 图标类）
- `initFailReason === 'licensed'` → 顶栏红色横幅「未激活，点击去激活」
- 启动检测到未激活自动 `view.value = 'activation'`（**一次性**，用户可自由离开）

**Overview.vue**：状态卡列表加授权卡（未激活红 + 「去激活」按钮跳激活页 / 已激活绿）

**stores/app.ts**：

| 收到的事件/快照 | licensed 推导 |
|---|---|
| state 事件含 `initFailReason: 'licensed'` | false（进激活引导） |
| state 事件含 `initFailReason: 'wechat_missing'` | 激活已过但微信未开 → 横幅「请打开微信客户端」，不跳激活页 |
| state 进入 `wx_init` 及之后 | true |

## 错误处理

| 场景 | 行为 | 用户看到 |
|---|---|---|
| 激活码无效/过期 | authenticate 返回 false → `{ok: false, message}` | 表单下方红字「激活失败：激活码无效或已过期」 |
| 激活时 sidecar 死了 | 直连报 RpcError → 命令返回 Err | 红字「服务未就绪，请稍后重试」+ 状态 tag 显示 SidecarDead |
| 激活成功但微信未开 | init 重试 failReason=wechat_missing | 激活页「激活成功 ✓ 请打开微信后点击重新初始化」 |
| wxautox4 库不在（Linux dev） | sidecar 延迟导入抛 ImportError → RPC 错误透传 | 开发态可预期；mock 路径覆盖 |
| 激活请求超时（网络抖动） | 30s 超时 → Err | 红字超时提示可重试；幂等安全（authenticate 重复调用无害） |
| 激活成功回查仍 false | 不静默吞 | 「激活码已接受但授权状态未生效，请重启应用」 |

## 测试策略

- **sidecar（pytest）**：wx.activate mock 三态（MOCK-ACTIVATION 成功/错码/空码）；
  wx.init failReason 三态注入
- **Rust（cargo test）**：spec.rs 契约测试加 failReason 解析（含向后兼容缺字段）；
  commands 层 activate_license 直连+错误透传（gui_bridge 测试基建复用）
- **前端（vitest）**：Activation.vue 快乐路径+失败提示；store licensed 推导
  规则表驱动；App.vue 未激活自动跳转
- **集成冒烟**：`WXAUTO_MOCK=1` GUI 走通「未激活跳转 → 输入 mock 码 → 进 WxInit」
- **真机验收**（Windows + 真实 wxautox4 + 真实激活码）随发版流程，不阻塞合并

## 功能差距报告（题目第二问产出，本轮不实现）

| 优先级 | 缺失功能 | 现状 | 建议 |
|---|---|---|---|
| **P1 必要** | 自动更新 | 零 updater 配置，发版=逐台手工重装 NSIS | tauri-plugin-updater + GitHub Release 签名 feed，随 CI 闭环 |
| **P1 必要** | 托盘常驻+开机自启 | 关窗即退出进程 | tauri-plugin-autostart + tray，断电重启自动恢复连接 |
| P2 | 崩溃/错误上报 | 仅本地日志 | 一键导出诊断包（zip 日志+配置脱敏） |
| P3 | 远程访问（SiverPanel 同类） | 无 | 有公网访问诉求再立项，与 device token 协同设计 |
| 存档 | 已知问题 12 条（WINDOWS-安装打包流程.txt §9） | Busy/Degraded 无驱动者等 | 属 bug 修复，随日常迭代 |

P1 两项各自走独立拷问→spec→计划循环。

## 全局约束（承继既有 plan 约束）

- 中文注释；Rust 禁 unwrap；TS 禁 any（用 unknown + 类型守卫）
- CLI `--cli` 模式零回归
- 每任务 cargo test / pytest / vitest 绿 + commit（TDD）
- 多租户铁律不涉及（desktop 独立仓，无租户构建问题）
