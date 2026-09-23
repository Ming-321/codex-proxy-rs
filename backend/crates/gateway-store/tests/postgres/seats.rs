use std::time::{Duration, SystemTime};

use gateway_admin::{
    model::{
        MutationActor, MutationContext,
        account_groups::{CarQuotaMode, CarQuotaObservation, JoinSeat, SaveCarWeights, SaveSeat},
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

fn observation(
    cycle_start: chrono::DateTime<chrono::Utc>,
    cycle_end: chrono::DateTime<chrono::Utc>,
    observed_at: chrono::DateTime<chrono::Utc>,
    used_percent_millis: u32,
) -> CarQuotaObservation {
    CarQuotaObservation {
        group_id: group(),
        window_key: "primary".to_owned(),
        cycle_start,
        cycle_end,
        observed_at,
        used_percent_millis,
        predicted_capacity_usd: None,
        prediction_reason: None,
        sample_start: None,
        sample_end: None,
        sample_percent_millis: None,
        cost_complete: true,
        pending_request_count: 0,
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
        weight: "1".parse().unwrap(),
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
    setup_through(label, i64::MAX).await
}
async fn setup_through(label: &str, version: i64) -> Option<TestDatabase> {
    let db = TestDatabase::create_through(label, version).await?;
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
    groups
        .save_car_weights(
            SaveCarWeights {
                group_id: group(),
                total_weight: "3".parse().unwrap(),
            },
            &context(),
        )
        .await
        .unwrap();
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
        .save_car_weights(
            SaveCarWeights {
                group_id: group(),
                total_weight: "2".parse().unwrap(),
            },
            &context(),
        )
        .await
        .unwrap();
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

#[tokio::test]
async fn account_cycle_activates_weighted_limits_and_requires_two_early_reset_observations() {
    let Some(db) = setup("car_account_cycle").await else {
        return;
    };
    let groups = PgAccountGroupRepository::new(db.pool.clone());
    let budgets = PgClientBudgetStore::new(db.pool.clone());
    budgets.admit(key("key_a"), None).await.unwrap();
    budgets
        .settle(charge("key_a", "req_cycle_history", "7", false))
        .await
        .unwrap();
    groups
        .save_car_weights(
            SaveCarWeights {
                group_id: group(),
                total_weight: "5".parse().unwrap(),
            },
            &context(),
        )
        .await
        .unwrap();
    let mut zym = command(SEAT, 2);
    zym.weight = "2".parse().unwrap();
    groups.save_seat(zym, &context()).await.unwrap();
    let mut wrh = command("seat_00000000000000000000000000000002", 1);
    wrh.weight = "3".parse().unwrap();
    wrh.limits.daily_usd = "200".parse().unwrap();
    groups.save_seat(wrh, &context()).await.unwrap();

    let observed = chrono::DateTime::from_timestamp_micros(chrono::Utc::now().timestamp_micros())
        .expect("current time is representable");
    let start = observed - chrono::Duration::days(1);
    let end = observed + chrono::Duration::days(6);
    let first = observation(start, end, observed, 60_000);
    let state = groups.reconcile_car_quota(first.clone()).await.unwrap();
    assert_eq!(state.mode, CarQuotaMode::Active);
    assert_eq!(state.published_capacity_usd.canonical(), "650");
    assert_eq!(state.account_used_percent_millis, Some(60_000));
    let seats = groups.list_seats(group()).await.unwrap();
    assert_eq!(seats[0].budget.limits.weekly_usd.canonical(), "260");
    assert_eq!(seats[1].budget.limits.weekly_usd.canonical(), "390");
    groups
        .join_seat(join(&["key_a"]), &context())
        .await
        .unwrap();
    let migrated = groups.list_seats(group()).await.unwrap().remove(0);
    assert_eq!(migrated.budget.weekly_used_usd.canonical(), "7");
    assert_eq!(
        migrated
            .budget
            .weekly_resets_at
            .map(chrono::DateTime::<chrono::Utc>::from)
            .map(|value| value.timestamp_micros()),
        Some(end.timestamp_micros())
    );

    let stale = groups.reconcile_car_quota(first).await.unwrap();
    assert_eq!(stale.cycle_end, Some(end));

    let next_start = observed + chrono::Duration::hours(1);
    let next_end = next_start + chrono::Duration::days(7);
    let candidate = observation(
        next_start,
        next_end,
        next_start + chrono::Duration::hours(1),
        5_000,
    );
    let waiting = groups.reconcile_car_quota(candidate).await.unwrap();
    assert_eq!(waiting.cycle_end, Some(end));
    assert_eq!(
        waiting.prediction_reason.as_deref(),
        Some("提前重置等待再次确认")
    );

    let confirmed = groups
        .reconcile_car_quota(observation(
            next_start,
            next_end,
            next_start + chrono::Duration::hours(2),
            6_000,
        ))
        .await
        .unwrap();
    assert_eq!(confirmed.cycle_start, Some(next_start));
    assert_eq!(confirmed.cycle_end, Some(next_end));
    let old_window = groups
        .reconcile_car_quota(observation(
            start,
            end,
            next_start + chrono::Duration::hours(3),
            70_000,
        ))
        .await
        .unwrap();
    assert_eq!(old_window.cycle_start, Some(next_start));
    assert_eq!(old_window.cycle_end, Some(next_end));
    db.close().await;
}

#[tokio::test]
async fn expired_account_cycle_blocks_new_requests_but_settles_and_recovers_from_the_ledger() {
    let Some(db) = setup("car_expired_cycle").await else {
        return;
    };
    let groups = PgAccountGroupRepository::new(db.pool.clone());
    let budgets = PgClientBudgetStore::new(db.pool.clone());
    groups
        .join_seat(join(&["key_a", "key_b"]), &context())
        .await
        .unwrap();
    let now =
        chrono::DateTime::from_timestamp_micros(chrono::Utc::now().timestamp_micros()).unwrap();
    let end = now - chrono::Duration::minutes(1);
    let start = end - chrono::Duration::days(7);
    groups
        .reconcile_car_quota(observation(
            start,
            end,
            end - chrono::Duration::minutes(2),
            60_000,
        ))
        .await
        .unwrap();
    let mut old_charge = charge("key_a", "req_old_cycle", "7", true);
    old_charge.completed_at = (end - chrono::Duration::minutes(1)).into();
    budgets.settle(old_charge.clone()).await.unwrap();
    for id in ["key_a", "key_b"] {
        let error = budgets
            .begin_request(
                key(id),
                Some(seat()),
                ModelRequestId::new(format!("req_blocked_{id}")).unwrap(),
                SystemTime::now() + Duration::from_secs(60),
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.client_error_code(),
            Some("seat_cycle_confirmation_pending")
        );
    }
    let pending: i64 = sqlx::query_scalar("select count(*) from client_budget_admissions")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(pending, 0);
    let late_charge = charge("key_b", "req_late_cycle", "3", true);
    budgets.settle(late_charge.clone()).await.unwrap();
    budgets.settle(late_charge).await.unwrap();
    let old = groups.list_seats(group()).await.unwrap().remove(0);
    assert_eq!(old.budget.weekly_used_usd.canonical(), "7");
    assert_eq!(old.budget.weekly_resets_at, Some(end.into()));
    let key_view = PgClientApiKeyRepository::new(db.pool.clone())
        .get_client_api_key("key_a")
        .await
        .unwrap()
        .unwrap();
    assert!(key_view.budget.seat.unwrap().account_cycle);
    assert_eq!(key_view.budget.weekly_used_usd.canonical(), "7");
    let new_end = end + chrono::Duration::days(7);
    groups
        .reconcile_car_quota(observation(end, new_end, now, 2_000))
        .await
        .unwrap();
    for id in ["key_a", "key_b"] {
        budgets.admit(key(id), Some(seat())).await.unwrap();
    }
    budgets.settle(old_charge).await.unwrap();
    let new = groups.list_seats(group()).await.unwrap().remove(0);
    assert_eq!(new.budget.weekly_used_usd.canonical(), "3");
    assert_eq!(new.budget.weekly_resets_at, Some(new_end.into()));
    let events: i64 = sqlx::query_scalar("select count(*) from client_key_charge_events")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(events, 2);
    db.close().await;
}

#[tokio::test]
async fn capacity_publication_blends_clamps_and_confirms_abnormal_samples() {
    let Some(db) = setup("car_capacity_publication").await else {
        return;
    };
    let groups = PgAccountGroupRepository::new(db.pool.clone());
    groups
        .save_car_weights(
            SaveCarWeights {
                group_id: group(),
                total_weight: "5".parse().unwrap(),
            },
            &context(),
        )
        .await
        .unwrap();
    let start = "2026-09-01T00:00:00Z".parse().unwrap();
    let end = "2026-09-08T00:00:00Z".parse().unwrap();
    let mut insufficient = observation(start, end, "2026-09-02T00:00:00Z".parse().unwrap(), 10_000);
    insufficient.predicted_capacity_usd = Some("700".parse().unwrap());
    insufficient.sample_start = Some(start);
    insufficient.sample_end = Some("2026-09-02T00:00:00Z".parse().unwrap());
    insufficient.sample_percent_millis = Some(5_000);
    let held = groups.reconcile_car_quota(insufficient).await.unwrap();
    assert_eq!(held.published_capacity_usd.canonical(), "650");

    let mut ordinary = observation(start, end, "2026-09-03T00:00:00Z".parse().unwrap(), 20_000);
    ordinary.predicted_capacity_usd = Some("700".parse().unwrap());
    ordinary.sample_start = Some("2026-09-02T00:00:00Z".parse().unwrap());
    ordinary.sample_end = Some("2026-09-03T00:00:00Z".parse().unwrap());
    ordinary.sample_percent_millis = Some(20_000);
    let published = groups.reconcile_car_quota(ordinary).await.unwrap();
    assert_eq!(published.published_capacity_usd.canonical(), "665");
    assert!(
        (chrono::Utc::now() - published.published_at.unwrap())
            .num_seconds()
            .abs()
            < 10
    );

    let mut too_soon = observation(start, end, "2026-09-03T01:00:00Z".parse().unwrap(), 22_000);
    too_soon.predicted_capacity_usd = Some("720".parse().unwrap());
    too_soon.sample_start = Some("2026-09-03T00:00:00Z".parse().unwrap());
    too_soon.sample_end = Some("2026-09-03T01:00:00Z".parse().unwrap());
    too_soon.sample_percent_millis = Some(12_000);
    let held = groups.reconcile_car_quota(too_soon).await.unwrap();
    assert_eq!(held.published_capacity_usd.canonical(), "665");

    // 只推进测试库的上次发布时间；新观测本身不能绕过实际6小时发布间隔。
    sqlx::query("update car_quota_cycles set published_at = now() - interval '6 hours'")
        .execute(&db.pool)
        .await
        .unwrap();
    let mut abnormal = observation(start, end, "2026-09-04T00:00:00Z".parse().unwrap(), 35_000);
    abnormal.predicted_capacity_usd = Some("1000".parse().unwrap());
    abnormal.sample_start = Some("2026-09-03T00:00:00Z".parse().unwrap());
    abnormal.sample_end = Some("2026-09-04T00:00:00Z".parse().unwrap());
    abnormal.sample_percent_millis = Some(15_000);
    let held = groups.reconcile_car_quota(abnormal).await.unwrap();
    assert_eq!(held.published_capacity_usd.canonical(), "665");
    assert_eq!(
        held.prediction_reason.as_deref(),
        Some("异常变化等待独立样本确认")
    );

    let mut confirmed = observation(start, end, "2026-09-05T00:00:00Z".parse().unwrap(), 50_000);
    confirmed.predicted_capacity_usd = Some("950".parse().unwrap());
    confirmed.sample_start = Some("2026-09-04T00:00:00Z".parse().unwrap());
    confirmed.sample_end = Some("2026-09-05T00:00:00Z".parse().unwrap());
    confirmed.sample_percent_millis = Some(15_000);
    let published = groups.reconcile_car_quota(confirmed).await.unwrap();
    assert_eq!(published.published_capacity_usd.canonical(), "731.5");
    db.close().await;
}

#[tokio::test]
async fn publication_preserves_guards_but_accepts_partial_costs() {
    let Some(db) = setup("car_publication_guards").await else {
        return;
    };
    let groups = PgAccountGroupRepository::new(db.pool.clone());
    sqlx::query("update car_quota_settings set automatic_updates = false")
        .execute(&db.pool)
        .await
        .unwrap();
    let start = "2026-09-01T00:00:00Z".parse().unwrap();
    let end = "2026-09-08T00:00:00Z".parse().unwrap();
    let mut sample = observation(start, end, "2026-09-02T00:00:00Z".parse().unwrap(), 20_000);
    sample.predicted_capacity_usd = Some("150".parse().unwrap());
    sample.sample_start = Some(start);
    sample.sample_end = Some(sample.observed_at);
    sample.sample_percent_millis = Some(20_000);
    let first = groups.reconcile_car_quota(sample.clone()).await.unwrap();
    assert_eq!(first.mode, CarQuotaMode::Active);
    assert_eq!(first.published_capacity_usd.canonical(), "130");
    assert_eq!(
        groups.list_seats(group()).await.unwrap()[0]
            .budget
            .limits
            .weekly_usd
            .canonical(),
        "130"
    );
    sqlx::query("update car_quota_settings set automatic_updates = true")
        .execute(&db.pool)
        .await
        .unwrap();
    for (cost_complete, pending, sampled) in
        [(false, 1, 20_000), (true, 1, 20_000), (true, 0, 5_000)]
    {
        sample.observed_at += chrono::Duration::hours(1);
        sample.sample_end = Some(sample.observed_at);
        sample.cost_complete = cost_complete;
        sample.pending_request_count = pending;
        sample.sample_percent_millis = Some(sampled);
        assert_eq!(
            groups
                .reconcile_car_quota(sample.clone())
                .await
                .unwrap()
                .published_capacity_usd
                .canonical(),
            "130"
        );
    }
    sample.observed_at += chrono::Duration::hours(1);
    sample.sample_end = Some(sample.observed_at);
    sample.cost_complete = false;
    sample.pending_request_count = 0;
    sample.sample_percent_millis = Some(20_000);
    let published = groups.reconcile_car_quota(sample.clone()).await.unwrap();
    assert_eq!(published.published_capacity_usd.canonical(), "136");
    assert_eq!(
        groups.list_seats(group()).await.unwrap()[0]
            .budget
            .limits
            .weekly_usd
            .canonical(),
        "136"
    );
    sample.observed_at += chrono::Duration::hours(1);
    sample.sample_end = Some(sample.observed_at);
    sample.predicted_capacity_usd = Some("160".parse().unwrap());
    let waiting = groups.reconcile_car_quota(sample).await.unwrap();
    assert_eq!(waiting.published_capacity_usd.canonical(), "136");
    assert!(waiting.prediction_reason.unwrap().contains("间隔"));
    db.close().await;
}

#[tokio::test]
async fn expired_account_cycle_preserves_revoked_member_usage_until_confirmation() {
    let Some(db) = setup("car_member_cycle").await else {
        return;
    };
    let groups = PgAccountGroupRepository::new(db.pool.clone());
    let budgets = PgClientBudgetStore::new(db.pool.clone());
    groups
        .join_seat(join(&["key_a", "key_b"]), &context())
        .await
        .unwrap();
    budgets
        .settle(charge("key_a", "req_cycle_a", "2", true))
        .await
        .unwrap();
    budgets
        .settle(charge("key_b", "req_cycle_b", "3", true))
        .await
        .unwrap();
    sqlx::raw_sql("update client_key_charge_events set completed_at = now() - interval '1 hour';
        update seat_budget_windows set weekly_start = now() - interval '1 day', weekly_end = now() - interval '1 minute';
        update client_api_keys set revoked_at = now(), enabled = false where id = 'key_b';
        update account_groups set car_quota_mode = 'active' where is_car;
        update car_quota_cycles set cycle_start = now() - interval '1 day', cycle_end = now() - interval '1 minute';")
        .execute(&db.pool).await.unwrap();
    let clients = PgAdminClientKeyStore::new(db.pool.clone());
    let members = clients.seat_key_usage(&key("key_a")).await.unwrap();
    assert_eq!(members.len(), 2);
    let revoked = members.iter().find(|m| m.id == key("key_b")).unwrap();
    assert!(revoked.revoked);
    assert_eq!(revoked.weekly_used_usd.canonical(), "3");
    assert_eq!(
        groups.list_seats(group()).await.unwrap()[0]
            .budget
            .weekly_used_usd
            .canonical(),
        "5"
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
    // 同样的过期数据在legacy模式按基础窗口展示为零，不清空底层账本。
    sqlx::query("update account_groups set car_quota_mode = 'legacy'")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        clients
            .seat_key_usage(&key("key_a"))
            .await
            .unwrap()
            .iter()
            .all(|m| m.weekly_used_usd == "0".parse().unwrap())
    );
    let count: i64 = sqlx::query_scalar("select count(*) from client_key_charge_events")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 2);
    db.close().await;
}

#[tokio::test]
async fn cycle_upgrade_backfills_multiple_seat_weights_without_replacing_manual_budgets() {
    let Some(db) = TestDatabase::create_through("cycle_upgrade", 18).await else {
        return;
    };
    sqlx::raw_sql("begin;
        insert into provider_accounts (id, provider_kind, name, upstream_user_id, authentication_kind, provider_credentials_json, credential_revision, has_refresh_token, enabled, credential_state, credential_observed_at, created_at, updated_at, concurrency_limit)
        values ('acct_car', 'openai', 'car', 'car-user', 'oauth', '{}', 1, false, true, 'ready', now(), now(), now(), 3);
        insert into account_groups (id,name,color,is_car,created_at,updated_at) values ('grp_00000000000000000000000000000001','car','#2563EBFF',true,now(),now());
        insert into account_group_accounts(account_group_id,provider_account_id,created_at) values ('grp_00000000000000000000000000000001','acct_car',now());
        insert into seats(id,account_group_id,name,enabled,max_concurrency,daily_limit_usd,weekly_limit_usd,created_at,updated_at)
        values ('seat_00000000000000000000000000000001','grp_00000000000000000000000000000001','one',true,1,10,50,now(),now()),
        ('seat_00000000000000000000000000000002','grp_00000000000000000000000000000001','two',false,1,10,80,now(),now());
        commit;")
        .execute(&db.pool).await.unwrap();
    let before: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("select version,checksum from _sqlx_migrations order by version")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    super::TEST_MIGRATOR.run(&db.pool).await.unwrap();
    super::TEST_MIGRATOR.run(&db.pool).await.unwrap();
    let after: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "select version,checksum from _sqlx_migrations where version <= 18 order by version",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(before, after);
    let groups = PgAccountGroupRepository::new(db.pool.clone());
    let state = groups.load_car_quota_state(group()).await.unwrap();
    assert_eq!(state.total_weight.canonical(), "2");
    assert_eq!(state.published_capacity_usd.canonical(), "260");
    assert_eq!(state.mode, CarQuotaMode::Legacy);
    let seats = groups.list_seats(group()).await.unwrap();
    assert_eq!(seats[0].budget.limits.weekly_usd.canonical(), "50");
    assert_eq!(seats[1].budget.limits.weekly_usd.canonical(), "80");
    let now = chrono::Utc::now();
    let active = groups
        .reconcile_car_quota(observation(
            now - chrono::Duration::days(1),
            now + chrono::Duration::days(6),
            now,
            20_000,
        ))
        .await
        .unwrap();
    assert_eq!(active.mode, CarQuotaMode::Active);
    assert!(
        groups.list_seats(group()).await.unwrap().iter().all(|s| s
            .budget
            .limits
            .weekly_usd
            .canonical()
            == "130")
    );
    db.close().await;
}
