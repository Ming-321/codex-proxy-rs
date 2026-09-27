use crate::{PluginFault, call::data};

use super::{HostClient, payload_call};

impl HostClient {
    /// 通过宿主刷新账号额度观测，返回与 quota_facts 相同的非秘密投影。
    /// 需要 quota_observations 权限；不修改上游额度，也不自动重置任何 Key。
    ///
    /// # Errors
    /// 未授权、阶段不符、账号不支持刷新或 Provider 查询失败时返回错误。
    pub async fn refresh_account_quota(
        &self,
        query: data::QuotaFactsQuery,
    ) -> Result<data::QuotaFacts, PluginFault> {
        payload_call(self, data::QUOTA_REFRESH, query).await
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
    /// 未获得 data 或 quota_observations 权限、调用阶段不符、账号不存在或宿主读取失败时返回错误。
    pub async fn quota_facts(
        &self,
        query: data::QuotaFactsQuery,
    ) -> Result<data::QuotaFacts, PluginFault> {
        payload_call(self, data::QUOTA_GET, query).await
    }
}
