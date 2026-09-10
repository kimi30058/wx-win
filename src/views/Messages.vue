<template>
  <t-card title="消息流水（只读）" :bordered="false">
    <template #description>实时上报消息，最多保留 {{ RING_LIMIT }} 条；数据来自 wxauto://message 事件</template>
    <t-table
      row-key="id"
      :data="store.messages"
      :columns="columns"
      :max-height="560"
      :pagination="{ defaultPageSize: 50, total: store.messages.length, pageSizeOptions: [] }"
    >
      <template #ts="{ row }">{{ formatTime(row.ts) }}</template>
      <template #msgType="{ row }">
        <t-tag size="small" variant="outline">{{ row.msgType || 'unknown' }}</t-tag>
      </template>
      <template #at="{ row }">
        <t-tag v-if="row.isAt" size="small" theme="warning" variant="light">是</t-tag>
        <span v-else>-</span>
      </template>
    </t-table>
  </t-card>
</template>

<script setup lang="ts">
/**
 * 消息视图：实时消息流水（会话/发送人/类型/摘要），只读（spec §3.4）。
 */
import type { PrimaryTableCol } from 'tdesign-vue-next';
import { useAppStore, type MessageItem, RING_LIMIT } from '../stores/app';

const store = useAppStore();

const columns: PrimaryTableCol<MessageItem>[] = [
  { colKey: 'ts', title: '时间', width: 180 },
  { colKey: 'chatName', title: '会话', width: 160, ellipsis: true },
  { colKey: 'chatType', title: '会话类型', width: 100 },
  { colKey: 'sender', title: '发送人', width: 140, ellipsis: true },
  { colKey: 'msgType', title: '类型', width: 90 },
  { colKey: 'isAt', title: '@我', width: 70, cell: 'at' },
  { colKey: 'content', title: '内容摘要', ellipsis: true },
];

function formatTime(ts: number): string {
  return new Date(ts).toLocaleString('zh-CN', { hour12: false });
}
</script>
