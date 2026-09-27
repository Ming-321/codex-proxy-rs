//! 插件发现 Client Key 公开身份的窄端口。

use async_trait::async_trait;
use gateway_core::policy::ClientApiKeyId;

use crate::model::{
    AdminError,
    plugin_client_keys::{PluginClientKeyFacts, PluginClientKeyListQuery, PluginClientKeyPage},
};

#[async_trait]
pub trait PluginClientKeyAccess: Send + Sync {
    async fn facts(&self, id: &ClientApiKeyId) -> Result<PluginClientKeyFacts, AdminError>;
    async fn list(
        &self,
        query: PluginClientKeyListQuery,
    ) -> Result<PluginClientKeyPage, AdminError>;
}
