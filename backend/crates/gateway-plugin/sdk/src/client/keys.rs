//! 非秘密 Key 目录与预算重置的类型化调用。

use crate::{
    ErrorCode, PluginFault,
    call::{
        host::{KeyListRequest, KeyListResult},
        key_budgets,
    },
};

use super::{HostClient, SessionError, payload_call};

impl HostClient {
    /// 查询 Key 的非秘密身份；模型或预算访问域决定可用阶段。
    ///
    /// # Errors
    /// 权限、阶段或分页参数不合法，或者宿主读取失败时返回错误。
    pub async fn list_keys(&self, query: KeyListRequest) -> Result<KeyListResult, PluginFault> {
        let invalid = || PluginFault::new(ErrorCode::InvalidInput, "invalid key list payload");
        let reply = self
            .call(
                "host.keys.list",
                serde_json::to_value(query).map_err(|_| invalid())?,
                Vec::new(),
            )
            .await
            .map_err(SessionError::into_plugin_fault)?;
        if !reply.payload.is_empty() {
            return Err(invalid());
        }
        serde_json::from_value(reply.result).map_err(|_| invalid())
    }

    /// 清零指定周期，保留限额和到期时间；结果未知时不能盲目重试。
    ///
    /// # Errors
    /// 未授权、实例过期、Key 不存在、宿主写入失败时返回错误。
    pub async fn reset_key_budget(
        &self,
        request: key_budgets::ResetKeyBudgetRequest,
    ) -> Result<key_budgets::ResetKeyBudgetResult, PluginFault> {
        payload_call(self, key_budgets::RESET, request).await
    }
}
