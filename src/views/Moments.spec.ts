/**
 * Moments 发布表单校验测试：同款 :data 漏绑事故的回归守卫
 * （2026-09-08，与 Settings.vue 同根因——文案 required 恒判空、发布恒被拦）。
 * 手动发布走 store.manualExecute('publish_moment')，mock invoke 断言到达。
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

import Moments from './Moments.vue';

describe('Moments 发布表单校验', () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    invokeMock.mockReset();
    invokeMock.mockImplementation((_cmd: string) => Promise.resolve({ ok: null }));
  });

  it('填文案提交 → 校验通过且 publish_moment 到达编排层（事故回归守卫）', async () => {
    const wrapper = mount(Moments, {
      global: { plugins: [TDesign, createPinia()] },
    });
    await flushPromises();

    // 发布表单的文案是页面首个 textarea（图片路径第二、拉取计数是 input）
    const textarea = wrapper.find('textarea');
    await textarea.setValue('hello moments');

    // 页面两处 t-form：拉取（无 submit）与发布；触发最后一个 form 的 submit
    const forms = wrapper.findAll('form');
    await forms[forms.length - 1].trigger('submit');
    await flushPromises();

    const call = invokeMock.mock.calls.find((c) => c[0] === 'manual_execute');
    expect(call).toBeDefined();
    const params = (call?.[1] as { params: Record<string, unknown> }).params;
    expect(params.text).toBe('hello moments');
  });

  it('文案留空提交 → 校验拦截，publish_moment 不到达', async () => {
    const wrapper = mount(Moments, {
      global: { plugins: [TDesign, createPinia()] },
    });
    await flushPromises();

    const forms = wrapper.findAll('form');
    await forms[forms.length - 1].trigger('submit');
    await flushPromises();

    const call = invokeMock.mock.calls.find(
      (c) =>
        c[0] === 'manual_execute' &&
        (c[1] as { action: string }).action === 'publish_moment',
    );
    expect(call).toBeUndefined();
  });
});
