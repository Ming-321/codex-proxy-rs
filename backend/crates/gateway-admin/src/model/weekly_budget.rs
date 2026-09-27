//! 周预算接管命令；版本用于重试去重和拒绝过期写入。

use chrono::{DateTime, Utc};
use gateway_core::policy::ClientApiKeyId;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum WeeklyBudgetAction {
    Claim {
        expires_at: DateTime<Utc>,
        clear_used: bool,
    },
    Sync {
        expires_at: DateTime<Utc>,
    },
    Release,
}

#[derive(Debug, Clone)]
pub struct ChangeWeeklyBudget {
    pub id: ClientApiKeyId,
    pub expected_revision: u64,
    pub action: WeeklyBudgetAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeeklyBudgetControl {
    pub revision: u64,
    pub controller: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub waiting: bool,
}
