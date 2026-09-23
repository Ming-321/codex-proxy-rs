<script setup lang="ts">
import type { CarQuotaSettings } from '@/api/modules/account-groups'
import { computed, onMounted, reactive } from 'vue'
import { getCarQuotaSettings, saveCarQuotaSettings } from '@/api/modules/account-groups'
import BaseButton from '@/components/base/BaseButton.vue'
import BaseCard from '@/components/base/BaseCard.vue'
import BaseFormItem from '@/components/base/BaseForm/FormItem.vue'
import BaseForm from '@/components/base/BaseForm/index.vue'
import BaseInput from '@/components/base/BaseInput.vue'
import BaseSwitch from '@/components/base/BaseSwitch.vue'
import { toast } from '@/components/base/BaseToast'
import { useAsyncAction } from '@/composables/useAsyncAction'

const action = useAsyncAction()
const busy = action.loading
const form = reactive({
  automaticUpdates: false,
  publishIntervalHours: '6',
  outsideUsageProtection: true,
  minimumSamplePercent: '10',
  estimateWeightPercent: '30',
  minimumChangePercent: '5',
  maximumAdjustmentPercent: '10',
  abnormalChangePercent: '30',
})

const percentFields = [
  'minimumSamplePercent',
  'estimateWeightPercent',
  'minimumChangePercent',
  'maximumAdjustmentPercent',
  'abnormalChangePercent',
] as const
const valid = computed(() => {
  const hours = Number(form.publishIntervalHours)
  return Number.isFinite(hours) && hours >= 5 / 60 && hours <= 720
    && percentFields.every((field) => {
      const value = Number(form[field])
      return Number.isFinite(value) && value >= (field === 'minimumChangePercent' ? 0 : 0.001) && value <= 100
    })
})

function apply(settings: CarQuotaSettings) {
  form.automaticUpdates = settings.automaticUpdates
  form.publishIntervalHours = String(settings.publishIntervalSeconds / 3600)
  form.outsideUsageProtection = settings.outsideUsageProtection
  for (const field of percentFields)
    form[field] = String(settings[field])
}

async function load() {
  await action.run(async () => apply(await getCarQuotaSettings()))
}

async function save() {
  if (!valid.value)
    return
  await action.run(async () => {
    const settings = await saveCarQuotaSettings({
      automaticUpdates: form.automaticUpdates,
      publishIntervalSeconds: Math.round(Number(form.publishIntervalHours) * 3600),
      outsideUsageProtection: form.outsideUsageProtection,
      minimumSamplePercent: Number(form.minimumSamplePercent),
      estimateWeightPercent: Number(form.estimateWeightPercent),
      minimumChangePercent: Number(form.minimumChangePercent),
      maximumAdjustmentPercent: Number(form.maximumAdjustmentPercent),
      abnormalChangePercent: Number(form.abnormalChangePercent),
    })
    apply(settings)
    toast.success('账号周期估算设置已保存')
  })
}

onMounted(load)
</script>

<template>
  <BaseCard title="拼车自动额度策略" description="各车是否自动调整，在分组管理中分别选择，这里的稳定参数全局共用">
    <BaseForm class="max-w-6xl sm:grid-cols-2">
      <p class="col-span-full text-cp-xs text-cp-text-secondary">
        新建时的默认选项不会改变已保存的拼车模式，自动管理需确认初始总额度后生效
      </p>
      <BaseSwitch v-model="form.automaticUpdates" class="col-span-full justify-self-start" label="新建拼车默认选择自动管理" show-label />
      <BaseFormItem label="最短发布间隔" description="即使获得新样本，也不会比此间隔更频繁地调整 seat 周期额度">
        <BaseInput v-model="form.publishIntervalHours" type="number" min="0.083333" max="720" step="any" aria-label="估算最短发布间隔" :disabled="busy">
          <template #suffix>
            <span class="text-cp-sm">小时</span>
          </template>
        </BaseInput>
      </BaseFormItem>
      <div class="flex items-end pb-2">
        <BaseSwitch v-model="form.outsideUsageProtection" label="减轻账号直登等外部用量影响" show-label :disabled="busy" />
      </div>

      <details class="col-span-full rounded-cp bg-cp-fill-quaternary p-4">
        <summary class="cursor-pointer text-cp-sm font-bold text-cp-text">
          高级稳定参数
        </summary>
        <div class="mt-4 grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
          <BaseFormItem label="最小有效样本" description="周期消耗达到此比例后才参与发布">
            <BaseInput v-model="form.minimumSamplePercent" type="number" min="0.001" max="100" step="any" aria-label="最小有效样本百分比">
              <template #suffix>
                <span class="text-cp-sm">%</span>
              </template>
            </BaseInput>
          </BaseFormItem>
          <BaseFormItem label="新估算权重" description="新样本在融合容量中所占比例">
            <BaseInput v-model="form.estimateWeightPercent" type="number" min="0.001" max="100" step="any" aria-label="新估算权重百分比">
              <template #suffix>
                <span class="text-cp-sm">%</span>
              </template>
            </BaseInput>
          </BaseFormItem>
          <BaseFormItem label="最小变化" description="变化不足时保持当前已发布容量">
            <BaseInput v-model="form.minimumChangePercent" type="number" min="0" max="100" step="any" aria-label="最小变化百分比">
              <template #suffix>
                <span class="text-cp-sm">%</span>
              </template>
            </BaseInput>
          </BaseFormItem>
          <BaseFormItem label="单次最大调整" description="限制每次发布的上调或下调幅度">
            <BaseInput v-model="form.maximumAdjustmentPercent" type="number" min="0.001" max="100" step="any" aria-label="单次最大调整百分比">
              <template #suffix>
                <span class="text-cp-sm">%</span>
              </template>
            </BaseInput>
          </BaseFormItem>
          <BaseFormItem label="异常变化阈值" description="超过阈值时等待下一批独立样本确认">
            <BaseInput v-model="form.abnormalChangePercent" type="number" min="0.001" max="100" step="any" aria-label="异常变化阈值百分比">
              <template #suffix>
                <span class="text-cp-sm">%</span>
              </template>
            </BaseInput>
          </BaseFormItem>
        </div>
      </details>
      <div class="col-span-full flex justify-end">
        <BaseButton variant="primary" :loading="busy" :disabled="!valid" @click="save">
          保存估算设置
        </BaseButton>
      </div>
    </BaseForm>
  </BaseCard>
</template>
