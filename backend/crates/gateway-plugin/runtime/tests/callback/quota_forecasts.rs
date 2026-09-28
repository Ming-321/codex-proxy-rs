use chrono::{Duration, Utc};
use gateway_admin::{
    model::{plugins::management::PluginManagementRequest, provider_credentials::*},
    ports::plugin_management::PluginManagement,
};
use gateway_core::policy::ClientApiKeyId;
use gateway_plugin_sdk::call::quota_forecasts::WeeklyQuotaForecast;
use serde_json::{Value, json};

use crate::support::{
    environment::{Environment, account_grant},
    native,
};

#[tokio::test]
async fn weekly_forecast_process_matches_native_predictions_without_budget_mutation() {
    for (days, percent, usage, cost, expired) in [
        (7, 20.0, true, Some("2"), false),
        (7, 7.0, true, Some("2"), false),
        (7, 20.0, true, None, false),
        (7, 20.0, false, None, false),
        (31, 20.0, true, Some("2"), false),
        (7, 20.0, true, Some("2"), true),
    ] {
        let Some(environment) = Environment::create().await else {
            eprintln!("SKIP: plugin integration environment absent");
            return;
        };
        let account = environment.account(None).await;
        environment
            .client_key("key_forecast", "sk-forecast-fixture")
            .await;
        environment.seed_client_key_budget("key_forecast").await;
        if usage {
            environment
                .seed_forecast_usage(account.as_str(), cost)
                .await;
        }
        let now = Utc::now();
        let quota = ProviderQuota {
            plan_type: None,
            observed_at: Some(now - Duration::minutes(1)),
            refresh_token_expires_at: None,
            limit_reached: false,
            provider_data: None,
            windows: vec![ProviderQuotaWindow {
                key: "fixture-window".into(),
                group: "weekly".into(),
                label: "真实源窗口".into(),
                limit_id: None,
                limit_name: None,
                role: None,
                local_usage_attribution: QuotaLocalUsageAttribution::AccountWide,
                window_seconds: Some(days * 86400),
                used_percent: Some(percent),
                reset_at: Some(if expired {
                    now - Duration::minutes(2)
                } else {
                    now + Duration::days(1)
                }),
                limit_reached: false,
                local_usage: None,
                provider_data: None,
            }],
        };
        let registry = native::forecast_admin_registry(quota);
        environment.install_plugin(json!({
            "management_registration":{"routes":[{"method":"GET","path":"forecast","request_content_types":[],"response_content_types":["application/json"]}]},
            "data_queries":[
                {"method":"host.quota_forecasts.get_weekly","query":{"account_id":account.as_str()}},
                {"method":"host.quota_forecasts.get_weekly","query":{"account_id":"acct_missing"}},
                {"method":"host.quota_forecasts.get_weekly","query":{"account_id":account.as_str(),"refresh":true}},
                {"method":"host.data.accounts.list","query":{"limit":10}},
                {"method":"host.quota_observations.refresh","query":{"account_id":account.as_str()}},
                {"method":"host.keys.reset_budget","query":{"client_key_id":"key_forecast","period":"all"}}
            ]
        }), vec![account_grant("quota_forecasts")]).await;
        let (runtime, core) = environment.runtime_with_registry(registry.clone()).await;
        let admin = environment
            .bind_admin_accounts_with_registry(&runtime, &core, registry)
            .await;
        let services = admin.services();
        let native = services.accounts().quota_forecast(&account).await.unwrap();
        let week = &native.forecasts[0];
        let key = ClientApiKeyId::new("key_forecast").unwrap();
        let store = environment.store.admin_ports().client_keys();
        let before = store.get_client_key(&key).await.unwrap().unwrap();
        let generation = core
            .snapshots()
            .acquire()
            .unwrap()
            .extensions()
            .unwrap()
            .clone();
        let view = runtime.views(&generation).await.unwrap().remove(0);
        let response = runtime
            .handle(
                &generation,
                &view.target,
                PluginManagementRequest {
                    method: "GET".into(),
                    path: "forecast".into(),
                    query: String::new(),
                    content_type: None,
                    body: vec![],
                    request_id: "forecast-fixture".into(),
                },
            )
            .await
            .unwrap();
        let result: Vec<Value> = serde_json::from_slice(&response.body).unwrap();
        let forecast: WeeklyQuotaForecast = serde_json::from_value(result[0].clone()).unwrap();
        assert_eq!(forecast.account_id, account.as_str());
        assert!(forecast.generated_at_ms >= native.generated_at.timestamp_millis());
        assert_eq!(forecast.estimated_usd, week.estimated_usd);
        assert_eq!(forecast.remaining_usd, week.remaining_usd);
        assert_eq!(forecast.extrapolated, week.extrapolated);
        assert_eq!(forecast.low_sample, week.low_sample);
        assert_eq!(forecast.incomplete_cost, week.incomplete_cost);
        assert_eq!(
            forecast.unavailable_reason.as_deref(),
            week.unavailable_reason
        );
        assert_eq!(
            forecast.source.as_ref().map(|s| s.reset_at_ms),
            week.source.as_ref().map(|s| s.reset_at.timestamp_millis())
        );
        assert_eq!(
            forecast.source.as_ref().and_then(|s| s.observed_at_ms),
            week.source
                .as_ref()
                .and_then(|s| s.observed_at.map(|t| t.timestamp_millis()))
        );
        if days == 7 && percent == 20.0 && usage && cost.is_some() && !expired {
            assert_eq!(forecast.estimated_usd, Some(10.0));
            assert_eq!(forecast.remaining_usd, Some(8.0));
        }
        if !usage || expired {
            assert_eq!(forecast.estimated_usd, None);
        }
        assert_eq!(result[1]["error"], "rejected");
        assert_eq!(result[2]["error"], "invalid_input");
        for denied in &result[3..] {
            assert_eq!(denied["error"], "permission_denied");
        }
        let after = store.get_client_key(&key).await.unwrap().unwrap();
        assert_eq!(before.budget, after.budget);
        assert!(environment.audit_requests("reset_budget").await.is_empty());
        drop(generation);
        runtime.shutdown().await;
        drop(services);
        drop(admin);
        drop(core);
        drop(runtime);
        environment.close().await;
    }
}
