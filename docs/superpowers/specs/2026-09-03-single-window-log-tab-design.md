# desktop 单窗口化 + 运行日志 tab 设计

> 日期：2026-09-03 · 状态：已批准（brainstorming 产出）
> 范围：仅 desktop/ 仓（wx-win 双仓同步不在本次范围）

## 1. 目标

1. **消除 cmd 样式日志窗口**：Windows 上运行（尤其安装态）会出现只滚动日志的控制台窗口，去掉它，App 只剩 Tauri 主窗口。
2. **新增「运行日志」tab**：在主窗口内实时查看 Rust 应用日志 + sidecar Python 进程 stderr，带级别/来源过滤。

现有「指令日志」tab（RPC 指令流水表格）**保持不动**。

## 2. cmd 窗口根因（三处）

| # | 根因 | 修复 |
|---|------|------|
| 1 | `main.rs` 缺 `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`，release 运行自动附带常驻控制台 | 加该属性（debug 保留控制台便于 dev 排障） |
| 2 | sidecar stderr `Stdio::inherit()` 直通父进程控制台 | 改 `piped()` + 逐行读进日志缓冲（同时是日志 tab 数据源） |
| 3 | `CREATE_NO_WINDOW` 仅覆盖「内置 sidecar」一条 spawn 路径 | 补齐 env 覆盖 / python3 回退两条路径（双保险） |

PyInstaller spec 的 `console=True` **不改**：stdout 是 stdio JSON-RPC 管道必须保留；stderr 被 Rust piped 接管后子进程不再产生可见窗口。

## 3. 架构

```
┌─ Rust 侧 ──────────────────────────────────────────────┐
│ main.rs        + windows_subsystem 属性                   │
│ gui.rs         tracing 初始化改 registry+fmt+UiLogLayer    │
│ ui_log.rs(新)  UiLogLayer(tracing Layer) + LogRing(环形)   │
│                + sidecar stderr 读循环                      │
│ ui_events.rs   + EVENT_APP_LOG 常量 + forward_app_log      │
│ sidecar/mod.rs stderr inherit→piped；CREATE_NO_WINDOW 三路径│
└────────────────────────────────────────────────────────┘
┌─ 前端 ─────────────────────────────────────────────────┐
│ stores/app.ts   + appLog state + pushAppLog + 事件订阅     │
│ views/AppLog.vue(新) 级别/来源过滤 + 自动滚动日志流         │
│ App.vue         菜单加「运行日志」项（icon: file-paste）    │
└────────────────────────────────────────────────────────┘
```

数据流：tracing 事件 → `UiLogLayer::on_event` → `LogRing`（VecDeque，上限 1000）→ mpsc → 转发任务经 `UiEventBridge` emit；sidecar stderr → 逐行读任务 → 同一 `LogRing`。

## 4. 事件契约

`wxauto://app-log` payload（与现有四个事件同风格）：

```json
{ "ts": 1693700000000, "level": "info", "source": "rust", "message": "..." }
```

- `level`: `'error' | 'warn' | 'info' | 'debug' | 'trace'`
- `source`: `'rust' | 'sidecar'`
- sidecar 行无级别概念，统一 info；含 `ERROR` / `Traceback` 关键字升 error（contains 判定）

## 5. 模块细节

### ui_log.rs（新，bin 侧）

- `AppLogEntry { ts, level, source, message }`（serde Serialize，字段即前端契约）
- `LogRing`：`Mutex<VecDeque<AppLogEntry>>` 上限 1000，满弹最旧
- `UiLogLayer` 实现 `tracing_subscriber::Layer<Registry>`：`on_event` 取 metadata level + `field::Empty` visitor 收 message 字段，push ring + `try_send` mpsc（容量 256；满则丢行计数——旁路观察者不反压业务，与 UiEventBridge「emit 失败仅 tracing」同哲学）
- 格式化简化：不带 target/thread，message 本体即可（前端按 level 染色定位）

### sidecar stderr 接管

- `spawn_default` / `spawn_with_python` stderr → `piped()`
- `do_spawn` 取 `child.stderr` spawn 逐行读（`BufReader::lines`），每行封装 entry 写入注入的 sink
- 注入方式：`SidecarHandle` 增 `with_log_sink(...)` 构建器；**未注入时 stderr 退回 `inherit()`**（CLI 行为完全不变，CI 冒烟零回归）
- 多代 sidecar（崩溃重启）：各代读循环写同一 ring，旧代 EOF 退出

### gui.rs tracing 初始化

```rust
tracing_subscriber::registry()
    .with(fmt::layer().with_ansi(stderr_is_tty).with_writer(std::io::stderr))
    .with(UiLogLayer::new(ring.clone(), tx))
    .with(env_filter)
    .init();
```

启动早期（bridge attach 前）日志也进 ring；前端 init 经新增 `get_recent_logs` invoke 拉快照补齐（对齐 `get_app_state` 补首值的既有模式——tauri 事件无重放）。

### 前端 AppLog.vue

- 工具条：级别下拉（默认 info 及以上）、来源多选（rust/sidecar 默认全选）、暂停滚动开关、清空按钮
- 主体：div 循环渲染（1000 上限直接渲染，不引虚拟滚动）；等宽字体
- 自动滚动：用户上滚暂停贴底、回底部恢复（console 经典行为）
- 染色：error 红 / warn 橙 / info 默认 / debug 灰

## 6. 边界情况

1. **CLI 零回归**：`--cli` 不注入 sink，stderr 仍 inherit，`smoke_cli.sh` 必须仍绿
2. **sidecar 死亡**：stderr 读循环 EOF 正常退出，reaper 既有回收逻辑不变
3. **日志洪水**：ring 1000 + mpsc 丢弃计数，内存有界
4. **windows_subsystem 仅 release**：debug 构建保留控制台

## 7. 测试策略

**Rust 单测**（`cargo test`）：
- `LogRing` 上限淘汰（push 1001 弹最旧）
- `UiLogLayer` 经 `tracing::subscriber::with_default` 收一条 info → entry 级别/来源正确
- stderr 读循环：spawn print 到 stderr 的 python 子进程 → 行进 ring、Traceback 升 error
- `get_recent_logs` mock_runtime invoke 集成（对齐 gui.rs C1 测试模式）

**前端 vitest**：
- store `pushAppLog` 环形 + `parseAppLogItem` 守卫（非法 payload 丢弃）
- AppLog.vue 过滤（级别/来源组合）

**冒烟**：Linux `smoke_cli.sh` 绿；`WXAUTO_MOCK=1` dev 起一遍日志 tab 渲染。

## 8. 明确不做（YAGNI）

- 文件日志落盘（用户已选纯内存方案）
- WS 网关收发帧记录
- 虚拟滚动 / 日志导出 / 关键字搜索（后续按需加）
- wx-win 仓同步（另行处理）
