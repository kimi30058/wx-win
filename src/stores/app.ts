/**
 * 全局 pinia store：连接状态 / 消息流水 / 指令日志 / 监听名单 / 配置。
 *
 * 数据流（spec §3.4）：数据来自 Task 9 的 tauri event，操作走 invoke 进同一条
 * 编排层（与 agent 指令共用串行队列）。本 store 不含业务逻辑分支，只做：
 * 订阅 → 落 state；action → invoke → 刷新 state。
 *
 * 事件契约（Task 9 Rust 侧 emit）：
 * - `wxauto://state`    payload string（六态名，见 AppStateName）
 * - `wxauto://status`   payload { wsConnected: boolean; wxOnline: boolean }
 * - `wxauto://message`  payload MessageItem
 * - `wxauto://command-log` payload CommandLogItem
 * - `wxauto://app-log`  payload AppLogItem（运行日志，1000 环形）
 * - `wxauto://init-fail` payload { reason: 'licensed'|'wechat_missing' }
 *
 * invoke 契约：get_config / save_config / get_listen_names / add_listen /
 * remove_listen / manual_execute / connect / disconnect / get_app_state /
 * get_init_fail_reason / get_recent_logs / clear_logs / activate_license /
 * retry_init。
 * 注意 manual_execute 在 Rust 侧是 `Result<Value, String>`——业务载荷 resolve、
 * 失败字符串 reject（无 {success} 包装帧），故本 store 的 manualExecute 返回
 * 判别联合 ManualOutcome，视图按 ok 分支处理。
 */
import { defineStore } from 'pinia';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { invoke } from '@tauri-apps/api/core';

/** 六态（与 Rust AppState 枚举一一对应，state.rs） */
export type AppStateName =
  | 'SidecarDead'
  | 'SidecarBooting'
  | 'WxInit'
  | 'Ready'
  | 'Busy'
  | 'Degraded';

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

/** 六态中文标签（概览指示灯 / 状态条展示） */
export const APP_STATE_LABELS: Record<AppStateName, string> = {
  SidecarDead: 'Sidecar 已崩溃（退避耗尽）',
  SidecarBooting: 'Sidecar 启动中',
  WxInit: '微信初始化中',
  Ready: '正常服务',
  Busy: '指令执行中',
  Degraded: '已降级（微信掉线探测中）',
};

/** 消息流水项（wxauto://message 载荷 + 面板本地自增 id 作表格 row-key） */
export interface MessageItem {
  /** 面板本地自增 id（ts 同毫秒可重复，不能作唯一 key） */
  id: number;
  chatName: string;
  chatType: string;
  sender: string;
  msgType: string;
  content: string;
  ts: number;
}

/** 指令日志项（wxauto://command-log 载荷） */
export interface CommandLogItem {
  requestId: string;
  action: string;
  success: boolean;
  error?: string;
  durationMs: number;
  ts: number;
}

/** 运行日志级别（Rust AppLogLevel serde 小写变体名一一对应） */
export type AppLogLevelName = 'error' | 'warn' | 'info' | 'debug' | 'trace';

/** 运行日志来源：rust=应用自身 / sidecar=Python 子进程 stderr */
export type AppLogSourceName = 'rust' | 'sidecar';

/** 运行日志条（wxauto://app-log 载荷） */
export interface AppLogItem {
  ts: number;
  level: AppLogLevelName;
  source: AppLogSourceName;
  message: string;
}

/** 运行日志环形上限（Rust LogRing 同容量——spec §5） */
export const APP_LOG_RING_LIMIT = 1000;

const APP_LOG_LEVELS: AppLogLevelName[] = ['error', 'warn', 'info', 'debug', 'trace'];
const APP_LOG_SOURCES: AppLogSourceName[] = ['rust', 'sidecar'];

/** 运行日志载荷守卫：级别/来源非法回退 info/rust（单条脏数据不炸日志流） */
function parseAppLogItem(v: unknown): AppLogItem {
  const r = isRecord(v) ? v : {};
  const level = str(r.level) as AppLogLevelName;
  const source = str(r.source) as AppLogSourceName;
  return {
    ts: num(r.ts) || Date.now(),
    level: APP_LOG_LEVELS.includes(level) ? level : 'info',
    source: APP_LOG_SOURCES.includes(source) ? source : 'rust',
    message: str(r.message),
  };
}

/** 设备配置（get_config 返回的业务字段子集；token 只写不读——keyring 侧不回传） */
export interface AppConfig {
  serverUrl: string;
  channelId: string;
  autoConnect: boolean;
}

/** saveConfig 入参：token 非空时随配置提交（Rust 侧写 keyring） */
export type SaveConfigInput = AppConfig & { token?: string };

/** manualExecute 结果：ok=true 时 data 为 sidecar 业务载荷（结构由 action 决定） */
export type ManualOutcome = { ok: true; data: unknown } | { ok: false; error: string };

/** 环形缓冲上限（实现收敛为 500——spec §3.3 原文 1000，实现取 500 已定稿） */
export const RING_LIMIT = 500;

/* ---------------- 未知载荷的类型守卫（禁 any 铁律：unknown + 收窄） ---------------- */

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null;
}

function str(v: unknown): string {
  return typeof v === 'string' ? v : '';
}

function num(v: unknown): number {
  return typeof v === 'number' && Number.isFinite(v) ? v : 0;
}

/** 状态帧载荷守卫：仅接受六态名，其余丢弃（防 Rust 侧枚举演进时前端脏渲染） */
function isAppStateName(v: unknown): v is AppStateName {
  return (
    typeof v === 'string' &&
    Object.prototype.hasOwnProperty.call(APP_STATE_LABELS, v)
  );
}

/** 状态帧载荷守卫 */
function isStatusPayload(v: unknown): v is { wsConnected: boolean; wxOnline: boolean } {
  return (
    isRecord(v) &&
    typeof v.wsConnected === 'boolean' &&
    typeof v.wxOnline === 'boolean'
  );
}

/** 消息帧载荷守卫：字段宽容归一（缺字段按空串/0 兜底，不让单条脏数据炸表格） */
let messageSeq = 0;
function parseMessageItem(v: unknown): MessageItem {
  const r = isRecord(v) ? v : {};
  messageSeq += 1;
  return {
    id: messageSeq,
    chatName: str(r.chatName),
    chatType: str(r.chatType),
    sender: str(r.sender),
    msgType: str(r.msgType),
    content: str(r.content),
    ts: num(r.ts) || Date.now(),
  };
}

/** 指令日志帧载荷守卫 */
function parseCommandLogItem(v: unknown): CommandLogItem {
  const r = isRecord(v) ? v : {};
  return {
    requestId: str(r.requestId),
    action: str(r.action),
    success: r.success === true,
    error: str(r.error) || undefined,
    durationMs: num(r.durationMs),
    ts: num(r.ts) || Date.now(),
  };
}

/** get_config 载荷守卫：只取面板用得到的三个字段 */
function parseAppConfig(v: unknown): AppConfig {
  const r = isRecord(v) ? v : {};
  return {
    serverUrl: str(r.serverUrl),
    channelId: str(r.channelId),
    autoConnect: r.autoConnect === true,
  };
}

/** get_listen_names 载荷守卫：string[] */
function parseNameList(v: unknown): string[] {
  return Array.isArray(v) ? v.filter((n): n is string => typeof n === 'string') : [];
}

/** 本地日期键（YYYY-MM-DD）：今日消息计数跨天归零用 */
function localDateKey(ts: number): string {
  const d = new Date(ts);
  const mm = String(d.getMonth() + 1).padStart(2, '0');
  const dd = String(d.getDate()).padStart(2, '0');
  return `${d.getFullYear()}-${mm}-${dd}`;
}

export const useAppStore = defineStore('app', {
  state: () => ({
    /** 应用六态（wxauto://state 最后值；初始按状态机构造值 SidecarBooting） */
    appState: 'SidecarBooting' as AppStateName,
    /** 当前视图（跨视图导航归 store：Overview 去激活/Activation 回概览） */
    activeView: 'overview',
    /** init 未就绪原因（wxauto://init-fail 最后值；''=无） */
    initFailReason: '' as InitFailReasonName,
    /** WS 连接态：null=尚未收到状态帧（灰色「未知」） */
    wsConnected: null as boolean | null,
    /** 微信在线态：null=尚未收到状态帧 */
    wxOnline: null as boolean | null,
    /** 今日消息计数（跨天自动归零） */
    todayMessages: 0,
    /** 今日日期键（计数归零判据） */
    todayKey: '' as string,
    /** 消息流水（新消息 unshift 头插；500 环形） */
    messages: [] as MessageItem[],
    /** 指令日志（同上环形） */
    commandLog: [] as CommandLogItem[],
    /** 运行日志（新条目头插；1000 环形） */
    appLog: [] as AppLogItem[],
    /** 监听名单（昵称列表） */
    listenNames: [] as string[],
    /** 设备配置（init 拉取；null=未加载） */
    config: null as AppConfig | null,
    /** init 失败描述（浏览器直开/Task 9 未装配时给用户看的原因） */
    initError: '' as string,
    /** init 是否已执行（防 App 重挂导致重复订阅事件） */
    inited: false,
  }),
  getters: {
    sidecarBooting(state): boolean {
      return state.appState === 'SidecarBooting';
    },
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
  },
  actions: {
    /**
     * 订阅 Rust 推送 + 拉取初始配置/监听名单。App 挂载时调用一次（幂等防重）。
     * 失败不抛：写 initError，视图降级展示（面板仍可看已缓存的 state）。
     */
    async init() {
      if (this.inited) return;
      this.inited = true;
      try {
        const unlisteners: UnlistenFn[] = [];
        unlisteners.push(
          await listen<unknown>('wxauto://state', (e) => {
            if (isAppStateName(e.payload)) {
              this.appState = e.payload;
              if (LICENSE_PASSED_STATES.includes(e.payload)) this.initFailReason = '';
            }
          }),
        );
        unlisteners.push(
          await listen<unknown>('wxauto://init-fail', (e) => {
            if (isInitFailPayload(e.payload) && KNOWN_INIT_FAIL_REASONS.includes(e.payload.reason)) {
              this.initFailReason = e.payload.reason as InitFailReasonName;
            }
          }),
        );
        unlisteners.push(
          await listen<unknown>('wxauto://status', (e) => {
            if (isStatusPayload(e.payload)) {
              this.wsConnected = e.payload.wsConnected;
              this.wxOnline = e.payload.wxOnline;
            }
          }),
        );
        unlisteners.push(
          await listen<unknown>('wxauto://message', (e) => {
            this.pushMessage(parseMessageItem(e.payload));
          }),
        );
        unlisteners.push(
          await listen<unknown>('wxauto://command-log', (e) => {
            this.pushCommandLog(parseCommandLogItem(e.payload));
          }),
        );
        unlisteners.push(
          await listen<unknown>('wxauto://app-log', (e) => {
            this.pushAppLog(e.payload);
          }),
        );
        // 桌面 App 生命周期 = 窗口生命周期，无需 unlisten；保留引用便于未来热重载清理
        void unlisteners;
        this.config = parseAppConfig(await invoke<unknown>('get_config'));
        this.listenNames = parseNameList(await invoke<unknown>('get_listen_names'));
        // 补齐状态首值（I4）：bridge attach 早于本 listen 注册时，先发的
        // wxauto://state 事件已丢（tauri 事件无重放），appState 会停在初始
        // SidecarBooting——主动拉一次快照校正；同值时状态机不重发，这是
        // 状态灯落到真值的唯一兜底路径。
        const snap = await invoke<unknown>('get_app_state');
        if (isAppStateName(snap)) this.appState = snap;
        // initFailReason 快照兜底（I1）：init_fail 事件每 sidecar 世代只发
        // 一次，若先于本 listen 注册发出即永久丢失（授权灯恒灰）——拉
        // Supervisor 缓存的最近失败原因补救。仅当前值为空时覆盖：事件
        // 路径（更实时）优先，快照只兜底不回写覆盖。
        const failSnap = await invoke<unknown>('get_init_fail_reason');
        if (
          this.initFailReason === '' &&
          typeof failSnap === 'string' &&
          KNOWN_INIT_FAIL_REASONS.includes(failSnap)
        ) {
          this.initFailReason = failSnap as InitFailReasonName;
        }
        // 运行日志历史补齐（bridge attach 前的条目事件无重放——快照兜底）
        const logs = await invoke<unknown>('get_recent_logs');
        if (Array.isArray(logs)) {
          // 快照旧在前 → reverse 后新在前，与本地已收条目 concat（不置空：
          // invoke 窗口期 app-log 事件可能已 push 部分同源条目，快照与本地
          // 短暂重叠属预期，由下方环形截断吸收；init 幂等只跑一次不会重复叠加）
          this.appLog = logs
            .map(parseAppLogItem)
            .reverse()
            .concat(this.appLog);
          if (this.appLog.length > APP_LOG_RING_LIMIT) {
            this.appLog.length = APP_LOG_RING_LIMIT;
          }
        }
      } catch (err) {
        this.initError = err instanceof Error ? err.message : String(err);
      }
    },
    /** 消息入列：头插 + 500 环形 + 今日计数（跨天归零） */
    pushMessage(msg: MessageItem) {
      this.messages.unshift(msg);
      if (this.messages.length > RING_LIMIT) this.messages.length = RING_LIMIT;
      const key = localDateKey(msg.ts);
      if (key !== this.todayKey) {
        this.todayKey = key;
        this.todayMessages = 0;
      }
      this.todayMessages++;
    },
    /** 指令日志入列：头插 + 500 环形 */
    pushCommandLog(item: CommandLogItem) {
      this.commandLog.unshift(item);
      if (this.commandLog.length > RING_LIMIT) this.commandLog.length = RING_LIMIT;
    },
    /** 运行日志入列：载荷归一 + 头插 + 1000 环形 */
    pushAppLog(payload: unknown) {
      this.appLog.unshift(parseAppLogItem(payload));
      if (this.appLog.length > APP_LOG_RING_LIMIT) this.appLog.length = APP_LOG_RING_LIMIT;
    },
    /** 清空运行日志（Rust ring + 本地双清） */
    async clearAppLog() {
      await invoke('clear_logs');
      this.appLog = [];
    },
    /** 保存配置（token 非空时随配置提交 → Rust 写 keyring），成功后回读刷新 */
    async saveConfig(c: SaveConfigInput) {
      await invoke('save_config', { config: c });
      this.config = parseAppConfig(await invoke<unknown>('get_config'));
    },
    /**
     * 手动执行 sidecar action（与 agent 指令共用串行队列）。
     * action 取值见 Rust map_action：send_message/get_moments/publish_moment/
     * get_friend_requests/accept_friend/…；params 结构随 action。
     */
    async manualExecute(action: string, params: Record<string, unknown>): Promise<ManualOutcome> {
      try {
        return { ok: true, data: await invoke<unknown>('manual_execute', { action, params }) };
      } catch (err) {
        return { ok: false, error: err instanceof Error ? err.message : String(err) };
      }
    },
    /** 添加监听（三重校验在 Rust/sidecar 侧），成功后回读名单 */
    async addListen(nickname: string) {
      await invoke('add_listen', { nickname });
      this.listenNames = parseNameList(await invoke<unknown>('get_listen_names'));
    },
    /** 移除监听，成功后回读名单 */
    async removeListen(nickname: string) {
      await invoke('remove_listen', { nickname });
      this.listenNames = parseNameList(await invoke<unknown>('get_listen_names'));
    },
    /** 连接服务端（WS + hello 重放由 Rust AgentLink 负责） */
    async connect() {
      await invoke('connect');
    },
    /** 断开服务端连接 */
    async disconnect() {
      await invoke('disconnect');
    },
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
  },
});
