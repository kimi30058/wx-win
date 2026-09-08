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
    expect(store.activeView, '未激活应自动跳激活页').toBe('activation');
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
    expect(store.activeView, '二次未激活不再抢跳').toBe('overview');
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
