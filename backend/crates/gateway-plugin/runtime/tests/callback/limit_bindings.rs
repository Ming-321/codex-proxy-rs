use gateway_admin::{PluginManagementService, model::plugins::management::PluginManagementRequest};
use gateway_core::policy::ClientApiKeyId;
use gateway_plugin_sdk::Permission;
use serde_json::{Value, json};

use crate::support::{
    environment::{Environment, account_grant},
    native,
};

#[tokio::test]
async fn plugin_binding_callbacks_require_their_own_permission_and_preserve_native_facts() {
    // 逐一授予已有域，证明预算、Key 创建和数据权限均不能隐含修改共享关系。
    for grant in std::iter::once(None).chain(Permission::ALL.into_iter().map(Some)) {
        let Some(environment) = Environment::create().await else {
            eprintln!("SKIP: plugin integration environment absent");
            return;
        };
        for (id, secret) in [
            ("member", "sk-member-fixture"),
            ("source", "sk-source-fixture"),
            ("next", "sk-next-fixture"),
        ] {
            environment.client_key(id, secret).await;
            environment.seed_client_key_budget(id).await;
        }
        let store = environment.store.admin_ports().client_keys();
        let member = ClientApiKeyId::new("member").unwrap();
        let before = store.get_client_key(&member).await.unwrap().unwrap();
        environment.install_plugin(json!({
            "management_registration":{"routes":[{"method":"POST","path":"binding","request_content_types":[],"response_content_types":["application/json"]}]},
            "data_queries":[
                {"method":"host.keys.get_limit_binding","query":{"client_key_id":"member"}},
                {"method":"host.keys.change_limit_binding","query":{"client_key_id":"member","source_key_id":"source","expected_revision":0}},
                {"method":"host.keys.change_limit_binding","query":{"client_key_id":"member","source_key_id":"source","expected_revision":0}},
                {"method":"host.keys.change_limit_binding","query":{"client_key_id":"member","source_key_id":"next","expected_revision":0}},
                {"method":"host.keys.get_limit_binding","query":{"client_key_id":"member"}},
                {"method":"host.keys.change_limit_binding","query":{"client_key_id":"member","source_key_id":"next","expected_revision":1}},
                {"method":"host.keys.change_limit_binding","query":{"client_key_id":"member","expected_revision":2}},
                {"method":"host.keys.change_limit_binding","query":{"client_key_id":"member","source_key_id":null,"expected_revision":2}},
                {"method":"host.keys.get_limit_binding","query":{"client_key_id":"member","instance_id":"forged"}},
                {"method":"host.keys.change_limit_binding","query":{"client_key_id":"member","source_key_id":"source","expected_revision":3,"instance_id":"forged"}},
                {"method":"host.keys.get_limit_binding","query":{"client_key_id":"missing"}},
                {"method":"host.keys.change_limit_binding","query":{"client_key_id":"member","source_key_id":"source","expected_revision":3}},
                {"method":"host.keys.get_budget","query":{"client_key_id":"member"}}
            ]
        }), grant.map(|p| vec![account_grant(p.as_str())]).unwrap_or_default()).await;
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
        let view = service.views().await.unwrap().remove(0);
        let reply = service
            .handle(
                &view.target,
                PluginManagementRequest {
                    method: "POST".into(),
                    path: "binding".into(),
                    query: String::new(),
                    content_type: None,
                    body: vec![],
                    request_id: "binding-fixture".into(),
                },
            )
            .await
            .unwrap();
        let results: Vec<Value> = serde_json::from_slice(&reply.body).unwrap();
        let binding = store.get_limit_binding(&member).await.unwrap();
        if grant == Some(Permission::KeyLimitBindings) {
            assert_eq!(results[0]["source_key_id"], "member");
            assert_eq!(results[0]["revision"], 0);
            assert_eq!(results[0]["binding_config_revision"], Value::Null);
            assert_eq!(results[1], results[2]);
            assert_eq!(results[1], results[4]);
            assert_eq!(results[1]["source_key_id"], "source");
            assert_eq!(results[1]["revision"], 1);
            assert_eq!(results[1].as_object().unwrap().len(), 7);
            let committed = results[1]["binding_config_revision"].as_u64().unwrap();
            assert_eq!(results[1]["config_revision"], committed);
            // 测试快照端口只通知发布，不伪造已加载全部新配置。
            assert!(
                results[1]["loaded_config_revision"]
                    .as_u64()
                    .is_none_or(|r| r <= committed)
            );
            assert_eq!(results[3], json!({"error":"conflict"}));
            assert_eq!(results[5]["source_key_id"], "next");
            assert_eq!(results[7]["source_key_id"], "member");
            for i in [6, 8, 9] {
                assert_eq!(results[i], json!({"error":"invalid_input"}));
            }
            assert_eq!(results[10], json!({"error":"rejected"}));
            assert_eq!(results[11]["source_key_id"], "source");
            assert_eq!(results[12], json!({"error":"permission_denied"}));
            assert_eq!(binding.revision, 4);
            assert_eq!(binding.source_key_id.as_str(), "source");
            let audits = environment.audit_requests("change_limit_binding").await;
            assert_eq!(audits.len(), 4);
            assert!(
                audits
                    .iter()
                    .all(|r| r.starts_with(&format!("plugin:{}:scope:", view.target.instance_id)))
            );
        } else {
            assert!(
                results[..12]
                    .iter()
                    .all(|r| r == &json!({"error":"permission_denied"})),
                "grant: {grant:?}"
            );
            assert_eq!(binding.revision, 0);
            assert!(
                environment
                    .audit_requests("change_limit_binding")
                    .await
                    .is_empty()
            );
        }
        let after = store.get_client_key(&member).await.unwrap().unwrap();
        assert_eq!(after.local_budget_limits, before.local_budget_limits);
        assert_eq!(after.groups, before.groups);
        assert_eq!(
            after.request_profile_overrides,
            before.request_profile_overrides
        );
        assert!(!std::str::from_utf8(&reply.body).unwrap().contains("sk-"));
        drop(service);
        environment.release_plugin_accounts(&runtime);
        runtime.shutdown().await;
        // 进程退出不会清理宿主持久关系。
        assert_eq!(store.get_limit_binding(&member).await.unwrap(), binding);
        drop(access);
        drop(core);
        drop(runtime);
        drop(store);
        environment.close().await;
    }
}
