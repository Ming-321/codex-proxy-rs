//! car 账号周期与已发布容量的后台协调。

use std::{str::FromStr as _, sync::Arc, time::Duration};

use chrono::{Duration as ChronoDuration, Utc};
use gateway_core::{
    metering::Decimal,
    task::{ScheduledTask, WorkerCycleContext, WorkerTaskError},
};
use tracing::warn;

use crate::{
    model::account_groups::CarQuotaObservation,
    use_case::{account_groups::AccountGroupService, accounts::AccountsService},
};

pub const CAR_QUOTA_INTERVAL: Duration = Duration::from_secs(5 * 60);
pub const CAR_QUOTA_WORKER_OWNER: &str = "car-quota";
pub const WORKER_INITIAL_BACKOFF: Duration = Duration::from_secs(5);
pub const WORKER_MAXIMUM_BACKOFF: Duration = Duration::from_secs(5 * 60);
pub const WORKER_LEASE_TTL: Duration = Duration::from_secs(15 * 60);
pub const WORKER_LEASE_RENEWAL: Duration = Duration::from_secs(5 * 60);

pub struct CarQuotaTask {
    accounts: Arc<dyn AccountsService>,
    groups: Arc<dyn AccountGroupService>,
}

impl CarQuotaTask {
    pub fn new(accounts: Arc<dyn AccountsService>, groups: Arc<dyn AccountGroupService>) -> Self {
        Self { accounts, groups }
    }

    async fn run_cycle_inner(&self) {
        let cars = match self.groups.car_accounts().await {
            Ok(cars) => cars,
            Err(error) => {
                warn!(error = %error, "读取 car 账号失败");
                return;
            }
        };
        for car in cars {
            if let Err(error) = self.reconcile(car).await {
                warn!(error = %error, "协调 car 账号周期失败");
            }
        }
    }

    async fn reconcile(
        &self,
        car: crate::model::account_groups::CarAccount,
    ) -> Result<(), crate::model::AdminError> {
        let state = self.groups.car_quota_state(car.group_id.clone()).await?;
        let now = Utc::now();
        let refresh = state.cycle_end.is_none_or(|end| now >= end)
            || now - state.updated_at >= ChronoDuration::minutes(30);
        let item = self.accounts.quota(&car.account_id, refresh).await?;
        let Some((window, period)) = item.quota.usage_window() else {
            return Ok(());
        };
        if state.mode == crate::model::account_groups::CarQuotaMode::Active
            && state.cycle_end.is_some_and(|end| now < end)
            && state
                .window_key
                .as_deref()
                .is_some_and(|key| key != window.key)
        {
            return Ok(());
        }
        let (Some(seconds), Some(reset_at), Some(observed_at), Some(used_percent)) = (
            window.window_seconds,
            window.reset_at,
            item.quota.observed_at,
            window.used_percent,
        ) else {
            return Ok(());
        };
        let Some(cycle_start) = i64::try_from(seconds)
            .ok()
            .and_then(ChronoDuration::try_seconds)
            .and_then(|duration| reset_at.checked_sub_signed(duration))
        else {
            return Ok(());
        };
        if observed_at < cycle_start || observed_at >= reset_at {
            return Ok(());
        }
        let report = self.accounts.quota_forecast(&car.account_id).await?;
        let forecast = report
            .forecasts
            .into_iter()
            .find(|forecast| forecast.period == period);
        let predicted_capacity_usd = forecast.as_ref().and_then(|forecast| {
            forecast
                .estimated_usd
                .filter(|value| value.is_finite() && *value >= 0.0)
                .and_then(|value| Decimal::from_str(&format!("{value:.10}")).ok())
        });
        let millis = |value: f64| {
            (value.is_finite() && (0.0..=100.0).contains(&value))
                .then(|| (value * 1_000.0).round() as u32)
        };
        let observation = CarQuotaObservation {
            group_id: car.group_id,
            window_key: window.key.clone(),
            cycle_start,
            cycle_end: reset_at,
            observed_at,
            used_percent_millis: millis(used_percent).unwrap_or(0),
            predicted_capacity_usd,
            prediction_reason: forecast.as_ref().and_then(|forecast| {
                forecast.unavailable_reason.map(str::to_owned).or_else(|| {
                    forecast
                        .low_sample
                        .then(|| "样本量偏低，暂不发布".to_owned())
                })
            }),
            sample_start: forecast
                .as_ref()
                .and_then(|forecast| forecast.sample_start_at),
            sample_end: forecast
                .as_ref()
                .and_then(|forecast| forecast.sample_end_at),
            sample_percent_millis: forecast
                .as_ref()
                .and_then(|forecast| forecast.sampled_percent)
                .and_then(millis),
            cost_complete: forecast
                .as_ref()
                .is_some_and(|forecast| !forecast.incomplete_cost),
            pending_request_count: forecast
                .as_ref()
                .map_or(0, |forecast| forecast.pending_request_count),
        };
        self.groups.reconcile_car_quota(observation).await?;
        Ok(())
    }
}

impl ScheduledTask for CarQuotaTask {
    fn run_cycle(
        &self,
        context: WorkerCycleContext,
    ) -> futures::future::BoxFuture<'_, Result<(), WorkerTaskError>> {
        Box::pin(async move {
            if !context.cancellation().is_cancelled() {
                self.run_cycle_inner().await;
            }
            Ok(())
        })
    }
}
