# 监听名单持久化 + 设备事件发件箱（outbox）+ 服务端 ACK/幂等 设计定稿

> 日期：2026-09-10 · 状态：已定稿（待实现）
> 范围：desktop 仓（Tauri/Rust）+ lyagent 仓 Server（wxauto-ws 网关）
> 背景：与 SiverWXbot_plus 横向评比后裁定的 P0 两项数据不丢缺口。

## 1. 问题陈述

1. **监听名单不持久化**：`Config.listen_names`（desktop `src-tauri/src/config.rs:33`）是死字段；`ListenerRegistry` add/remove 只改内存 BTreeSet（`wx/listener.rs:34,44`），应用重启监听名单全丢，需逐个手加。
2. **上行事件可丢**：
   - WS 断线窗口的事件帧写入 unbounded 通道后，重连时被 `WsTransport` **排空丢弃**（`transport/ws.rs:62-70`）；
   - sidecar 崩溃窗口的消息事件随 broadcast 通道消亡；
   - 服务端对 event 帧**无 ACK、无幂等**（`eventSchema` 无 id 字段，`wxauto-inbound.service.ts:78` msgId 纯随机）——设备补发即重复入库 + agent 重复回复。

## 2. 目标 / 非目标

**目标**
- G1 监听名单随 add/remove 落盘 `config.json`，启动时种子恢复并经 resync 重放。
- G2 设备侧上行事件（message / friend_request）先落盘后发送（durability before transmission），收到服务端 ACK 才清除；断线/崩溃后重连补发。
- G3 服务端对带 `eventId` 的 event 帧回 ACK，并按 `(channelId, eventId)` LRU 幂等去重；`eventId` 透传为 `InboundMessage.id` → `platformMessageId`。
- G4 兼容旧设备（无 eventId = 现行为不变）与新设备连旧服务端（ degrade 见 §7 发布顺序）。

**非目标（明确不做）**
- 不做 DB 唯一索引/migration：服务端幂等仅进程内 LRU（容量 5000）。残余风险=服务端重启且设备恰有积压补发时可能重复消费，接受（触发条件罕见，Phase 2 视生产表现再加索引）。
- status/ping 等瞬态事件不入 outbox（可由心跳再推导）。
- 不改 hello `protocolVersion`（新增字段全部 optional，加法式演进）。
- 不做 webhook/邮件外发告警、is_at 透传、msg 级跨重启去重（P1/P2 另立计划）。

## 3. 协议变更（WS 帧）

### 3.1 上行 event 帧加 `eventId`（optional string）

```json
{ "kind": "event", "type": "message", "eventId": "evt-1756000000000-0",
  "data": { ... }, "ts": 1756000000000 }
```

- 设备在组帧时生成（`inbound.rs` 三个 event 组帧函数统一附加）：`evt-{unix_ms}-{进程内 AtomicU64 递增}`。
- 仅 `type: message / friend_request` 进 outbox；status 也带 eventId（服务端统一 ack，ack 非入箱帧为 no-op）。
- zod `z.string().min(1).optional()`——旧设备不带字段，行为不变。

### 3.2 新增下行 ack 帧

```json
{ "kind": "ack", "eventId": "evt-...", "ts": 1756000000123 }
```

- 服务端在 `handleEvent` **正常 resolve 后**回送（含 `routed / no_route / ignored / duplicate` 四种结果）。
- **ACK 语义 = 已受理（at-least-once）**，不是"已持久化"。`handleEvent` 抛异常（被 onMessage catch）不 ack → 设备择机补发。选后置 ack 而非前置：前置 ack 在 agent 瞬时失败时会丢消息（丢 > 重）。
- `duplicate` 也必须 ack——设备发件箱据此清除。

## 4. 设备侧设计（desktop）

### 4.1 监听持久化

- `ListenerRegistry::new_seeded(session, initial)`：构造时以 `config.listen_names` 为种子；启动后由既有 `resync()`（Supervisor init / WS on_connect）重放穿透 sidecar。
- `set_persist_hook(hook)`：add/remove 穿透 sidecar 成功后，以最新名单快照异步调用 hook；hook 失败只记日志不回滚（名单以内存为准，下次写盘自愈）。
- `file_persist_hook(config_lock, path)` 工厂：更新共享 `Arc<RwLock<Config>>` 的 `listen_names` 并 `save_config`（复用既有 tmp+rename 原子写）。
- GUI（`app_state.rs`）：`Assembled.config` 类型改为 `Arc<RwLock<Config>>` 与 hook 共享同一把锁——**关键不变量：hook 与 save_settings 经同一把内存锁串行化，消除文件级读改写竞争**（save_settings 清空 listen_names 的旧隐患一并根治）。
- CLI（`main.rs`）：同工厂，锁内初始值即磁盘值。

### 4.2 Outbox（`src-tauri/src/outbox.rs`，core 不引 tauri）

- 文件：`~/.wxauto-desktop/outbox.jsonl`，每行 `{"eventId","ts","frame"}`。
- API：`open(path)` / `disabled()` / `enqueue(&frame)` / `ack(event_id)` / `pending() -> Vec<Value>` / `len()`。
- `enqueue`：仅 `type ∈ {message, friend_request}` 且带 eventId 的帧追加落盘（append 单行）；磁盘失败记 error、内存保留（当前会话 flush 仍可用，后续 rewrite 自愈）。
- `ack`：按 eventId 移除并整文件原子重写（tmp+rename）。
- 容量与过期：上限 **2000 条**（超限丢最旧并 warn）、**7 天**过期，open 时与 enqueue 超限时执行修剪。
- 崩溃安全：先落盘后发送；重写走原子改名。

### 4.3 AgentLink 接线

- `emit_event`：event_sink（GUI 桥）→ `outbox.enqueue` → `send_frame`。
- `LinkHandler::on_frame`：新增 `kind == "ack"` 分支 → `outbox.ack(eventId)`。
- `on_connect`：hello → 门闩缓冲 flush → **outbox 补发（pending 按序）** → resync → 初始 status。补发放 resync 前（resync 每监听对象 0.5~1s 拟人间隙，别让补发排队其后）。
- `set_outbox(Arc<Outbox>)` 运行期注入（对齐 event_sink 注入模式）；默认 `disabled()`——测试默认无 IO，装配层（GUI `Assembled` / CLI main）显式注入真路径。
- 门闩缓冲 flush 与 outbox 补发可能对同一事件双发（连接握手窗口内产生的事件同时进两路）——两路携带相同 eventId，服务端 LRU 判重，可接受（spec 明示）。

## 5. 服务端设计（Server wxauto-ws）

- `wxauto-frame.ts`：`eventSchema` 加 `eventId: z.string().min(1).optional()`；新增 `ackSchema` 进 `frameSchema` union（**不进** `parseFrame.known` 上行白名单——设备上行 ack 属协议错误被拒）。
- `wxauto-device.gateway.ts` `case 'event'`：`await handleEvent(...)` 后，`frame.eventId` 存在则 `safeSend(ack)`。
- `wxauto-inbound.service.ts`：
  - 返回类型加 `'duplicate'`；
  - message 事件且带 eventId → `admitEvent(channelId, eventId)` LRU（`Map` 插入序 FIFO 淘汰，容量 `WXAUTO_DEDUP_CAPACITY = 5000` 导出常量）判重，重复直接返回 `duplicate`（**不进 pipeline、不触发 agent**）；
  - `toInboundMessage` 第三参 `eventId?`：有则 `id = "wxauto-" + eventId`（落 `platformMessageId`），无则维持旧随机（旧设备）。
- friend_request/status：维持现状（记日志 ignored），gateway 层照常 ack（friend_request 是入箱帧，无 ack 则设备永不清除）。

## 6. 故障矩阵

| 场景 | 行为 |
|---|---|
| WS 断线期间来消息 | 入 outbox + 发进死通道（重连被排空）→ 重连补发 → ack 清除 |
| sidecar 崩溃窗口消息 | 从未进入 Rust，天然丢（wxautox4 回调随进程死，不在本设计范围） |
| 发送后 ack 在途时断线 | outbox 未清除 → 重连补发 → 服务端 LRU 判重 → duplicate + ack 清除 |
| 设备崩溃重启 | outbox 文件留存 → 首次 connect 后补发 |
| 服务端重启（LRU 失忆） | 补发被当新消息重复消费（**已接受的残余风险**，见 §2 非目标） |
| 旧 server + 新 desktop | server strip 掉 eventId、永不 ack → 设备每次重连补发直至 7 天过期/2000 条淘汰（降级，见 §7） |
| 新 server + 旧 desktop | 无 eventId → 不 ack 不判重，现行为完全不变 |
| outbox 磁盘失败 | enqueue 记 error，消息照发（发件箱故障不阻断业务） |
| persist hook 写盘失败 | 记 warn，add/remove 仍成功，名单以内存为准下次自愈 |

## 7. 发布顺序（硬约束）

**Server 先行**（ack + 幂等上线、strip eventId 兼容）→ **desktop 后发**。反向顺序会让设备在旧 server 上无限补发。desktop 发版无需 migration，server 零 DB 变更——两者均不触发六租户 migration 纪律。

## 8. 测试策略

- desktop：outbox 单测（tempfile：入箱/清除/重开恢复/容量/坏行/禁用）；listener 持久化集成（剧本 sidecar + 计数 hook + 文件断言）；outbox 补发集成（单 listener 双连接 mock server：首连不 ack 收帧即断 → 重连补发同 eventId → ack 清空）；GUI `persist_settings` 防清空回归。
- Server：frame schema（eventId 兼容/空串拒绝）；gateway 真实 ws 集成（event 带 id 收 ack、不带无 ack、duplicate 也 ack）；inbound（同 id 二次 duplicate 且 pipeline 单次、无 id 不判重、id 透传、LRU 淘汰）。
- 冒烟：`scripts/mock_ws_server.py` 补 ack 回送；`smoke_cli.sh` 断言运行后 outbox 文件为空/不存在。
