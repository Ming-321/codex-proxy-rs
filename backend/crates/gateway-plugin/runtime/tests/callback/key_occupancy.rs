use std::{sync::Arc, time::Duration};

use gateway_admin::ports::plugin_management::PluginManagement;
use gateway_admin::{
    model::{
        AdminError,
        client_keys::{
            ChangeClientLimitBinding, ClientLimitBindingMutationOrigin, UpdateClientKey,
        },
        plugin_client_keys::PluginClientAdmissionSnapshot,
        plugins::{instances::PluginPermissionGrant, management::PluginManagementRequest},
    },
    ports::plugin_client_keys::PluginClientAdmissionReader,
};
use gateway_core::policy::{ClientApiKeyId, RateLimits};
use gateway_store::redis::{
    ClientAdmissionDecision, ClientAdmissionLimits, ClientAdmissionRepository,
    ClientAdmissionRequest, RedisClientAdmissionRepository,
};
use serde_json::{Value, json};

use crate::support::{environment::Environment, native};

fn request(id: &str, source: &str, ttl: Duration) -> ClientAdmissionRequest {
    ClientAdmissionRequest {
        model_request_id: id.into(),
        client_api_key_ref: source.into(),
        lease_ttl: ttl,
        allow_concurrency_acquire: true,
        limits: ClientAdmissionLimits {
            max_concurrency: 8,
            requests_per_minute: 60,
        },
    }
}

async fn query(
    runtime: &gateway_plugin_runtime::PluginRuntime,
    core: &gateway_core::CoreBundle,
) -> Vec<Value> {
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
                path: "occupancy".into(),
                query: String::new(),
                content_type: None,
                body: vec![],
                request_id: "occupancy-fixture".into(),
            },
        )
        .await
        .unwrap();
    serde_json::from_slice(&response.body).unwrap()
}

#[tokio::test]
async fn shared_source_occupancy_tracks_current_binding_and_excludes_expired_leases() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let limits = RateLimits {
        max_concurrency: 8,
        requests_per_minute: 60,
    };
    for (id, limit) in [
        ("occupancy_a", 1),
        ("occupancy_b", 2),
        ("occupancy_x", 8),
        ("occupancy_y", 4),
    ] {
        environment
            .client_key_with_limits(
                id,
                &format!("sk-{id}"),
                RateLimits {
                    max_concurrency: limit,
                    requests_per_minute: if id == "occupancy_x" { 60 } else { limit * 10 },
                },
            )
            .await;
    }
    let keys = environment.store.admin_ports().client_keys();
    for id in ["occupancy_a", "occupancy_b"] {
        keys.change_limit_binding(
            ChangeClientLimitBinding {
                id: ClientApiKeyId::new(id).unwrap(),
                source_key_id: Some(ClientApiKeyId::new("occupancy_x").unwrap()),
                expected_revision: 0,
            },
            &crate::support::environment::mutation(),
            ClientLimitBindingMutationOrigin::Admin,
        )
        .await
        .unwrap();
    }
    let redis_url = std::env::var("CPR_PLUGIN_TEST_REDIS_URL").unwrap();
    let client = redis::Client::open(redis_url).unwrap();
    let connection = redis::aio::ConnectionManager::new(client).await.unwrap();
    let admission = RedisClientAdmissionRepository::new(connection, "codex-proxy-rs").unwrap();
    admission
        .clear_client_admission("occupancy_x")
        .await
        .unwrap();
    admission
        .clear_client_admission("occupancy_y")
        .await
        .unwrap();
    for id in ["one", "two", "three"] {
        assert_eq!(
            admission
                .admit_client_request(&request(id, "occupancy_x", Duration::from_secs(120)))
                .await
                .unwrap(),
            ClientAdmissionDecision::Granted
        );
    }
    admission
        .admit_client_request(&request(
            "expired",
            "occupancy_x",
            Duration::from_millis(40),
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (runtime, core) = environment
        .plugin(
            json!({
                "management_registration":{"routes":[{"method":"GET","path":"occupancy","request_content_types":[],"response_content_types":["application/json"]}]},
                "data_queries":[
                    {"method":"host.data.keys.get","query":{"client_key_id":"occupancy_a"}},
                    {"method":"host.data.keys.get","query":{"client_key_id":"occupancy_b"}},
                    {"method":"host.data.keys.get_occupancy","query":{"client_key_id":"occupancy_a"}},
                    {"method":"host.data.keys.get_occupancy","query":{"client_key_id":"occupancy_b"}}
                ]
            }),
            vec![PluginPermissionGrant { permission: "data".into() }],
        )
        .await;
    let access = gateway_admin::initialize_plugin_client_keys_with_admission(
        native::admin_registry(),
        keys.clone(),
        core.snapshot_control(),
        Some(environment.store.client_admission_reader()),
    );
    runtime.bind_client_key_ports(&access).unwrap();
    let first = query(&runtime, &core).await;
    assert_eq!(first[0]["configured_max_concurrency"], 1);
    assert_eq!(first[1]["configured_max_concurrency"], 2);
    for item in &first[..2] {
        assert_eq!(item["effective_source_key_id"], "occupancy_x");
        assert_eq!(item["effective_max_concurrency"], limits.max_concurrency);
        assert_eq!(
            item["effective_requests_per_minute"],
            limits.requests_per_minute
        );
    }
    for item in &first[2..] {
        assert_eq!(item["source_key_id"], "occupancy_x");
        assert_eq!(item["max_concurrency"], 8);
        assert_eq!(item["active_requests"], 3);
        assert!(item["observed_at_ms"].as_i64().unwrap() > 0);
    }
    keys.update_client_key(
        UpdateClientKey {
            request_profile_override_updates: Default::default(),
            id: ClientApiKeyId::new("occupancy_x").unwrap(),
            name: "fixture occupancy_x".into(),
            label: None,
            group_ids: vec![],
            limits: RateLimits {
                max_concurrency: 2,
                requests_per_minute: 60,
            },
            daily_limit_usd: None,
            weekly_limit_usd: None,
        },
        &crate::support::environment::mutation(),
    )
    .await
    .unwrap();
    let lowered = query(&runtime, &core).await;
    assert_eq!(lowered[2]["max_concurrency"], 2);
    assert_eq!(lowered[2]["active_requests"], 3);
    keys.change_limit_binding(
        ChangeClientLimitBinding {
            id: ClientApiKeyId::new("occupancy_a").unwrap(),
            source_key_id: Some(ClientApiKeyId::new("occupancy_y").unwrap()),
            expected_revision: 1,
        },
        &crate::support::environment::mutation(),
        ClientLimitBindingMutationOrigin::Admin,
    )
    .await
    .unwrap();
    let moved = query(&runtime, &core).await;
    assert_eq!(moved[2]["source_key_id"], "occupancy_y");
    assert_eq!(moved[2]["active_requests"], 0);
    assert_eq!(moved[2]["max_concurrency"], 4);
    assert_eq!(moved[3]["source_key_id"], "occupancy_x");
    assert_eq!(moved[3]["active_requests"], 3);
    assert_eq!(moved[3]["max_concurrency"], 2);
    for id in ["one", "two", "three"] {
        admission
            .release_client_request("occupancy_x", id)
            .await
            .unwrap();
    }
    let zero = query(&runtime, &core).await;
    assert_eq!(zero[3]["active_requests"], 0);
    admission
        .clear_client_admission("occupancy_x")
        .await
        .unwrap();
    admission
        .clear_client_admission("occupancy_y")
        .await
        .unwrap();
    environment.release_plugin_accounts(&runtime);
    runtime.shutdown().await;
    drop(core);
    drop(access);
    drop(runtime);
    environment.close().await;
}

struct UnavailableAdmission;

#[async_trait::async_trait]
impl PluginClientAdmissionReader for UnavailableAdmission {
    async fn read_active(
        &self,
        _: &ClientApiKeyId,
    ) -> Result<PluginClientAdmissionSnapshot, AdminError> {
        Err(AdminError::unavailable("Redis unavailable"))
    }
}

#[tokio::test]
async fn occupancy_failure_is_unknown_while_facts_remain_readable() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    environment
        .client_key("occupancy_unknown", "sk-unknown")
        .await;
    let (runtime, core) = environment
        .plugin(
            json!({
                "management_registration":{"routes":[{"method":"GET","path":"occupancy","request_content_types":[],"response_content_types":["application/json"]}]},
                "data_queries":[
                    {"method":"host.data.keys.get","query":{"client_key_id":"occupancy_unknown"}},
                    {"method":"host.data.keys.get_occupancy","query":{"client_key_id":"occupancy_unknown"}}
                ]
            }),
            vec![PluginPermissionGrant { permission: "data".into() }],
        )
        .await;
    let access = gateway_admin::initialize_plugin_client_keys_with_admission(
        native::admin_registry(),
        environment.store.admin_ports().client_keys(),
        core.snapshot_control(),
        Some(Arc::new(UnavailableAdmission)),
    );
    runtime.bind_client_key_ports(&access).unwrap();
    let values = query(&runtime, &core).await;
    assert_eq!(values[0]["client_key_id"], "occupancy_unknown");
    assert_eq!(values[1]["active_requests"], Value::Null);
    assert_eq!(values[1]["observed_at_ms"], Value::Null);
    environment.release_plugin_accounts(&runtime);
    runtime.shutdown().await;
    drop(core);
    drop(access);
    drop(runtime);
    environment.close().await;
}
