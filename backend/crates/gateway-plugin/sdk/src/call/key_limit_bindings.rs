//! 原生共享限额关系；修改不迁移历史消费或改变真实请求身份。

use serde::{Deserialize, Serialize};

pub const GET: &str = "host.keys.get_limit_binding";
pub const CHANGE: &str = "host.keys.change_limit_binding";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetKeyLimitBindingRequest {
    pub client_key_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeKeyLimitBindingRequest {
    pub client_key_id: String,
    /// null 表示恢复自身限额；不允许显式绑定自身。
    #[serde(deserialize_with = "Deserialize::deserialize")]
    pub source_key_id: Option<String>,
    pub expected_revision: u64,
}

/// 当前持久事实；配置提交不表示所有宿主实例已加载。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyLimitBinding {
    pub client_key_id: String,
    /// 未绑定时返回真实 Key 自身。
    pub source_key_id: String,
    pub revision: u64,
    pub config_revision: u64,
    pub binding_config_revision: Option<u64>,
    pub loaded_config_revision: Option<u64>,
    pub source_enabled: bool,
}
