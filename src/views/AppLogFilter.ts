/**
 * 运行日志过滤纯函数（AppLog.vue 与 spec 共用；抽离 .vue 便于直测）。
 */
import type { AppLogItem, AppLogLevelName, AppLogSourceName } from '../stores/app';

export interface LogFilter {
  minLevel: AppLogLevelName;
  sources: AppLogSourceName[];
}

/** 级别从低到高（error 最高；与 store 的 APP_LOG_LEVELS 同序不同向，勿混用） */
const LEVEL_ORDER: AppLogLevelName[] = ['trace', 'debug', 'info', 'warn', 'error'];

/** 级别过滤：minLevel 及以上保留（error 最高）；来源过滤：白名单交集 */
export function filterAppLog(items: AppLogItem[], f: LogFilter): AppLogItem[] {
  const min = LEVEL_ORDER.indexOf(f.minLevel);
  return items.filter(
    (it) => LEVEL_ORDER.indexOf(it.level) >= min && f.sources.includes(it.source),
  );
}
