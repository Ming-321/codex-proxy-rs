//! 插件 Client Key 非秘密目录与受控预算重置端口。

use async_trait::async_trait;
use gateway_core::policy::ClientApiKeyId;

use crate::model::{
    AdminError, MutationContext,
    client_keys::ResetClientKeyBudget,
    plugin_client_keys::{PluginClientKeyListQuery, PluginClientKeyPage},
    plugin_resources::PluginResourceOwner,
};

#[async_trait]
pub trait PluginClientKeyAccess: Send + Sync {
    async fn reset_budget(
        &self,
        owner: &PluginResourceOwner,
        command: ResetClientKeyBudget,
        context: &MutationContext,
    ) -> Result<ClientApiKeyId, AdminError>;

    async fn list(
        &self,
        query: PluginClientKeyListQuery,
    ) -> Result<PluginClientKeyPage, AdminError>;
}
