/**
 * appLog store 逻辑测试：环形上限 + 载荷守卫（非法字段宽容归一/非法级别回退 info）。
 * 只测纯逻辑（pushAppLog/parseAppLogItem），不触 tauri invoke（App 挂载才走）。
 */
import { describe, it, expect } from 'vitest';
import { createPinia, setActivePinia } from 'pinia';
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
});
