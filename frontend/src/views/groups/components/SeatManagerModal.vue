<script setup lang="ts">
import type { AccountGroup, ApiKey, CarQuotaState, Seat } from '@/api'
import { computed, ref, watch } from 'vue'
import { convertToCar, getApiKeys, getCarQuota, getSeats, joinSeat, saveCarWeights, saveSeat } from '@/api'
import BaseButton from '@/components/base/BaseButton.vue'
import BaseCheckbox from '@/components/base/BaseCheckbox.vue'
import BaseFormItem from '@/components/base/BaseForm/FormItem.vue'
import BaseInput from '@/components/base/BaseInput.vue'
import BaseModal from '@/components/base/BaseModal/index.vue'
import { useAsyncAction } from '@/composables/useAsyncAction'
import { formatDateTime } from '@/utils/date'

const props = defineProps<{ group: AccountGroup | null }>()
const emit = defineEmits<{ changed: [] }>()
const open = defineModel<boolean>({ default: false })
const action = useAsyncAction()
const busy = action.loading
const isCar = ref(false)
const seats = ref<Seat[]>([])
const quota = ref<CarQuotaState | null>(null)
const totalWeight = ref('1')
const keys = ref<ApiKey[]>([])
const editing = ref(false)
const joining = ref<Seat | null>(null)
const selected = ref<string[]>([])
const form = ref(emptyForm())
const candidates = computed(() => keys.value.filter(key => !key.seatId))
const valid = computed(() => form.value.name.trim()
  && /^\d+$/.test(form.value.maxConcurrency) && Number(form.value.maxConcurrency) > 0
  && /^\d{1,10}(?:\.\d{1,2})?$/.test(form.value.weight) && Number(form.value.weight) > 0
  && [form.value.dailyLimitUsd, form.value.weeklyLimitUsd].every(value => /^\d{1,10}(?:\.\d{1,10})?$/.test(value)))
const weightValid = computed(() => /^\d{1,10}(?:\.\d{1,2})?$/.test(totalWeight.value) && Number(totalWeight.value) > 0)

function emptyForm() {
  return { id: '', name: '', enabled: true, maxConcurrency: '2', weight: '1', dailyLimitUsd: '0', weeklyLimitUsd: '0' }
}
async function load() {
  if (!props.group)
    return
  if (isCar.value) {
    [seats.value, quota.value] = await Promise.all([getSeats(props.group.id), getCarQuota(props.group.id)])
    totalWeight.value = quota.value.totalWeight
  }
  else {
    seats.value = []
    quota.value = null
  }
  const all: ApiKey[] = []
  let cursor: string | undefined
  do {
    const page = await getApiKeys({ limit: 100, cursor })
    all.push(...page.items)
    cursor = page.nextCursor ?? undefined
  } while (cursor)
  keys.value = all
}
watch(open, (value) => {
  if (!value)
    return
  isCar.value = props.group?.isCar ?? false
  editing.value = false
  joining.value = null
  seats.value = []
  keys.value = []
  void action.run(load)
})
function edit(seat?: Seat) {
  joining.value = null
  form.value = seat
    ? { id: seat.id, name: seat.name, enabled: seat.enabled, maxConcurrency: String(seat.maxConcurrency), weight: seat.weight, dailyLimitUsd: seat.dailyLimitUsd, weeklyLimitUsd: seat.weeklyLimitUsd }
    : emptyForm()
  editing.value = true
}
async function convert() {
  if (!props.group)
    return
  await action.run(async () => {
    await convertToCar(props.group!.id)
    isCar.value = true
    emit('changed')
    await load()
  })
}
async function save() {
  if (!valid.value || !props.group)
    return
  await action.run(async () => {
    await saveSeat({ ...form.value, id: form.value.id || undefined, groupId: props.group!.id, name: form.value.name.trim(), maxConcurrency: Number(form.value.maxConcurrency) })
    editing.value = false
    emit('changed')
    await load()
  })
}
async function toggle(seat: Seat) {
  await action.run(async () => {
    const { id, groupId, name, maxConcurrency, weight, dailyLimitUsd, weeklyLimitUsd } = seat
    await saveSeat({ id, groupId, name, maxConcurrency, weight, dailyLimitUsd, weeklyLimitUsd, enabled: !seat.enabled })
    await load()
    emit('changed')
  })
}
async function saveWeights() {
  if (!props.group || !weightValid.value)
    return
  await action.run(async () => {
    await saveCarWeights(props.group!.id, totalWeight.value)
    await load()
    emit('changed')
  })
}
function choose(seat: Seat) {
  editing.value = false
  joining.value = seat
  selected.value = []
}
async function join() {
  if (!joining.value || !selected.value.length)
    return
  await action.run(async () => {
    await joinSeat(joining.value!.id, selected.value)
    joining.value = null
    await load()
    emit('changed')
  })
}
function select(id: string, checked: boolean) {
  selected.value = checked ? [...selected.value, id] : selected.value.filter(value => value !== id)
}
function amount(value: string) {
  return Number(value).toLocaleString('en-US', { maximumFractionDigits: 4 })
}
function reset(value: string | null) {
  return value ? formatDateTime(value, '—', 'Asia/Shanghai') : '首次使用后起算'
}
function remaining(used: string, limit: string) {
  return Number(limit) === 0 ? '不限额' : `$${amount(String(Math.max(0, Number(limit) - Number(used))))}`
}
function modeLabel(mode: CarQuotaState['mode']) {
  return mode === 'active' ? '已按账号周期运行' : mode === 'waiting' ? '等待账号周期确认' : '沿用原周窗口，确认后自动切换'
}
</script>

<template>
  <BaseModal v-model="open" :title="`${group?.name ?? ''} · car / seat`" size="lg" :dismissible="!busy">
    <div v-if="!isCar" class="grid gap-4 text-cp-sm text-cp-text-secondary">
      <p>car 独占一个账号，成员通过 seat 使用账号，共享费用限额与并发</p>
      <p>转换后，普通 Key 将不能使用此账号，请随后将已有 Key 加入对应 seat</p>
      <p>当前分组需恰好包含一个账号，且该账号未加入其他分组</p>
      <BaseButton variant="primary" :loading="busy" :disabled="group?.memberCount !== 1" @click="convert">
        转为 car
      </BaseButton>
    </div>
    <div v-else class="grid gap-5">
      <div v-if="quota" class="grid gap-3 rounded-xl border border-cp-border bg-cp-fill-quaternary p-4">
        <div class="flex flex-wrap items-start justify-between gap-3">
          <div>
            <strong class="text-cp-text">账号周期额度</strong>
            <p class="mt-1 text-cp-xs text-cp-text-secondary">
              {{ modeLabel(quota.mode) }} · 周期结束 {{ quota.cycleEnd ? reset(quota.cycleEnd) : '等待账号返回' }} · 更新于 {{ reset(quota.updatedAt) }}
            </p>
          </div>
          <div class="text-right text-cp-sm">
            <p>已发布 ${{ amount(quota.publishedCapacityUsd) }}</p>
            <p class="text-cp-xs text-cp-text-secondary">
              当前估算 {{ quota.predictedCapacityUsd ? `$${amount(quota.predictedCapacityUsd)}` : '暂无' }} · 账号已用 {{ quota.accountUsedPercent == null ? '暂无' : `${quota.accountUsedPercent.toFixed(1)}%` }}
            </p>
          </div>
        </div>
        <p v-if="quota.predictionReason" class="text-cp-xs text-cp-text-secondary">
          {{ quota.predictionReason }}
        </p>
        <div class="flex flex-wrap items-end gap-3">
          <BaseFormItem class="min-w-48 flex-1" label="总权重" description="seat 权重之和不能超过总权重，未分配部分保留">
            <BaseInput v-model="totalWeight" type="number" min="0.01" step="0.01" aria-label="car 总权重" :disabled="busy" />
          </BaseFormItem>
          <BaseButton variant="secondary" :loading="busy" :disabled="!weightValid" @click="saveWeights">
            保存总权重
          </BaseButton>
        </div>
      </div>
      <div class="flex flex-wrap items-center justify-between gap-3">
        <p class="text-cp-sm text-cp-text-secondary">
          同一 seat 下的 Key 共用一份预算与并发上限，客户端身份和 RPM 各自保留
        </p>
        <BaseButton v-if="!editing && !joining" variant="primary" size="sm" :disabled="busy" @click="edit()">
          创建 seat
        </BaseButton>
      </div>
      <div v-for="seat in seats" :key="seat.id" class="grid gap-3 rounded-xl border border-cp-border p-4">
        <div class="flex flex-wrap items-center justify-between gap-2">
          <strong class="text-cp-text">{{ seat.name }} <span class="text-cp-xs font-normal text-cp-text-secondary">{{ seat.enabled ? '已启用' : '已禁用' }} · 权重 {{ seat.weight }} · 并发 {{ seat.maxConcurrency }} · {{ seat.keyCount }} 个 Key</span></strong>
          <div class="flex gap-2">
            <BaseButton variant="secondary" size="sm" :disabled="busy" @click="edit(seat)">
              编辑
            </BaseButton>
            <BaseButton variant="secondary" size="sm" :disabled="busy" @click="choose(seat)">
              加入 Key
            </BaseButton>
            <BaseButton variant="secondary" size="sm" :disabled="busy" @click="toggle(seat)">
              {{ seat.enabled ? '禁用' : '启用' }}
            </BaseButton>
          </div>
        </div>
        <div class="grid gap-3 text-cp-sm sm:grid-cols-2">
          <div>
            日已用 ${{ amount(seat.dailyUsedUsd) }} / {{ Number(seat.dailyLimitUsd) ? `$${amount(seat.dailyLimitUsd)}` : '不限额' }}<p class="text-cp-xs text-cp-text-secondary">
              剩余 {{ remaining(seat.dailyUsedUsd, seat.dailyLimitUsd) }} · {{ reset(seat.dailyResetsAt) }}
            </p>
          </div>
          <div>
            账号周期已用 ${{ amount(seat.weeklyUsedUsd) }} / {{ Number(seat.weeklyLimitUsd) ? `$${amount(seat.weeklyLimitUsd)}` : '不限额' }}<p class="text-cp-xs text-cp-text-secondary">
              剩余 {{ remaining(seat.weeklyUsedUsd, seat.weeklyLimitUsd) }} · {{ reset(seat.weeklyResetsAt) }}
            </p>
          </div>
        </div>
        <p class="text-cp-xs text-cp-text-secondary">
          {{ keys.filter(key => key.seatId === seat.id).map(key => key.name).join('、') || '尚未加入 Key' }}
        </p>
      </div>
      <p v-if="!seats.length && !busy" class="text-cp-sm text-cp-text-secondary">
        尚无 seat，请先创建
      </p>
      <div v-if="editing" class="grid gap-4 rounded-xl bg-cp-fill-tertiary p-4">
        <strong>{{ form.id ? '编辑 seat' : '创建 seat' }}</strong>
        <BaseFormItem label="seat 名称">
          <BaseInput v-model="form.name" aria-label="seat 名称" :disabled="busy" />
        </BaseFormItem>
        <div class="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
          <BaseFormItem label="共享并发">
            <BaseInput v-model="form.maxConcurrency" type="number" min="1" step="1" aria-label="seat 共享并发" :disabled="busy" />
          </BaseFormItem>
          <BaseFormItem label="额度权重">
            <BaseInput v-model="form.weight" type="number" min="0.01" step="0.01" aria-label="seat 额度权重" :disabled="busy" />
          </BaseFormItem>
          <BaseFormItem label="日限额（美元）">
            <BaseInput v-model="form.dailyLimitUsd" type="number" min="0" step="any" aria-label="seat 日限额" :disabled="busy" />
          </BaseFormItem>
          <BaseFormItem :label="quota?.mode === 'active' ? '账号周期限额（自动）' : '原周限额（过渡）'">
            <BaseInput v-model="form.weeklyLimitUsd" type="number" min="0" step="any" aria-label="seat 周期限额" :disabled="busy || quota?.mode === 'active'" />
          </BaseFormItem>
        </div>
        <p class="text-cp-xs text-cp-text-secondary">
          日限额按北京时间零点重置；账号周期限额由已发布容量和权重自动计算，费用在请求结束后计入
        </p>
        <div class="flex justify-end gap-2">
          <BaseButton variant="secondary" :disabled="busy" @click="editing = false">
            取消
          </BaseButton><BaseButton variant="primary" :loading="busy" :disabled="!valid" @click="save">
            保存
          </BaseButton>
        </div>
      </div>
      <div v-if="joining" class="grid gap-4 rounded-xl bg-cp-fill-tertiary p-4">
        <strong>加入 {{ joining.name }}</strong>
        <p class="text-cp-sm text-cp-text-secondary">
          承接当前已用费用，限额采用 seat 设置，不相加<br>加入后不能转移到其他 seat 或恢复为独立 Key，有在途请求时请稍后重试
        </p>
        <div class="grid max-h-64 gap-3 overflow-y-auto">
          <BaseCheckbox v-for="key in candidates" :key="key.id" show-label :model-value="selected.includes(key.id)" :label="key.name" :disabled="busy" @update:model-value="select(key.id, $event)" />
          <p v-if="!candidates.length" class="text-cp-sm text-cp-text-secondary">
            没有可加入的独立 Key
          </p>
        </div>
        <div class="flex justify-end gap-2">
          <BaseButton variant="secondary" :disabled="busy" @click="joining = null">
            取消
          </BaseButton><BaseButton variant="primary" :loading="busy" :disabled="!selected.length" @click="join">
            确认加入
          </BaseButton>
        </div>
      </div>
    </div>
    <template #footer>
      <BaseButton variant="secondary" :disabled="busy" @click="open = false">
        关闭
      </BaseButton>
    </template>
  </BaseModal>
</template>
