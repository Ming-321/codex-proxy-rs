<script setup lang="ts">
import type { ApiKey } from '@/api'
import { BaseButton, BaseConfirmModal, BasePopover, toast } from '@codex-proxy/ui'
import { computed, shallowRef } from 'vue'
import { releaseApiKeyWeeklyControl } from '@/api'
import { useAsyncAction } from '@/composables/useAsyncAction'
import { formatDateTime } from '@/utils/format'

const props = defineProps<{ apiKey: ApiKey }>()
const emit = defineEmits<{ released: [] }>()
const confirming = shallowRef(false)
const { loading, run } = useAsyncAction()
async function releaseControl() {
  await run(async () => {
    await releaseApiKeyWeeklyControl({ id: props.apiKey.id, expectedRevision: props.apiKey.weeklyControlRevision })
    confirming.value = false
    toast.success('已恢复固定七天周窗口，已用金额保持不变')
    emit('released')
  })
}
const windows = computed(() => [
  { label: '日', heading: '日用量', used: props.apiKey.dailyUsedUsd, limit: props.apiKey.dailyLimitUsd, reset: props.apiKey.dailyResetsAt },
  { label: '周', heading: '周用量', used: props.apiKey.weeklyUsedUsd, limit: props.apiKey.weeklyLimitUsd, reset: props.apiKey.weeklyResetsAt },
])
function amount(value: string) {
  // 列表最多显示两位小数，不补末尾零；明细保留原始金额的全部精度。
  return Number(value).toLocaleString('en-US', { maximumFractionDigits: 2 })
}
</script>

<template>
  <BasePopover class="w-full min-w-0" trigger="hover-click" placement="right" :hover-delay="240">
    <template #trigger="{ open }">
      <button
        type="button"
        class="grid w-full min-w-0 cursor-pointer gap-1 rounded-sm border-0 bg-transparent p-0 text-left text-xs tabular-nums outline-none focus-visible:ring-2 focus-visible:ring-cp-control-outline"
        :aria-label="`查看 ${apiKey.name} 的费用用量`"
        :aria-expanded="open"
        aria-haspopup="dialog"
      >
        <span v-for="window in windows" :key="window.label" class="flex min-w-0 items-center gap-1.5">
          <span class="shrink-0 text-cp-text-tertiary">{{ window.label }}</span>
          <span class="truncate" :class="Number(window.limit) > 0 && Number(window.used) >= Number(window.limit) ? 'text-cp-error' : 'text-cp-text'">
            ${{ amount(window.used) }} / {{ Number(window.limit) === 0 ? '∞' : `$${amount(window.limit)}` }}
          </span>
        </span>
        <span v-if="apiKey.weeklyController" :class="apiKey.weeklyWaiting ? 'text-cp-error' : 'text-cp-text-tertiary'">
          {{ apiKey.weeklyWaiting ? '等待账号窗口更新' : '周窗口由插件接管' }}
        </span>
      </button>
    </template>

    <section class="grid min-w-56 max-w-[calc(100vw-1rem)] gap-3 p-3" role="dialog" aria-label="费用用量详情（美元）">
      <div v-if="apiKey.weeklyController" class="grid gap-2 text-cp-xs">
        <span>{{ apiKey.weeklyWaiting ? '等待账号窗口更新，新请求已暂停' : '周窗口由插件接管' }}</span>
        <span class="break-all text-cp-text-tertiary">插件实例：{{ apiKey.weeklyController }}</span>
        <BaseButton variant="secondary" @click="confirming = true">解除周窗口接管</BaseButton>
      </div>
      <div v-for="window in windows" :key="window.label" class="grid gap-1">
        <div class="flex items-baseline justify-between gap-6 text-cp-sm">
          <span class="shrink-0 text-cp-text-secondary">{{ window.heading }}</span>
          <span class="min-w-0 break-all text-right font-mono tabular-nums">
            <span :class="Number(window.limit) > 0 && Number(window.used) >= Number(window.limit) ? 'text-cp-error' : 'text-cp-text'">${{ window.used }}</span>
            <span class="text-cp-text-tertiary"> / {{ Number(window.limit) === 0 ? '∞' : `$${window.limit}` }}</span>
          </span>
        </div>
        <div class="flex items-baseline justify-between gap-3 text-cp-xs text-cp-text-tertiary">
          <span class="shrink-0">{{ window.reset ? '重置（北京时间）' : '重置' }}</span>
          <time v-if="window.reset" :datetime="window.reset" class="text-right font-mono tabular-nums">{{ formatDateTime(window.reset, '—', 'Asia/Shanghai') }}</time>
          <span v-else>下次使用时确定</span>
        </div>
      </div>
    </section>
  </BasePopover>
  <BaseConfirmModal
    v-model="confirming"
    title="解除周窗口接管"
    description="保留已用金额和限额，恢复固定七天模式"
    confirm-text="确认解除"
    :loading="loading"
    @confirm="releaseControl"
  >
    <p>下次重置时间按今天北京时间零点起七天计算</p>
  </BaseConfirmModal>
</template>
