<template>
  <div>
    <!-- 朋友圈浏览：get_moments 结果 -->
    <t-card title="朋友圈" :bordered="false">
      <template #description>moments.get 拉取（打开朋友圈窗口 + 拟人延时，耗时可达 10s+，请耐心）</template>
      <div class="toolbar">
        <t-input-number v-model="count" :min="1" :max="50" theme="column" style="width: 120px" tips="拉取条数" />
        <t-button theme="primary" :loading="loading" :disabled="count < 1" @click="onLoad">拉取朋友圈</t-button>
      </div>
      <t-table
        v-if="moments.length > 0"
        row-key="content"
        :data="rows"
        :columns="columns"
        :max-height="420"
      >
        <template #index="{ rowIndex }">{{ rowIndex + 1 }}</template>
      </t-table>
      <t-alert v-else-if="loaded" theme="info" message="拉取成功，但朋友圈无内容" class="block" />
    </t-card>

    <!-- 手动发布：publish_moment -->
    <t-card title="发布朋友圈" :bordered="false" class="block">
      <!-- :data 必绑：同 Settings.vue 事故（FormItem 校验按 name 从 form.data 取值，漏绑则文案必填恒拦） -->
      <t-form :data="{ text }" label-width="90px" @submit="onPublish">
        <t-form-item label="文案" name="text" :rules="[{ required: true, message: '文案必填' }]">
          <t-textarea
            v-model="text"
            placeholder="朋友圈文案"
            :autosize="{ minRows: 3, maxRows: 6 }"
            :maxlength="500"
          />
        </t-form-item>
        <t-form-item label="图片路径" name="images">
          <t-textarea
            v-model="imagesText"
            placeholder="本机图片绝对路径，每行一个（可空 = 纯文字）"
            :autosize="{ minRows: 2, maxRows: 4 }"
          />
        </t-form-item>
        <t-form-item label="可见范围" name="privacy">
          <t-radio-group v-model="privacy">
            <t-radio value="public">公开</t-radio>
            <t-radio value="whitelist">白名单</t-radio>
            <t-radio value="blacklist">黑名单</t-radio>
          </t-radio-group>
        </t-form-item>
        <t-form-item v-if="privacy !== 'public'" label="标签" name="tags">
          <t-input v-model="tagsText" placeholder="白/黑名单标签名，逗号分隔" />
        </t-form-item>
        <t-form-item>
          <t-space>
            <t-button type="submit" theme="primary" :loading="publishing">发布</t-button>
          </t-space>
        </t-form-item>
      </t-form>
    </t-card>
  </div>
</template>

<script setup lang="ts">
/**
 * 朋友圈视图：get_moments 结果浏览 + 手动发布入口（spec §3.4）。
 * 均走 store.manualExecute（action=get_moments / publish_moment，
 * 与 agent 指令共用串行队列——面板不绕过编排层直连 sidecar）。
 */
import { computed, ref } from 'vue';
import { MessagePlugin } from 'tdesign-vue-next';
import type { PrimaryTableCol } from 'tdesign-vue-next';
import { useAppStore } from '../stores/app';

/** 朋友圈条目（moments.get 返回 { moments: [{ content, sender }] }） */
interface MomentItem {
  content: string;
  sender: string;
}

const store = useAppStore();

/* ---------- 拉取 ---------- */
const count = ref(10);
const loading = ref(false);
const loaded = ref(false);
const moments = ref<MomentItem[]>([]);

const rows = computed<MomentItem[]>(() => moments.value);

const columns: PrimaryTableCol<MomentItem>[] = [
  { colKey: 'index', title: '#', width: 60 },
  { colKey: 'sender', title: '发布者', width: 160, ellipsis: true },
  { colKey: 'content', title: '内容', ellipsis: true },
];

/** get_moments 载荷收窄：{ moments: [{ content, sender }] } */
function parseMoments(data: unknown): MomentItem[] {
  if (typeof data !== 'object' || data === null) return [];
  const list = (data as Record<string, unknown>).moments;
  if (!Array.isArray(list)) return [];
  return list
    .filter((m): m is Record<string, unknown> => typeof m === 'object' && m !== null)
    .map((m) => ({
      content: typeof m.content === 'string' ? m.content : '',
      sender: typeof m.sender === 'string' ? m.sender : '',
    }));
}

async function onLoad() {
  loading.value = true;
  try {
    const outcome = await store.manualExecute('get_moments', { count: count.value });
    if (outcome.ok) {
      moments.value = parseMoments(outcome.data);
      loaded.value = true;
      MessagePlugin.success(`拉取成功：${moments.value.length} 条`);
    } else {
      MessagePlugin.error(`拉取失败：${outcome.error}`);
    }
  } finally {
    loading.value = false;
  }
}

/* ---------- 发布 ---------- */
const text = ref('');
const imagesText = ref('');
const privacy = ref<'public' | 'whitelist' | 'blacklist'>('public');
const tagsText = ref('');
const publishing = ref(false);

async function onPublish({ validateResult }: { validateResult: unknown }) {
  // TDesign onSubmit 校验结果：'success' 或 true 视为通过（防 unknown 直判）
  const valid = validateResult === true || validateResult === 'success';
  if (!valid) return;
  const images = imagesText.value
    .split('\n')
    .map((s) => s.trim())
    .filter((s) => s !== '');
  const tags = tagsText.value
    .split(/[,，]/)
    .map((s) => s.trim())
    .filter((s) => s !== '');
  publishing.value = true;
  try {
    const params: Record<string, unknown> = {
      text: text.value,
      privacy: privacy.value,
      images,
    };
    if (privacy.value !== 'public') params.tags = tags;
    const outcome = await store.manualExecute('publish_moment', params);
    if (outcome.ok) {
      MessagePlugin.success('朋友圈已发布');
      text.value = '';
      imagesText.value = '';
      tagsText.value = '';
      privacy.value = 'public';
    } else {
      MessagePlugin.error(`发布失败：${outcome.error}`);
    }
  } finally {
    publishing.value = false;
  }
}
</script>

<style scoped>
.toolbar {
  display: flex;
  gap: 8px;
  margin-bottom: 12px;
  align-items: center;
}
.block {
  margin-top: 16px;
}
</style>
