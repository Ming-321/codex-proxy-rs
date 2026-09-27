use gateway_admin::{
    model::{
        client_keys::UpdateClientKey,
        plugins::{instances::PluginPermissionGrant, management::PluginManagementRequest},
    },
    ports::plugin_management::PluginManagement,
};
use gateway_core::policy::{ClientApiKeyId, RateLimits};
use serde_json::{Value, json};

#[tokio::test]
async fn key_facts_read_current_database_groups_without_secrets() {
    let Some(environment) = crate::support::environment::Environment::create().await else {
        return;
    };
    let account = environment.account(None).await;
    let group = environment.account_group_with_account(&account).await;
    let id = "key_facts_fixture";
    environment.client_key(id, "sk-test-key-facts-secret").await;
    let (runtime, core) = environment.plugin(json!({
        "management_registration":{"routes":[{"method":"GET","path":"facts","request_content_types":[],"response_content_types":["application/json"]}]},
        "data_queries":[
            {"method":"host.data.keys.get","query":{"client_key_id":id}},
            {"method":"host.data.keys.get","query":{"client_key_id":"key_missing"}},
            {"method":"host.data.keys.get","query":{"client_key_id":id,"secret":true}}
        ]
    }), vec![PluginPermissionGrant { permission: "data".into() }]).await;
    let access = gateway_admin::initialize_plugin_client_keys(
        crate::support::native::admin_registry(),
        environment.store.admin_ports().client_keys(),
        core.snapshot_control(),
    );
    runtime.bind_client_key_ports(&access).unwrap();
    let generation = core
        .snapshots()
        .acquire()
        .unwrap()
        .extensions()
        .unwrap()
        .clone();
    let view = runtime.views(&generation).await.unwrap().remove(0);
    for groups in [vec![], vec![group.clone()], vec![]] {
        environment
            .store
            .admin_ports()
            .client_keys()
            .update_client_key(
                UpdateClientKey {
                    request_profile_override_updates: Default::default(),
                    id: ClientApiKeyId::new(id).unwrap(),
                    name: "facts".into(),
                    label: None,
                    group_ids: groups.clone(),
                    limits: RateLimits::unlimited(),
                    daily_limit_usd: None,
                    weekly_limit_usd: None,
                },
                &crate::support::environment::mutation(),
            )
            .await
            .unwrap();
        let response = runtime
            .handle(
                &generation,
                &view.target,
                PluginManagementRequest {
                    method: "GET".into(),
                    path: "facts".into(),
                    query: String::new(),
                    content_type: None,
                    body: vec![],
                    request_id: "key-facts".into(),
                },
            )
            .await
            .unwrap();
        let result: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(
            result[0],
            json!({"schema_version":1,"client_key_id":id,"enabled":true,
            "group_ids": groups.iter().map(|id| id.as_str()).collect::<Vec<_>>() })
        );
        assert_eq!(result[1]["error"], "rejected");
        assert_eq!(result[2]["error"], "invalid_input");
    }
    drop(generation);
    runtime.shutdown().await;
    drop(core);
    drop(access);
    drop(runtime);
    environment.close().await;
}
