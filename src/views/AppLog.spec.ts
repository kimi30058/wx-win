/**
 * AppLog 视图纯逻辑测试：级别+来源过滤（视图挂载依赖 tdesign，过滤函数
 * 抽成 AppLogFilter.ts 纯函数导出直测）。
 */
import { describe, it, expect } from 'vitest';
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
