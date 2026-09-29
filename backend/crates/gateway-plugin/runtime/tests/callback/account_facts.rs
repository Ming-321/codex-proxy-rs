use gateway_admin::{
    model::plugins::{instances::PluginPermissionGrant, management::PluginManagementRequest},
    ports::plugin_management::PluginManagement,
};
use serde_json::{Value, json};

#[tokio::test]
async fn account_facts_use_current_page_and_live_lease_snapshot() {
    let Some(environment) = crate::support::environment::Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let id = environment
        .account_for_with_concurrency("openai", None, Some(3))
        .await;
    let snapshot = environment
        .store
        .admin_ports()
        .account_runtime()
        .account_runtime(&[id.as_str().to_owned()])
        .await
        .unwrap();
    assert_eq!(
        snapshot
            .in_flight
            .as_ref()
            .and_then(|counts| counts.get(id.as_str())),
        Some(&0)
    );
    let (runtime, core) = environment
        .plugin(
            json!({
                "management_registration":{"routes":[{"method":"GET","path":"facts","request_content_types":[],"response_content_types":["application/json"]}]},
                "data_queries":[{"method":"host.data.accounts.list","query":{"provider_id":"openai","cursor":null,"limit":1}}]
            }),
            vec![PluginPermissionGrant { permission: "data".into() }],
        )
        .await;
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
                path: "facts".into(),
                query: String::new(),
                content_type: None,
                body: vec![],
                request_id: "account-facts".into(),
            },
        )
        .await
        .unwrap();
    let result: Value = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(result[0]["accounts"][0]["account_id"], id.as_str());
    assert_eq!(result[0]["accounts"][0]["configured_concurrency_limit"], 3);
    assert_eq!(result[0]["accounts"][0]["effective_concurrency_limit"], 3);
    assert_eq!(result[0]["accounts"][0]["used_slots"], 0);
    assert!(result[0]["accounts"][0].get("credential").is_none());
    assert!(result[0]["accounts"][0].get("outbound_proxy").is_none());
    drop(generation);
    environment.release_plugin_accounts(&runtime);
    runtime.shutdown().await;
    drop(core);
    drop(runtime);
    environment.close().await;
}
