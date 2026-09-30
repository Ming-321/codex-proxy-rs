use gateway_admin::{
    PluginManagementService,
    model::{
        MutationContext,
        client_keys::{
            ClientKeyBudgetMutationOrigin, ClientKeyBudgetPeriod, ClientKeyBudgetWindowMode,
            ClientKeyBudgetWindowPeriod, ResetClientKeyBudget,
        },
        plugins::management::PluginManagementRequest,
    },
};
use gateway_core::engine::{
    ModelRequestId,
    budget::{ClientBudgetCharge, ClientBudgetPort},
};
use gateway_core::policy::ClientApiKeyId;
use serde_json::{Value, json};
use std::time::{Duration, SystemTime};

use super::keys::seed_budget;
use crate::support::{environment::Environment, native};

fn request(body: Value) -> PluginManagementRequest {
    PluginManagementRequest {
        headers: vec![],
        method: "POST".into(),
        path: "cycle".into(),
        query: String::new(),
        content_type: Some("application/json".into()),
        body: serde_json::to_vec(&body).unwrap(),
        request_id: "cycle-fixture".into(),
    }
}

fn charge(request: &str, amount: &str) -> ClientBudgetCharge {
    ClientBudgetCharge {
        key_id: ClientApiKeyId::new("key_budget").unwrap(),
        request_id: ModelRequestId::new(format!("req_{request}")).unwrap(),
        amount_usd: amount.parse().unwrap(),
        completed_at: SystemTime::now(),
    }
}

#[tokio::test]
async fn plugin_composes_native_windows_with_private_cycles_and_recovers_a_committed_reply_loss() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let key = seed_budget(&environment).await;
    let marker = environment
        .directory
        .path()
        .join("budget-cycle-startups.jsonl");
    let error_marker = environment
        .directory
        .path()
        .join("budget-cycle-errors.jsonl");
    environment.install_plugin(json!({
        "budget_cycle_fixture":true, "startup_marker":marker, "budget_cycle_error_marker":error_marker,
        "state_namespaces":[{"namespace":"cycles","schemaVersion":1,"schema":{"type":"object"},"maximumRecords":8,"maximumBytes":8192,"maximumValueBytes":4096}],
        "management_registration":{"routes":[{"method":"POST","path":"cycle","request_content_types":["application/json"],"response_content_types":["application/json"]}]}
    })).await;
    let store = environment.store.admin_ports().client_keys();
    let budgets = environment.client_budgets().await;
    let (runtime, core) = environment.runtime().await;
    let access = gateway_admin::initialize_plugin_client_keys(
        native::admin_registry(),
        store.clone(),
        core.snapshot_control(),
    );
    runtime.bind_client_key_ports(&access).unwrap();
    let service = PluginManagementService::new(
        runtime.clone(),
        environment.store.admin_ports().plugins(),
        core.snapshots(),
    );
    let target = service.views().await.unwrap().remove(0).target;
    // 给并行套件中的数据库排队留余量，仍在真实时间越过截止后验证准入。
    let expires_at = chrono::Utc::now().timestamp_millis() + 10_000;
    let first = service
        .handle(
            &target,
            request(json!({"cycle_id":"external-1","expires_at_ms":expires_at})),
        )
        .await
        .unwrap_or_else(|error| {
            panic!(
                "{error:?}: {}",
                std::fs::read_to_string(&error_marker).unwrap_or_default()
            )
        });
    let first: Value = serde_json::from_slice(&first.body).unwrap();
    assert_eq!(first["mode"], "fixed");
    assert_eq!(first["expires_at_ms"], expires_at);
    assert_eq!(
        store
            .get_client_key(&key)
            .await
            .unwrap()
            .unwrap()
            .budget
            .weekly_used_usd
            .canonical(),
        "4"
    );
    budgets.admit(key.clone()).await.unwrap();
    // 使用真实截止时间验证原生准入；插件没有给宿主提供账号关联或等待状态。
    let remaining = (expires_at - chrono::Utc::now().timestamp_millis()).max(0) as u64;
    tokio::time::sleep(Duration::from_millis(remaining + 20)).await;
    assert_eq!(
        budgets
            .admit(key.clone())
            .await
            .unwrap_err()
            .client_error_code(),
        Some("key_budget_window_expired")
    );
    let retry = service
        .handle(
            &target,
            request(json!({"cycle_id":"external-1","expires_at_ms":expires_at})),
        )
        .await
        .unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&retry.body).unwrap(), first);
    budgets.settle(charge("cycle-late", "2")).await.unwrap();
    assert_eq!(
        store
            .get_client_key(&key)
            .await
            .unwrap()
            .unwrap()
            .budget
            .weekly_used_usd
            .canonical(),
        "6"
    );

    let old_completion = charge("before-next-cycle", "0.5");
    let next_expiry = chrono::Utc::now().timestamp_millis() + 3_600_000;
    assert!(service.handle(&target,request(json!({"cycle_id":"external-2","expires_at_ms":next_expiry,"crash_after_commit":true}))).await.is_err());
    let committed = store
        .client_key_budget_window(&key, ClientKeyBudgetWindowPeriod::Weekly)
        .await
        .unwrap();
    assert_eq!(committed.revision, 2);
    budgets.settle(old_completion).await.unwrap();
    assert_eq!(
        store
            .get_client_key(&key)
            .await
            .unwrap()
            .unwrap()
            .budget
            .weekly_used_usd
            .canonical(),
        "0"
    );
    budgets
        .settle(charge("after-next-cycle", "1"))
        .await
        .unwrap();
    budgets.admit(key.clone()).await.unwrap();
    drop(service);
    drop(access);
    environment.release_plugin_accounts(&runtime);
    runtime.shutdown().await;
    drop(core);
    drop(runtime);

    let (runtime, core) = environment.runtime().await;
    let access = gateway_admin::initialize_plugin_client_keys(
        native::admin_registry(),
        store.clone(),
        core.snapshot_control(),
    );
    runtime.bind_client_key_ports(&access).unwrap();
    let service = PluginManagementService::new(
        runtime.clone(),
        environment.store.admin_ports().plugins(),
        core.snapshots(),
    );
    let restarted = service.views().await.unwrap().remove(0).target;
    assert_eq!(restarted.instance_id, target.instance_id);
    let second_cycle = json!({"cycle_id":"external-2","expires_at_ms":next_expiry});
    let reply = service
        .handle(&restarted, request(second_cycle.clone()))
        .await
        .unwrap();
    let reply: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(reply["revision"], 2);
    assert_eq!(
        reply["accounting_start_at_ms"],
        committed.accounting_start.unwrap().timestamp_millis()
    );
    assert_eq!(
        store
            .get_client_key(&key)
            .await
            .unwrap()
            .unwrap()
            .budget
            .weekly_used_usd
            .canonical(),
        "1"
    );
    assert_eq!(
        environment
            .audit_requests("change_budget_window")
            .await
            .len(),
        2
    );
    assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 2);

    // 管理员原生重置优先：关闭窗口、恢复自动滚动，旧周期重试被版本校验拒绝。
    store
        .reset_client_key_budget(
            ResetClientKeyBudget {
                id: key.clone(),
                period: ClientKeyBudgetPeriod::Weekly,
            },
            ClientKeyBudgetMutationOrigin::Admin,
            &MutationContext {
                actor: gateway_admin::model::MutationActor::System,
                request_id: "admin-reset".into(),
            },
        )
        .await
        .unwrap();
    assert!(
        service
            .handle(&restarted, request(second_cycle))
            .await
            .is_err()
    );
    let reset = store
        .client_key_budget_window(&key, ClientKeyBudgetWindowPeriod::Weekly)
        .await
        .unwrap();
    assert_eq!(reset.mode, ClientKeyBudgetWindowMode::Automatic);
    assert_eq!(reset.expires_at, None);
    budgets.admit(key.clone()).await.unwrap();
    assert!(
        store
            .get_client_key(&key)
            .await
            .unwrap()
            .unwrap()
            .budget
            .weekly_resets_at
            .is_some()
    );

    drop(service);
    drop(access);
    environment.release_plugin_accounts(&runtime);
    runtime.shutdown().await;
    drop(core);
    drop(runtime);
    drop(store);
    drop(budgets);
    environment.close().await;
}
