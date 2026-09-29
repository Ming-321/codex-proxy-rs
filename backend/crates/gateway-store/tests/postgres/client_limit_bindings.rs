use std::time::SystemTime;

use gateway_admin::{
    model::{
        MutationActor, MutationContext,
        client_keys::{
            ChangeClientLimitBinding, ClientKeyBudgetMutationOrigin, ClientKeyBudgetPeriod,
            DeleteClientKey, ResetClientKeyBudget,
        },
    },
    ports::store::{AdminStoreErrorKind, AdminStoreResult, ClientKeyStore as _},
};
use gateway_core::{
    engine::{
        ModelRequestId,
        budget::{ClientBudgetAdmission, ClientBudgetCharge, ClientBudgetPort as _},
    },
    policy::ClientApiKeyId,
};
use gateway_store::postgres::{
    PgAdminClientKeyStore, PgClientBudgetStore, PgRuntimeSnapshotRepository,
    RuntimeSnapshotRepository as _,
};

use super::TestDatabase;

use gateway_admin::{
    model::{
        Revision,
        client_keys::ClientLimitBindingMutationOrigin,
        plugin_resources::PluginResourceOwner,
        plugins::{PluginSource, instances::PluginInstance},
    },
    ports::plugins::PluginStore as _,
};
use gateway_store::postgres::PgPluginStore;

async fn binding_plugin(db: &TestDatabase, digest: char) -> PluginResourceOwner {
    super::plugins::artifacts::initialize_revision(db).await;
    let plugins = PgPluginStore::new(db.pool.clone());
    let mut artifact = super::plugins::artifacts::artifact(digest, &["linux-x86_64"]);
    artifact.metadata.plugin_id = format!("test.binding{digest}");
    artifact.metadata.requested_permissions = vec!["key_limit_bindings".into()];
    let installed = plugins
        .install_artifact(artifact, PluginSource::Upload, &context())
        .await
        .unwrap();
    let accepted = plugins
        .accept_artifact(&installed.artifact.metadata.sha256, &context())
        .await
        .unwrap();
    let instance = plugins
        .save_instance(
            PluginInstance {
                id: uuid::Uuid::now_v7().to_string(),
                name: format!("binding {digest}"),
                artifact_sha256: installed.artifact.metadata.sha256,
                enabled: true,
                trusted_process: true,
                configuration: serde_json::json!({}),
                secrets: Default::default(),
                grants: vec![
                    gateway_admin::model::plugins::instances::PluginPermissionGrant {
                        permission: "key_limit_bindings".into(),
                    },
                ],
                bindings: vec![],
                revision: Revision::new(1).unwrap(),
            },
            accepted.config_revision,
            &context(),
        )
        .await
        .unwrap()
        .instance;
    PluginResourceOwner {
        instance_id: instance.id,
        artifact_sha256: instance.artifact_sha256,
        revision: instance.revision,
    }
}

async fn binding_counters(db: &TestDatabase) -> (i64, i64) {
    sqlx::query_as("select config_revision, (select count(*) from admin_audit_events where action='change_limit_binding') from runtime_settings where id=1")
        .fetch_one(&db.pool).await.unwrap()
}

#[tokio::test]
async fn plugin_binding_retry_identity_authorization_and_lifecycle_are_independent() {
    let Some(db) = TestDatabase::create("binding_plugin_contract").await else {
        return;
    };
    seed(&db).await;
    let owner = binding_plugin(&db, 'b').await;
    let other = binding_plugin(&db, 'c').await;
    let store = PgAdminClientKeyStore::new(db.pool.clone());
    let origin = || ClientLimitBindingMutationOrigin::Plugin(owner.clone());
    let first = store
        .change_limit_binding(command("a", Some("x"), 0), &context(), origin())
        .await
        .unwrap();
    let counters = binding_counters(&db).await;
    assert_eq!(
        store
            .change_limit_binding(command("a", Some("x"), 0), &context(), origin())
            .await
            .unwrap(),
        first
    );
    assert_eq!(binding_counters(&db).await, counters);
    assert_eq!(
        store
            .change_limit_binding(
                command("a", Some("x"), 0),
                &context(),
                ClientLimitBindingMutationOrigin::Plugin(other)
            )
            .await
            .unwrap_err()
            .kind(),
        AdminStoreErrorKind::StaleRevision
    );
    let audit: String = sqlx::query_scalar(
        "select actor_ref from admin_audit_events where action='change_limit_binding'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        audit,
        format!(
            "plugin:{}:revision:{}:artifact:{}",
            owner.instance_id,
            owner.revision.get(),
            owner.artifact_sha256
        )
    );
    // 当前代次有效也不能被当作上一代次丢失回包的重试。
    sqlx::query("update plugin_instances set revision=revision+1 where id=$1::uuid")
        .bind(&owner.instance_id)
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        store
            .change_limit_binding(command("a", Some("x"), 0), &context(), origin())
            .await
            .unwrap_err()
            .kind(),
        AdminStoreErrorKind::Conflict
    );
    let current = PluginResourceOwner {
        revision: Revision::new(owner.revision.get() + 1).unwrap(),
        ..owner.clone()
    };
    assert_eq!(
        store
            .change_limit_binding(
                command("a", Some("x"), 0),
                &context(),
                ClientLimitBindingMutationOrigin::Plugin(current.clone())
            )
            .await
            .unwrap_err()
            .kind(),
        AdminStoreErrorKind::StaleRevision
    );
    sqlx::query("update plugin_instances set revision=$2 where id=$1::uuid")
        .bind(&owner.instance_id)
        .bind(i64::try_from(owner.revision.get()).unwrap())
        .execute(&db.pool)
        .await
        .unwrap();
    let mut wrong_artifact = owner.clone();
    wrong_artifact.artifact_sha256 = "f".repeat(64);
    assert_eq!(
        store
            .change_limit_binding(
                command("a", Some("x"), 0),
                &context(),
                ClientLimitBindingMutationOrigin::Plugin(wrong_artifact)
            )
            .await
            .unwrap_err()
            .kind(),
        AdminStoreErrorKind::Conflict
    );
    for permission in ["[]", "[\"key_budgets\"]", "[\"keys\"]"] {
        sqlx::query("update plugin_artifacts set metadata_json=jsonb_set(metadata_json,'{requestedPermissions}',$2::jsonb) where sha256=$1")
            .bind(&owner.artifact_sha256).bind(permission).execute(&db.pool).await.unwrap();
        // 同时验证最近操作重试和新操作都必须复验授权。
        for change in [command("a", Some("x"), 0), command("a", Some("y"), 1)] {
            assert_eq!(
                store
                    .change_limit_binding(change, &context(), origin())
                    .await
                    .unwrap_err()
                    .kind(),
                AdminStoreErrorKind::Conflict
            );
        }
        assert_eq!(binding_counters(&db).await, counters);
        assert_eq!(store.get_limit_binding(&key("a")).await.unwrap(), first);
    }
    sqlx::query("update plugin_artifacts set metadata_json=jsonb_set(metadata_json,'{requestedPermissions}','[\"key_limit_bindings\"]'::jsonb), accepted_at=null where sha256=$1")
        .bind(&owner.artifact_sha256).execute(&db.pool).await.unwrap();
    assert!(
        store
            .change_limit_binding(command("a", Some("x"), 0), &context(), origin())
            .await
            .is_err()
    );
    sqlx::query("update plugin_artifacts set accepted_at=now() where sha256=$1")
        .bind(&owner.artifact_sha256)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("update plugin_instances set enabled=false where id=$1::uuid")
        .bind(&owner.instance_id)
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        store
            .change_limit_binding(command("a", Some("x"), 0), &context(), origin())
            .await
            .is_err()
    );
    assert_eq!(binding_counters(&db).await, counters);
    assert_eq!(store.get_limit_binding(&key("a")).await.unwrap(), first);
    PgPluginStore::new(db.pool.clone())
        .delete_instance(
            &owner.instance_id,
            Revision::new(u64::try_from(counters.0).unwrap()).unwrap(),
            &context(),
        )
        .await
        .unwrap();
    assert!(
        store
            .change_limit_binding(command("a", Some("x"), 0), &context(), origin())
            .await
            .is_err()
    );
    let retained = store.get_limit_binding(&key("a")).await.unwrap();
    assert_eq!(retained.source_key_id, first.source_key_id);
    assert_eq!(retained.revision, first.revision);
    assert_eq!(binding_counters(&db).await.1, counters.1);
    // 删除插件之后管理员仍可修改，不需要 takeover。
    store
        .change_limit_binding(
            command("a", None, 1),
            &context(),
            ClientLimitBindingMutationOrigin::Admin,
        )
        .await
        .unwrap();
    db.close().await;
}

#[tokio::test]
async fn binding_mutation_waiting_behind_revocation_cannot_commit_or_replay() {
    let Some(db) = TestDatabase::create("binding_revoke_race").await else {
        return;
    };
    seed(&db).await;
    let owner = binding_plugin(&db, 'b').await;
    let store = PgAdminClientKeyStore::new(db.pool.clone());
    store
        .change_limit_binding(
            command("a", Some("x"), 0),
            &context(),
            ClientLimitBindingMutationOrigin::Plugin(owner.clone()),
        )
        .await
        .unwrap();
    let before = binding_counters(&db).await;
    let mut revoke = db.pool.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("select pg_backend_pid()")
        .fetch_one(&mut *revoke)
        .await
        .unwrap();
    sqlx::query("select config_revision from runtime_settings where id=1 for update")
        .execute(&mut *revoke)
        .await
        .unwrap();
    sqlx::query("update plugin_instances set enabled=false,revision=revision+1 where id=$1::uuid")
        .bind(&owner.instance_id)
        .execute(&mut *revoke)
        .await
        .unwrap();
    let pool = db.pool.clone();
    let retry_owner = owner.clone();
    let waiting = tokio::spawn(async move {
        PgAdminClientKeyStore::new(pool)
            .change_limit_binding(
                command("a", Some("x"), 0),
                &context(),
                ClientLimitBindingMutationOrigin::Plugin(retry_owner),
            )
            .await
    });
    // 通过 PostgreSQL 锁事实确认重试已到达授权边界，不依赖固定延迟。
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let blocked: bool = sqlx::query_scalar(
                "select exists(select 1 from pg_stat_activity where $1=any(pg_blocking_pids(pid)))",
            )
            .bind(pid)
            .fetch_one(&mut *revoke)
            .await
            .unwrap();
            if blocked {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("binding retry reached revocation lock");
    revoke.commit().await.unwrap();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .kind(),
        AdminStoreErrorKind::Conflict
    );
    assert_eq!(binding_counters(&db).await, before);
    assert_eq!(
        store
            .get_limit_binding(&key("a"))
            .await
            .unwrap()
            .source_key_id,
        key("x")
    );
    assert!(
        store
            .change_limit_binding(
                command("a", Some("y"), 1),
                &context(),
                ClientLimitBindingMutationOrigin::Plugin(owner)
            )
            .await
            .is_err()
    );
    db.close().await;
}

fn key(id: &str) -> ClientApiKeyId {
    ClientApiKeyId::new(id).unwrap()
}
fn context() -> MutationContext {
    MutationContext {
        actor: MutationActor::System,
        request_id: "binding-test".to_owned(),
    }
}

#[tokio::test]
async fn migration_preserves_old_identity_without_guessing_independent_admission() {
    let Some(db) = TestDatabase::create_through("shared_upgrade", 19).await else {
        return;
    };
    sqlx::query("insert into model_requests(id,client_api_key_ref,config_revision,protocol,operation,endpoint,client_transport,started_at,deadline_at,routing_scope) values('req_legacy','legacy',1,'openai','responses','/v1/responses','http_sse',now(),now()+interval '1 minute','all')")
        .execute(&db.pool).await.unwrap();
    super::TEST_MIGRATOR.run(&db.pool).await.unwrap();
    let facts:(String,bool)=sqlx::query_as("select limit_source_key_ref,client_admission_acquired from model_requests where id='req_legacy'").fetch_one(&db.pool).await.unwrap();
    assert_eq!(facts, ("legacy".to_owned(), false));
    db.close().await;
}
fn command(id: &str, source: Option<&str>, revision: u64) -> ChangeClientLimitBinding {
    ChangeClientLimitBinding {
        id: key(id),
        source_key_id: source.map(key),
        expected_revision: revision,
    }
}
fn admission(id: &str, source: &str) -> ClientBudgetAdmission {
    ClientBudgetAdmission {
        client_key_id: key(id),
        source_key_id: key(source),
    }
}
fn charge(id: &str, source: &str, request: &str, amount: &str) -> ClientBudgetCharge {
    ClientBudgetCharge {
        client_key_ref: key(id),
        key_id: key(source),
        request_id: ModelRequestId::new(request).unwrap(),
        amount_usd: amount.parse().unwrap(),
        completed_at: SystemTime::now(),
    }
}
async fn seed(db: &TestDatabase) {
    for id in ["a", "b", "x", "y", "unrelated"] {
        sqlx::query("insert into client_api_keys(id,name,key,weekly_limit_usd,max_concurrency,requests_per_minute,created_at,updated_at) values($1,$1,$2,1,2,3,now(),now())")
            .bind(id).bind(format!("sk_{id:0<43}")).execute(&db.pool).await.unwrap();
    }
}

#[tokio::test]
async fn member_edits_are_atomic_and_detach_keeps_current_effective_limits() {
    use gateway_admin::model::client_keys::UpdateClientKey;
    let Some(db) = TestDatabase::create("shared_member_edits").await else {
        return;
    };
    seed(&db).await;
    sqlx::query("update client_api_keys set daily_limit_usd=7,weekly_limit_usd=9,max_concurrency=99,requests_per_minute=88 where id='a'")
        .execute(&db.pool).await.unwrap();
    sqlx::query("update client_api_keys set daily_limit_usd=10,weekly_limit_usd=20 where id='x'")
        .execute(&db.pool)
        .await
        .unwrap();
    let store = PgAdminClientKeyStore::new(db.pool.clone());
    store
        .change_limit_binding(
            command("a", Some("x"), 0),
            &context(),
            ClientLimitBindingMutationOrigin::Admin,
        )
        .await
        .unwrap();
    let record = store.get_client_key(&key("a")).await.unwrap().unwrap();
    assert_eq!(record.limits.max_concurrency, 2);
    assert_eq!(record.limits.requests_per_minute, 3);
    assert_eq!(record.budget.limits.daily_usd.canonical(), "10");
    let edit = UpdateClientKey {
        id: key("a"),
        name: "member renamed".into(),
        label: Some("independent".into()),
        group_ids: vec![],
        request_profile_override_updates: Default::default(),
        limits: record.limits,
        daily_limit_usd: Some("10.00".parse().unwrap()),
        weekly_limit_usd: Some("20.0".parse().unwrap()),
    };
    store
        .update_client_key(edit.clone(), &context())
        .await
        .unwrap();
    let local: (String,String,i64,i64) = sqlx::query_as("select daily_limit_usd::text,weekly_limit_usd::text,max_concurrency,requests_per_minute from client_api_keys where id='a'").fetch_one(&db.pool).await.unwrap();
    assert_eq!(local.2, 99);
    assert_eq!(local.3, 88);
    let before: (i64,i64) = sqlx::query_as("select config_revision,(select count(*) from admin_audit_events) from runtime_settings where id=1").fetch_one(&db.pool).await.unwrap();
    for field in 0..4 {
        let mut invalid = edit.clone();
        invalid.name = "must not save".into();
        let label = match field {
            0 => {
                invalid.limits.max_concurrency = 4;
                "并发上限"
            }
            1 => {
                invalid.limits.requests_per_minute = 5;
                "RPM 上限"
            }
            2 => {
                invalid.daily_limit_usd = Some("11".parse().unwrap());
                "日预算上限"
            }
            _ => {
                invalid.weekly_limit_usd = Some("21".parse().unwrap());
                "周预算上限"
            }
        };
        let error = store
            .update_client_key(invalid, &context())
            .await
            .unwrap_err();
        assert_eq!(error.kind(), AdminStoreErrorKind::Conflict);
        assert!(error.message().contains(label));
        assert!(error.message().contains("「x」"));
        assert!(error.message().contains("本次修改未保存"));
        let after: (i64,i64) = sqlx::query_as("select config_revision,(select count(*) from admin_audit_events) from runtime_settings where id=1").fetch_one(&db.pool).await.unwrap();
        assert_eq!(before, after);
        assert_eq!(
            store.get_client_key(&key("a")).await.unwrap().unwrap().name,
            "member renamed"
        );
    }
    // 来源变化后，成员查询立即跟随；旧表单携带旧上限不得保存独立属性。
    let mut source_edit = edit.clone();
    source_edit.id = key("x");
    source_edit.name = "source updated".into();
    source_edit.limits.max_concurrency = 6;
    source_edit.daily_limit_usd = Some("30".parse().unwrap());
    store
        .update_client_key(source_edit, &context())
        .await
        .unwrap();
    assert!(
        store
            .update_client_key(edit.clone(), &context())
            .await
            .is_err()
    );
    let member = store.get_client_key(&key("a")).await.unwrap().unwrap();
    assert_eq!(member.limits.max_concurrency, 6);
    assert_eq!(member.budget.limits.daily_usd.canonical(), "30");
    // 解绑不能迁账或开启窗口；之后来源变化与独立编辑互不影响。
    let windows_before: i64 = sqlx::query_scalar("select count(*) from client_key_budget_windows")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    store
        .change_limit_binding(
            command("a", None, 1),
            &context(),
            ClientLimitBindingMutationOrigin::Admin,
        )
        .await
        .unwrap();
    let detached = store.get_client_key(&key("a")).await.unwrap().unwrap();
    assert!(detached.limit_source.is_none());
    assert_eq!(detached.limits, member.limits);
    assert_eq!(detached.budget.limits, member.budget.limits);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("select count(*) from client_key_budget_windows")
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        windows_before
    );
    sqlx::query("update client_api_keys set max_concurrency=70 where id='x'")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        store
            .get_client_key(&key("a"))
            .await
            .unwrap()
            .unwrap()
            .limits
            .max_concurrency,
        6
    );
    store.update_client_key(edit, &context()).await.unwrap();
    // 换源直接跟随新来源；零上限沿用不限语义。
    sqlx::query("update client_api_keys set daily_limit_usd=0,weekly_limit_usd=0,max_concurrency=0,requests_per_minute=0 where id='y'").execute(&db.pool).await.unwrap();
    store
        .change_limit_binding(
            command("a", Some("x"), 2),
            &context(),
            ClientLimitBindingMutationOrigin::Admin,
        )
        .await
        .unwrap();
    store
        .change_limit_binding(
            command("a", Some("y"), 3),
            &context(),
            ClientLimitBindingMutationOrigin::Admin,
        )
        .await
        .unwrap();
    let member = store.get_client_key(&key("a")).await.unwrap().unwrap();
    assert_eq!(member.limits.max_concurrency, 0);
    store
        .change_limit_binding(
            command("a", None, 4),
            &context(),
            ClientLimitBindingMutationOrigin::Admin,
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .get_client_key(&key("a"))
            .await
            .unwrap()
            .unwrap()
            .budget
            .limits
            .weekly_usd
            .canonical(),
        "0"
    );
    db.close().await;
}

#[tokio::test]
async fn binding_cas_retry_chains_and_anchor_retention_are_transactional() {
    let Some(db) = TestDatabase::create("limit_binding_contract").await else {
        return;
    };
    seed(&db).await;
    let store = PgAdminClientKeyStore::new(db.pool.clone());
    assert_eq!(
        store.get_limit_binding(&key("a")).await.unwrap().revision,
        0
    );
    let first = store
        .change_limit_binding(
            command("a", Some("x"), 0),
            &context(),
            gateway_admin::model::client_keys::ClientLimitBindingMutationOrigin::Admin,
        )
        .await
        .unwrap();
    assert_eq!(first.revision, 1);
    let retry = store
        .change_limit_binding(
            command("a", Some("x"), 0),
            &context(),
            gateway_admin::model::client_keys::ClientLimitBindingMutationOrigin::Admin,
        )
        .await
        .unwrap();
    assert_eq!(retry, first);
    let mut different_actor = context();
    different_actor.actor = MutationActor::AdminApiKey;
    assert_eq!(
        store
            .change_limit_binding(
                command("a", Some("x"), 0),
                &different_actor,
                gateway_admin::model::client_keys::ClientLimitBindingMutationOrigin::Admin
            )
            .await
            .unwrap_err()
            .kind(),
        AdminStoreErrorKind::StaleRevision
    );
    assert!(
        store
            .change_limit_binding(
                command("x", Some("y"), 0),
                &context(),
                gateway_admin::model::client_keys::ClientLimitBindingMutationOrigin::Admin
            )
            .await
            .is_err()
    );
    assert!(
        store
            .change_limit_binding(
                command("b", Some("a"), 0),
                &context(),
                gateway_admin::model::client_keys::ClientLimitBindingMutationOrigin::Admin
            )
            .await
            .is_err()
    );
    assert!(
        store
            .change_limit_binding(
                command("b", Some("b"), 0),
                &context(),
                gateway_admin::model::client_keys::ClientLimitBindingMutationOrigin::Admin
            )
            .await
            .is_err()
    );
    let unbound = store
        .change_limit_binding(
            command("a", None, 1),
            &context(),
            gateway_admin::model::client_keys::ClientLimitBindingMutationOrigin::Admin,
        )
        .await
        .unwrap();
    assert_eq!(unbound.revision, 2);
    assert_eq!(unbound.source_key_id, key("a"));
    assert_eq!(
        store
            .change_limit_binding(
                command("a", Some("x"), 0),
                &context(),
                gateway_admin::model::client_keys::ClientLimitBindingMutationOrigin::Admin
            )
            .await
            .unwrap_err()
            .kind(),
        AdminStoreErrorKind::StaleRevision
    );
    assert!(
        store
            .delete_client_key(DeleteClientKey { id: key("x") }, &context())
            .await
            .is_err()
    );
    let count: i64 = sqlx::query_scalar(
        "select count(*) from admin_audit_events where action='change_limit_binding'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(count, 2);
    db.close().await;
}

#[tokio::test]
async fn racing_bindings_cannot_create_cycles_and_audit_failure_rolls_back() {
    let Some(db) = TestDatabase::create("limit_binding_races").await else {
        return;
    };
    seed(&db).await;
    let store = PgAdminClientKeyStore::new(db.pool.clone());
    let ctx = context();
    let (left, right) = tokio::join!(
        store.change_limit_binding(
            command("a", Some("b"), 0),
            &ctx,
            gateway_admin::model::client_keys::ClientLimitBindingMutationOrigin::Admin
        ),
        store.change_limit_binding(
            command("b", Some("a"), 0),
            &ctx,
            gateway_admin::model::client_keys::ClientLimitBindingMutationOrigin::Admin
        )
    );
    assert_ne!(left.is_ok(), right.is_ok());
    sqlx::raw_sql("create function reject_binding_audit() returns trigger language plpgsql as $$ begin raise exception 'test audit failure'; end; $$; create trigger reject_binding_audit before insert on admin_audit_events for each row execute function reject_binding_audit();")
        .execute(&db.pool).await.unwrap();
    assert!(
        store
            .change_limit_binding(
                command("unrelated", Some("y"), 0),
                &ctx,
                gateway_admin::model::client_keys::ClientLimitBindingMutationOrigin::Admin
            )
            .await
            .is_err()
    );
    assert_eq!(
        store
            .get_limit_binding(&key("unrelated"))
            .await
            .unwrap()
            .revision,
        0
    );
    let anchor: bool = sqlx::query_scalar("select limit_anchor from client_api_keys where id='y'")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert!(!anchor);
    db.close().await;
}

#[tokio::test]
async fn shared_budget_preserves_identity_history_and_late_charge_after_rebind_and_delete() {
    let Some(db) = TestDatabase::create("shared_budget").await else {
        return;
    };
    seed(&db).await;
    let store = PgAdminClientKeyStore::new(db.pool.clone());
    let budget = PgClientBudgetStore::new(db.pool.clone());
    for id in ["a", "b"] {
        store
            .change_limit_binding(
                command(id, Some("x"), 0),
                &context(),
                gateway_admin::model::client_keys::ClientLimitBindingMutationOrigin::Admin,
            )
            .await
            .unwrap();
    }
    budget.admit(admission("a", "x")).await.unwrap();
    budget
        .settle(charge("a", "x", "req_a", "0.4"))
        .await
        .unwrap();
    budget.admit(admission("b", "x")).await.unwrap();
    budget
        .settle(charge("b", "x", "req_b", "0.6"))
        .await
        .unwrap();
    assert!(budget.admit(admission("a", "x")).await.is_err());
    budget
        .admit(admission("unrelated", "unrelated"))
        .await
        .unwrap();
    assert_eq!(
        store
            .get_client_key(&key("a"))
            .await
            .unwrap()
            .unwrap()
            .budget
            .weekly_used_usd
            .canonical(),
        "1"
    );
    assert!(
        store
            .reset_client_key_budget(
                ResetClientKeyBudget {
                    id: key("a"),
                    period: ClientKeyBudgetPeriod::All
                },
                ClientKeyBudgetMutationOrigin::Admin,
                &context()
            )
            .await
            .is_err()
    );
    store
        .change_limit_binding(
            command("a", Some("y"), 1),
            &context(),
            gateway_admin::model::client_keys::ClientLimitBindingMutationOrigin::Admin,
        )
        .await
        .unwrap();
    budget.admit(admission("a", "y")).await.unwrap();
    store
        .change_limit_binding(
            command("b", None, 1),
            &context(),
            gateway_admin::model::client_keys::ClientLimitBindingMutationOrigin::Admin,
        )
        .await
        .unwrap();
    store
        .delete_client_key(DeleteClientKey { id: key("a") }, &context())
        .await
        .unwrap();
    sqlx::query("update client_api_keys set enabled=false where id='x'")
        .execute(&db.pool)
        .await
        .unwrap();
    let late = charge("a", "x", "req_late", "0.3");
    budget.settle(late.clone()).await.unwrap();
    budget.settle(late).await.unwrap();
    assert_eq!(
        store
            .get_limit_binding(&key("x"))
            .await
            .unwrap()
            .budget
            .weekly_used_usd
            .canonical(),
        "1.3"
    );
    assert_eq!(
        store
            .get_limit_binding(&key("y"))
            .await
            .unwrap()
            .budget
            .weekly_used_usd
            .canonical(),
        "0"
    );
    let identity:(String,String)=sqlx::query_as("select client_key_ref,client_api_key_id from client_key_charge_events where request_id='req_late'").fetch_one(&db.pool).await.unwrap();
    assert_eq!(identity, ("a".to_owned(), "x".to_owned()));
    assert!(
        store
            .delete_client_key(DeleteClientKey { id: key("x") }, &context())
            .await
            .is_err()
    );
    db.close().await;
}

async fn reset_budget(
    store: &PgAdminClientKeyStore,
    id: &str,
    period: ClientKeyBudgetPeriod,
) -> AdminStoreResult<()> {
    store
        .reset_client_key_budget(
            ResetClientKeyBudget {
                id: key(id),
                period,
            },
            ClientKeyBudgetMutationOrigin::Admin,
            &context(),
        )
        .await
}

async fn weekly_used(store: &PgAdminClientKeyStore, id: &str) -> String {
    store
        .get_client_key(&key(id))
        .await
        .unwrap()
        .unwrap()
        .budget
        .weekly_used_usd
        .canonical()
}

/// 逐表取出成员与来源相关的持久状态，用于证明被拒绝的重置没有留下任何修改。
async fn shared_budget_state(db: &TestDatabase) -> serde_json::Value {
    sqlx::query_scalar(
        "select jsonb_build_object(
            'windows', (select coalesce(jsonb_agg(to_jsonb(w) order by w.client_api_key_id), '[]'::jsonb) from client_key_budget_windows w),
            'charges', (select coalesce(jsonb_agg(to_jsonb(c) order by c.request_id), '[]'::jsonb) from client_key_charge_events c),
            'bindings', (select coalesce(jsonb_agg(to_jsonb(b) order by b.client_api_key_id), '[]'::jsonb) from client_key_limit_bindings b),
            'keys', (select coalesce(jsonb_agg(to_jsonb(k) - 'key' order by k.id), '[]'::jsonb) from client_api_keys k),
            'config_revision', (select config_revision from runtime_settings where id = 1),
            'audits', (select coalesce(jsonb_agg(to_jsonb(a) order by a.id), '[]'::jsonb) from admin_audit_events a)
        )",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn member_budget_reset_names_the_source_and_leaves_shared_state_untouched() {
    let Some(db) = TestDatabase::create("shared_budget_reset_rejection").await else {
        return;
    };
    seed(&db).await;
    // 来源名称与 ID 不同，提示必须展示管理员在页面上看到的名称。
    sqlx::query("update client_api_keys set name='Shared Source' where id='x'")
        .execute(&db.pool)
        .await
        .unwrap();
    let store = PgAdminClientKeyStore::new(db.pool.clone());
    let budget = PgClientBudgetStore::new(db.pool.clone());
    store
        .change_limit_binding(
            command("a", Some("x"), 0),
            &context(),
            ClientLimitBindingMutationOrigin::Admin,
        )
        .await
        .unwrap();
    budget.admit(admission("a", "x")).await.unwrap();
    budget
        .settle(charge("a", "x", "req_shared", "0.4"))
        .await
        .unwrap();
    // unrelated 不参与共享，用来对照原有的重置行为。
    budget
        .admit(admission("unrelated", "unrelated"))
        .await
        .unwrap();
    budget
        .settle(charge("unrelated", "unrelated", "req_plain", "0.7"))
        .await
        .unwrap();
    assert_eq!(weekly_used(&store, "a").await, "0.4");

    let expected = "此 Key 共用来源 Key「Shared Source」的已用额度，不能在此重置，请重置来源。";
    let before = shared_budget_state(&db).await;
    for period in [
        ClientKeyBudgetPeriod::Daily,
        ClientKeyBudgetPeriod::Weekly,
        ClientKeyBudgetPeriod::All,
    ] {
        let error = reset_budget(&store, "a", period).await.unwrap_err();
        assert_eq!(error.kind(), AdminStoreErrorKind::Conflict);
        assert_eq!(error.resource(), "controlled client limits");
        assert_eq!(error.message(), expected);
        assert_eq!(shared_budget_state(&db).await, before);
    }

    // 拒绝成员不改变其他 Key 的重置：未共享 Key 与来源本身仍按原语义清零并写审计。
    reset_budget(&store, "unrelated", ClientKeyBudgetPeriod::All)
        .await
        .unwrap();
    assert_eq!(weekly_used(&store, "unrelated").await, "0");
    assert_eq!(weekly_used(&store, "a").await, "0.4");
    reset_budget(&store, "x", ClientKeyBudgetPeriod::All)
        .await
        .unwrap();
    assert_eq!(weekly_used(&store, "a").await, "0");

    // 解绑后绑定行仍保留但来源为空，成员回到独立 Key，不应再被拒绝重置。
    store
        .change_limit_binding(
            command("a", None, 1),
            &context(),
            ClientLimitBindingMutationOrigin::Admin,
        )
        .await
        .unwrap();
    let detached: i64 = sqlx::query_scalar(
        "select count(*) from client_key_limit_bindings where client_api_key_id='a' and source_key_id is null",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(detached, 1);
    reset_budget(&store, "a", ClientKeyBudgetPeriod::All)
        .await
        .unwrap();
    let audits: i64 =
        sqlx::query_scalar("select count(*) from admin_audit_events where action='reset_budget'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(audits, 3);
    db.close().await;
}

#[tokio::test]
async fn snapshot_reads_only_source_limits_and_disabled_source_rejects_new_admission() {
    let Some(db) = TestDatabase::create("limit_source_snapshot").await else {
        return;
    };
    seed(&db).await;
    let store = PgAdminClientKeyStore::new(db.pool.clone());
    sqlx::query("update client_api_keys set max_concurrency=99,requests_per_minute=88,weekly_limit_usd=9 where id='a'").execute(&db.pool).await.unwrap();
    store
        .change_limit_binding(
            command("a", Some("x"), 0),
            &context(),
            gateway_admin::model::client_keys::ClientLimitBindingMutationOrigin::Admin,
        )
        .await
        .unwrap();
    sqlx::query("update client_api_keys set enabled=false where id='x'")
        .execute(&db.pool)
        .await
        .unwrap();
    let snapshot = PgRuntimeSnapshotRepository::new(db.pool.clone())
        .load_runtime_snapshot()
        .await
        .unwrap();
    let a = snapshot
        .client_api_keys
        .iter()
        .find(|record| record.id == key("a"))
        .unwrap();
    assert_eq!(
        a.plaintext_key.expose_for_auth(),
        format!("sk_{:0<43}", "a")
    );
    let source = a.limit_source.as_ref().unwrap();
    assert_eq!(source.key_id, key("x"));
    assert!(!source.enabled);
    assert_eq!(source.limits.max_concurrency, 2);
    let record = store.get_client_key(&key("a")).await.unwrap().unwrap();
    assert_eq!(record.limits.max_concurrency, 2);
    assert_eq!(record.local_budget_limits.weekly_usd.canonical(), "9");
    assert_eq!(record.budget.limits.weekly_usd.canonical(), "1");
    let budget = PgClientBudgetStore::new(db.pool.clone());
    assert!(budget.admit(admission("a", "x")).await.is_err());
    sqlx::query("update client_api_keys set enabled=(id<>'a') where id in ('a','x')")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(budget.admit(admission("a", "x")).await.is_err());
    db.close().await;
}

#[tokio::test]
async fn recovery_restores_original_source_in_real_redis_without_nested_double_counting() {
    use gateway_core::engine::admission::{
        ClientAdmissionDecision, ClientAdmissionPort as _, ClientAdmissionRecoveryPort as _,
        ClientAdmissionRejection, ClientAdmissionRequest,
    };
    use gateway_store::postgres::{PgClientAdmissionRecoveryRepository, PgExecutionStore};
    use gateway_store::redis::RedisClientAdmissionRepository;
    use std::time::Duration;
    let Some(redis_url) = crate::support::test_env("CPR_TEST_REDIS_URL") else {
        return;
    };
    let Some(db) = TestDatabase::create("shared_recovery_redis").await else {
        return;
    };
    seed(&db).await;
    let store = PgAdminClientKeyStore::new(db.pool.clone());
    store
        .change_limit_binding(
            command("a", Some("x"), 0),
            &context(),
            gateway_admin::model::client_keys::ClientLimitBindingMutationOrigin::Admin,
        )
        .await
        .unwrap();
    let execution = PgExecutionStore::new(db.pool.clone());
    // 持久化独立准入和继承名额的子执行，二者身份相同、来源均为旧来源 X。
    for (id, acquired) in [("req_parent", true), ("req_nested", false)] {
        let mut request = super::execution::accepted_request(id);
        request.client_api_key_id = Some(key("a"));
        request.client_api_key_ref = key("a");
        request.limit_source_key_ref = key("x");
        request.client_admission_acquired = acquired;
        gateway_core::engine::ExecutionStore::create_model_request(&execution, request)
            .await
            .unwrap();
    }
    store
        .change_limit_binding(
            command("a", Some("y"), 1),
            &context(),
            gateway_admin::model::client_keys::ClientLimitBindingMutationOrigin::Admin,
        )
        .await
        .unwrap();
    let facts = PgClientAdmissionRecoveryRepository::new(db.pool.clone())
        .load_recovery(SystemTime::now() - Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].client_api_key_id, key("x"));
    assert_eq!(facts[0].running_requests.len(), 1);
    assert_eq!(facts[0].recent_requests.len(), 1);
    let client = redis::Client::open(redis_url).unwrap();
    let connection = redis::aio::ConnectionManager::new(client).await.unwrap();
    let redis = RedisClientAdmissionRepository::new(
        connection,
        &format!("shared_limits_{}", uuid::Uuid::new_v4().simple()),
    )
    .unwrap();
    let restored = redis
        .restore(facts.into_iter().next().unwrap())
        .await
        .unwrap();
    assert_eq!(restored.restored_running_requests, 1);
    let request = |id: &str, source: &str, rpm| ClientAdmissionRequest {
        model_request_id: ModelRequestId::new(id).unwrap(),
        client_api_key_id: key(source),
        lease_ttl: Duration::from_secs(30),
        allow_concurrency_acquire: true,
        limits: gateway_core::policy::RateLimits {
            max_concurrency: 1,
            requests_per_minute: rpm,
        },
    };
    assert_eq!(
        redis.admit(request("req_blocked", "x", 3)).await.unwrap(),
        ClientAdmissionDecision::Rejected(ClientAdmissionRejection::ConcurrencyLimited)
    );
    assert_eq!(
        redis.admit(request("req_y", "y", 3)).await.unwrap(),
        ClientAdmissionDecision::Granted
    );
    assert_eq!(
        redis
            .admit(request("req_unrelated", "unrelated", 3))
            .await
            .unwrap(),
        ClientAdmissionDecision::Granted
    );
    assert!(
        redis
            .release(&key("x"), &ModelRequestId::new("req_parent").unwrap())
            .await
            .unwrap()
    );
    assert_eq!(
        redis.admit(request("req_next", "x", 2)).await.unwrap(),
        ClientAdmissionDecision::Granted
    );
    assert!(
        redis
            .release(&key("x"), &ModelRequestId::new("req_next").unwrap())
            .await
            .unwrap()
    );
    assert_eq!(
        redis.admit(request("req_rate", "x", 2)).await.unwrap(),
        ClientAdmissionDecision::Rejected(ClientAdmissionRejection::RateLimited)
    );
    use gateway_store::redis::ClientAdmissionRepository as _;
    for source in ["x", "y", "unrelated"] {
        redis.clear_client_admission(source).await.unwrap();
    }
    db.close().await;
}
