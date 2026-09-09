/**
 * Activation 视图测试：状态卡三态 + 激活表单提交 + 重新初始化入口。
 * mock tauri invoke（activate_license / retry_init 按用例注入）。
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

import Activation from './Activation.vue';

function mountActivation() {
  return mount(Activation, { global: { plugins: [TDesign, createPinia()] } });
}

/** 未激活现场：Booting + licensed（视图各分支的前提态） */
async function setUnlicensed() {
  const { useAppStore } = await import('../stores/app');
  const store = useAppStore();
  store.appState = 'SidecarBooting';
  store.initFailReason = 'licensed';
  await flushPromises();
  return store;
}

/** 初始化错误现场：Booting + 未知错误串（P0-3 第 4 态） */
async function setInitError() {
  const { useAppStore } = await import('../stores/app');
  const store = useAppStore();
  store.appState = 'SidecarBooting';
  store.initFailReason = '初始化失败：sidecar 错误: [-32603] ModuleNotFoundError: No module named \'requests\'';
  await flushPromises();
  return store;
}

describe('Activation 视图', () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    invokeMock.mockReset();
    invokeMock.mockImplementation(() => Promise.resolve(null));
  });

  it('未激活：状态卡显示未激活文案 + 联系运营引导', async () => {
    const wrapper = mountActivation();
    await setUnlicensed();
    expect(wrapper.text()).toContain('未激活');
    expect(wrapper.text()).toContain('服务管理员');
  });

  it('已通过：状态卡显示授权正常且激活表单隐藏', async () => {
    const wrapper = mountActivation();
    const { useAppStore } = await import('../stores/app');
    useAppStore().appState = 'Ready';
    await flushPromises();
    expect(wrapper.text()).toContain('授权正常');
    // 已通过后激活表单整体 v-if 摘除（比按钮禁用更强——杜绝重复激活入口）
    expect(wrapper.find('button[type="submit"]').exists()).toBe(false);
    expect(wrapper.text()).not.toContain('请输入 wxautox4 激活码');
  });

  it('填码提交 → activate_license invoke 透传 + 成功提示', async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === 'activate_license') return Promise.resolve({ ok: true, message: '激活成功' });
      return Promise.resolve(null);
    });
    const wrapper = mountActivation();
    await setUnlicensed();
    const input = wrapper.find('input');
    await input.setValue('ABC-123');
    await wrapper.find('form').trigger('submit');
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith('activate_license', { code: 'ABC-123' });
    expect(wrapper.text()).toContain('激活成功');
  });

  it('激活失败（invoke reject）→ 红字错误不白屏', async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === 'activate_license') return Promise.reject(new Error('激活请求失败：超时'));
      return Promise.resolve(null);
    });
    const wrapper = mountActivation();
    await setUnlicensed();
    await wrapper.find('input').setValue('BAD');
    await wrapper.find('form').trigger('submit');
    await flushPromises();
    expect(wrapper.text()).toContain('激活请求失败：超时');
  });

  it('wechat_missing：显示重新初始化按钮 → 点击调 retry_init', async () => {
    const wrapper = mountActivation();
    const { useAppStore } = await import('../stores/app');
    const store = useAppStore();
    store.appState = 'SidecarBooting';
    store.initFailReason = 'wechat_missing';
    await flushPromises();
    const btn = wrapper.findAll('button').find((b) => b.text().includes('重新初始化'));
    expect(btn).toBeTruthy();
    if (!btn) return;
    await btn.trigger('click');
    await flushPromises();
    // invoke('retry_init') 无第二参,mock 包装层透传 undefined——按命令名 find 调用(Settings.spec 同款)
    const retryCall = invokeMock.mock.calls.find((c) => c[0] === 'retry_init');
    expect(retryCall).toBeDefined();
  });

  it('初始化错误态（第 4 态）显示原始错误串与重装引导 + 橙灯', async () => {
    const wrapper = mountActivation();
    await setInitError();
    await flushPromises();
    // mount 挂 detached 容器（body.innerText 空）——与现有用例一致用 wrapper.text()
    const text = wrapper.text();
    expect(text).toContain('ModuleNotFoundError');
    expect(text).toContain('重新安装');
    expect(wrapper.find('.lamp--orange').exists()).toBe(true);
  });
});
