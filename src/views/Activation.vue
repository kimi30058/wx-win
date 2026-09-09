<template>
  <div class="activation">
    <!-- 授权状态卡：未激活红 / 已通过绿 / 微信未开橙 -->
    <t-card title="授权状态" :bordered="false">
      <div class="status-line">
        <span class="lamp" :class="lampClass" />
        <span>{{ statusText }}</span>
      </div>
      <!-- 已激活但微信未开：显式重新初始化入口（sidecar 活着 Supervisor 不自动重跑 init） -->
      <t-button
        v-if="store.wechatMissing"
        theme="warning"
        variant="outline"
        :loading="retrying"
        class="block-inline"
        @click="onRetryInit"
      >
        重新初始化
      </t-button>
    </t-card>

    <!-- 激活表单（已通过后隐藏） -->
    <t-card v-if="!store.licensePassed" title="激活" :bordered="false" class="block">
      <!-- :data 必绑：TDesign FormItem 校验取值是 form.data[name] 而非 v-model（Settings 事故同款） -->
      <t-form :data="form" label-width="80px" @submit="onActivate">
        <t-form-item label="激活码" name="code" :rules="[{ required: true, message: '激活码必填' }]">
          <t-input v-model="form.code" placeholder="请输入 wxautox4 激活码" clearable />
        </t-form-item>
        <t-form-item>
          <t-button type="submit" theme="primary" :loading="activating">立即激活</t-button>
        </t-form-item>
      </t-form>
      <t-alert v-if="resultMessage" class="block-inline" :theme="resultOk ? 'success' : 'error'" :message="resultMessage" />
    </t-card>

    <!-- 获取激活码引导（企业统一采购发码模式） -->
    <t-card title="获取激活码" :bordered="false" class="block contact">
      {{ ACTIVATION_CONTACT }}
    </t-card>
  </div>
</template>

<script setup lang="ts">
/**
 * 激活视图：wxautox4 内核授权（spec 2026-09-08）。
 * 状态卡消费 store 三 getter；激活走 activateLicense（Rust 成功即内联
 * 重试 init）；闭环由 state/init-fail 事件回流驱动，本视图只呈现。
 */
import { computed, onUnmounted, reactive, ref, watch } from 'vue';
import { useAppStore } from '../stores/app';

/** 运营联系方式（改文案随发版即可——YAGNI 不做配置项） */
const ACTIVATION_CONTACT = 'wxautox4 激活码为按设备授权（一机一码，激活后永久有效），请联系您的服务管理员获取。';

const store = useAppStore();
const form = reactive({ code: '' });
const activating = ref(false);
const retrying = ref(false);
const resultMessage = ref('');
const resultOk = ref(false);
let jumpTimer: ReturnType<typeof setTimeout> | null = null;
onUnmounted(() => {
  if (jumpTimer) clearTimeout(jumpTimer);
});

/** 初始化错误（P0-3 第 4 态）：RPC 错误帧透传的原始串 */
const initErrorText = computed(() =>
  store.sidecarBooting && !store.needsActivation && !store.wechatMissing && store.initFailReason !== ''
    ? store.initFailReason
    : ''
);

/** 状态卡文案（三态 + 初始化错误第 4 态 + 检测中兜底） */
const statusText = ref('正在检测授权状态…');
function refreshStatusText() {
  if (store.licensePassed) statusText.value = 'wxautox4 授权正常';
  else if (store.needsActivation) statusText.value = 'wxautox4 未激活：微信自动化核心功能不可用';
  else if (store.wechatMissing) statusText.value = '已激活，但未检测到微信客户端——请打开微信 PC 客户端后点击「重新初始化」';
  else if (initErrorText.value) {
    statusText.value = `初始化错误：${initErrorText.value}——程序文件可能不完整，请重新安装或联系管理员`;
  }
  else statusText.value = '正在检测授权状态…';
}
const lampClass = ref('lamp--gray');
function refreshLampClass() {
  if (store.licensePassed) lampClass.value = 'lamp--green';
  else if (store.needsActivation) lampClass.value = 'lamp--red';
  else if (store.wechatMissing) lampClass.value = 'lamp--yellow';
  else if (initErrorText.value) lampClass.value = 'lamp--orange';
  else lampClass.value = 'lamp--gray';
}
watch(
  () => [store.licensePassed, store.needsActivation, store.wechatMissing, initErrorText.value],
  () => {
    refreshStatusText();
    refreshLampClass();
  },
  { immediate: true },
);

/** 激活提交（TDesign form submit 回调携 validateResult） */
async function onActivate({ validateResult }: { validateResult: boolean }) {
  if (validateResult !== true || activating.value) return;
  activating.value = true;
  resultMessage.value = '';
  const r = await store.activateLicense(form.code.trim());
  resultOk.value = r.ok;
  resultMessage.value = r.message || (r.ok ? '激活成功' : '激活失败');
  activating.value = false;
}

/** 手动重新初始化（微信打开后）——失败写结果条红字（M1：无 catch 时
 * invoke reject 变未处理 Promise rejection，用户看不到任何反馈） */
async function onRetryInit() {
  retrying.value = true;
  try {
    await store.retryInit();
  } catch (err) {
    resultOk.value = false;
    resultMessage.value = `重新初始化失败：${err instanceof Error ? err.message : String(err)}`;
  } finally {
    retrying.value = false;
  }
}

/** 激活闭环：状态离开 Booting（WxInit 及之后）→ 成功提示 + 2s 后回概览 */
watch(
  () => store.licensePassed,
  (passed) => {
    if (!passed) return;
    resultOk.value = true;
    resultMessage.value = '激活成功，正在初始化';
    if (jumpTimer) clearTimeout(jumpTimer);
    jumpTimer = setTimeout(() => store.switchView('overview'), 2000);
  },
);
</script>

<style scoped>
.status-line {
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
.lamp--orange {
  background: #e37318;
  box-shadow: 0 0 6px #e37318;
}
.lamp--red {
  background: var(--td-error-color);
  box-shadow: 0 0 6px var(--td-error-color);
}
.lamp--gray {
  background: var(--td-gray-color-6);
}
.block {
  margin-top: 16px;
}
.block-inline {
  margin-top: 12px;
}
.contact {
  color: var(--td-text-color-secondary);
  font-size: 13px;
}
</style>
