//! Client Key 预算重置；每次成功调用均执行一次清零。

use serde::{Deserialize, Serialize};

pub const RESET: &str = "host.keys.reset_budget";

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
