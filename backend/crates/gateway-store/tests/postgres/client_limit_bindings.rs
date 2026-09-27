use std::time::SystemTime;

use gateway_admin::{
    model::{
        MutationActor, MutationContext,
        client_keys::{
            ChangeClientLimitBinding, ClientKeyBudgetMutationOrigin, ClientKeyBudgetPeriod,
            DeleteClientKey, ResetClientKeyBudget,
        },
    },
    ports::store::{AdminStoreErrorKind, ClientKeyStore as _},
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
        .change_limit_binding(command("a", Some("x"), 0), &context())
        .await
        .unwrap();
    assert_eq!(first.revision, 1);
    let retry = store
        .change_limit_binding(command("a", Some("x"), 0), &context())
        .await
        .unwrap();
    assert_eq!(retry, first);
    let mut different_actor = context();
    different_actor.actor = MutationActor::AdminApiKey;
    assert_eq!(
        store
            .change_limit_binding(command("a", Some("x"), 0), &different_actor)
            .await
            .unwrap_err()
            .kind(),
        AdminStoreErrorKind::StaleRevision
    );
    assert!(
        store
            .change_limit_binding(command("x", Some("y"), 0), &context())
            .await
            .is_err()
    );
    assert!(
        store
            .change_limit_binding(command("b", Some("a"), 0), &context())
            .await
            .is_err()
    );
    assert!(
        store
            .change_limit_binding(command("b", Some("b"), 0), &context())
            .await
            .is_err()
    );
    let unbound = store
        .change_limit_binding(command("a", None, 1), &context())
        .await
        .unwrap();
    assert_eq!(unbound.revision, 2);
    assert_eq!(unbound.source_key_id, key("a"));
    assert_eq!(
        store
            .change_limit_binding(command("a", Some("x"), 0), &context())
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
        store.change_limit_binding(command("a", Some("b"), 0), &ctx),
        store.change_limit_binding(command("b", Some("a"), 0), &ctx)
    );
    assert_ne!(left.is_ok(), right.is_ok());
    sqlx::raw_sql("create function reject_binding_audit() returns trigger language plpgsql as $$ begin raise exception 'test audit failure'; end; $$; create trigger reject_binding_audit before insert on admin_audit_events for each row execute function reject_binding_audit();")
        .execute(&db.pool).await.unwrap();
    assert!(
        store
            .change_limit_binding(command("unrelated", Some("y"), 0), &ctx)
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
            .change_limit_binding(command(id, Some("x"), 0), &context())
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
        .change_limit_binding(command("a", Some("y"), 1), &context())
        .await
        .unwrap();
    budget.admit(admission("a", "y")).await.unwrap();
    store
        .change_limit_binding(command("b", None, 1), &context())
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

#[tokio::test]
async fn snapshot_reads_only_source_limits_and_disabled_source_rejects_new_admission() {
    let Some(db) = TestDatabase::create("limit_source_snapshot").await else {
        return;
    };
    seed(&db).await;
    let store = PgAdminClientKeyStore::new(db.pool.clone());
    sqlx::query("update client_api_keys set max_concurrency=99,requests_per_minute=88,weekly_limit_usd=9 where id='a'").execute(&db.pool).await.unwrap();
    store
        .change_limit_binding(command("a", Some("x"), 0), &context())
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
    assert_eq!(record.limits.max_concurrency, 99);
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
        .change_limit_binding(command("a", Some("x"), 0), &context())
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
        .change_limit_binding(command("a", Some("y"), 1), &context())
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
