import type { Account, AccountGroup, ApiKey, CarKeyDraft, CarManagementDraft, CarQuotaState, CarSeatDraft, Seat } from '@/api'
import { toast } from '@codex-proxy/ui'
import { computed, nextTick, onMounted, ref, shallowRef, watch } from 'vue'
import { getAccountGroups, getAccounts, getApiKeys, getCarQuota, getCarQuotaSettings, getSeats, revealApiKey, saveCarManagement } from '@/api'
import { errorMessage } from '@/utils/async'
import { DEFAULT_ACCOUNT_GROUP_COLOR } from '../constants'

function id(prefix: string) {
  return `${prefix}_${crypto.randomUUID().replaceAll('-', '')}`
}

function keyDraft(key: ApiKey): CarKeyDraft {
  return {
    id: key.id,
    create: false,
    name: key.name,
    label: key.label,
    enabled: key.enabled,
    revoke: false,
    openaiClientProfileOverride: key.openaiClientProfileOverride,
    xaiClientProfileOverride: key.xaiClientProfileOverride,
  }
}

export function useCarManagement(group: AccountGroup | null) {
  const loading = shallowRef(true)
  const saving = shallowRef(false)
  const error = shallowRef('')
  const accounts = shallowRef<Account[]>([])
  const keys = shallowRef<ApiKey[]>([])
  const originalSeats = shallowRef<Seat[]>([])
  const quota = shallowRef<CarQuotaState | null>(null)
  const preview = shallowRef(false)
  const saved = shallowRef(false)
  const secrets = ref<{ id: string, name: string, value: string }[]>([])
  const pending = shallowRef<CarManagementDraft | null>(null)
  const people = ref(2)
  const joinChoices = ref<Record<string, string>>({})
  const totalTouched = shallowRef(false)
  const draft = ref<CarManagementDraft>({
    requestId: crypto.randomUUID(),
    expectedRevision: 0,
    quotaUpdatedAt: null,
    groupId: group?.id ?? id('grp'),
    create: !group,
    name: group?.name ?? '',
    description: group?.description ?? null,
    color: group?.color ?? DEFAULT_ACCOUNT_GROUP_COLOR,
    enabled: group?.enabled ?? true,
    disableFast: group?.disableFast ?? false,
    accountId: '',
    quotaPolicy: 'manual',
    allocation: 'equal',
    totalWeight: '20',
    initialCapacityUsd: null,
    seats: [],
  })
  const account = computed(() => accounts.value.find(item => item.id === draft.value.accountId))
  const accountOptions = computed(() => accounts.value.map(item => ({ label: `${item.name} · ${item.planTypeDisplay}`, value: item.id })))
  const needsInitial = computed(() => draft.value.quotaPolicy === 'automatic' && quota.value?.quotaPolicy !== 'automatic')
  const automatic = computed(() => draft.value.quotaPolicy === 'automatic')
  const allocated = computed(() => draft.value.allocation === 'equal'
    ? Number(draft.value.totalWeight)
    : draft.value.seats.reduce((sum, seat) => sum + Number(seat.weight), 0))
  const remaining = computed(() => Number(draft.value.totalWeight) - allocated.value)
  const capacity = computed(() => needsInitial.value ? Number(draft.value.initialCapacityUsd) : Number(quota.value?.publishedCapacityUsd ?? 0))
  const availableKeys = computed(() => keys.value.filter(key => !key.seatId && !draft.value.seats.some(seat => seat.keys.some(item => item.id === key.id))))
  const planHint = computed(() => {
    const plan = account.value?.planType
    if (plan === 'plus' || plan === 'team')
      return '已识别标准套餐，建议总份额 1'
    return '未能可靠识别套餐倍率，默认 20 份，可自行选择'
  })

  function newSeat(): CarSeatDraft {
    return { id: id('seat'), name: `车位 ${draft.value.seats.length + 1}`, enabled: true, maxConcurrency: 2, requestsPerMinute: 20, weight: '1', dailyLimitUsd: '0', weeklyLimitUsd: '0', keys: [] }
  }

  function resizeSeats() {
    const count = Number(people.value)
    if (!Number.isInteger(count) || count < 1 || count > 100) {
      error.value = '人数应为 1 至 100 的整数'
      return
    }
    if (count < draft.value.seats.length && draft.value.seats.slice(count).some(seat => originalSeats.value.some(old => old.id === seat.id) || seat.keys.some(key => !key.create))) {
      error.value = '已有车位不能删除，请保留或停用，并通过自定义份额调整分配'
      return
    }
    draft.value.seats.splice(count)
    while (draft.value.seats.length < count)
      draft.value.seats.push(newSeat())
    error.value = ''
  }

  function addKey(seat: CarSeatDraft) {
    seat.keys.push({ id: id('key'), create: true, name: `${seat.name} Key ${seat.keys.length + 1}`, label: null, enabled: true, revoke: false, openaiClientProfileOverride: null, xaiClientProfileOverride: null })
  }

  function joinKey(seat: CarSeatDraft) {
    const key = availableKeys.value.find(item => item.id === joinChoices.value[seat.id])
    if (key)
      seat.keys.push(keyDraft(key))
    joinChoices.value[seat.id] = ''
  }

  function share(seat: CarSeatDraft) {
    return draft.value.allocation === 'equal' ? 1 / draft.value.seats.length : Number(seat.weight) / Number(draft.value.totalWeight)
  }

  function seatLimit(seat: CarSeatDraft) {
    return automatic.value ? capacity.value * share(seat) : Number(seat.weeklyLimitUsd)
  }

  function beginPreview() {
    if (!draft.value.name.trim() || !draft.value.accountId || !draft.value.seats.length) {
      error.value = '请填写分组名称、绑定账号并设置车位'
      return
    }
    if (!(Number(draft.value.totalWeight) > 0) || remaining.value < -0.000001) {
      error.value = '总份额必须大于零，已分配份额不能超过总份额'
      return
    }
    if (needsInitial.value && !(capacity.value > 0)) {
      error.value = '请填写并确认自动管理的初始总额度'
      return
    }
    error.value = ''
    pending.value ??= JSON.parse(JSON.stringify({ ...draft.value, requestId: crypto.randomUUID(), initialCapacityUsd: needsInitial.value ? String(draft.value.initialCapacityUsd) : null, seats: draft.value.seats.map(seat => ({ ...seat, maxConcurrency: Number(seat.maxConcurrency), requestsPerMinute: Number(seat.requestsPerMinute), weight: draft.value.allocation === 'equal' ? '1' : String(seat.weight), dailyLimitUsd: String(seat.dailyLimitUsd), weeklyLimitUsd: String(seat.weeklyLimitUsd) })) })) as CarManagementDraft
    preview.value = true
  }

  async function revealCreated() {
    for (const item of secrets.value) {
      if (!item.value)
        item.value = (await revealApiKey({ id: item.id })).plaintextKey
    }
  }

  async function save() {
    if (!pending.value || saving.value)
      return
    saving.value = true
    error.value = ''
    try {
      const result = await saveCarManagement(pending.value)
      saved.value = true
      secrets.value = result.createdKeyIds.map(keyId => ({ id: keyId, name: draft.value.seats.flatMap(seat => seat.keys).find(key => key.id === keyId)?.name ?? '新 Key', value: '' }))
      toast.success('拼车配置已保存')
      await revealCreated()
    }
    catch (cause) {
      error.value = saved.value ? '配置已保存，读取新 Key 失败，可重试读取或在 API Key 页面获取' : errorMessage(cause)
    }
    finally {
      saving.value = false
    }
  }

  async function load() {
    loading.value = true
    error.value = ''
    try {
      const [groups, settings, state, seats] = await Promise.all([
        getAccountGroups({ page: 1, pageSize: 1 }),
        getCarQuotaSettings(),
        group ? getCarQuota(group.id) : Promise.resolve(null),
        group?.isCar ? getSeats(group.id) : Promise.resolve([]),
      ])
      const allAccounts: Account[] = []
      for (let page = 1; ; page++) {
        const result = await getAccounts({ page, pageSize: 200 })
        allAccounts.push(...result.items)
        if (page >= result.page.totalPages)
          break
      }
      const allKeys: ApiKey[] = []
      let cursor: string | undefined
      do {
        const result = await getApiKeys({ limit: 200, cursor })
        allKeys.push(...result.items)
        cursor = result.nextCursor ?? undefined
      } while (cursor)
      accounts.value = allAccounts
      keys.value = allKeys
      quota.value = state
      originalSeats.value = seats
      draft.value.expectedRevision = state?.configRevision ?? groups.configRevision
      draft.value.quotaUpdatedAt = group?.isCar ? state?.updatedAt ?? null : null
      draft.value.accountId = state?.accountId ?? ''
      draft.value.quotaPolicy = group?.isCar ? state!.quotaPolicy : settings.automaticUpdates ? 'automatic' : 'manual'
      draft.value.allocation = group?.isCar ? state!.allocation : 'equal'
      draft.value.totalWeight = group?.isCar ? state!.totalWeight : '20'
      draft.value.seats = seats.map(seat => ({ id: seat.id, name: seat.name, enabled: seat.enabled, maxConcurrency: seat.maxConcurrency, requestsPerMinute: seat.requestsPerMinute, weight: seat.weight, dailyLimitUsd: seat.dailyLimitUsd, weeklyLimitUsd: seat.weeklyLimitUsd, keys: allKeys.filter(key => key.seatId === seat.id).map(keyDraft) }))
      people.value = seats.length || 2
      if (!seats.length)
        resizeSeats()
    }
    catch (cause) {
      error.value = errorMessage(cause)
    }
    finally {
      // 等待草稿回填触发的监听完成，不能把读取旧配置当成用户切换分配方式。
      await nextTick()
      loading.value = false
    }
  }

  watch(() => draft.value.accountId, () => {
    if (!group?.isCar && !totalTouched.value)
      draft.value.totalWeight = ['plus', 'team'].includes(account.value?.planType ?? '') ? '1' : '20'
  })
  watch(() => draft.value.quotaPolicy, (policy) => {
    if (policy === 'automatic' && quota.value?.quotaPolicy !== 'automatic') {
      const values = draft.value.seats.map(seat => Number(seat.weeklyLimitUsd))
      draft.value.initialCapacityUsd = values.length && values.every(value => value > 0) ? String(values.reduce((a, b) => a + b, 0)) : null
    }
  })
  watch(() => draft.value.allocation, (mode, previous) => {
    if (loading.value || mode !== 'custom' || previous !== 'equal')
      return
    for (const seat of draft.value.seats)
      seat.weight = String(Math.floor(Number(draft.value.totalWeight) * 10 / draft.value.seats.length) / 10)
    toast.info('已转为一位小数，请核对未分配份额和各车位占比')
  })
  watch(draft, () => {
    pending.value = null
    preview.value = false
  }, { deep: true })
  onMounted(load)
  return { draft, loading, saving, error, accounts, account, accountOptions, keys, originalSeats, quota, people, joinChoices, planHint, totalTouched, needsInitial, automatic, allocated, remaining, capacity, availableKeys, preview, saved, secrets, resizeSeats, addKey, joinKey, share, seatLimit, beginPreview, save, revealCreated, load }
}
