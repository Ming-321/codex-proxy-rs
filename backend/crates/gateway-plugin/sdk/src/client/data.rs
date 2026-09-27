use crate::{PluginFault, call::data};

use super::{HostClient, payload_call};

impl HostClient {
    /// 读取 Key 当前启用状态与显式分组绑定，不返回密钥。
    ///
    /// # Errors
    ///
    /// 未获得 data 权限、调用阶段不符、Key 不存在或宿主读取失败时返回错误。
    pub async fn key_facts(
        &self,
        query: data::ClientKeyFactsQuery,
    ) -> Result<data::ClientKeyFacts, PluginFault> {
        payload_call(self, data::KEYS_GET, query).await
    }

    /// 在已授权的 management／command_line／maintenance 调用中分页读取账号基础事实。
    ///
    /// # Errors
    ///
    /// 未获得 data 权限、调用阶段不符、参数无效或宿主读取失败时返回错误。
    pub async fn account_facts(
        &self,
        query: data::AccountFactsQuery,
    ) -> Result<data::AccountFactsPage, PluginFault> {
        payload_call(self, data::ACCOUNTS_LIST, query).await
    }

    /// 读取已有额度观测，不触发上游刷新，也不返回宿主预测结果。
    ///
    /// # Errors
    ///
    /// 未获得 data 权限、调用阶段不符、账号不存在或宿主读取失败时返回错误。
    pub async fn quota_facts(
        &self,
        query: data::QuotaFactsQuery,
    ) -> Result<data::QuotaFacts, PluginFault> {
        payload_call(self, data::QUOTA_GET, query).await
    }
}
