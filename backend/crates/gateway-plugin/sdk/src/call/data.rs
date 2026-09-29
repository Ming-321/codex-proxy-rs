//! 基础事实投影；主动刷新由独立的 quota_observations 访问域授权。
//! 响应忽略未知字段，以兼容宿主新增事实；查询仍严格校验字段。

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};

fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

pub const ACCOUNTS_LIST: &str = "host.data.accounts.list";
pub const QUOTA_REFRESH: &str = "host.quota_observations.refresh";
pub const QUOTA_GET: &str = "host.data.quota.get";
pub const KEYS_GET: &str = "host.data.keys.get";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientKeyFactsQuery {
    pub client_key_id: String,
}

/// 当前显式分组绑定；空列表不是单账号范围，不包含密钥或凭据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientKeyFacts {
    pub schema_version: u32,
    pub client_key_id: String,
    pub enabled: bool,
    pub group_ids: Vec<String>,
    /// 持久化配置；零表示不限，绑定来源生效值由独立查询提供。
    pub configured_max_concurrency: u64,
    pub configured_requests_per_minute: u64,
    /// Provider 专属请求画像配置，不表示实际客户端软件。
    pub request_profile_overrides: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountFactsQuery {
    pub provider_id: Option<String>,
    pub cursor: Option<String>,
    pub limit: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountFacts {
    pub account_id: String,
    pub provider_id: String,
    pub name: String,
    pub email: Option<String>,
    pub group_ids: Vec<String>,
    pub enabled: bool,
    #[serde(default)]
    pub notes: Option<String>,
    /// `None` 表示继承全局默认值。
    #[serde(deserialize_with = "required_nullable")]
    pub configured_concurrency_limit: Option<u32>,
    /// `None` 表示不限。
    #[serde(deserialize_with = "required_nullable")]
    pub effective_concurrency_limit: Option<u64>,
    /// `None` 表示租约读取不可用，`Some(0)` 表示已知空闲。
    #[serde(default)]
    pub used_slots: Option<u64>,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountFactsPage {
    pub schema_version: u32,
    pub accounts: Vec<AccountFacts>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaFactsQuery {
    pub account_id: String,
}

/// Provider 已有快照的必要投影；空观测时间表示没有可用样本，不代表额度为零。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaFacts {
    pub schema_version: u32,
    pub account_id: String,
    pub observed_at_ms: Option<i64>,
    pub windows: Vec<QuotaWindowFacts>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindowFacts {
    pub key: String,
    pub window_seconds: Option<u64>,
    /// 百分比而非 0～1 比率；未知值为 null。
    pub used_percent: Option<f64>,
    pub reset_at_ms: Option<i64>,
}
