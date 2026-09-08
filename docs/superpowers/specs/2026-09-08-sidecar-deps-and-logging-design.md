# desktop sidecar 传递依赖补齐 + 三通道日志系统 设计文档

> 2026-09-08 拷问定稿。背景：真机验收发现三症状——日志缺失、无法激活、sidecar 依赖缺失——
> 收敛为一条因果链：冻结包缺 wxautox4 传递依赖 requests → activate 抛 ModuleNotFoundError(-32603)
> → 全链路无日志留痕。本 spec 修复 P0 激活事故并建立可诊断性基建。
> ADR：主仓 `docs/adr/0010-desktop-pyinstaller-transitive-deps.md`、`0011-desktop-rpc-error-frame-init-fail.md`。

## 0. 真机事故复盘（已验证的因果链）

```
CI 冻结时 pip install wxautox4 连带装上传递依赖（构建机上有）
  → wxautox4 是 cp312-win_amd64 编译型 wheel，requests 的 import 藏在 .pyd 二进制里
  → PyInstaller 静态分析看不见 → 冻结包缺 requests（及另外 8 个兄弟）
  → 真机 wx.init：check_license() 内部吞异常返回 licensed:false → Booting（日志 07:05:14 warn）
  → 输激活码 → authenticate(code) 连授权服务器 → import requests → ModuleNotFoundError
  → sidecar.py:136 兜底 except 转 -32603 错误帧 → 前端 toast「激活请求失败」
  → 激活永远失败 → init 永远 Booting → 无法连接
```

三个观察盲区（全部代码验证）：

1. sidecar.py 兜底 except 只回错误帧不打日志——stderr 一行没有；
2. Rust `activate_license` 失败路径不 `tracing::error!`（commands.rs:144 只 format 进 Err）——错误不进 ring；
3. 日志 tab 纯内存 ring（1000 条）退出即丢，全仓无落盘；`WINDOWS-安装打包流程.txt` §10 承诺的日志目录实际只有 config.json。

打包链本身已验证是通的：真机日志「使用内置 sidecar（安装态）」= 探测链第 2 级命中，PyInstaller + externalBin 架构保留不换（换嵌入式 Python 发行版的方案已否决，见 ADR-0010）。

## 1. P0-1 冻结包传递依赖补齐

**改动**：`sidecar-python/wxauto-sidecar.spec` 的 `hiddenimports` 从 5 项扩到 14 项。

现有 5 项（保留）：`wxautox4`、`wxautox4.utils.useful`、`pythoncom`、`win32com`、`comtypes`。

新增（PyPI `wxautox4 41.1.1.post1` 的 `requires_dist` 实测，comtypes 与现有去重后实加 8 个）：
`colorama`、`pillow`、`psutil`、`pyperclip`、`pywin32`、`requests`、`sounddevice`、`tenacity`。

**注释要求**：spec 内注明「wxautox4 升级后须对照 PyPI requires_dist 重新核对此清单」。

**验收**：冻结 exe 体积 24.7MB → 预计 ~35MB（sounddevice 携带 portaudio 原生库）；CI 冻结步骤打印产物体积进日志。

## 2. P0-2 CI 真路径冒烟

**改动**：`scripts/build_sidecar_windows.ps1` 冒烟段 1 条 → 3 条：

| # | 输入 | 断言 |
|---|---|---|
| 1 | `wx.get_my_info`（MOCK，现状保留） | mock 数据回来 |
| 2 | `wx.init`（非 MOCK） | 收到**结果帧**（CI 无授权 → `licensed:false`）而非 `-32603` 错误帧 |
| 3 | `wx.activate` 假码（非 MOCK） | 收到错误帧但错误串**不含** `ModuleNotFoundError` |

断言原理：业务错（授权服务器拒绝/无授权）≠ 打包错（模块缺失）。第 2/3 条跑真 wxautox4 导入链，是 MOCK 冒烟穿透不了的事故路径的直接复现。onefile 自解压 + 真导入每条预计 +5~15s CI 时长，可接受。

## 3. P0-3 init RPC 错误帧归入 init_fail

**改动**：`src-tauri/src/state.rs` `init_sequence` 的 `direct_wx_init().await.ok()` 改为 match：

- `Ok(v)` → 现有 licensed/failReason 逻辑不变；
- `Err(RpcError::Sidecar(msg))` → `last_init_fail = Some("初始化失败：{msg}")` + emit `init_fail` 帧 + warn 日志（sidecar 活着，wx 层可诊断）；
- `Err(Timeout | Io | Closed)` → 不写快照不发帧（transport 级 = sidecar 死亡，归 Supervisor 重启域，维持原设计，不谎报授权失败）。

**前端配套**：

- `src/stores/app.ts`：`KNOWN_INIT_FAIL_REASONS` 白名单外增加透传分支——快照/事件值非 `licensed`/`wechat_missing` 时原样存 `initFailReason`（类型从两字面量放宽为 `string`）；
- `src/views/Activation.vue` 状态卡新增第 4 态「初始化错误」：`initFailReason` 非空且非已知两值 → 橙灯 + 原始错误串 + 引导文案「程序文件可能不完整，请重新安装或联系管理员」。

## 4. P1-1 Python 侧日志

**新增** `sidecar-python/sidecar_log.py`（~40 行，零依赖，参考 SiverWXbot logger.py 精简）：

- `log(level, msg)` → `[YYYY-MM-DD HH:MM:SS] [SIDECAR] [LEVEL] msg` 写 stderr + 线程锁；
- **stdout 是 JSON-RPC 帧专用通道，日志只走 stderr**（铁律，混入即协议损坏）；
- 写失败静默吞（日志永不阻断业务）。

**埋点位**（每处一行）：

| 位置 | 内容 |
|---|---|
| sidecar.py 主循环兜底 except | `-32603` 转帧前先 `log("ERROR", "dispatch {method} 失败: {type}: {e}")` |
| methods.py `_init` | 进入/结果（licensed/failReason/异常） |
| methods.py `_activate` | 进入/结果（ok/失败原因） |
| methods.py 监听回调 except | 现有 3 处 print 改 log |

**进 ring 路径**：stderr 管道 → 现有 `RingStderrSink`（ERROR/Traceback 关键字升 error）→ 内存 ring + 前端日志 tab + 落盘（§5）同源。

## 5. P1-2 落盘（Rust 侧统一收口）

**新增** `src-tauri/src/file_log.rs`——ring 写入侧挂文件 sink，与 UI mpsc 并联：

```
UiLogLayer / RingStderrSink
  ├─→ LogRing（内存 1000，现状不动）
  ├─→ mpsc → 前端（现状不动）
  └─→ FileSink（新）→ ~/.wxauto-desktop/logs/app-YYYYMMDD.log（utf-8-sig，追加）
```

- 目录：`~/.wxauto-desktop/logs/`（与 config.json 同根，兑现 §10 文档口径）；
- 保留：启动时清理 mtime > 7 天的 `app-*.log`；
- CLI 模式同样落盘；
- 失败策略：目录创建/写失败 → tracing warn 后降级纯内存，日志系统永不阻断启动；
- 格式：`[YYYY-MM-DD HH:MM:SS] [rust|sidecar] [LEVEL] msg`，与日志 tab 同构；
- Rust `activate_license` 等 invoke 失败路径补 `tracing::error!`——错误同时进 toast + 日志 tab + 落盘。

## 6. P1-3 bootstrap 自检报告 + 测试与文档

**bootstrap.log**：每次启动（覆盖写）`~/.wxauto-desktop/logs/bootstrap.log`：

```
2026-09-09 07:05:12 [probe] env WXAUTO_SIDECAR_CMD: 未设置
2026-09-09 07:05:12 [probe] bundled: 命中 wxauto-sidecar.exe (安装态)
2026-09-09 07:05:12 [probe] 最终命令: <绝对路径> | PID 1234 | CREATE_NO_WINDOW=1
2026-09-09 07:05:14 [probe] sidecar 首响应: 997ms
```

探测落空走 python3 回退时写「⚠ 回退 python3——安装可能不完整，bundled 候选 [A, B] 均不存在」。

**测试**（三线，见仓规）：

| 线 | 新增 |
|---|---|
| cargo | init RPC 错误帧→快照+帧；FileSink 降级；bootstrap 探测行格式 |
| pytest | sidecar_log 格式/线程安全；dispatch 兜底写日志；`_init`/`_activate` 埋点 |
| vitest | store 未知 reason 透传；Activation 第 4 态文案/灯色 |

**文档**：`WINDOWS-安装打包流程.txt` §10 日志目录口径兑现；§9 follow-up 加「07:05:12 重复装配行」留档。

## 7. 不做（YAGNI 留档）

日志导出按钮；UI 日志暂停开关重设计；mpsc 洪峰扩容；自动更新；代码签名；07:05:12「GUI 装配完成」重复行修复（快照重叠既有 follow-up，另行处置）。
