<template>
  <t-card title="设置" :bordered="false">
    <template #description>服务器连接参数；token 存系统凭据管理器（keyring），不回显</template>
    <!-- :data 必绑：TDesign FormItem 校验按 name 从 form.data 取值（而非输入框 v-model），
         漏绑则 required 恒判空、保存永远被"必填"拦截（2026-09-08 事故，见 Settings.spec.ts） -->
    <t-form v-if="formReady" :data="form" label-width="120px" @submit="onSave">
      <t-form-item label="服务器地址" name="serverUrl" :rules="[{ required: true, message: '服务器地址必填' }]">
        <t-input v-model="form.serverUrl" placeholder="ws://127.0.0.1:60021" />
      </t-form-item>
      <t-form-item label="渠道 ID" name="channelId" :rules="[{ required: true, message: '渠道 ID 必填' }]">
        <t-input v-model="form.channelId" placeholder="服务端分配的渠道 ID" />
      </t-form-item>
      <t-form-item label="设备 Token" name="token">
        <t-input
          v-model="form.token"
          type="password"
          placeholder="留空 = 不修改已存 token（keyring 不回显）"
          clearable
        />
      </t-form-item>
      <t-form-item label="自动连接" name="autoConnect">
        <t-switch v-model="form.autoConnect" />
        <span class="hint">启动即自动连接服务器</span>
      </t-form-item>
      <t-form-item>
        <t-space>
          <t-button type="submit" theme="primary" :loading="saving">保存</t-button>
        </t-space>
      </t-form-item>
    </t-form>
    <t-skeleton v-else :row-col="[{ width: '80%' }, { width: '60%' }, { width: '70%' }]" animation="gradient" />
  </t-card>
</template>

<script setup lang="ts">
/**
 * 设置视图：serverUrl/channelId/token/autoConnect 表单（spec §3.4）。
 * 保存调 store.saveConfig（Rust 侧落 config.json；token 非空时写 keyring）。
 * 表单在 config 加载完成后才渲染（formReady），避免空值闪染。
 */
import { computed, reactive, ref, watch } from 'vue';
import { MessagePlugin } from 'tdesign-vue-next';
import { useAppStore } from '../stores/app';

const store = useAppStore();

/** 表单就绪：config 已拉取（null=未加载，浏览器直开时保持骨架屏） */
const formReady = computed(() => store.config !== null);

const form = reactive({
  serverUrl: '',
  channelId: '',
  token: '',
  autoConnect: false,
});

// config 到达后回填一次（init 异步；watch 保证任何时序下都能填上）
watch(
  () => store.config,
  (cfg) => {
    if (cfg) {
      form.serverUrl = cfg.serverUrl;
      form.channelId = cfg.channelId;
      form.autoConnect = cfg.autoConnect;
    }
  },
  { immediate: true },
);

const saving = ref(false);

async function onSave({ validateResult }: { validateResult: unknown }) {
  const valid = validateResult === true || validateResult === 'success';
  if (!valid) return;
  saving.value = true;
  try {
    await store.saveConfig({
      serverUrl: form.serverUrl.trim(),
      channelId: form.channelId.trim(),
      autoConnect: form.autoConnect,
      // 留空不提交 token 字段——保留 keyring 已存值（Rust 侧空串跳过写入）
      ...(form.token.trim() !== '' ? { token: form.token.trim() } : {}),
    });
    MessagePlugin.success('设置已保存');
    form.token = '';
  } catch (err) {
    MessagePlugin.error(`保存失败：${err instanceof Error ? err.message : String(err)}`);
  } finally {
    saving.value = false;
  }
}
</script>

<style scoped>
.hint {
  margin-left: 8px;
  color: var(--td-text-color-secondary);
  font-size: 13px;
}
</style>
