//! 持续接管 Key 周窗口；需要 key_budgets 权限，仅管理、命令、维护阶段可用。
use serde::{Deserialize, Serialize};

pub const GET: &str = "host.keys.weekly_control.get";
pub const CHANGE: &str = "host.keys.weekly_control.change";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WeeklyBudgetQuery {
    pub client_key_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum WeeklyBudgetAction {
    /// 首次接管默认保留已用金额；重启恢复必须保留。
    Claim {
        expires_at_ms: i64,
        #[serde(default)]
        clear_used: bool,
    },
    /// 正常换周和提前重置共用此操作；宿主执行时清零并确定新的计费起点。
    Sync {
        expires_at_ms: i64,
    },
    Release,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeWeeklyBudgetRequest {
    pub client_key_id: String,
    pub expected_revision: u64,
    pub operation: WeeklyBudgetAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WeeklyBudgetControl {
    pub revision: u64,
    pub controller: Option<String>,
    pub expires_at_ms: Option<i64>,
    pub waiting: bool,
}
