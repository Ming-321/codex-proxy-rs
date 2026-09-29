//! 插件可见的 Client Key 非秘密资源投影。

use gateway_core::policy::ClientApiKeyId;
use gateway_core::routing::AccountGroupId;

use super::PageSize;

/// 插件可读取的当前 Key 范围，不含密钥或配置秘密。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginClientKeyFacts {
    pub id: ClientApiKeyId,
    pub enabled: bool,
    pub group_ids: Vec<AccountGroupId>,
    pub limits: gateway_core::policy::RateLimits,
    pub effective: super::client_keys::ClientLimitBinding,
    pub request_profile_overrides: super::client_keys::ProviderRequestProfileOverrides,
}

/// Redis 中指定限额来源的当前有效租约；None 表示运行态读取不可用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginClientAdmissionSnapshot {
    pub active_requests: u64,
    pub observed_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginClientKeyOccupancy {
    pub binding: super::client_keys::ClientLimitBinding,
    pub admission: Option<PluginClientAdmissionSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginClientKeyCursor {
    pub name: String,
    pub id: ClientApiKeyId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginClientKeyListQuery {
    pub cursor: Option<PluginClientKeyCursor>,
    pub limit: PageSize,
}

/// 插件只能发现调用所需的公开身份，不取得 Key 前缀、策略细节或明文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginClientKey {
    pub id: ClientApiKeyId,
    pub name: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginClientKeyPage {
    pub items: Vec<PluginClientKey>,
    pub next_cursor: Option<PluginClientKeyCursor>,
}
