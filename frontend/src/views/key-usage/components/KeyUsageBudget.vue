<script setup lang="ts">
import type { KeyUsageBudget, SeatKeyUsage } from '@/api/modules/key-usage'
import { Clock3, Gauge, Network } from '@lucide/vue'
import { computed } from 'vue'
import BaseCard from '@/components/base/BaseCard.vue'
import { useUiClock } from '@/composables/useUiClock'
import { keyUsageTime, money } from '../utils/format'

const props = defineProps<{ budget: KeyUsageBudget, seatKeys: SeatKeyUsage[] }>()
const now = useUiClock()
const waitingForCycle = computed(() => props.budget.accountCycle
  && (!props.budget.weeklyResetsAt || Date.parse(props.budget.weeklyResetsAt) <= now.value.getTime()))
const windows = computed(() => [
  { label: '今日额度', limit: props.budget.dailyLimitUsd, used: props.budget.dailyUsedUsd, reset: props.budget.dailyResetsAt },
  { label: props.budget.accountCycle ? '账号周期额度' : '周额度', limit: props.budget.weeklyLimitUsd, used: props.budget.weeklyUsedUsd, reset: props.budget.weeklyResetsAt },
].map(window => ({
  ...window,
  limited: Number(window.limit) > 0,
  remaining: Math.max(0, Number(window.limit) - Number(window.used)),
  percentage: Number(window.limit) > 0 ? Number(window.used) / Number(window.limit) * 100 : 0,
})))
</script>

<template>
  <BaseCard :title="budget.seatName ? `${budget.seatName} · 共享额度` : '额度概览'">
    <div class="flex flex-1 flex-col justify-between gap-6">
      <p v-if="waitingForCycle" role="status" class="text-cp-sm text-cp-warning">
        等待账号新周期确认，暂不接受新请求，确认后自动恢复
      </p>
      <div class="grid flex-1 gap-6 sm:grid-cols-2">
        <div v-for="window in windows" :key="window.label" class="flex min-w-0 flex-col justify-between gap-4">
          <div>
            <div class="text-cp-sm text-cp-text-secondary">
              {{ window.label }}
            </div>
            <div class="mt-3 flex flex-wrap items-baseline gap-1.5">
              <strong class="font-mono text-2xl text-cp-text">{{ window.limited ? money(window.remaining) : '不限额' }}</strong>
              <span v-if="window.limited" class="text-cp-xs text-cp-text-tertiary">剩余 / {{ money(window.limit) }}</span>
            </div>
          </div>
          <div>
            <div class="grid grid-cols-[repeat(20,minmax(0,1fr))] gap-0.75" :aria-label="window.limited ? `已用 ${window.percentage.toFixed(1)}%` : '不限额'">
              <span v-for="block in 20" :key="block" class="h-7 rounded-xs" :class="block <= Math.ceil(Math.min(100, window.percentage) / 5) ? (window.percentage >= 100 ? 'bg-cp-error' : 'bg-cp-success') : 'bg-cp-fill-secondary'" />
            </div>
            <div class="mt-2 flex justify-between gap-2 font-mono text-cp-xs text-cp-text-secondary">
              <span>已用 {{ money(window.used) }}</span><span v-if="window.limited">{{ window.percentage.toFixed(1) }}%</span>
            </div>
          </div>
          <div class="text-cp-xs leading-relaxed text-cp-text-tertiary">
            <span class="flex items-center gap-1.5"><Clock3 class="size-3" />重置时间</span>
            <span class="mt-1 block font-mono">{{ window.reset ? keyUsageTime(window.reset) : '首次使用后开始计时' }}</span>
          </div>
        </div>
      </div>
      <div class="grid grid-cols-2 gap-3">
        <div class="flex items-center justify-between gap-2 rounded-cp bg-cp-fill-quaternary p-3 text-cp-xs text-cp-text-secondary">
          <span class="flex items-center gap-1.5 leading-none"><Network class="size-3.5 shrink-0 -translate-y-px" />并发上限</span><strong class="font-mono">{{ budget.maxConcurrency || '∞' }}</strong>
        </div>
        <div class="flex items-center justify-between gap-2 rounded-cp bg-cp-fill-quaternary p-3 text-cp-xs text-cp-text-secondary">
          <span class="flex items-center gap-1.5 leading-none"><Gauge class="size-3.5 shrink-0 -translate-y-px" />每分钟请求</span><strong class="font-mono">{{ budget.requestsPerMinute || '∞' }}</strong>
        </div>
      </div>
      <div v-if="budget.seatName && seatKeys.length" class="border-t border-cp-border pt-4">
        <div class="mb-3 flex items-center justify-between gap-3">
          <strong class="text-cp-sm text-cp-text">seat 内 Key 用量</strong>
          <span class="text-cp-xs text-cp-text-tertiary">共享额度，分别统计</span>
        </div>
        <div class="grid gap-2">
          <div v-for="key in seatKeys" :key="key.id" class="grid grid-cols-2 gap-x-3 gap-y-2 rounded-cp bg-cp-fill-quaternary px-3 py-2 text-cp-xs sm:grid-cols-[minmax(0,1fr)_auto_auto] sm:items-center">
            <span class="col-span-2 min-w-0 break-words text-cp-text sm:col-span-1"><strong>{{ key.name }}</strong> <span class="font-mono text-cp-text-tertiary">{{ key.prefix }}</span> <span v-if="key.current" class="text-cp-primary">当前</span> <span v-if="key.revoked" class="text-cp-text-tertiary">已撤销</span></span>
            <span class="font-mono text-cp-text-secondary">今日 {{ money(key.dailyUsedUsd) }}</span>
            <span class="font-mono text-cp-text-secondary">{{ budget.accountCycle ? '周期' : '本周' }} {{ money(key.cycleUsedUsd) }}</span>
          </div>
        </div>
      </div>
    </div>
  </BaseCard>
</template>
