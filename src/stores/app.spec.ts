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

import { useAppStore, APP_LOG_RING_LIMIT } from './app';

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
    expect(store.initFailReason, '未知 reason 不覆盖').toBe('licensed');
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
