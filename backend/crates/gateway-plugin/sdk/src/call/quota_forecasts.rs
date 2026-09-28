//! 账号周容量的只读估计；不代表可消费预算，也不触发上游刷新。

use serde::{Deserialize, Serialize};

pub const GET_WEEKLY: &str = "host.quota_forecasts.get_weekly";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WeeklyQuotaForecastQuery {
    pub account_id: String,
}

/// 金额单位为美元，保留宿主预测精度；未知值为 None，不等于零。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeeklyQuotaForecast {
    pub account_id: String,
    pub generated_at_ms: i64,
    pub estimated_usd: Option<f64>,
    /// 始终属于 source 窗口，不随周总量折算。
    pub remaining_usd: Option<f64>,
    pub extrapolated: bool,
    pub low_sample: bool,
    pub incomplete_cost: bool,
    pub unavailable_reason: Option<String>,
    pub source: Option<QuotaForecastSource>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaForecastSource {
    pub label: String,
    pub used_percent: Option<f64>,
    pub observed_at_ms: Option<i64>,
    pub reset_at_ms: i64,
}
