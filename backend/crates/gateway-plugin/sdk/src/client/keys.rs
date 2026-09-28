//! 非秘密 Key 目录、预算与共享关系管理的类型化调用。

use crate::{
    ErrorCode, PluginFault,
    call::{
        host::{KeyListRequest, KeyListResult},
        key_budgets, key_limit_bindings,
    },
};

use super::{HostClient, SessionError, payload_call};

impl HostClient {
    /// 查询共享限额来源及配置加载状态，需要独立共享关系权限。
    ///
    /// # Errors
    /// 未授权、阶段不符、Key 不存在或宿主读取失败时返回错误。
    pub async fn get_key_limit_binding(
        &self,
        request: key_limit_bindings::GetKeyLimitBindingRequest,
    ) -> Result<key_limit_bindings::KeyLimitBinding, PluginFault> {
        payload_call(self, key_limit_bindings::GET, request).await
    }

    /// 修改原生限额来源；null 解绑，不迁账，不自动重试或覆盖冲突。
    ///
    /// # Errors
    /// 未授权、实例过期、版本冲突、关系无效或宿主写入失败时返回错误。
    /// 结果未知时仅可在相同实例代次下用原版本和完整参数重试最近操作。
    pub async fn change_key_limit_binding(
        &self,
        request: key_limit_bindings::ChangeKeyLimitBindingRequest,
    ) -> Result<key_limit_bindings::KeyLimitBinding, PluginFault> {
        payload_call(self, key_limit_bindings::CHANGE, request).await
    }

    /// 只读查询单个 Key 的预算，不开启或重置窗口。
    ///
    /// # Errors
    /// 未授权、阶段不符、Key 不存在或宿主读取失败时返回错误。
    pub async fn get_key_budget(
        &self,
        request: key_budgets::GetKeyBudgetRequest,
    ) -> Result<key_budgets::KeyBudget, PluginFault> {
        payload_call(self, key_budgets::GET, request).await
    }

    /// 更新指定日／周上限；省略项不变，零表示不限，不清零用量。
    ///
    /// # Errors
    /// 未授权、阶段不符、参数无效、Key 不存在或宿主写入失败时返回错误。
    /// 已绑定共享来源的成员返回 conflict，须显式操作来源 Key。
    pub async fn update_key_budget_limits(
        &self,
        request: key_budgets::UpdateKeyBudgetLimitsRequest,
    ) -> Result<key_budgets::UpdateKeyBudgetLimitsResult, PluginFault> {
        payload_call(self, key_budgets::UPDATE_LIMITS, request).await
    }

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
