//! Provider-neutral account group commands and projections.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use gateway_core::{
    account::{AccountStatusFacts, ProviderAccountId},
    metering::Decimal,
    routing::AccountGroupId,
};

use super::{PageSize, Revision, observability::DecimalAmount};

/// Canonical `#RRGGBBAA` color persisted with an account group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountGroupColor(String);

impl AccountGroupColor {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        (value.len() == 9
            && value.starts_with('#')
            && value[1..].bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| Self(value.to_ascii_uppercase()))
    }
}

/// Group member availability at the current observation time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountGroupAccountSummary {
    pub available: u64,
    pub limited: u64,
    pub total: u64,
}

/// Group scheduling slots derived from available accounts and runtime leases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountGroupCapacity {
    pub used_slots: Option<u64>,
    /// `None` 表示可用账号中存在不限制并发的账号。
    pub total_slots: Option<u64>,
}

/// Successful, downstream-committed USD request costs for group accounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountGroupUsage {
    pub today_usd: DecimalAmount,
    /// 当前 usage retention 窗口内、按请求发生时 routing group 快照归属的累计成本。
    pub retained_total_usd: DecimalAmount,
}

/// 当前页账号组 membership 对应的持久账号事实；运行态在 Admin query service 合并。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountGroupMemberFact {
    pub group_id: AccountGroupId,
    pub account_id: String,
    pub status: AccountStatusFacts,
    pub total_slots: Option<u64>,
}

/// Lightweight group reference embedded in account and client-key views.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountGroupRef {
    pub id: AccountGroupId,
    pub name: String,
    pub color: AccountGroupColor,
    pub enabled: bool,
}

/// Account group list query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountGroupListQuery {
    pub page: u32,
    pub page_size: PageSize,
    pub search: Option<String>,
    pub enabled: Option<bool>,
}

/// Complete account group summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountGroupRecord {
    pub is_car: bool,
    pub disable_fast: bool,
    pub id: AccountGroupId,
    pub name: String,
    pub description: Option<String>,
    pub color: AccountGroupColor,
    pub enabled: bool,
    pub member_count: u64,
    pub provider_counts: BTreeMap<String, u64>,
    pub client_key_count: u64,
    pub account_summary: AccountGroupAccountSummary,
    pub capacity: AccountGroupCapacity,
    pub usage: AccountGroupUsage,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Paginated account groups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountGroupPage {
    pub config_revision: Revision,
    pub items: Vec<AccountGroupRecord>,
    pub total: u64,
    pub page: u32,
    pub page_size: u16,
}

/// Create an account group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateAccountGroup {
    pub disable_fast: bool,
    pub name: String,
    pub description: Option<String>,
    pub color: AccountGroupColor,
}

/// Store-ready create command with a generated stable ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAccountGroup {
    pub disable_fast: bool,
    pub id: AccountGroupId,
    pub name: String,
    pub description: Option<String>,
    pub color: AccountGroupColor,
}

/// Update an account group's descriptive fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateAccountGroup {
    pub disable_fast: Option<bool>,
    pub id: AccountGroupId,
    pub name: String,
    pub description: Option<String>,
    pub color: AccountGroupColor,
}

/// Enable or disable an account group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetAccountGroupEnabled {
    pub id: AccountGroupId,
    pub enabled: bool,
}

/// Delete an account group that is not referenced by a client key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteAccountGroup {
    pub id: AccountGroupId,
}

/// Result of any account-group mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountGroupMutation {
    pub config_revision: Revision,
    pub id: AccountGroupId,
    pub record: Option<AccountGroupRecord>,
}

/// car 下一个 seat 的配置与共享余额，不包含成员 Key 凭据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatRecord {
    pub id: gateway_core::policy::SeatId,
    pub group_id: AccountGroupId,
    pub name: String,
    pub enabled: bool,
    pub max_concurrency: u64,
    pub weight: Decimal,
    pub key_count: u64,
    pub budget: gateway_core::engine::budget::ClientBudgetStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveSeat {
    pub id: Option<gateway_core::policy::SeatId>,
    pub group_id: AccountGroupId,
    pub name: String,
    pub enabled: bool,
    pub max_concurrency: u64,
    pub weight: Decimal,
    pub limits: gateway_core::engine::budget::ClientBudgetLimits,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinSeat {
    pub seat_id: gateway_core::policy::SeatId,
    pub key_ids: Vec<gateway_core::policy::ClientApiKeyId>,
}

/// 全局 car 容量估算发布设置；百分点参数按千分之一保存。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarQuotaSettings {
    pub automatic_updates: bool,
    pub publish_interval_seconds: u64,
    pub outside_usage_protection: bool,
    pub minimum_sample_millis: u32,
    pub estimate_weight_millis: u32,
    pub minimum_change_millis: u32,
    pub maximum_adjustment_millis: u32,
    pub abnormal_change_millis: u32,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplaceCarQuotaSettings {
    pub automatic_updates: bool,
    pub publish_interval_seconds: u64,
    pub outside_usage_protection: bool,
    pub minimum_sample_millis: u32,
    pub estimate_weight_millis: u32,
    pub minimum_change_millis: u32,
    pub maximum_adjustment_millis: u32,
    pub abnormal_change_millis: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarQuotaMode {
    Legacy,
    Waiting,
    Active,
}

impl CarQuotaMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Waiting => "waiting",
            Self::Active => "active",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarQuotaState {
    pub group_id: AccountGroupId,
    pub total_weight: Decimal,
    pub mode: CarQuotaMode,
    pub window_key: Option<String>,
    pub cycle_start: Option<DateTime<Utc>>,
    pub cycle_end: Option<DateTime<Utc>>,
    pub account_used_percent_millis: Option<u32>,
    pub published_capacity_usd: Decimal,
    pub predicted_capacity_usd: Option<Decimal>,
    pub prediction_reason: Option<String>,
    pub published_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarAccount {
    pub group_id: AccountGroupId,
    pub account_id: ProviderAccountId,
}

/// Worker 从账号真实窗口和现有预测系统投影出的候选事实。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarQuotaObservation {
    pub group_id: AccountGroupId,
    pub window_key: String,
    pub cycle_start: DateTime<Utc>,
    pub cycle_end: DateTime<Utc>,
    pub observed_at: DateTime<Utc>,
    pub used_percent_millis: u32,
    pub predicted_capacity_usd: Option<Decimal>,
    pub prediction_reason: Option<String>,
    pub sample_start: Option<DateTime<Utc>>,
    pub sample_end: Option<DateTime<Utc>>,
    pub sample_percent_millis: Option<u32>,
    pub cost_complete: bool,
    pub pending_request_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveCarWeights {
    pub group_id: AccountGroupId,
    pub total_weight: Decimal,
}
