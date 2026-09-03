/**
 * AppLog 视图纯逻辑测试：级别+来源过滤（视图挂载依赖 tdesign，过滤函数
 * 抽成 AppLogFilter.ts 纯函数导出直测）；另含贴底触发源用例（watch
 * .length 的依据 + 渲染层反转后的顺序，对应审查 I2）。
 */
import { describe, it, expect } from 'vitest';
import { createPinia, setActivePinia } from 'pinia';
import { useAppStore } from '../stores/app';
import { filterAppLog, type LogFilter } from './AppLogFilter';

describe('filterAppLog', () => {
  const items = [
    { ts: 3, level: 'error', source: 'sidecar', message: 'e' },
    { ts: 2, level: 'info', source: 'rust', message: 'i' },
    { ts: 1, level: 'debug', source: 'rust', message: 'd' },
  ] as const;

  it('级别过滤：info 档含 warn/error，不含 debug', () => {
    const f: LogFilter = { minLevel: 'info', sources: ['rust', 'sidecar'] };
    expect(filterAppLog([...items], f).map((x) => x.message)).toEqual(['e', 'i']);
  });

  it('来源过滤：只看 sidecar', () => {
    const f: LogFilter = { minLevel: 'debug', sources: ['sidecar'] };
    expect(filterAppLog([...items], f).map((x) => x.message)).toEqual(['e']);
  });

  it('空来源=全不显示', () => {
    const f: LogFilter = { minLevel: 'debug', sources: [] };
    expect(filterAppLog([...items], f)).toEqual([]);
  });
});

describe('AppLog 贴底触发源', () => {
  it('pushAppLog 头插使 appLog.length 变化（watch 源 .length 的依据）', () => {
    setActivePinia(createPinia());
    const store = useAppStore();
    store.pushAppLog({ ts: 1, level: 'info', source: 'rust', message: 'a' });
    store.pushAppLog({ ts: 2, level: 'info', source: 'rust', message: 'b' });
    expect(store.appLog.length).toBe(2);
    expect(store.appLog[0].message).toBe('b'); // 头插最新在前
  });

  it('reverse 渲染序：过滤结果反转后旧在前（贴底=最新）', () => {
    setActivePinia(createPinia());
    const store = useAppStore();
    store.pushAppLog({ ts: 1, level: 'info', source: 'rust', message: '旧' });
    store.pushAppLog({ ts: 2, level: 'error', source: 'sidecar', message: '新' });
    const f: LogFilter = { minLevel: 'trace', sources: ['rust', 'sidecar'] };
    const rendered = filterAppLog(store.appLog, f).slice().reverse();
    expect(rendered.map((x) => x.message)).toEqual(['旧', '新']);
  });
});
