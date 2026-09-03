<template>
  <t-layout style="height: 100vh">
    <t-aside width="200px">
      <div class="brand">WxAuto 桌面端</div>
      <t-menu :value="view" @change="(v: unknown) => (view = typeof v === 'string' ? v : 'overview')">
        <t-menu-item value="overview">
          <template #icon><t-icon name="dashboard" /></template>概览
        </t-menu-item>
        <t-menu-item value="messages">
          <template #icon><t-icon name="chat" /></template>消息
        </t-menu-item>
        <t-menu-item value="listen">
          <template #icon><t-icon name="user-circle" /></template>监听
        </t-menu-item>
        <t-menu-item value="commands">
          <template #icon><t-icon name="terminal" /></template>指令日志
        </t-menu-item>
        <t-menu-item value="applog">
          <template #icon><t-icon name="file-paste" /></template>运行日志
        </t-menu-item>
        <t-menu-item value="moments">
          <template #icon><t-icon name="image" /></template>朋友圈
        </t-menu-item>
        <t-menu-item value="settings">
          <template #icon><t-icon name="setting" /></template>设置
        </t-menu-item>
      </t-menu>
    </t-aside>
    <t-layout>
      <t-header class="topbar">
        <t-tag theme="primary" variant="light">{{ APP_STATE_LABELS[store.appState] }}</t-tag>
        <t-tag v-if="store.initError" theme="danger" variant="light">
          初始化失败：{{ store.initError }}
        </t-tag>
        <span class="topbar__msg-count">今日消息 {{ store.todayMessages }} 条</span>
      </t-header>
      <t-content class="content">
        <Overview v-if="view === 'overview'" />
        <Messages v-else-if="view === 'messages'" />
        <ListenView v-else-if="view === 'listen'" />
        <CommandLog v-else-if="view === 'commands'" />
        <AppLogView v-else-if="view === 'applog'" />
        <Moments v-else-if="view === 'moments'" />
        <Settings v-else />
      </t-content>
    </t-layout>
  </t-layout>
</template>

<script setup lang="ts">
/**
 * 桌面面板壳：TDesign 侧边菜单 + activeView 切换（router-less——
 * 单窗口桌面 App 不需要 vue-router，视图切换即组件 v-if 分支）。
 * 六视图按 spec §3.4；数据源统一为 stores/app.ts。
 */
import { ref, onMounted } from 'vue';
import { useAppStore, APP_STATE_LABELS } from './stores/app';
import Overview from './views/Overview.vue';
import Messages from './views/Messages.vue';
import ListenView from './views/Listen.vue';
import CommandLog from './views/CommandLog.vue';
import AppLogView from './views/AppLog.vue';
import Moments from './views/Moments.vue';
import Settings from './views/Settings.vue';

const view = ref('overview');
const store = useAppStore();
onMounted(() => void store.init());
</script>

<style scoped>
.brand {
  height: 56px;
  display: flex;
  align-items: center;
  justify-content: center;
  font-weight: 600;
  font-size: 16px;
  border-bottom: 1px solid var(--td-component-border);
}
.topbar {
  height: 48px;
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 0 16px;
  background: var(--td-bg-color-container);
  border-bottom: 1px solid var(--td-component-border);
}
.topbar__msg-count {
  margin-left: auto;
  color: var(--td-text-color-secondary);
  font-size: 13px;
}
.content {
  padding: 16px;
  overflow: auto;
}
</style>
