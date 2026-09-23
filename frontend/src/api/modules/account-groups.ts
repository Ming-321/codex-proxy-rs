import type { RequestOptions } from '../request'
import request from '../request'

export interface AccountGroupRef {
  id: string
  name: string
  color: string
  enabled: boolean
}

export interface AccountGroupAccountSummary {
  available: number
  limited: number
  total: number
}

export interface AccountGroupCapacity {
  usedSlots: number | null
  totalSlots: number | null
}

export interface AccountGroupUsage {
  todayUsd: string
  retainedTotalUsd: string
}

export interface AccountGroup extends AccountGroupRef {
  isCar: boolean
  disableFast: boolean
  description: string | null
  memberCount: number
  providerCounts: Record<string, number>
  clientKeyCount: number
  accountSummary: AccountGroupAccountSummary
  capacity: AccountGroupCapacity
  usage: AccountGroupUsage
  createdAt: string
  updatedAt: string
}

export interface Seat {
  id: string
  groupId: string
  name: string
  enabled: boolean
  maxConcurrency: number
  keyCount: number
  dailyLimitUsd: string
  weeklyLimitUsd: string
  dailyUsedUsd: string
  weeklyUsedUsd: string
  dailyResetsAt: string | null
  weeklyResetsAt: string | null
}

export function getSeats(groupId: string) {
  return request<Seat[]>({ url: '/api/admin/seats', method: 'GET', params: { groupId } })
}

export function convertToCar(id: string) {
  return request({ url: '/api/admin/account-groups/convert-car', method: 'POST', data: { id } })
}

export function saveSeat(data: Pick<Seat, 'groupId' | 'name' | 'enabled' | 'maxConcurrency' | 'dailyLimitUsd' | 'weeklyLimitUsd'> & { id?: string }) {
  return request({ url: '/api/admin/seats/save', method: 'POST', data })
}

export function joinSeat(seatId: string, keyIds: string[]) {
  return request({ url: '/api/admin/seats/join', method: 'POST', data: { seatId, keyIds } })
}

export interface AccountGroupPageMeta {
  page: number
  pageSize: number
  total: number
  totalPages: number
}

export interface AccountGroupListResponse {
  items: AccountGroup[]
  page: AccountGroupPageMeta
  configRevision: number
}

export interface AccountGroupMutationResponse {
  id: string
  record: AccountGroup | null
  configRevision: number
}

interface AccountGroupListParams {
  page: number
  pageSize: number
  search?: string
  enabled?: boolean
}

interface AccountGroupCreateParam {
  disableFast?: boolean
  name: string
  description: string | null
  color: string
}

interface AccountGroupUpdateParam {
  disableFast?: boolean
  id: string
  name: string
  description: string | null
  color: string
}

interface AccountGroupIdParam {
  id: string
}

export function getAccountGroups(data: AccountGroupListParams, options: RequestOptions = {}) {
  return request<AccountGroupListResponse>({
    url: '/api/admin/account-groups',
    method: 'GET',
    params: data,
    ...options,
  })
}

export function createAccountGroup(data: AccountGroupCreateParam) {
  return request<AccountGroupMutationResponse>({
    url: '/api/admin/account-groups/create',
    method: 'POST',
    data,
  })
}

export function updateAccountGroup(data: AccountGroupUpdateParam) {
  return request<AccountGroupMutationResponse>({
    url: '/api/admin/account-groups/update',
    method: 'POST',
    data,
  })
}

export function enableAccountGroup(data: AccountGroupIdParam) {
  return request<AccountGroupMutationResponse>({
    url: '/api/admin/account-groups/enable',
    method: 'POST',
    data,
  })
}

export function disableAccountGroup(data: AccountGroupIdParam) {
  return request<AccountGroupMutationResponse>({
    url: '/api/admin/account-groups/disable',
    method: 'POST',
    data,
  })
}

export function deleteAccountGroup(data: AccountGroupIdParam, options: RequestOptions = {}) {
  return request<AccountGroupMutationResponse>({
    url: '/api/admin/account-groups/delete',
    method: 'POST',
    data,
    ...options,
  })
}
