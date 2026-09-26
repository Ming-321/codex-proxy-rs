import type { OutboundProxyRecord, OutboundProxyTest } from '@/api'

export function effectiveProxyLocation(proxy: OutboundProxyRecord) {
  return proxy.autoLocation ? proxy.detectedLocation?.location : proxy.location
}

export function locationDetectionWarning(test: OutboundProxyTest | null | undefined) {
  if (test?.location.status === 'conflict')
    return 'IPv4 与 IPv6 的出口时区不一致，自动位置未生效'
  if (test?.location.status === 'failed')
    return test.location.message
  return ''
}
