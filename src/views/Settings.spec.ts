/**
 * Settings 视图表单校验测试：修复回归的守卫（2026-09-08 事故——
 * t-form 未绑 :data，TDesign FormItem 校验取值路径是 form.data[name]
 * 而非输入框 v-model，导致 serverUrl/channelId 恒判空、保存永远被拦）。
 *
 * 挂载真实 Settings.vue + mock tauri invoke（get_config 回配置、
 * save_config 记调用），填入合法值提交，断言 save_config 真被调用。
 */
// @vitest-environment happy-dom
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { mount, flushPromises } from '@vue/test-utils';
import { createPinia, setActivePinia } from 'pinia';
import TDesign from 'tdesign-vue-next';

// mock tauri 核心 API：invoke 按命令名分发；event.listen 返回哑 unlisten
const invokeMock = vi.fn<(cmd: string, args?: unknown) => Promise<unknown>>();
vi.mock('@tauri-apps/api/core', () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));
vi.mock('@tauri-apps/api/event', () => ({
  listen: async () => () => undefined,
}));

import Settings from './Settings.vue';

/** 模拟 get_config 回包（token 不回显属正常契约；webhookUrl/webhookTemplate 为 Task 9 新增） */
const FAKE_CONFIG = {
  serverUrl: 'ws://127.0.0.1:60021',
  channelId: 'ch-001',
  autoConnect: false,
  webhookUrl: 'https://open.feishu.cn/hook/demo',
  webhookTemplate: '',
};

function mountSettings() {
  return mount(Settings, {
    global: { plugins: [TDesign, createPinia()] },
  });
}

describe('Settings 表单校验与保存', () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    invokeMock.mockReset();
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === 'get_config') return Promise.resolve({ ...FAKE_CONFIG });
      if (cmd === 'save_config') return Promise.resolve(null);
      return Promise.resolve(null);
    });
  });

  it('config 到达后回填表单（serverUrl/channelId 非空）', async () => {
    const wrapper = mountSettings();
    // store.init 由 App.vue 调用，spec 直接预置 config 触发 watch 回填
    const { useAppStore } = await import('../stores/app');
    useAppStore().config = { ...FAKE_CONFIG };
    await flushPromises();
    const inputs = wrapper.findAll('input');
    expect((inputs[0].element as HTMLInputElement).value).toBe(FAKE_CONFIG.serverUrl);
    expect((inputs[1].element as HTMLInputElement).value).toBe(FAKE_CONFIG.channelId);
  });

  it('填入合法值提交 → 必填校验通过且 save_config 被调用（事故回归守卫）', async () => {
    const wrapper = mountSettings();
    const { useAppStore } = await import('../stores/app');
    useAppStore().config = { ...FAKE_CONFIG };
    await flushPromises();

    // 用户改值再保存：直接驱动输入框（模拟真实输入路径）
    const inputs = wrapper.findAll('input');
    await inputs[0].setValue('ws://10.0.0.5:60021');
    await inputs[1].setValue('ch-002');

    // 触发表单提交（t-button type=submit → form submit 事件）
    await wrapper.find('form').trigger('submit');
    await flushPromises();

    const saveCall = invokeMock.mock.calls.find((c) => c[0] === 'save_config');
    expect(saveCall).toBeDefined();
    const saved = saveCall?.[1] as { config: { serverUrl: string; channelId: string } };
    expect(saved.config.serverUrl).toBe('ws://10.0.0.5:60021');
    expect(saved.config.channelId).toBe('ch-002');
  });

  it('清空必填字段提交 → 校验拦截，save_config 不被调用', async () => {
    const wrapper = mountSettings();
    const { useAppStore } = await import('../stores/app');
    useAppStore().config = { ...FAKE_CONFIG };
    await flushPromises();

    const inputs = wrapper.findAll('input');
    await inputs[0].setValue('');
    await inputs[1].setValue('ch-003');

    await wrapper.find('form').trigger('submit');
    await flushPromises();

    const saveCall = invokeMock.mock.calls.find((c) => c[0] === 'save_config');
    expect(saveCall).toBeUndefined();
  });

  it('webhook 字段回填与提交——URL 必须随保存透传', async () => {
    const wrapper = mountSettings();
    const { useAppStore } = await import('../stores/app');
    useAppStore().config = { ...FAKE_CONFIG };
    await flushPromises();
    const inputs = wrapper.findAll('input');
    const urlInput = inputs.find((i) =>
      (i.element as HTMLInputElement).value.includes('feishu'),
    );
    expect(urlInput).toBeTruthy();
    // 提交：填 token 触发完整保存链
    wrapper.find('form').trigger('submit');
    await flushPromises();
    const call = invokeMock.mock.calls.find((c) => c[0] === 'save_config');
    expect(call).toBeTruthy();
    const payload = (call?.[1] as { config: Record<string, unknown> }).config;
    expect(payload.webhookUrl).toBe(FAKE_CONFIG.webhookUrl);
    expect(payload.webhookTemplate).toBe('');
  });
});
