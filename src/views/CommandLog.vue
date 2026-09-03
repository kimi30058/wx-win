<template>
  <t-card title="指令日志" :bordered="false">
    <template #description>
      下行指令 + 结果流水（requestId/action/耗时/成败），排障用；最多保留 {{ RING_LIMIT }} 条
    </template>
    <t-table
      row-key="requestId"
      :data="store.commandLog"
      :columns="columns"
      :max-height="560"
      :pagination="{ defaultPageSize: 50, total: store.commandLog.length, pageSizeOptions: [] }"
    >
      <template #ts="{ row }">{{ formatTime(row.ts) }}</template>
      <template #requestId="{ row }">
        <span class="mono">{{ row.requestId || '-' }}</span>
      </template>
      <template #success="{ row }">
        <t-tag size="small" :theme="row.success ? 'success' : 'danger'">
          {{ row.success ? '成功' : '失败' }}
        </t-tag>
      </template>
      <template #durationMs="{ row }">{{ row.durationMs }} ms</template>
      <template #error="{ row }">
        <span class="mono error-text">{{ row.error || '-' }}</span>
      </template>
    </t-table>
  </t-card>
</template>

<script setup lang="ts">
/**
 * 指令日志视图：下行指令 + 结果流水（排障用，spec §3.4）。
 * 只读消费 store.commandLog（wxauto://command-log 事件，500 环形）。
 */
import type { PrimaryTableCol } from 'tdesign-vue-next';
import { useAppStore, type CommandLogItem, RING_LIMIT } from '../stores/app';

const store = useAppStore();

const columns: PrimaryTableCol<CommandLogItem>[] = [
  { colKey: 'ts', title: '时间', width: 180 },
  { colKey: 'requestId', title: '请求 ID', width: 200, ellipsis: true },
  { colKey: 'action', title: 'Action', width: 150, ellipsis: true },
  { colKey: 'success', title: '结果', width: 90 },
  { colKey: 'durationMs', title: '耗时', width: 100 },
  { colKey: 'error', title: '错误信息', ellipsis: true },
];

function formatTime(ts: number): string {
  return new Date(ts).toLocaleString('zh-CN', { hour12: false });
}
</script>

<style scoped>
.mono {
  font-family: var(--td-font-family-code, monospace);
  font-size: 12px;
}
.error-text {
  color: var(--td-error-color);
}
</style>
