<template>
  <div>
    <!-- 监听名单 CRUD（spec §3.4 监听行：三重校验在 sidecar 侧，失败以报错提示呈现） -->
    <t-card title="监听名单" :bordered="false">
      <template #description>
        逐会话注册监听（昵称须与微信会话名一致）；名单持久化于配置，sidecar 重启后自动重放
      </template>
      <div class="add-row">
        <t-input
          v-model="newName"
          placeholder="微信昵称 / 群名"
          clearable
          style="max-width: 280px"
          @enter="onAdd"
        />
        <t-button theme="primary" :loading="adding" :disabled="!newName.trim()" @click="onAdd">添加监听</t-button>
      </div>
      <t-table
        row-key="nickname"
        :data="listenRows"
        :columns="listenColumns"
        :max-height="320"
        :loading="listLoading"
      >
        <template #op="{ row }">
          <t-popconfirm content="确认移除该监听？" @confirm="onRemove(row.nickname)">
            <t-button size="small" theme="danger" variant="text">移除</t-button>
          </t-popconfirm>
        </template>
      </t-table>
    </t-card>

    <!-- 好友申请：一键通过（备注/标签）/忽略 -->
    <t-card title="好友申请" :bordered="false" class="block">
      <template #description>读取 wx.GetNewFriends(acceptable=True) 列表；通过时 remark GBK 32 字节截断由 sidecar 处理</template>
      <div class="add-row">
        <t-button variant="outline" :loading="fetching" @click="onFetchRequests">刷新申请列表</t-button>
      </div>
      <t-table row-key="name" :data="requests" :columns="requestColumns" :max-height="320">
        <template #op="{ row }">
          <t-space size="small">
            <t-button size="small" theme="primary" variant="text" @click="openAccept(row.name)">通过</t-button>
            <t-button size="small" theme="default" variant="text" @click="onIgnore(row.name)">忽略</t-button>
          </t-space>
        </template>
        <template #empty>暂无好友申请（点击「刷新申请列表」拉取）</template>
      </t-table>
    </t-card>

    <!-- 通过好友对话框：备注 + 标签 -->
    <t-dialog
      v-model:visible="acceptVisible"
      :header="`通过好友申请：${acceptName}`"
      :confirm-btn="{ content: '通过', loading: accepting }"
      @confirm="onAccept"
    >
      <t-form label-width="72px">
        <t-form-item label="备注" name="remark">
          <t-input v-model="acceptRemark" placeholder="备注（可空；超长自动 GBK 32 字节截断）" :maxlength="40" />
        </t-form-item>
        <t-form-item label="标签" name="tags">
          <t-input v-model="acceptTags" placeholder="标签（可空，逗号分隔）" />
        </t-form-item>
      </t-form>
    </t-dialog>
  </div>
</template>

<script setup lang="ts">
/**
 * 监听视图：监听名单 CRUD + 好友申请入口（spec §3.4）。
 * 名单操作走 store.addListen/removeListen；好友申请走 manualExecute
 * （action=get_friend_requests / accept_friend，Rust map_action 契约）。
 */
import { computed, ref } from 'vue';
import { MessagePlugin } from 'tdesign-vue-next';
import type { PrimaryTableCol } from 'tdesign-vue-next';
import { useAppStore } from '../stores/app';

/** 好友申请行（friends.new_requests 返回 { requests: [{ name }] }） */
interface FriendRequestRow {
  name: string;
}

const store = useAppStore();

/* ---------- 监听名单 ---------- */
const newName = ref('');
const adding = ref(false);
// 初始名单加载态：store.inited 完成前表格转圈（名单空 ≠ 加载完）
const listLoading = computed(() => !store.inited);

interface ListenRow {
  nickname: string;
}

const listenRows = computed<ListenRow[]>(() =>
  store.listenNames.map((nickname) => ({ nickname })),
);

const listenColumns: PrimaryTableCol<ListenRow>[] = [
  { colKey: 'nickname', title: '昵称 / 群名', ellipsis: true },
  { colKey: 'op', title: '操作', width: 100 },
];

async function onAdd() {
  const nickname = newName.value.trim();
  if (!nickname) return;
  adding.value = true;
  try {
    await store.addListen(nickname);
    MessagePlugin.success(`已添加监听：${nickname}`);
    newName.value = '';
  } catch (err) {
    MessagePlugin.error(`添加失败：${errText(err)}`);
  } finally {
    adding.value = false;
  }
}

async function onRemove(nickname: string) {
  try {
    await store.removeListen(nickname);
    MessagePlugin.success(`已移除监听：${nickname}`);
  } catch (err) {
    MessagePlugin.error(`移除失败：${errText(err)}`);
  }
}

/* ---------- 好友申请 ---------- */
const requests = ref<FriendRequestRow[]>([]);
const fetching = ref(false);
const acceptVisible = ref(false);
const acceptName = ref('');
const acceptRemark = ref('');
const acceptTags = ref('');
const accepting = ref(false);

const requestColumns: PrimaryTableCol<FriendRequestRow>[] = [
  { colKey: 'name', title: '申请人', ellipsis: true },
  { colKey: 'op', title: '操作', width: 120 },
];

/** manualExecute 返回载荷收窄：{ requests: [{ name }] }（未知结构防御） */
function parseRequests(data: unknown): FriendRequestRow[] {
  if (typeof data !== 'object' || data === null) return [];
  const list = (data as Record<string, unknown>).requests;
  if (!Array.isArray(list)) return [];
  return list
    .filter((r): r is Record<string, unknown> => typeof r === 'object' && r !== null)
    .map((r) => ({ name: typeof r.name === 'string' ? r.name : '' }))
    .filter((r) => r.name !== '');
}

async function onFetchRequests() {
  fetching.value = true;
  try {
    const outcome = await store.manualExecute('get_friend_requests', {});
    if (outcome.ok) {
      requests.value = parseRequests(outcome.data);
      MessagePlugin.success(`拉取成功：${requests.value.length} 条申请`);
    } else {
      MessagePlugin.error(`拉取失败：${outcome.error}`);
    }
  } finally {
    fetching.value = false;
  }
}

function openAccept(name: string) {
  acceptName.value = name;
  acceptRemark.value = '';
  acceptTags.value = '';
  acceptVisible.value = true;
}

function onIgnore(name: string) {
  requests.value = requests.value.filter((r) => r.name !== name);
  MessagePlugin.info(`已忽略：${name}（仅从列表移除，不影响微信侧）`);
}

async function onAccept() {
  accepting.value = true;
  try {
    const outcome = await store.manualExecute('accept_friend', {
      name: acceptName.value,
      remark: acceptRemark.value.trim(),
      tags: acceptTags.value
        .split(/[,，]/)
        .map((t) => t.trim())
        .filter((t) => t !== ''),
    });
    if (outcome.ok) {
      MessagePlugin.success(`已通过好友申请：${acceptName.value}`);
      requests.value = requests.value.filter((r) => r.name !== acceptName.value);
      acceptVisible.value = false;
    } else {
      MessagePlugin.error(`通过失败：${outcome.error}`);
    }
  } finally {
    accepting.value = false;
  }
}

function errText(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}
</script>

<style scoped>
.add-row {
  display: flex;
  gap: 8px;
  margin-bottom: 12px;
}
.block {
  margin-top: 16px;
}
</style>
