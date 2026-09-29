use crate::{PluginFault, call::data};

use super::{HostClient, payload_call};

impl HostClient {
    /// 查询宿主已有观测与用量产生的周容量估计，不刷新上游或修改预算。
    ///
    /// # Errors
    /// 缺少 quota_forecasts 权限、调用阶段不符、账号不存在或宿主读取失败时返回错误。
    pub async fn weekly_quota_forecast(
        &self,
        query: crate::call::quota_forecasts::WeeklyQuotaForecastQuery,
    ) -> Result<crate::call::quota_forecasts::WeeklyQuotaForecast, PluginFault> {
        payload_call(self, crate::call::quota_forecasts::GET_WEEKLY, query).await
    }

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

    /// 读取当前持久化限额来源的并发租约快照；共享成员返回同一来源总占用。
    ///
    /// # Errors
    /// 缺少 data 权限、阶段不符或 Key 不存在时返回错误；Redis 不可用时占用为 None。
    pub async fn key_occupancy(
        &self,
        query: data::ClientKeyFactsQuery,
    ) -> Result<data::ClientKeyOccupancy, PluginFault> {
        payload_call(self, data::KEYS_OCCUPANCY, query).await
    }

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
