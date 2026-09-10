/**
 * appLog store 逻辑测试：环形上限 + 载荷守卫（非法字段宽容归一/非法级别回退 info）。
 * 只测纯逻辑（pushAppLog/parseAppLogItem），不触 tauri invoke（App 挂载才走）。
 * 另覆盖激活状态推导（initFailReason × appState 三 getter）与激活 actions
 * （init-fail 订阅 / activateLicense / retryInit / switchView——tauri mock 注入）。
 */
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { createPinia, setActivePinia } from 'pinia';

/* tauri mock：init() 订阅测试需捕获 listen 回调（纯逻辑用例不受影响） */
const listenMock = vi.fn<(evt: string, cb: (e: { payload: unknown }) => void) => Promise<() => void>>();
const invokeMock = vi.fn<(cmd: string, args?: unknown) => Promise<unknown>>();
vi.mock('@tauri-apps/api/event', () => ({
  listen: (evt: string, cb: (e: { payload: unknown }) => void) => listenMock(evt, cb),
}));
vi.mock('@tauri-apps/api/core', () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));

import { useAppStore, APP_LOG_RING_LIMIT, type MessageItem } from './app';

describe('appLog', () => {
  it('pushAppLog 头插且环形淘汰', () => {
    setActivePinia(createPinia());
    const store = useAppStore();
    for (let i = 0; i < APP_LOG_RING_LIMIT + 5; i += 1) {
      store.pushAppLog({ ts: i, level: 'info', source: 'rust', message: `m${i}` });
    }
    expect(store.appLog.length).toBe(APP_LOG_RING_LIMIT);
    expect(store.appLog[0].message).toBe(`m${APP_LOG_RING_LIMIT + 4}`);
  });

  it('pushAppLog 接受合法条目结构不变', () => {
    setActivePinia(createPinia());
    const store = useAppStore();
    store.pushAppLog({ ts: 1, level: 'warn', source: 'sidecar', message: 'x' });
    expect(store.appLog[0]).toEqual({ ts: 1, level: 'warn', source: 'sidecar', message: 'x' });
  });

  it('pushAppLog 守卫：非法级别/来源回退 info/rust', () => {
    setActivePinia(createPinia());
    const store = useAppStore();
    store.pushAppLog({ ts: 1, level: 'fatal', source: 'ghost', message: 'x' });
    expect(store.appLog[0].level).toBe('info');
    expect(store.appLog[0].source).toBe('rust');
  });

  it('pushMessage 保留 isAt 字段（群@语义透传）', () => {
    const store = useAppStore();
    store.pushMessage({
      id: 0, chatName: '客户群', chatType: 'group', sender: '李四',
      msgType: 'text', content: '@机器人 报价', ts: 1, isAt: true,
    } as MessageItem);
    expect(store.messages[0].isAt).toBe(true);
  });
});

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

  it('wxauto://init-fail 事件落 initFailReason（未知 reason 原样透传 P0-3）', async () => {
    const store = useAppStore();
    await store.init();
    const reg = listenMock.mock.calls.find((c) => c[0] === 'wxauto://init-fail');
    expect(reg).toBeTruthy();
    if (!reg) return;
    const cb = reg[1];
    cb({ payload: { reason: 'licensed' } });
    expect(store.initFailReason).toBe('licensed');
    expect(store.initFailDetail, '无 detail 帧不落细节').toBe('');
    cb({ payload: { reason: '初始化失败：sidecar 错误: [-32603] X' } });
    expect(store.initFailReason, '未知错误串原样覆盖（透传）').toBe('初始化失败：sidecar 错误: [-32603] X');
  });

  it('init-fail 事件携带 detail 落 initFailDetail（wechat_missing 误报修复）', async () => {
    const store = useAppStore();
    await store.init();
    const reg = listenMock.mock.calls.find((c) => c[0] === 'wxauto://init-fail');
    expect(reg).toBeTruthy();
    if (!reg) return;
    reg[1]({
      payload: { reason: 'wechat_missing', detail: 'RuntimeError: 微信窗口未找到' },
    });
    expect(store.initFailReason).toBe('wechat_missing');
    expect(store.initFailDetail).toBe('RuntimeError: 微信窗口未找到');
    // 无 detail 的后续帧（licensed）清细节——旧值不残留误导
    reg[1]({ payload: { reason: 'licensed' } });
    expect(store.initFailDetail).toBe('');
  });

  it('状态进入 WxInit 及之后清 initFailReason', async () => {
    const store = useAppStore();
    await store.init();
    store.initFailReason = 'wechat_missing';
    store.initFailDetail = 'RuntimeError: X';
    const reg = listenMock.mock.calls.find((c) => c[0] === 'wxauto://state');
    expect(reg).toBeTruthy();
    if (!reg) return;
    reg[1]({ payload: 'WxInit' });
    expect(store.appState).toBe('WxInit');
    expect(store.initFailReason).toBe('');
    expect(store.initFailDetail, '成功路径双清（细节不残留）').toBe('');
  });

  it('get_init_fail_reason 快照兜底：init() 拉缓存落 initFailReason（I1）', async () => {
    // 场景：init_fail 事件先于 listen 注册发出（永久丢失）——init() 经
    // get_init_fail_reason 快照补救。Rust 侧返回 {reason, detail} 对象
    const store = useAppStore();
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === 'get_init_fail_reason') {
        return Promise.resolve({ reason: 'wechat_missing', detail: 'RuntimeError: 微信窗口未找到' });
      }
      return Promise.resolve(null);
    });
    await store.init();
    expect(invokeMock).toHaveBeenCalledWith('get_init_fail_reason', undefined);
    expect(store.initFailReason).toBe('wechat_missing');
    expect(store.initFailDetail, '快照路径同样落细节').toBe('RuntimeError: 微信窗口未找到');
  });

  it('快照兜底仅空值时覆盖：事件路径已落值不被快照回写', async () => {
    const store = useAppStore();
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === 'get_init_fail_reason') {
        return Promise.resolve({ reason: 'wechat_missing', detail: null });
      }
      return Promise.resolve(null);
    });
    await store.init();
    // 事件路径先落 licensed（模拟 listen 注册后收到事件）
    const reg = listenMock.mock.calls.find((c) => c[0] === 'wxauto://init-fail');
    expect(reg).toBeTruthy();
    if (!reg) return;
    reg[1]({ payload: { reason: 'licensed' } });
    expect(store.initFailReason, '事件值优先，快照不覆盖').toBe('licensed');
  });

  it('快照兜底守卫：空串/null 快照不落值（未知串透传）', async () => {
    const store = useAppStore();
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === 'get_init_fail_reason') return Promise.resolve(null);
      return Promise.resolve(null);
    });
    await store.init();
    expect(store.initFailReason, 'null 快照不落值').toBe('');
  });

  it('快照兜底守卫：旧形状字符串快照兼容不误伤', async () => {
    // Rust 侧 InitFailInfo 序列化恒为对象；此用例锁定守卫行为——
    // 非对象快照（异常形态）不落值也不抛错
    const store = useAppStore();
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === 'get_init_fail_reason') return Promise.resolve('licensed');
      return Promise.resolve(null);
    });
    await store.init();
    expect(store.initFailReason, '非对象形状静默忽略').toBe('');
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

describe('initFailReason 未知值透传（P0-3）', () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    listenMock.mockReset();
    listenMock.mockImplementation(async () => () => undefined);
    invokeMock.mockReset();
    invokeMock.mockImplementation(() => Promise.resolve(null));
  });

  function bootStore() {
    const store = useAppStore();
    store.appState = 'SidecarBooting';
    return store;
  }

  it('init_fail 事件携带未知 reason 时原样存储且不触发激活/缺微信 getter', () => {
    const store = bootStore();
    // 直接落状态字段（订阅 handler 逻辑由上方事件用例覆盖，这里测字段容错）
    store.initFailReason = '初始化失败：sidecar 错误: [-32603] ModuleNotFoundError';
    expect(store.initFailReason).toBe('初始化失败：sidecar 错误: [-32603] ModuleNotFoundError');
    expect(store.needsActivation).toBe(false);
    expect(store.wechatMissing).toBe(false);
  });

  it('快照兜底不再丢弃未知值', async () => {
    const store = bootStore();
    // get_init_fail_reason 返回未知 reason（Task 3 起 Rust 会写 init RPC 错误帧）
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === 'get_init_fail_reason') {
        return Promise.resolve({ reason: '初始化失败：sidecar 错误: [-32603] X', detail: null });
      }
      if (cmd === 'get_app_state') return Promise.resolve('SidecarBooting');
      if (cmd === 'get_recent_logs') return Promise.resolve([]);
      return Promise.resolve(null);
    });
    await store.init();
    expect(store.initFailReason).toContain('初始化失败');
  });
});
