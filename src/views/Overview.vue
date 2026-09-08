<template>
  <div class="overview">
    <!-- 四指示灯：sidecar / WS / 微信 / 授权（spec §3.4 概览行） -->
    <!-- TDesign 为 12 列栅格：4 卡各 span3（4×3=12）恰满一行，span4 会溢出换行 -->
    <t-row :gutter="16">
      <t-col :span="3">
        <t-card title="Sidecar" :bordered="false">
          <div class="lamp-row">
            <span
              class="lamp"
              :class="store.appState === 'SidecarDead' ? 'lamp--red' : store.sidecarBooting ? 'lamp--yellow' : 'lamp--green'"
            />
            <span>{{ store.appState === 'SidecarDead' ? '已崩溃' : store.sidecarBooting ? '启动中' : '运行中' }}</span>
          </div>
        </t-card>
      </t-col>
      <t-col :span="3">
        <t-card title="服务器连接" :bordered="false">
          <div class="lamp-row">
            <span class="lamp" :class="store.wsConnected === null ? 'lamp--gray' : store.wsConnected ? 'lamp--green' : 'lamp--red'" />
            <span>{{ store.wsConnected === null ? '未知' : store.wsConnected ? '已连接' : '未连接' }}</span>
          </div>
        </t-card>
      </t-col>
      <t-col :span="3">
        <t-card title="微信" :bordered="false">
          <div class="lamp-row">
            <span class="lamp" :class="store.wxOnline === null ? 'lamp--gray' : store.wxOnline ? 'lamp--green' : 'lamp--red'" />
            <span>{{ store.wxOnline === null ? '未知' : store.wxOnline ? '在线' : '离线' }}</span>
          </div>
        </t-card>
      </t-col>
      <t-col :span="3">
        <t-card title="授权" :bordered="false">
          <div class="lamp-row">
            <span
              class="lamp"
              :class="store.licensePassed ? 'lamp--green' : store.needsActivation ? 'lamp--red' : store.wechatMissing ? 'lamp--yellow' : 'lamp--gray'"
            />
            <span>{{
              store.licensePassed ? '正常' : store.needsActivation ? '未激活' : store.wechatMissing ? '已激活·微信未开' : '检测中'
            }}</span>
          </div>
          <t-button
            v-if="store.needsActivation"
            size="small"
            theme="danger"
            variant="outline"
            style="margin-top: 8px"
            @click="store.switchView('activation')"
          >
            去激活
          </t-button>
        </t-card>
      </t-col>
    </t-row>

    <!-- 应用状态 + 今日消息 -->
    <t-row :gutter="16" class="block">
      <t-col :span="8">
        <t-card title="运行状态" :bordered="false">
          <t-descriptions>
            <t-descriptions-item label="应用状态">
              <t-tag :theme="stateTheme">{{ APP_STATE_LABELS[store.appState] }}</t-tag>
            </t-descriptions-item>
            <t-descriptions-item label="监听会话数">{{ store.listenNames.length }}</t-descriptions-item>
            <t-descriptions-item label="今日消息">{{ store.todayMessages }} 条</t-descriptions-item>
            <t-descriptions-item label="服务器">
              {{ store.config?.serverUrl || '（未配置，见设置）' }}
            </t-descriptions-item>
          </t-descriptions>
        </t-card>
      </t-col>
    </t-row>

    <!-- 快捷操作：连接/断开（共用 Rust 编排层；重启 sidecar 属 Supervisor 域，不在此面板操作） -->
    <t-card title="快捷操作" :bordered="false" class="block">
      <t-space>
        <t-button theme="primary" :loading="connecting" :disabled="store.wsConnected === true" @click="onConnect">
          连接服务器
        </t-button>
        <t-button theme="default" variant="outline" :loading="disconnecting" :disabled="store.wsConnected !== true" @click="onDisconnect">
          断开连接
        </t-button>
      </t-space>
    </t-card>
  </div>
</template>

<script setup lang="ts">
/**
 * 概览视图：四指示灯（sidecar/WS/微信/授权）+ 运行状态 + 快捷操作（spec §3.4）。
 * 只读消费 store；连接/断开调 store 的 connect/disconnect。
 */
import { computed, ref } from 'vue';
import { MessagePlugin } from 'tdesign-vue-next';
import { useAppStore, APP_STATE_LABELS } from '../stores/app';

const store = useAppStore();
const connecting = ref(false);
const disconnecting = ref(false);

/** 状态 tag 主题色：正常绿系 / 降级与启动橙系 / 崩溃红系 */
const stateTheme = computed(() => {
  switch (store.appState) {
    case 'Ready':
    case 'Busy':
      return 'success';
    case 'SidecarDead':
      return 'danger';
    default:
      return 'warning';
  }
});

async function onConnect() {
  connecting.value = true;
  try {
    await store.connect();
    MessagePlugin.success('已发起连接（状态以指示灯为准）');
  } catch (err) {
    MessagePlugin.error(`连接失败：${errText(err)}`);
  } finally {
    connecting.value = false;
  }
}

async function onDisconnect() {
  disconnecting.value = true;
  try {
    await store.disconnect();
    MessagePlugin.success('已断开连接');
  } catch (err) {
    MessagePlugin.error(`断开失败：${errText(err)}`);
  } finally {
    disconnecting.value = false;
  }
}

/** invoke reject 载荷归一为可展示文本（Rust 侧 Err(String) → string） */
function errText(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}
</script>

<style scoped>
.block {
  margin-top: 16px;
}
.lamp-row {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 14px;
}
.lamp {
  width: 12px;
  height: 12px;
  border-radius: 50%;
  display: inline-block;
}
.lamp--green {
  background: var(--td-success-color);
  box-shadow: 0 0 6px var(--td-success-color);
}
.lamp--yellow {
  background: var(--td-warning-color);
  box-shadow: 0 0 6px var(--td-warning-color);
}
.lamp--red {
  background: var(--td-error-color);
  box-shadow: 0 0 6px var(--td-error-color);
}
.lamp--gray {
  background: var(--td-gray-color-6);
}
</style>
