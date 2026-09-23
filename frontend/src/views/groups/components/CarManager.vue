<script setup lang="ts">
import type { AccountGroup } from '@/api'
import { shallowRef } from 'vue'
import BaseButton from '@/components/base/BaseButton.vue'
import BaseCard from '@/components/base/BaseCard.vue'
import BaseColorPicker from '@/components/base/BaseColorPicker/index.vue'
import BaseConfirmModal from '@/components/base/BaseConfirmModal.vue'
import BaseFormItem from '@/components/base/BaseForm/FormItem.vue'
import BaseInput from '@/components/base/BaseInput.vue'
import BaseSelect from '@/components/base/BaseSelect.vue'
import BaseSwitch from '@/components/base/BaseSwitch.vue'
import { toast } from '@/components/base/BaseToast'
import ClientProfileEditor from '@/components/client-profile/ClientProfileEditor.vue'
import XaiClientProfileEditor from '@/components/client-profile/XaiClientProfileEditor.vue'
import { formatDateTime } from '@/utils/date'
import { useCarManagement } from '../composables/useCarManagement'
import { ACCOUNT_GROUP_COLOR_PRESETS } from '../constants'

const props = defineProps<{ group: AccountGroup | null }>()
const emit = defineEmits<{ close: [], changed: [] }>()
const confirmCancel = shallowRef(false)
const {
  draft,
  loading,
  saving,
  error,
  account,
  accountOptions,
  keys,
  originalSeats,
  quota,
  people,
  joinChoices,
  planHint,
  totalTouched,
  needsInitial,
  automatic,
  capacity,
  allocated,
  remaining,
  availableKeys,
  preview,
  saved,
  secrets,
  resizeSeats,
  addKey,
  joinKey,
  share,
  seatLimit,
  beginPreview,
  save,
  revealCreated,
  load,
} = useCarManagement(props.group)
const policies = [{ label: '手动额度与窗口', value: 'manual' }, { label: '跟随账号周期，手动额度', value: 'cycle' }, { label: '自动估算并按份额分配', value: 'automatic' }]
const allocations = [{ label: '几人车均分', value: 'equal' }, { label: '自定义份额', value: 'custom' }]
function keysBelongToSeat(keyId: string, seatId: string) {
  return keys.value.some(key => key.id === keyId && key.seatId === seatId)
}
function money(value: number | string) {
  return Number(value).toLocaleString('en-US', { maximumFractionDigits: 2 })
}
function beforeShare(seatId: string) {
  const seat = originalSeats.value.find(item => item.id === seatId)
  if (!seat || !quota.value)
    return '新建'
  const ratio = quota.value.allocation === 'equal' ? 1 / originalSeats.value.length : Number(seat.weight) / Number(quota.value.totalWeight)
  return `${quota.value.allocation === 'equal' ? `1/${originalSeats.value.length}` : `${Number(seat.weight)} 份`} · ${(ratio * 100).toFixed(1)}%`
}
function close() {
  if (saved.value) {
    emit('changed')
    emit('close')
  }
  else if (loading.value) {
    emit('close')
  }
  else {
    confirmCancel.value = true
  }
}
async function copy(value: string) {
  await navigator.clipboard.writeText(value)
  toast.success('Key 已复制')
}
</script>

<template>
  <BaseConfirmModal v-model="confirmCancel" title="放弃本次修改" description="尚未保存的修改将丢弃，现有拼车配置保持不变" confirm-text="放弃修改" @confirm="emit('close')" />
  <div class="flex h-full min-h-0 flex-col gap-4 overflow-y-auto pb-6">
    <div class="sticky top-0 z-10 flex shrink-0 flex-wrap items-center justify-between gap-3 bg-cp-bg-layout py-3">
      <div>
        <h2 class="m-0 text-cp-lg font-bold text-cp-text">
          {{ group ? `${group.name} · 拼车管理` : '创建拼车分组' }}
        </h2>
        <p class="mt-1 mb-0 text-cp-sm text-cp-text-secondary">
          账号下按车位管理使用者，每个车位可持有多个客户端 Key
        </p>
      </div>
      <div class="flex gap-2">
        <BaseButton :disabled="saving" @click="close">
          {{ saved ? '返回分组' : '取消' }}
        </BaseButton>
        <BaseButton v-if="!saved && !preview" variant="primary" :disabled="loading || saving" @click="beginPreview">
          预览并保存
        </BaseButton>
      </div>
    </div>

    <p v-if="error" role="alert" class="rounded-cp bg-cp-error-container p-3 text-cp-error-text">
      {{ error }}
    </p>
    <p v-if="loading" class="text-cp-text-secondary">
      正在读取账号与共享配置…
    </p>
    <div v-else-if="saved" class="grid gap-4">
      <BaseCard title="拼车配置已保存">
        <p class="text-cp-sm text-cp-text-secondary">
          所有修改已一起生效，原有已用费用保留
        </p>
        <div v-for="item in secrets" :key="item.id" class="mt-4 grid gap-2">
          <strong>{{ item.name }}</strong>
          <div class="flex gap-2">
            <BaseInput :model-value="item.value" readonly aria-label="新 Key 凭据" />
            <BaseButton v-if="item.value" @click="copy(item.value)">
              复制
            </BaseButton>
          </div>
        </div>
        <BaseButton v-if="secrets.some(item => !item.value)" class="mt-3" @click="revealCreated">
          重试读取新 Key
        </BaseButton>
      </BaseCard>
    </div>
    <template v-else>
      <BaseCard v-if="preview" title="确认本次调整">
        <p class="text-cp-sm text-cp-text-secondary">
          {{ draft.name }}，{{ account?.name }}，{{ draft.seats.length }} 个车位
          所有修改一起生效，已用费用不清零
        </p>
        <p v-if="!group?.isCar" class="text-cp-sm text-cp-warning-text">
          保存后账号由该拼车独占，普通 Key 将不能再访问它，请确认需要继续使用的 Key 已加入车位
        </p>
        <p v-if="quota?.quotaPolicy !== draft.quotaPolicy" class="text-cp-sm text-cp-warning-text">
          额度模式将变更为 {{ policies.find(item => item.value === draft.quotaPolicy)?.label }}
          {{ draft.quotaPolicy === 'manual' ? '将按原窗口起点衔接 168 小时周窗并重新核对账本，不清空历史费用' : '账号周期需可信观测确认，日额度继续独立生效' }}
        </p>
        <p class="text-cp-sm text-cp-text-secondary">
          总份额 {{ quota?.totalWeight ?? '新建' }} → {{ draft.totalWeight }}，未分配 {{ remaining.toFixed(1) }} 份{{ automatic ? `，保留额度 $${money(capacity * remaining / Number(draft.totalWeight))}` : '' }}
        </p>
        <div class="overflow-x-auto">
          <table class="w-full text-left text-cp-sm">
            <thead>
              <tr class="text-cp-text-secondary">
                <th class="py-2">
                  车位
                </th><th>份额 / 占比</th><th>周期额度</th><th>已用</th><th>并发 / RPM</th><th>Key</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="seat in draft.seats" :key="seat.id">
                <td class="py-3 pr-3">
                  {{ seat.name }}{{ seat.enabled ? '' : '（停用）' }}
                </td>
                <td class="pr-3">
                  {{ beforeShare(seat.id) }} → {{ draft.allocation === 'equal' ? `1/${draft.seats.length}` : `${seat.weight} 份` }} · {{ (share(seat) * 100).toFixed(1) }}%
                </td>
                <td class="pr-3">
                  {{ originalSeats.find(item => item.id === seat.id)?.weeklyLimitUsd ?? '新建' }} → {{ money(seatLimit(seat)) }}
                </td>
                <td class="pr-3">
                  {{ money(originalSeats.find(item => item.id === seat.id)?.weeklyUsedUsd ?? 0) }}
                </td>
                <td class="pr-3">
                  {{ originalSeats.find(item => item.id === seat.id)?.maxConcurrency ?? '新建' }} / {{ originalSeats.find(item => item.id === seat.id)?.requestsPerMinute ?? '新建' }} → {{ seat.maxConcurrency }} / {{ seat.requestsPerMinute || '不限' }}
                </td>
                <td>
                  <div v-for="key in seat.keys" :key="key.id">
                    {{ key.name }}{{ key.revoke ? '（撤销）' : key.create ? '（新增）' : key.enabled ? '' : '（停用）' }} · {{ key.openaiClientProfileOverride ? `${key.openaiClientProfileOverride.platform} / ${key.openaiClientProfileOverride.client}` : key.xaiClientProfileOverride ? '独立身份' : '继承默认身份' }}
                  </div>
                  <span v-if="!seat.keys.length">空车位</span>
                </td>
              </tr>
            </tbody>
          </table>
        </div>
        <div class="mt-4 flex justify-end gap-2">
          <BaseButton :disabled="saving" @click="preview = false">
            继续编辑
          </BaseButton>
          <BaseButton variant="primary" :loading="saving" @click="save">
            确认并统一保存
          </BaseButton>
        </div>
      </BaseCard>

      <fieldset :disabled="saving || preview" class="m-0 grid min-w-0 gap-4 border-0 p-0">
        <BaseCard title="分组与账号">
          <div class="grid gap-4 sm:grid-cols-2">
            <BaseFormItem label="分组名称" required>
              <BaseInput v-model="draft.name" aria-label="分组名称" />
            </BaseFormItem>
            <BaseFormItem label="绑定账号" required :description="group?.isCar ? '已有车位后不能换绑账号' : '保存时将该账号纳入拼车分组'">
              <BaseSelect v-model="draft.accountId" :options="accountOptions" :disabled="Boolean(group?.isCar)" aria-label="绑定账号" />
            </BaseFormItem>
            <BaseFormItem label="说明">
              <BaseInput :model-value="draft.description ?? ''" aria-label="分组说明" @update:model-value="draft.description = $event || null" />
            </BaseFormItem>
            <BaseFormItem label="分组颜色">
              <BaseColorPicker v-model="draft.color" label="选择分组颜色" :presets="ACCOUNT_GROUP_COLOR_PRESETS" :disabled="saving || preview" />
            </BaseFormItem>
            <div class="flex flex-wrap items-center gap-6">
              <BaseSwitch v-model="draft.enabled" label="启用分组" show-label /><BaseSwitch v-model="draft.disableFast" label="关闭 Fast" show-label />
            </div>
          </div>
        </BaseCard>

        <BaseCard title="额度与分配">
          <div class="grid gap-4 sm:grid-cols-2">
            <BaseFormItem label="额度模式">
              <BaseSelect v-model="draft.quotaPolicy" :options="policies" aria-label="额度模式" />
            </BaseFormItem>
            <BaseFormItem label="分配方式">
              <BaseSelect v-model="draft.allocation" :options="allocations" aria-label="分配方式" />
            </BaseFormItem>
            <BaseFormItem label="总份额" :description="group?.isCar ? '已保存的份额不会随套餐刷新改变' : planHint">
              <div class="flex gap-2">
                <BaseInput v-model="draft.totalWeight" type="number" min="0.1" step="0.1" aria-label="总份额" @update:model-value="totalTouched = true" /><BaseButton v-for="value in ['20', '5', '1']" :key="value" @click="draft.totalWeight = value; totalTouched = true">
                  {{ value }}
                </BaseButton>
              </div>
            </BaseFormItem>
            <BaseFormItem :label="draft.allocation === 'equal' ? '几人车' : '车位数量'" description="空车位和停用车位仍保留份额">
              <div class="flex gap-2">
                <BaseInput :model-value="String(people)" type="number" min="1" max="100" step="1" aria-label="车位数量" @update:model-value="people = Number($event)" /><BaseButton @click="resizeSeats">
                  调整车位
                </BaseButton>
              </div>
            </BaseFormItem>
            <BaseFormItem v-if="needsInitial" label="确认初始周期总额度（美元）" description="可修改建议值，以保存预览中的分配为准，不按套餐倍率推算金额" required>
              <BaseInput :model-value="draft.initialCapacityUsd ?? ''" type="number" min="0.0000000001" step="any" aria-label="初始总额度" @update:model-value="draft.initialCapacityUsd = $event || null" />
            </BaseFormItem>
          </div>
          <p class="mt-4 mb-0 text-cp-sm text-cp-text-secondary">
            总份额 {{ draft.totalWeight }}，已分配 {{ allocated.toFixed(1) }}，未分配 {{ remaining.toFixed(1) }}
            {{ automatic ? '未分配额度保留，不自动分给其他车位' : '当前手动设置各车位金额，份额仅在自动模式下分配周期额度' }}
          </p>
          <div v-if="quota && group?.isCar" class="mt-3 grid gap-1 text-cp-sm text-cp-text-secondary">
            <span>运行状态：{{ quota.quotaPolicy === 'manual' ? '使用手动窗口' : quota.mode !== 'active' ? '等待账号周期确认' : quota.cycleEnd && new Date(quota.cycleEnd).getTime() <= Date.now() ? '周期已到期，等待新周期确认' : '账号周期已生效' }}</span>
            <span>当前生效总额度 ${{ money(quota.publishedCapacityUsd) }}，最新预测 {{ quota.predictedCapacityUsd === null ? '暂不可用' : `$${money(quota.predictedCapacityUsd)}` }}</span>
            <span>最近实际调整 {{ quota.publishedAt ? formatDateTime(quota.publishedAt) : '尚未调整' }}，周期结束 {{ quota.cycleEnd ? formatDateTime(quota.cycleEnd) : '等待账号观测' }}</span>
            <span v-if="quota.predictionReason">{{ quota.predictionReason }}</span>
          </div>
        </BaseCard>

        <BaseCard v-for="seat in draft.seats" :key="seat.id" :title="seat.name || '未命名车位'">
          <div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
            <BaseFormItem label="使用者 / 车位名称">
              <BaseInput v-model="seat.name" aria-label="车位名称" />
            </BaseFormItem>
            <BaseFormItem v-if="draft.allocation === 'custom'" label="车位份额">
              <BaseInput v-model="seat.weight" type="number" min="0.1" step="0.1" aria-label="车位份额" />
            </BaseFormItem>
            <BaseFormItem v-else label="均分份额">
              <p class="m-0 py-2 text-cp-sm">
                1/{{ draft.seats.length }}，约 {{ (Number(draft.totalWeight) / draft.seats.length).toFixed(1) }} 份
              </p>
            </BaseFormItem>
            <BaseFormItem label="共享并发">
              <BaseInput :model-value="String(seat.maxConcurrency)" type="number" min="1" step="1" aria-label="共享并发" @update:model-value="seat.maxConcurrency = Number($event)" />
            </BaseFormItem>
            <BaseFormItem label="共享 RPM" description="所有成员 Key 合计，0 为不限">
              <BaseInput :model-value="String(seat.requestsPerMinute)" type="number" min="0" step="1" aria-label="共享 RPM" @update:model-value="seat.requestsPerMinute = Number($event)" />
            </BaseFormItem>
            <BaseFormItem label="日额度（美元）" description="0 为不限，日窗口保持原规则">
              <BaseInput v-model="seat.dailyLimitUsd" type="number" min="0" step="any" aria-label="日额度" />
            </BaseFormItem>
            <BaseFormItem v-if="!automatic" label="周 / 账号周期额度（美元）" description="0 为不限">
              <BaseInput v-model="seat.weeklyLimitUsd" type="number" min="0" step="any" aria-label="周期额度" />
            </BaseFormItem>
            <div class="flex items-center">
              <BaseSwitch v-model="seat.enabled" label="启用车位" show-label />
            </div>
          </div>
          <p class="mt-3 text-cp-sm text-cp-text-secondary">
            占比 {{ (share(seat) * 100).toFixed(1) }}%{{ automatic ? `，分配周期额度 $${money(seatLimit(seat))}` : '' }}，已用 ${{ money(originalSeats.find(item => item.id === seat.id)?.weeklyUsedUsd ?? 0) }}{{ seat.enabled ? '' : '，停用不释放份额或清空费用' }}
          </p>

          <div v-for="key in seat.keys" :key="key.id" class="mt-3 rounded-cp bg-cp-fill-quaternary p-3">
            <div class="flex flex-wrap items-center gap-3">
              <BaseInput v-model="key.name" class="min-w-40 flex-1" aria-label="Key 名称" :disabled="key.revoke" />
              <BaseSwitch v-model="key.enabled" label="启用 Key" show-label :disabled="key.revoke" />
              <BaseButton v-if="key.create || !keysBelongToSeat(key.id, seat.id)" @click="seat.keys.splice(seat.keys.indexOf(key), 1)">
                移出草稿
              </BaseButton>
              <BaseSwitch v-else v-model="key.revoke" label="撤销 Key" show-label />
            </div>
            <p v-if="key.revoke" class="mb-0 text-cp-xs text-cp-warning-text">
              保存后该 Key 永久失效，历史费用保留
            </p>
            <details v-else class="mt-3">
              <summary class="cursor-pointer text-cp-sm text-cp-text-secondary">
                客户端身份 {{ key.openaiClientProfileOverride ? `${key.openaiClientProfileOverride.client} / ${key.openaiClientProfileOverride.platform}` : '继承默认' }}
              </summary>
              <div class="mt-3">
                <XaiClientProfileEditor v-if="account?.provider === 'xai'" v-model="key.xaiClientProfileOverride" allow-inherit /><ClientProfileEditor v-else v-model="key.openaiClientProfileOverride" allow-inherit />
              </div>
            </details>
          </div>
          <p v-if="!seat.keys.length" class="text-cp-sm text-cp-text-tertiary">
            尚未添加 Key，车位份额已保留
          </p>
          <div class="mt-4 flex flex-wrap gap-2">
            <BaseButton @click="addKey(seat)">
              新增 Key
            </BaseButton>
            <BaseSelect v-model="joinChoices[seat.id]" class="min-w-48 flex-1" :options="availableKeys.map(key => ({ label: key.name, value: key.id }))" placeholder="选择已有独立 Key" aria-label="选择已有 Key" />
            <BaseButton :disabled="!joinChoices[seat.id]" @click="joinKey(seat)">
              纳入车位
            </BaseButton>
          </div>
        </BaseCard>
      </fieldset>
      <div v-if="!preview" class="flex justify-end gap-2">
        <BaseButton :disabled="saving" @click="close">
          取消
        </BaseButton>
        <BaseButton variant="primary" :disabled="loading || saving" @click="beginPreview">
          预览并保存
        </BaseButton>
      </div>
      <BaseButton v-if="error && !draft.expectedRevision" @click="load">
        重新读取配置
      </BaseButton>
    </template>
  </div>
</template>
