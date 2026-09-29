use super::{HostClient, payload_call};
use crate::{PluginFault, call::weekly_budget};

impl HostClient {
    /// 查询持久化周窗口接管版本及等待状态。
    ///
    /// # Errors
    /// 权限、阶段、Key 不存在或宿主读取失败时返回错误。
    pub async fn weekly_budget_control(
        &self,
        query: weekly_budget::WeeklyBudgetQuery,
    ) -> Result<weekly_budget::WeeklyBudgetControl, PluginFault> {
        payload_call(self, weekly_budget::GET, query).await
    }

    /// 按预期版本接管、同步或解除周窗口；重试必须保留原请求。
    ///
    /// # Errors
    /// 授权失效、版本过期、接管者冲突、到期时间无效或宿主写入失败时返回错误。
    pub async fn change_weekly_budget(
        &self,
        request: weekly_budget::ChangeWeeklyBudgetRequest,
    ) -> Result<weekly_budget::WeeklyBudgetControl, PluginFault> {
        payload_call(self, weekly_budget::CHANGE, request).await
    }
}
