<template>
  <t-card title="运行日志" :bordered="false">
    <template #description>
      应用（rust）+ sidecar（python）运行日志实时流；最多保留 {{ APP_LOG_RING_LIMIT }} 条
    </template>
    <div class="toolbar">
      <t-select
        :value="filter.minLevel"
        class="toolbar__level"
        :options="LEVEL_OPTIONS"
        @change="(v: unknown) => (filter.minLevel = typeof v === 'string' ? (v as AppLogLevelName) : 'info')"
      />
      <t-checkbox-group
        :value="filter.sources"
        :options="SOURCE_OPTIONS"
        @change="(v: unknown) => {
          if (Array.isArray(v)) filter.sources = v.filter((s): s is AppLogSourceName => typeof s === 'string');
        }"
      />
      <t-switch v-model="paused" label="暂停滚动" />
      <t-button theme="default" variant="outline" size="small" @click="onClear">清空</t-button>
    </div>
    <div ref="scrollRef" class="logbox" @scroll="onScroll">
      <div
        v-for="(item, i) in visible"
        :key="`${item.ts}-${i}`"
        class="logline"
        :class="`logline--${item.level}`"
      >
        <span class="logline__ts">{{ formatTime(item.ts) }}</span>
        <span class="logline__source tag">{{ item.source }}</span>
        <span class="logline__msg">{{ item.message }}</span>
      </div>
      <div v-if="visible.length === 0" class="logbox__empty">暂无符合条件的日志</div>
    </div>
  </t-card>
</template>

<script setup lang="ts">
/**
 * 运行日志视图（spec §3.4 新增第七视图）：Rust + sidecar 日志实时流。
 * - 级别下拉（默认 info 及以上）+ 来源多选（默认全选）
 * - 自动贴底滚动；用户上滚暂停、回底部恢复（console 经典行为）
 * - 1000 条直接渲染（spec：不引虚拟滚动）
 */
import { computed, nextTick, reactive, ref, watch } from 'vue';
import { MessagePlugin } from 'tdesign-vue-next';
import type { SelectOption } from 'tdesign-vue-next';
import {
  useAppStore,
  APP_LOG_RING_LIMIT,
  type AppLogLevelName,
  type AppLogSourceName,
} from '../stores/app';
import { filterAppLog, type LogFilter } from './AppLogFilter';

const store = useAppStore();
const filter = reactive<LogFilter>({ minLevel: 'info', sources: ['rust', 'sidecar'] });
const paused = ref(false);
const scrollRef = ref<HTMLElement | null>(null);

const LEVEL_OPTIONS: SelectOption[] = [
  { label: 'ERROR 及以上', value: 'error' },
  { label: 'WARN 及以上', value: 'warn' },
  { label: 'INFO 及以上（默认）', value: 'info' },
  { label: 'DEBUG 及以上', value: 'debug' },
  { label: '全部（TRACE）', value: 'trace' },
];
const SOURCE_OPTIONS = [
  { label: 'rust', value: 'rust' },
  { label: 'sidecar', value: 'sidecar' },
];

const visible = computed(() => filterAppLog(store.appLog, filter));

/** 新日志到达且未暂停时贴底（watch 数组引用变化） */
watch(
  () => store.appLog,
  () => {
    if (paused.value) return;
    void nextTick(() => {
      const el = scrollRef.value;
      if (el) el.scrollTop = el.scrollHeight;
    });
  },
);

/** 用户上滚离底即暂停；回底恢复 */
function onScroll(): void {
  const el = scrollRef.value;
  if (!el) return;
  const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 8;
  paused.value = !atBottom;
}

/** 清空（Rust ring + 本地双清）；invoke reject 须 catch——失败给用户可见提示 */
async function onClear(): Promise<void> {
  try {
    await store.clearAppLog();
    MessagePlugin.success('运行日志已清空');
  } catch (err) {
    MessagePlugin.error(`清空失败：${err instanceof Error ? err.message : String(err)}`);
  }
}

function formatTime(ts: number): string {
  return new Date(ts).toLocaleString('zh-CN', { hour12: false });
}
</script>

<style scoped>
.toolbar {
  display: flex;
  align-items: center;
  gap: 12px;
  margin-bottom: 12px;
}
.toolbar__level {
  width: 180px;
}
.logbox {
  height: 560px;
  overflow: auto;
  padding: 8px 12px;
  background: var(--td-bg-color-page, #f5f6f7);
  border-radius: 4px;
  font-family: var(--td-font-family-code, monospace);
  font-size: 12px;
}
.logline {
  display: flex;
  gap: 8px;
  padding: 1px 0;
  white-space: pre-wrap;
  word-break: break-all;
}
.logline__ts {
  color: var(--td-text-color-secondary);
  flex-shrink: 0;
}
.logline__source {
  flex-shrink: 0;
}
.logline--error .logline__msg {
  color: var(--td-error-color);
}
.logline--warn .logline__msg {
  color: var(--td-warning-color);
}
.logline--debug .logline__msg,
.logline--trace .logline__msg {
  color: var(--td-text-color-placeholder);
}
.tag {
  border: 1px solid var(--td-component-border);
  border-radius: 2px;
  padding: 0 4px;
  font-size: 11px;
  line-height: 18px;
}
.logbox__empty {
  color: var(--td-text-color-placeholder);
  text-align: center;
  padding-top: 24px;
}
</style>
