//! Client Key 预算查询、上限更新、用量重置与周窗口接管。

use serde::{Deserialize, Serialize};

pub const GET: &str = "host.keys.get_budget";
pub const UPDATE_LIMITS: &str = "host.keys.update_budget_limits";
pub const RESET: &str = "host.keys.reset_budget";
pub const WEEKLY_CONTROL_GET: &str = "host.keys.weekly_control.get";
pub const WEEKLY_CONTROL_CHANGE: &str = "host.keys.weekly_control.change";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetPeriod {
    Daily,
    Weekly,
    All,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResetKeyBudgetRequest {
    pub client_key_id: String,
    pub period: BudgetPeriod,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResetKeyBudgetResult {
    pub client_key_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetKeyBudgetRequest {
    pub client_key_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyBudget {
    pub client_key_id: String,
    pub daily_limit_usd: String,
    pub weekly_limit_usd: String,
    pub daily_used_usd: String,
    pub weekly_used_usd: String,
    pub daily_resets_at_ms: Option<i64>,
    pub weekly_resets_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateKeyBudgetLimitsRequest {
    pub client_key_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_limit_usd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_limit_usd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateKeyBudgetLimitsResult {
    pub client_key_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WeeklyWindowQuery {
    pub client_key_id: String,
}

/// 到期时间必须晚于宿主执行时刻；宿主不解释操作的触发原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum WeeklyWindowAction {
    /// 首次接管；默认保留已用金额，`clear_used` 时从当前时刻重新计费。
    Claim {
        expires_at_ms: i64,
        #[serde(default)]
        clear_used: bool,
    },
    /// 清零并以宿主执行时刻作为新的计费起点。
    Sync { expires_at_ms: i64 },
    /// 只修正到期时间，保留已用金额、计费起点和接管者。
    Align { expires_at_ms: i64 },
    /// 解除接管，窗口由原生规则继续。
    Release,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeWeeklyWindowRequest {
    pub client_key_id: String,
    pub expected_revision: u64,
    pub operation: WeeklyWindowAction,
}

/// 接管期间的窗口事实；未接管时只有 `revision` 有意义。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WeeklyWindowControl {
    pub revision: u64,
    pub controller: Option<String>,
    pub expires_at_ms: Option<i64>,
    pub accounting_start_at_ms: Option<i64>,
    pub waiting: bool,
}
