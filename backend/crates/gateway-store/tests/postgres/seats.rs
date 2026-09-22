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
