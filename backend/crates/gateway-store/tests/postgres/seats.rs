use std::time::{Duration, SystemTime};

use gateway_admin::{
    model::{
        MutationActor, MutationContext,
        account_groups::{JoinSeat, SaveSeat},
        client_keys::DeleteClientKey,
    },
    ports::store::{AccountGroupStore, ClientKeyStore},
};
use gateway_core::{
    engine::{
        ModelRequestId,
        budget::{ClientBudgetCharge, ClientBudgetLimits, ClientBudgetPort},
    },
    policy::{ClientApiKeyId, SeatId},
    routing::AccountGroupId,
};
use gateway_store::postgres::{
    ClientApiKeyRepository, PgAccountGroupRepository, PgAdminClientKeyStore,
    PgClientApiKeyRepository, PgClientBudgetStore,
};

use super::TestDatabase;

const GROUP: &str = "grp_00000000000000000000000000000001";
const SEAT: &str = "seat_00000000000000000000000000000001";

fn context() -> MutationContext {
    MutationContext {
        actor: MutationActor::System,
        request_id: "seat-test".to_owned(),
    }
}
fn key(value: &str) -> ClientApiKeyId {
    ClientApiKeyId::new(value).unwrap()
}
fn seat() -> SeatId {
    SeatId::new(SEAT).unwrap()
}
fn group() -> AccountGroupId {
    AccountGroupId::new(GROUP).unwrap()
}
fn command(id: &str, capacity: u64) -> SaveSeat {
    SaveSeat {
        id: Some(SeatId::new(id).unwrap()),
        group_id: group(),
        name: id.to_owned(),
        enabled: true,
        max_concurrency: capacity,
        limits: ClientBudgetLimits {
            daily_usd: "130".parse().unwrap(),
            weekly_usd: "260".parse().unwrap(),
        },
    }
}
fn join(ids: &[&str]) -> JoinSeat {
    JoinSeat {
        seat_id: seat(),
        key_ids: ids.iter().map(|id| key(id)).collect(),
    }
}
fn charge(id: &str, request: &str, amount: &str, shared: bool) -> ClientBudgetCharge {
    ClientBudgetCharge {
        key_id: key(id),
        seat_id: shared.then(seat),
        request_id: ModelRequestId::new(request).unwrap(),
        amount_usd: amount.parse().unwrap(),
        completed_at: SystemTime::now(),
    }
}
async fn setup(label: &str) -> Option<TestDatabase> {
    let db = TestDatabase::create(label).await?;
    sqlx::raw_sql("insert into provider_accounts (id, provider_kind, name, upstream_user_id, authentication_kind, provider_credentials_json, credential_revision, has_refresh_token, enabled, credential_state, credential_observed_at, created_at, updated_at, concurrency_limit)
        values ('acct_car', 'openai', 'car', 'car-user', 'oauth', '{}', 1, false, true, 'ready', now(), now(), now(), 3);
        insert into account_groups (id, name, color, created_at, updated_at) values ('grp_00000000000000000000000000000001', 'car', '#2563EBFF', now(), now());
        insert into account_group_accounts (account_group_id, provider_account_id, created_at) values ('grp_00000000000000000000000000000001', 'acct_car', now());")
        .execute(&db.pool).await.unwrap();
    for id in ["key_a", "key_b", "key_c"] {
        sqlx::query("insert into client_api_keys (id, name, key, daily_limit_usd, weekly_limit_usd, requests_per_minute, provider_request_profiles_json, created_at, updated_at) values ($1, $1, $2, 130, 260, 20, $3, now(), now())")
            .bind(id).bind(format!("sk_{id:a<43}"))
            .bind(serde_json::json!({"openai": {"testIdentity": id}})).execute(&db.pool).await.unwrap();
    }
    let store = PgAccountGroupRepository::new(db.pool.clone());
    store.convert_to_car(group(), &context()).await.unwrap();
    store.save_seat(command(SEAT, 2), &context()).await.unwrap();
    Some(db)
}

#[tokio::test]
async fn shared_budget_carries_usage_once_and_preserves_client_settings_and_revoked_history() {
    let Some(db) = setup("seat_shared_budget").await else {
        return;
    };
    let groups = PgAccountGroupRepository::new(db.pool.clone());
    let budgets = PgClientBudgetStore::new(db.pool.clone());
    for id in ["key_a", "key_b"] {
        budgets.admit(key(id), None).await.unwrap();
    }
    budgets
        .settle(charge("key_a", "req_before_a", "56.3", false))
        .await
        .unwrap();
    budgets
        .settle(charge("key_b", "req_before_b", "1.2", false))
        .await
        .unwrap();
    groups
        .join_seat(join(&["key_a", "key_b"]), &context())
        .await
        .unwrap();
    groups
        .join_seat(join(&["key_b", "key_a"]), &context())
        .await
        .unwrap();
    let shared = groups.list_seats(group()).await.unwrap().remove(0);
    assert_eq!(shared.budget.daily_used_usd.canonical(), "57.5");
    assert_eq!(shared.budget.limits.daily_usd.canonical(), "130");
    assert_eq!(shared.budget.limits.weekly_usd.canonical(), "260");
    let clients = PgClientApiKeyRepository::new(db.pool.clone());
    for id in ["key_a", "key_b"] {
        let record = clients.get_client_api_key(id).await.unwrap().unwrap();
        assert_eq!(record.budget.daily_used_usd, shared.budget.daily_used_usd);
        assert_eq!(record.groups[0].id, GROUP);
        let fields: (i64, serde_json::Value) = sqlx::query_as("select requests_per_minute, provider_request_profiles_json from client_api_keys where id = $1").bind(id).fetch_one(&db.pool).await.unwrap();
        assert_eq!(fields.0, 20);
        assert_eq!(fields.1["openai"]["testIdentity"], id);
    }
    assert!(budgets.admit(key("key_a"), None).await.is_err());
    let a = charge("key_a", "req_shared_a", "40", true);
    let b = charge("key_b", "req_shared_b", "40", true);
    let (left, right) = tokio::join!(budgets.settle(a.clone()), budgets.settle(b));
    left.unwrap();
    right.unwrap();
    budgets.settle(a).await.unwrap();
    for id in ["key_a", "key_b"] {
        assert!(budgets.admit(key(id), Some(seat())).await.is_err());
    }
    PgAdminClientKeyStore::new(db.pool.clone())
        .delete_client_key(DeleteClientKey { id: key("key_a") }, &context())
        .await
        .unwrap();
    budgets
        .settle(charge("key_a", "req_late", "1", true))
        .await
        .unwrap();
    assert!(clients.get_client_api_key("key_a").await.unwrap().is_none());
    let shared = groups.list_seats(group()).await.unwrap().remove(0);
    assert_eq!(shared.budget.daily_used_usd.canonical(), "138.5");
    assert_eq!(shared.key_count, 1);
    let events: i64 = sqlx::query_scalar(
        "select count(*) from client_key_charge_events where client_api_key_id = 'key_a'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(events, 3);
    db.close().await;
}

#[tokio::test]
async fn joining_rejects_live_admission_and_mismatched_windows_without_partial_transfer() {
    let Some(db) = setup("seat_join_guard").await else {
        return;
    };
    let groups = PgAccountGroupRepository::new(db.pool.clone());
    let budgets = PgClientBudgetStore::new(db.pool.clone());
    let pending = charge("key_a", "req_pending", "2", false);
    budgets
        .begin_request(
            key("key_a"),
            None,
            pending.request_id.clone(),
            SystemTime::now() + Duration::from_secs(60),
        )
        .await
        .unwrap();
    assert!(
        groups
            .join_seat(join(&["key_a"]), &context())
            .await
            .is_err()
    );
    budgets.settle(pending).await.unwrap();
    groups
        .join_seat(join(&["key_a"]), &context())
        .await
        .unwrap();
    budgets.admit(key("key_b"), None).await.unwrap();
    budgets
        .settle(charge("key_b", "req_offset", "3", false))
        .await
        .unwrap();
    sqlx::query("update client_key_budget_windows set weekly_start = weekly_start - interval '1 day', weekly_end = weekly_end - interval '1 day' where client_api_key_id = 'key_b'").execute(&db.pool).await.unwrap();
    assert!(
        groups
            .join_seat(join(&["key_b", "key_c"]), &context())
            .await
            .is_err()
    );
    let count: i64 =
        sqlx::query_scalar("select count(*) from client_api_keys where seat_id is not null")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
    assert_eq!(
        groups.list_seats(group()).await.unwrap()[0]
            .budget
            .daily_used_usd
            .canonical(),
        "2"
    );
    db.close().await;
}

#[tokio::test]
async fn car_integrity_rejects_excess_capacity_and_account_reassignment() {
    let Some(db) = setup("seat_integrity").await else {
        return;
    };
    let groups = PgAccountGroupRepository::new(db.pool.clone());
    assert!(
        groups
            .save_seat(command(SEAT, 4), &context())
            .await
            .is_err()
    );
    for suffix in [2, 3] {
        groups
            .save_seat(command(&format!("seat_{suffix:032x}"), 2), &context())
            .await
            .unwrap();
    }
    assert!(
        groups
            .save_seat(
                command("seat_00000000000000000000000000000004", 1),
                &context()
            )
            .await
            .is_err()
    );
    assert!(
        sqlx::query("update provider_accounts set concurrency_limit = 2 where id = 'acct_car'")
            .execute(&db.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("delete from account_group_accounts where provider_account_id = 'acct_car'")
            .execute(&db.pool)
            .await
            .is_err()
    );
    sqlx::query("insert into account_groups (id, name, color, created_at, updated_at) values ('grp_00000000000000000000000000000002', 'ordinary', '#2563EBFF', now(), now())").execute(&db.pool).await.unwrap();
    assert!(sqlx::query("insert into account_group_accounts (account_group_id, provider_account_id, created_at) values ('grp_00000000000000000000000000000002', 'acct_car', now())").execute(&db.pool).await.is_err());
    db.close().await;
}

#[tokio::test]
async fn car_capacity_stays_finite_with_unlimited_defaults_including_empty_cars() {
    let Some(db) = setup("car_unlimited_default").await else {
        return;
    };
    sqlx::query("update runtime_settings set max_concurrent_per_account = 0")
        .execute(&db.pool)
        .await
        .unwrap();
    let error =
        sqlx::query("update provider_accounts set concurrency_limit = null where id = 'acct_car'")
            .execute(&db.pool)
            .await
            .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().constraint(),
        Some("car_finite_capacity")
    );
    sqlx::query("delete from seats")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        sqlx::query("update provider_accounts set concurrency_limit = null where id = 'acct_car'")
            .execute(&db.pool)
            .await
            .is_err()
    );
    sqlx::query("update runtime_settings set max_concurrent_per_account = 3")
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("update provider_accounts set concurrency_limit = null where id = 'acct_car'")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        sqlx::query("update runtime_settings set max_concurrent_per_account = 0")
            .execute(&db.pool)
            .await
            .is_err()
    );
    let current: i64 =
        sqlx::query_scalar("select max_concurrent_per_account from runtime_settings")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(current, 3);
    db.close().await;
}

#[tokio::test]
async fn member_usage_tracks_shared_windows_and_preserves_revoked_costs_without_cross_seat_access()
{
    let Some(db) = setup("seat_member_usage").await else {
        return;
    };
    let groups = PgAccountGroupRepository::new(db.pool.clone());
    let budgets = PgClientBudgetStore::new(db.pool.clone());
    let clients = PgAdminClientKeyStore::new(db.pool.clone());
    groups
        .join_seat(join(&["key_a", "key_b"]), &context())
        .await
        .unwrap();
    let empty = clients.seat_key_usage(&key("key_a")).await.unwrap();
    assert_eq!(empty.len(), 2);
    assert!(empty.iter().all(|m| m.daily_used_usd.canonical() == "0"));
    budgets
        .settle(charge("key_a", "req_member_a", "2", true))
        .await
        .unwrap();
    budgets
        .settle(charge("key_b", "req_member_b", "3", true))
        .await
        .unwrap();
    clients
        .delete_client_key(DeleteClientKey { id: key("key_b") }, &context())
        .await
        .unwrap();
    budgets
        .settle(charge("key_b", "req_member_late", "1", true))
        .await
        .unwrap();
    let members = clients.seat_key_usage(&key("key_a")).await.unwrap();
    assert_eq!(members.len(), 2);
    let revoked = members.iter().find(|m| m.id == key("key_b")).unwrap();
    assert!(revoked.revoked);
    assert_eq!(revoked.daily_used_usd.canonical(), "4");
    assert_eq!(revoked.weekly_used_usd.canonical(), "4");
    assert_eq!(revoked.prefix.len(), 10);
    assert_eq!(
        groups.list_seats(group()).await.unwrap()[0]
            .budget
            .daily_used_usd
            .canonical(),
        "6"
    );
    assert!(
        clients
            .seat_key_usage(&key("key_b"))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        clients
            .seat_key_usage(&key("key_c"))
            .await
            .unwrap()
            .is_empty()
    );
    let other = "seat_00000000000000000000000000000002";
    groups
        .save_seat(command(other, 1), &context())
        .await
        .unwrap();
    groups
        .join_seat(
            JoinSeat {
                seat_id: SeatId::new(other).unwrap(),
                key_ids: vec![key("key_c")],
            },
            &context(),
        )
        .await
        .unwrap();
    let other_members = clients.seat_key_usage(&key("key_c")).await.unwrap();
    assert_eq!(other_members.len(), 1);
    assert_eq!(other_members[0].id, key("key_c"));
    // 日窗口已过期而周窗口仍有效：自助查询与共享预算都只清除日展示。
    sqlx::query("update seat_budget_windows set daily_start = daily_start - interval '1 day', daily_end = now() - interval '1 second' where seat_id = $1")
        .bind(SEAT).execute(&db.pool).await.unwrap();
    let members = clients.seat_key_usage(&key("key_a")).await.unwrap();
    assert!(members.iter().all(|m| m.daily_used_usd.canonical() == "0"));
    assert_eq!(
        members
            .iter()
            .find(|m| m.id == key("key_b"))
            .unwrap()
            .weekly_used_usd
            .canonical(),
        "4"
    );
    sqlx::query("update seat_budget_windows set weekly_end = now() - interval '1 second' where seat_id = $1")
        .bind(SEAT).execute(&db.pool).await.unwrap();
    assert!(
        clients
            .seat_key_usage(&key("key_a"))
            .await
            .unwrap()
            .iter()
            .all(|m| m.weekly_used_usd.canonical() == "0")
    );
    let persisted: String = sqlx::query_scalar(
        "select weekly_used_usd::text from seat_budget_windows where seat_id = $1",
    )
    .bind(SEAT)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        persisted
            .parse::<gateway_core::metering::Decimal>()
            .unwrap()
            .canonical(),
        "6"
    );
    db.close().await;
}

#[tokio::test]
async fn upstream_upgrade_preserves_existing_keys_budgets_and_migration_history() {
    let Some(db) = TestDatabase::create_through("seat_upstream_upgrade", 17).await else {
        return;
    };
    sqlx::raw_sql("insert into client_api_keys (id, name, key, daily_limit_usd, weekly_limit_usd, requests_per_minute, created_at, updated_at)
        values ('legacy_key', 'Legacy', 'sk_legacy_upgrade_fixture', 10, 50, 20, now(), now());
        insert into client_key_budget_windows (client_api_key_id, daily_start, daily_end, weekly_start, weekly_end, daily_used_usd, weekly_used_usd)
        values ('legacy_key', now() - interval '1 hour', now() + interval '23 hours', now() - interval '1 day', now() + interval '6 days', 2, 3);
        insert into client_key_charge_events (request_id, client_api_key_id, completed_at, amount_usd)
        values ('legacy_request', 'legacy_key', now(), 2);
        insert into account_groups (id, name, color, created_at, updated_at)
        values ('grp_00000000000000000000000000000099', 'Legacy group', '#2563EBFF', now(), now());
        insert into client_api_key_groups (client_api_key_id, account_group_id, created_at)
        values ('legacy_key', 'grp_00000000000000000000000000000099', now());
        update runtime_settings set max_concurrent_per_account = 0;")
        .execute(&db.pool).await.unwrap();
    let before: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("select version, checksum from _sqlx_migrations order by version")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    let budget_before: serde_json::Value = sqlx::query_scalar("select to_jsonb(w) from client_key_budget_windows w where client_api_key_id = 'legacy_key'").fetch_one(&db.pool).await.unwrap();
    let ledger_before: serde_json::Value = sqlx::query_scalar(
        "select to_jsonb(e) from client_key_charge_events e where request_id = 'legacy_request'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    let groups_before: serde_json::Value = sqlx::query_scalar(
        "select to_jsonb(g) from client_api_key_groups g where client_api_key_id = 'legacy_key'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    super::TEST_MIGRATOR.run(&db.pool).await.unwrap();
    super::TEST_MIGRATOR.run(&db.pool).await.unwrap();
    let after: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "select version, checksum from _sqlx_migrations where version <= 17 order by version",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(before, after);
    let budget_after: serde_json::Value = sqlx::query_scalar("select to_jsonb(w) from client_key_budget_windows w where client_api_key_id = 'legacy_key'").fetch_one(&db.pool).await.unwrap();
    assert_eq!(budget_before, budget_after);
    let ledger_after: serde_json::Value = sqlx::query_scalar("select to_jsonb(e) - 'seat_id' from client_key_charge_events e where request_id = 'legacy_request'").fetch_one(&db.pool).await.unwrap();
    assert_eq!(ledger_before, ledger_after);
    let groups_after: serde_json::Value = sqlx::query_scalar(
        "select to_jsonb(g) from client_api_key_groups g where client_api_key_id = 'legacy_key'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(groups_before, groups_after);
    let record = PgClientApiKeyRepository::new(db.pool.clone())
        .get_client_api_key("legacy_key")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.budget.daily_used_usd.canonical(), "2");
    assert_eq!(record.budget.weekly_used_usd.canonical(), "3");
    assert!(record.budget.seat.is_none());
    let unlimited: i64 =
        sqlx::query_scalar("select max_concurrent_per_account from runtime_settings")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(unlimited, 0);
    db.close().await;
}

#[tokio::test]
async fn recovery_preserves_seat_ownership_for_each_member_request() {
    use gateway_core::engine::admission::ClientAdmissionRecoveryPort;
    let Some(db) = setup("seat_recovery_facts").await else {
        return;
    };
    sqlx::query("insert into model_requests (id, client_api_key_ref, config_revision, protocol, operation, endpoint, client_transport, requested_model_id, outcome, started_at, deadline_at, routing_scope, routing_group_refs, routing_group_names_snapshot, seat_id)
        select 'req_running_' || id, id, 1, 'openai', 'responses', '/v1/responses', 'http_sse', 'coding', 'running', now() - interval '1 second', now() + interval '1 minute', 'all', '{}'::text[], '[]'::jsonb, $1
        from client_api_keys where id in ('key_a', 'key_b')")
        .bind(SEAT).execute(&db.pool).await.unwrap();
    let repository =
        gateway_store::postgres::PgClientAdmissionRecoveryRepository::new(db.pool.clone());
    let facts = repository
        .load_recovery(SystemTime::now() - Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(facts.len(), 2);
    for fact in facts {
        assert_eq!(
            fact.concurrency_id,
            gateway_core::policy::ClientConcurrencyId::Seat(seat())
        );
        assert_eq!(fact.running_requests.len(), 1);
        assert_eq!(
            fact.running_requests[0].model_request_id.as_str(),
            format!("req_running_{}", fact.client_api_key_id.as_str())
        );
    }
    db.close().await;
}

#[tokio::test]
async fn failed_sharing_migration_rolls_back_schema_and_rejects_changed_history() {
    let Some(db) = TestDatabase::create_through("seat_migration_atomic", 17).await else {
        return;
    };
    // 模拟迁移中途遇到名称冲突，确认前面已执行的 DDL 也回滚。
    sqlx::query("create table seat_budget_windows (sentinel text)")
        .execute(&db.pool)
        .await
        .unwrap();
    let mut connection = db.pool.acquire().await.unwrap();
    assert!(super::TEST_MIGRATOR.run(&mut *connection).await.is_err());
    connection.close().await.unwrap();
    let (count, has_seats, has_marker): (i64, bool, bool) = sqlx::query_as("select
        (select count(*) from _sqlx_migrations), to_regclass('seats') is not null,
        exists(select 1 from information_schema.columns where table_schema = current_schema() and table_name = 'account_groups' and column_name = 'is_car')")
        .fetch_one(&db.pool).await.unwrap();
    assert_eq!(count, 17);
    assert!(!has_seats);
    assert!(!has_marker);
    sqlx::query("drop table seat_budget_windows")
        .execute(&db.pool)
        .await
        .unwrap();
    super::TEST_MIGRATOR.run(&db.pool).await.unwrap();
    sqlx::query("update _sqlx_migrations set checksum = decode('aa', 'hex') where version = 17")
        .execute(&db.pool)
        .await
        .unwrap();
    let mut connection = db.pool.acquire().await.unwrap();
    let rejected = super::TEST_MIGRATOR.run(&mut *connection).await;
    connection.close().await.unwrap();
    assert!(matches!(
        rejected,
        Err(sqlx::migrate::MigrateError::VersionMismatch(17))
    ));
    db.close().await;
}
