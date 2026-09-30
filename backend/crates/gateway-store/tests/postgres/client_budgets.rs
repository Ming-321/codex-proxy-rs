use std::{collections::BTreeMap, time::SystemTime};

use chrono::{DateTime, Utc};
use gateway_admin::{
    model::{
        MutationActor, MutationContext, Revision,
        client_keys::{
            ChangeClientKeyWeeklyWindow, ClientKeyBudgetMutationOrigin, ClientKeyBudgetPeriod,
            ClientKeyWeeklyControl, ClientKeyWeeklyWindowAction, ResetClientKeyBudget,
            UpdateClientKey, UpdateClientKeyBudgetLimits,
        },
        plugin_resources::PluginResourceOwner,
        plugins::{
            PluginSource,
            instances::{PluginInstance, PluginInstanceReplacement},
            state::{PluginStateCommit, PluginStateConfiguration},
        },
    },
    ports::{
        plugins::PluginStore as _,
        store::{AdminStoreErrorKind, ClientKeyStore as _},
    },
};
use gateway_core::{
    engine::{
        ModelRequestId,
        budget::{ClientBudgetCharge, ClientBudgetPort, ClientBudgetStatus},
    },
    error::GatewayErrorKind,
    policy::{ClientApiKeyId, RateLimits},
};
use gateway_store::postgres::{
    ClientApiKeyRepository as _, PgAdminClientKeyStore, PgClientApiKeyRepository,
    PgClientBudgetStore, PgPluginStore,
};

use super::TestDatabase;

fn key_id(key: &str) -> ClientApiKeyId {
    ClientApiKeyId::new(key).unwrap()
}

fn charge(key: &str, request: &str, amount: &str) -> ClientBudgetCharge {
    ClientBudgetCharge {
        key_id: key_id(key),
        request_id: ModelRequestId::new(format!("req_{request}")).unwrap(),
        amount_usd: amount.parse().unwrap(),
        completed_at: SystemTime::now(),
    }
}

async fn seed(database: &TestDatabase, key: &str, daily: &str, weekly: &str) {
    sqlx::query(
        "insert into client_api_keys (id, name, key, daily_limit_usd, weekly_limit_usd, created_at, updated_at)
        values ($1, $1, $2, $3::text::numeric, $4::text::numeric, now(), now())",
    )
    .bind(key)
    .bind(format!("sk_{key:a<43}"))
    .bind(daily)
    .bind(weekly)
    .execute(&database.pool)
    .await
    .unwrap();
}

async fn status(database: &TestDatabase, key: &str) -> ClientBudgetStatus {
    PgClientApiKeyRepository::new(database.pool.clone())
        .get_client_api_key(key)
        .await
        .unwrap()
        .unwrap()
        .budget
}

fn context() -> MutationContext {
    MutationContext {
        actor: MutationActor::System,
        request_id: "budget-test".to_owned(),
    }
}

#[tokio::test]
async fn manual_reset_clears_only_selected_budget_and_preserves_policy_history_and_expiry() {
    let Some(database) = TestDatabase::create("budget_manual_reset").await else {
        return;
    };
    let store = PgClientBudgetStore::new(database.pool.clone());
    let admin = PgAdminClientKeyStore::new(database.pool.clone());
    let revision: i64 =
        sqlx::query_scalar("select config_revision from runtime_settings where id = 1")
            .fetch_one(&database.pool)
            .await
            .unwrap();
    for (key, period, daily, weekly) in [
        ("daily", ClientKeyBudgetPeriod::Daily, "0", "2.5"),
        ("weekly", ClientKeyBudgetPeriod::Weekly, "2.5", "0"),
        ("all", ClientKeyBudgetPeriod::All, "0", "0"),
    ] {
        seed(&database, key, "1", "2").await;
        store.admit(key_id(key)).await.unwrap();
        let billed = charge(key, key, "2.5");
        store.settle(billed.clone()).await.unwrap();
        assert!(store.admit(key_id(key)).await.is_err());
        let before = status(&database, key).await;
        admin
            .reset_client_key_budget(
                ResetClientKeyBudget {
                    id: key_id(key),
                    period,
                },
                ClientKeyBudgetMutationOrigin::Admin,
                &context(),
            )
            .await
            .unwrap();
        // 同一费用重试仍由事件 ID 去重，不因清零而被再次累计。
        store.settle(billed).await.unwrap();
        let after = status(&database, key).await;
        assert_eq!(after.daily_used_usd.canonical(), daily);
        assert_eq!(after.weekly_used_usd.canonical(), weekly);
        assert_eq!(after.limits, before.limits);
        assert_eq!(after.daily_resets_at, before.daily_resets_at);
        assert_eq!(after.weekly_resets_at, before.weekly_resets_at);
        assert_eq!(
            store.admit(key_id(key)).await.is_ok(),
            period == ClientKeyBudgetPeriod::All
        );
    }
    let counts: (i64, i64, i64) = sqlx::query_as(
        "select (select count(*) from client_key_charge_events),
        (select count(*) from admin_audit_events where action = 'reset_budget' and config_revision is null and actor_kind = 'system'),
        (select config_revision from runtime_settings where id = 1)"
    ).fetch_one(&database.pool).await.unwrap();
    assert_eq!(counts, (3, 3, revision));
    database.close().await;
}

#[tokio::test]
async fn manual_reset_excludes_old_completions_but_counts_inflight_requests_after_reset() {
    let Some(database) = TestDatabase::create("budget_reset_late").await else {
        return;
    };
    seed(&database, "key", "1", "5").await;
    let store = PgClientBudgetStore::new(database.pool.clone());
    let admin = PgAdminClientKeyStore::new(database.pool.clone());
    store.admit(key_id("key")).await.unwrap();
    store
        .settle(charge("key", "before-reset", "1.2"))
        .await
        .unwrap();
    let delayed = charge("key", "delayed", "0.3");
    let reset_context = context();
    // 无论迟到结算还是重置先取得锁，重置前完成的费用都不应重新进入日额度。
    let (reset, settled) = tokio::join!(
        admin.reset_client_key_budget(
            ResetClientKeyBudget {
                id: key_id("key"),
                period: ClientKeyBudgetPeriod::Daily
            },
            ClientKeyBudgetMutationOrigin::Admin,
            &reset_context
        ),
        store.settle(delayed),
    );
    reset.unwrap();
    settled.unwrap();
    let after = status(&database, "key").await;
    assert_eq!(after.daily_used_usd.canonical(), "0");
    assert_eq!(after.weekly_used_usd.canonical(), "1.5");
    store
        .settle(charge("key", "completed-after-reset", "0.4"))
        .await
        .unwrap();
    let after = status(&database, "key").await;
    assert_eq!(after.daily_used_usd.canonical(), "0.4");
    assert_eq!(after.weekly_used_usd.canonical(), "1.9");
    database.close().await;
}

#[tokio::test]
async fn manual_reset_leaves_unused_and_expired_windows_inactive_and_reports_missing_keys() {
    let Some(database) = TestDatabase::create("budget_reset_inactive").await else {
        return;
    };
    let store = PgClientBudgetStore::new(database.pool.clone());
    let admin = PgAdminClientKeyStore::new(database.pool.clone());
    for key in ["unused", "expired"] {
        seed(&database, key, "1", "5").await;
    }
    store.admit(key_id("expired")).await.unwrap();
    store.settle(charge("expired", "old", "2")).await.unwrap();
    sqlx::query("update client_key_budget_windows set daily_end = now() - interval '1 second', weekly_end = now() - interval '1 second'")
        .execute(&database.pool).await.unwrap();
    for key in ["unused", "expired"] {
        admin
            .reset_client_key_budget(
                ResetClientKeyBudget {
                    id: key_id(key),
                    period: ClientKeyBudgetPeriod::All,
                },
                ClientKeyBudgetMutationOrigin::Admin,
                &context(),
            )
            .await
            .unwrap();
        let after = status(&database, key).await;
        assert!(after.daily_resets_at.is_none());
        assert!(after.weekly_resets_at.is_none());
        assert_eq!(after.daily_used_usd.canonical(), "0");
    }
    let windows: i64 = sqlx::query_scalar("select count(*) from client_key_budget_windows")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(windows, 1);
    let error = admin
        .reset_client_key_budget(
            ResetClientKeyBudget {
                id: key_id("missing"),
                period: ClientKeyBudgetPeriod::All,
            },
            ClientKeyBudgetMutationOrigin::Admin,
            &context(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.kind(),
        gateway_admin::ports::store::AdminStoreErrorKind::NotFound
    );
    database.close().await;
}

#[tokio::test]
async fn manual_reset_rolls_back_when_audit_cannot_be_written() {
    let Some(database) = TestDatabase::create("budget_reset_atomic").await else {
        return;
    };
    seed(&database, "key", "1", "5").await;
    let store = PgClientBudgetStore::new(database.pool.clone());
    let admin = PgAdminClientKeyStore::new(database.pool.clone());
    store.settle(charge("key", "bill", "1.2")).await.unwrap();
    let before = status(&database, "key").await;
    sqlx::raw_sql(
        "create function reject_reset_audit() returns trigger language plpgsql as $$
        begin raise exception 'test audit unavailable'; end $$;
        create trigger reject_reset_audit before insert on admin_audit_events
        for each row execute function reject_reset_audit();",
    )
    .execute(&database.pool)
    .await
    .unwrap();
    assert!(
        admin
            .reset_client_key_budget(
                ResetClientKeyBudget {
                    id: key_id("key"),
                    period: ClientKeyBudgetPeriod::All
                },
                ClientKeyBudgetMutationOrigin::Admin,
                &context()
            )
            .await
            .is_err()
    );
    assert_eq!(status(&database, "key").await, before);
    database.close().await;
}

#[tokio::test]
async fn key_usage_profile_reuses_current_budget_without_revealing_or_advancing_it() {
    let Some(database) = TestDatabase::create("key_usage_profile").await else {
        return;
    };
    let keys = PgAdminClientKeyStore::new(database.pool.clone());
    let id = key_id("profile");
    assert!(keys.get_client_key(&id).await.unwrap().is_none());
    seed(&database, "profile", "1", "5").await;
    let initial = keys.get_client_key(&id).await.unwrap().unwrap();
    assert_eq!(initial.budget.limits.daily_usd.canonical(), "1");
    assert!(initial.budget.daily_resets_at.is_none());
    let budgets = PgClientBudgetStore::new(database.pool.clone());
    budgets
        .settle(charge("profile", "profile-charge", "0.640001"))
        .await
        .unwrap();
    let current = keys.get_client_key(&id).await.unwrap().unwrap();
    assert_eq!(current.budget, status(&database, "profile").await);
    assert_eq!(current.budget.daily_used_usd.canonical(), "0.640001");
    assert!(!format!("{current:?}").contains(&format!("sk_{:a<43}", "profile")));
    sqlx::query("update client_key_budget_windows set daily_end = now() - interval '1 second' where client_api_key_id = 'profile'")
        .execute(&database.pool).await.unwrap();
    let after_reset = keys.get_client_key(&id).await.unwrap().unwrap();
    assert_eq!(after_reset.budget.daily_used_usd.canonical(), "0");
    assert_eq!(after_reset.budget.weekly_used_usd.canonical(), "0.640001");
    let persisted: String = sqlx::query_scalar("select daily_used_usd::text from client_key_budget_windows where client_api_key_id = 'profile'")
        .fetch_one(&database.pool).await.unwrap();
    assert_eq!(
        persisted
            .parse::<gateway_core::metering::Decimal>()
            .unwrap()
            .canonical(),
        "0.640001"
    );
    database.close().await;
}

#[tokio::test]
async fn budgets_settle_exactly_once_and_enforce_each_threshold_across_store_instances() {
    let Some(database) = TestDatabase::create("budgets_exact").await else {
        return;
    };
    seed(&database, "day", "0.3", "2").await;
    seed(&database, "week", "2", "0.2").await;
    let first = PgClientBudgetStore::new(database.pool.clone());
    let second = PgClientBudgetStore::new(database.pool.clone());
    for (key, prefix, amount) in [("day", "d", "0.1"), ("week", "w", "0.1")] {
        for _ in 0..3 {
            // 已准入请求可完成并超过限额，不预占估算费用。
            first.admit(key_id(key)).await.unwrap();
        }
        for index in 0..3 {
            let id = format!("{prefix}-{index}");
            let (a, b) = tokio::join!(
                first.settle(charge(key, &id, amount)),
                second.settle(charge(key, &id, amount))
            );
            a.unwrap();
            b.unwrap();
        }
    }
    let day = status(&database, "day").await;
    assert_eq!(day.daily_used_usd.canonical(), "0.3");
    assert_eq!(day.weekly_used_usd.canonical(), "0.3");
    let day_error = second.admit(key_id("day")).await.unwrap_err();
    assert_eq!(day_error.kind(), GatewayErrorKind::RateLimited);
    assert_eq!(
        day_error.client_error_code(),
        Some("key_daily_budget_exceeded")
    );
    assert!(day_error.retry_after().is_some());
    let week_error = first.admit(key_id("week")).await.unwrap_err();
    assert_eq!(
        week_error.client_error_code(),
        Some("key_weekly_budget_exceeded")
    );
    let event_count: i64 = sqlx::query_scalar("select count(*) from client_key_charge_events")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(event_count, 6, "rejected admission must not create charges");
    database.close().await;
}

#[tokio::test]
async fn window_rollover_is_shanghai_midnight_and_seven_days_with_late_settlement() {
    let Some(database) = TestDatabase::create("budgets_windows").await else {
        return;
    };
    seed(&database, "key", "1", "2").await;
    let store = PgClientBudgetStore::new(database.pool.clone());
    store.admit(key_id("key")).await.unwrap();
    let (day_start, day_end, week_end): (DateTime<Utc>, DateTime<Utc>, DateTime<Utc>) =
        sqlx::query_as("select daily_start, daily_end, weekly_end from client_key_budget_windows where client_api_key_id = 'key'")
            .fetch_one(&database.pool).await.unwrap();
    assert_eq!(day_start.timestamp().rem_euclid(86400), 16 * 3600);
    assert_eq!((day_end - day_start).num_hours(), 24);
    assert_eq!((week_end - day_start).num_hours(), 168);
    store.settle(charge("key", "first", "1")).await.unwrap();
    sqlx::query("update client_key_budget_windows set daily_start = daily_start - interval '1 day', daily_end = daily_start")
        .execute(&database.pool).await.unwrap();
    store.admit(key_id("key")).await.unwrap();
    assert_eq!(
        status(&database, "key").await.daily_used_usd.canonical(),
        "0"
    );
    assert_eq!(
        status(&database, "key").await.weekly_used_usd.canonical(),
        "1"
    );
    store.settle(charge("key", "second", "1")).await.unwrap();
    sqlx::query("update client_key_budget_windows set daily_end = now() - interval '1 second', weekly_end = now() - interval '1 second'")
        .execute(&database.pool).await.unwrap();
    let virtual_reset = status(&database, "key").await;
    assert_eq!(virtual_reset.daily_used_usd.canonical(), "0");
    assert_eq!(virtual_reset.weekly_used_usd.canonical(), "0");
    store.admit(key_id("key")).await.unwrap();
    let old = ClientBudgetCharge {
        completed_at: (day_start - chrono::Duration::seconds(1)).into(),
        ..charge("key", "after-reset", "0.9")
    };
    store.settle(old).await.unwrap();
    let reset = status(&database, "key").await;
    assert_eq!(reset.daily_used_usd.canonical(), "0");
    assert_eq!(reset.weekly_used_usd.canonical(), "0");
    assert!(reset.weekly_resets_at.is_some());
    database.close().await;
}

#[tokio::test]
async fn zero_cost_and_interrupted_requests_never_block_limited_keys() {
    let Some(database) = TestDatabase::create("budgets_zero").await else {
        return;
    };
    seed(&database, "key", "1", "5").await;
    let store = PgClientBudgetStore::new(database.pool.clone());
    store.admit(key_id("key")).await.unwrap();
    store.settle(charge("key", "no-cost", "0")).await.unwrap();
    // 模拟准入后进程退出，没有费用可结算；重启后仍应允许同一 Key 使用。
    store.admit(key_id("key")).await.unwrap();
    let restarted = PgClientBudgetStore::new(database.pool.clone());
    restarted.admit(key_id("key")).await.unwrap();
    let events: i64 = sqlx::query_scalar("select count(*) from client_key_charge_events")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(events, 1, "admission must not leave pending charges");
    assert_eq!(
        status(&database, "key").await.daily_used_usd.canonical(),
        "0"
    );
    restarted
        .settle(charge("key", "known", "0.4"))
        .await
        .unwrap();
    assert_eq!(
        status(&database, "key").await.daily_used_usd.canonical(),
        "0.4"
    );
    database.close().await;
}

#[tokio::test]
async fn transient_settlement_failure_rolls_back_and_retries_exact_cost_before_admission() {
    let Some(database) = TestDatabase::create("budgets_retry").await else {
        return;
    };
    seed(&database, "key", "1", "5").await;
    let store = PgClientBudgetStore::new(database.pool.clone());
    store.admit(key_id("key")).await.unwrap();
    // 在费用事件插入后让窗口写入失败，验证整个事务回滚。
    sqlx::raw_sql(
        "create function reject_budget_update() returns trigger language plpgsql as $$
        begin
            if new.daily_used_usd > 0 then raise exception 'temporary test failure'; end if;
            return new;
        end $$;
        create trigger reject_budget_update before update on client_key_budget_windows
        for each row execute function reject_budget_update();",
    )
    .execute(&database.pool)
    .await
    .unwrap();
    assert!(store.settle(charge("key", "retry", "1.25")).await.is_err());
    let events: i64 = sqlx::query_scalar("select count(*) from client_key_charge_events")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(events, 0);
    assert_eq!(
        status(&database, "key").await.daily_used_usd.canonical(),
        "0"
    );
    sqlx::query("drop trigger reject_budget_update on client_key_budget_windows")
        .execute(&database.pool)
        .await
        .unwrap();
    let error = store.admit(key_id("key")).await.unwrap_err();
    assert_eq!(error.client_error_code(), Some("key_daily_budget_exceeded"));
    store.settle(charge("key", "retry", "1.25")).await.unwrap();
    assert_eq!(
        status(&database, "key").await.daily_used_usd.canonical(),
        "1.25"
    );
    database.close().await;
}

#[tokio::test]
async fn budget_updates_preserve_omitted_limits_and_do_not_clear_usage() {
    let Some(database) = TestDatabase::create("budgets_policy").await else {
        return;
    };
    seed(&database, "key", "0", "0").await;
    let store = PgClientBudgetStore::new(database.pool.clone());
    let admin = PgAdminClientKeyStore::new(database.pool.clone());
    store.admit(key_id("key")).await.unwrap();
    store
        .settle(charge("key", "unlimited", "2.75"))
        .await
        .unwrap();
    let update = UpdateClientKey {
        request_profile_override_updates: Default::default(),
        id: ClientApiKeyId::new("key").unwrap(),
        name: "key".to_owned(),
        label: None,
        group_ids: vec![],
        limits: RateLimits {
            max_concurrency: 3,
            requests_per_minute: 0,
        },
        daily_limit_usd: Some("2".parse().unwrap()),
        weekly_limit_usd: Some("10".parse().unwrap()),
    };
    admin
        .update_client_key(update.clone(), &context())
        .await
        .unwrap();
    admin
        .update_client_key(
            UpdateClientKey {
                request_profile_override_updates: Default::default(),
                daily_limit_usd: None,
                weekly_limit_usd: None,
                ..update.clone()
            },
            &context(),
        )
        .await
        .unwrap();
    let current = status(&database, "key").await;
    assert_eq!(current.limits.daily_usd.canonical(), "2");
    assert_eq!(current.limits.weekly_usd.canonical(), "10");
    assert_eq!(current.daily_used_usd.canonical(), "2.75");
    assert_eq!(
        store
            .admit(key_id("key"))
            .await
            .unwrap_err()
            .client_error_code(),
        Some("key_daily_budget_exceeded")
    );
    admin
        .update_client_key(
            UpdateClientKey {
                request_profile_override_updates: Default::default(),
                daily_limit_usd: Some("0".parse().unwrap()),
                weekly_limit_usd: None,
                ..update
            },
            &context(),
        )
        .await
        .unwrap();
    store.admit(key_id("key")).await.unwrap();
    sqlx::query("update client_api_keys set enabled = false where id = 'key'")
        .execute(&database.pool)
        .await
        .unwrap();
    assert_eq!(
        store.admit(key_id("key")).await.unwrap_err().kind(),
        GatewayErrorKind::PolicyDenied
    );
    sqlx::query("delete from client_api_keys where id = 'key'")
        .execute(&database.pool)
        .await
        .unwrap();
    assert_eq!(
        store.admit(key_id("key")).await.unwrap_err().kind(),
        GatewayErrorKind::Unauthorized
    );
    store.settle(charge("key", "allowed", "1")).await.unwrap();
    database.close().await;
}

#[tokio::test]
async fn budget_database_outage_fails_closed() {
    let Some(database) = TestDatabase::create("budgets_outage").await else {
        return;
    };
    let store = PgClientBudgetStore::new(database.pool.clone());
    database.pool.close().await;
    assert_eq!(
        store
            .admit(key_id("key"))
            .await
            .unwrap_err()
            .client_error_code(),
        Some("key_budget_unavailable")
    );
    assert!(store.settle(charge("key", "offline", "1")).await.is_err());
    database.close().await;
}

async fn plugin_reset_owner(database: &TestDatabase) -> PluginResourceOwner {
    super::plugins::artifacts::initialize_revision(database).await;
    let store = PgPluginStore::new(database.pool.clone());
    let package = super::plugins::artifacts::artifact('b', &["linux-x86_64"]);
    let installed = store
        .install_artifact(package, PluginSource::Upload, &context())
        .await
        .unwrap();
    let accepted = store
        .accept_artifact(&installed.artifact.metadata.sha256, &context())
        .await
        .unwrap();
    let instance = store
        .save_instance(
            PluginInstance {
                id: uuid::Uuid::now_v7().to_string(),
                name: "budget reset".into(),
                artifact_sha256: installed.artifact.metadata.sha256,
                enabled: true,
                trusted_process: true,
                configuration: serde_json::json!({}),
                secrets: BTreeMap::new(),

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

#[tokio::test]
async fn plugin_resets_share_the_native_ledger_and_each_call_resets_again() {
    let Some(database) = TestDatabase::create("plugin_budget_reset").await else {
        return;
    };
    let owner = plugin_reset_owner(&database).await;
    seed(&database, "key", "10", "20").await;
    let budgets = PgClientBudgetStore::new(database.pool.clone());
    budgets.settle(charge("key", "before", "3")).await.unwrap();
    let before = status(&database, "key").await;
    let revision: i64 =
        sqlx::query_scalar("select config_revision from runtime_settings where id=1")
            .fetch_one(&database.pool)
            .await
            .unwrap();
    let store = PgAdminClientKeyStore::new(database.pool.clone());
    let command = ResetClientKeyBudget {
        id: key_id("key"),
        period: ClientKeyBudgetPeriod::Weekly,
    };
    store
        .reset_client_key_budget(
            command.clone(),
            ClientKeyBudgetMutationOrigin::Plugin(owner.clone()),
            &context(),
        )
        .await
        .unwrap();
    let after = status(&database, "key").await;
    assert_eq!(after.daily_used_usd.canonical(), "3");
    assert_eq!(after.weekly_used_usd.canonical(), "0");
    assert_eq!(after.limits, before.limits);
    assert_eq!(after.daily_resets_at, before.daily_resets_at);
    assert_eq!(after.weekly_resets_at, before.weekly_resets_at);

    budgets.settle(charge("key", "after", "2")).await.unwrap();
    assert_eq!(
        status(&database, "key").await.weekly_used_usd.canonical(),
        "2"
    );
    store
        .reset_client_key_budget(
            command,
            ClientKeyBudgetMutationOrigin::Plugin(owner),
            &context(),
        )
        .await
        .unwrap();
    let after = status(&database, "key").await;
    assert_eq!(after.daily_used_usd.canonical(), "5");
    assert_eq!(after.weekly_used_usd.canonical(), "0");
    let (audits, charges, current_revision): (i64, i64, i64) = sqlx::query_as(
        "select (select count(*) from admin_audit_events where action='reset_budget' and config_revision is null),
                (select count(*) from client_key_charge_events),
                (select config_revision from runtime_settings where id=1)"
    ).fetch_one(&database.pool).await.unwrap();
    assert_eq!((audits, charges, current_revision), (2, 2, revision));
    database.close().await;
}

#[tokio::test]
async fn plugin_reset_revalidates_current_revision_without_changing_the_ledger() {
    let Some(database) = TestDatabase::create("plugin_budget_authorization").await else {
        return;
    };
    let owner = plugin_reset_owner(&database).await;
    seed(&database, "key", "10", "20").await;
    PgClientBudgetStore::new(database.pool.clone())
        .settle(charge("key", "before", "4"))
        .await
        .unwrap();
    let before = status(&database, "key").await;
    let store = PgAdminClientKeyStore::new(database.pool.clone());
    for sql in [
        "update plugin_instances set revision=revision+1",
        "update plugin_instances set revision=revision-1, enabled=false",
        "update plugin_instances set enabled=true; update plugin_artifacts set accepted_at=null",
    ] {
        sqlx::raw_sql(sql).execute(&database.pool).await.unwrap();
        assert_eq!(
            store
                .reset_client_key_budget(
                    ResetClientKeyBudget {
                        id: key_id("key"),
                        period: ClientKeyBudgetPeriod::All
                    },
                    ClientKeyBudgetMutationOrigin::Plugin(owner.clone()),
                    &context()
                )
                .await
                .unwrap_err()
                .kind(),
            AdminStoreErrorKind::Conflict
        );
        assert_eq!(status(&database, "key").await, before);
    }
    sqlx::query("update plugin_artifacts set accepted_at=now()")
        .execute(&database.pool)
        .await
        .unwrap();
    let wrong_artifact = PluginResourceOwner {
        artifact_sha256: "c".repeat(64),
        ..owner.clone()
    };
    assert_eq!(
        store
            .reset_client_key_budget(
                ResetClientKeyBudget {
                    id: key_id("key"),
                    period: ClientKeyBudgetPeriod::All
                },
                ClientKeyBudgetMutationOrigin::Plugin(wrong_artifact),
                &context()
            )
            .await
            .unwrap_err()
            .kind(),
        AdminStoreErrorKind::Conflict
    );
    assert_eq!(status(&database, "key").await, before);
    assert_eq!(
        store
            .reset_client_key_budget(
                ResetClientKeyBudget {
                    id: key_id("missing"),
                    period: ClientKeyBudgetPeriod::All
                },
                ClientKeyBudgetMutationOrigin::Plugin(owner.clone()),
                &context()
            )
            .await
            .unwrap_err()
            .kind(),
        AdminStoreErrorKind::NotFound
    );
    // 验证拒绝后的事务和锁已释放，当前身份仍可正常执行。
    store
        .reset_client_key_budget(
            ResetClientKeyBudget {
                id: key_id("key"),
                period: ClientKeyBudgetPeriod::All,
            },
            ClientKeyBudgetMutationOrigin::Plugin(owner),
            &context(),
        )
        .await
        .unwrap();
    assert_eq!(
        status(&database, "key").await.daily_used_usd.canonical(),
        "0"
    );
    database.close().await;
}

#[tokio::test]
async fn plugin_reset_rolls_back_with_audit_and_serializes_with_late_settlement() {
    let Some(database) = TestDatabase::create("plugin_budget_atomicity").await else {
        return;
    };
    let owner = plugin_reset_owner(&database).await;
    seed(&database, "key", "10", "20").await;
    let budgets = PgClientBudgetStore::new(database.pool.clone());
    budgets.settle(charge("key", "before", "4")).await.unwrap();
    let before = status(&database, "key").await;
    let store = PgAdminClientKeyStore::new(database.pool.clone());
    let command = ResetClientKeyBudget {
        id: key_id("key"),
        period: ClientKeyBudgetPeriod::Weekly,
    };
    sqlx::raw_sql("create function reject_budget_audit() returns trigger language plpgsql as $$ begin raise exception 'test rollback'; end $$;
        create trigger reject_budget_audit before insert on admin_audit_events for each row execute function reject_budget_audit()")
        .execute(&database.pool).await.unwrap();
    assert!(
        store
            .reset_client_key_budget(
                command.clone(),
                ClientKeyBudgetMutationOrigin::Plugin(owner.clone()),
                &context()
            )
            .await
            .is_err()
    );
    assert_eq!(status(&database, "key").await, before);
    sqlx::query("drop trigger reject_budget_audit on admin_audit_events")
        .execute(&database.pool)
        .await
        .unwrap();
    let mutation = context();
    // 完成时间在重置前，无论谁先拿到行锁，周用量都不能被迟到结算重新扣回。
    let delayed = charge("key", "delayed", "1");
    let (reset, settlement) = tokio::join!(
        store.reset_client_key_budget(
            command,
            ClientKeyBudgetMutationOrigin::Plugin(owner),
            &mutation
        ),
        budgets.settle(delayed),
    );
    reset.unwrap();
    settlement.unwrap();
    let after = status(&database, "key").await;
    assert_eq!(after.daily_used_usd.canonical(), "5");
    assert_eq!(after.weekly_used_usd.canonical(), "0");
    database.close().await;
}

fn budget_update(
    key: &str,
    daily: Option<&str>,
    weekly: Option<&str>,
) -> UpdateClientKeyBudgetLimits {
    UpdateClientKeyBudgetLimits {
        id: key_id(key),
        daily_limit_usd: daily.map(|value| value.parse().unwrap()),
        weekly_limit_usd: weekly.map(|value| value.parse().unwrap()),
    }
}

#[tokio::test]
async fn plugin_budget_limits_preserve_consumption_and_unrelated_configuration_and_control_admission()
 {
    let Some(database) = TestDatabase::create("plugin_budget_limits").await else {
        return;
    };
    let owner = plugin_reset_owner(&database).await;
    seed(&database, "key", "10", "20").await;
    sqlx::query("update client_api_keys set label='preserved', max_concurrency=2, requests_per_minute=30 where id='key'")
        .execute(&database.pool).await.unwrap();
    let budgets = PgClientBudgetStore::new(database.pool.clone());
    budgets.settle(charge("key", "before", "4")).await.unwrap();
    let store = PgAdminClientKeyStore::new(database.pool.clone());
    let before = store.get_client_key(&key_id("key")).await.unwrap().unwrap();
    let revision: i64 =
        sqlx::query_scalar("select config_revision from runtime_settings where id=1")
            .fetch_one(&database.pool)
            .await
            .unwrap();
    let command = budget_update("key", None, Some("3"));
    let changed = store
        .update_client_key_budget_limits(
            command.clone(),
            ClientKeyBudgetMutationOrigin::Plugin(owner.clone()),
            &context(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(changed.get(), revision as u64 + 1);
    let after = store.get_client_key(&key_id("key")).await.unwrap().unwrap();
    assert_eq!(after.budget.daily_used_usd, before.budget.daily_used_usd);
    assert_eq!(after.budget.weekly_used_usd, before.budget.weekly_used_usd);
    assert_eq!(after.budget.daily_resets_at, before.budget.daily_resets_at);
    assert_eq!(
        after.budget.weekly_resets_at,
        before.budget.weekly_resets_at
    );
    assert_eq!(
        after.budget.limits.daily_usd,
        before.budget.limits.daily_usd
    );
    assert_eq!(after.budget.limits.weekly_usd.canonical(), "3");
    assert_eq!(
        (
            &after.name,
            &after.label,
            &after.groups,
            after.limits,
            &after.request_profile_overrides
        ),
        (
            &before.name,
            &before.label,
            &before.groups,
            before.limits,
            &before.request_profile_overrides
        )
    );
    assert_eq!(
        budgets.admit(key_id("key")).await.unwrap_err().kind(),
        GatewayErrorKind::RateLimited
    );
    assert!(
        store
            .update_client_key_budget_limits(
                command,
                ClientKeyBudgetMutationOrigin::Plugin(owner.clone()),
                &context()
            )
            .await
            .unwrap()
            .is_none()
    );
    let (new_revision, audits): (i64, i64) = sqlx::query_as("select config_revision, (select count(*) from admin_audit_events where action='update_budget_limits') from runtime_settings where id=1")
        .fetch_one(&database.pool).await.unwrap();
    assert_eq!((new_revision, audits), (revision + 1, 1));
    store
        .update_client_key_budget_limits(
            budget_update("key", None, Some("5")),
            ClientKeyBudgetMutationOrigin::Plugin(owner.clone()),
            &context(),
        )
        .await
        .unwrap();
    budgets.admit(key_id("key")).await.unwrap();
    let mutation = context();
    let (updated, settled) = tokio::join!(
        store.update_client_key_budget_limits(
            budget_update("key", Some("0"), Some("0")),
            ClientKeyBudgetMutationOrigin::Plugin(owner),
            &mutation
        ),
        budgets.settle(charge("key", "inflight", "2")),
    );
    updated.unwrap();
    settled.unwrap();
    let after = status(&database, "key").await;
    assert_eq!(after.daily_used_usd.canonical(), "6");
    assert_eq!(after.weekly_used_usd.canonical(), "6");
    assert!(!after.limits.is_limited());
    budgets.admit(key_id("key")).await.unwrap();
    database.close().await;
}

#[tokio::test]
async fn plugin_budget_limits_revalidate_authority_and_rollback_with_audit() {
    let Some(database) = TestDatabase::create("plugin_budget_limits_rollback").await else {
        return;
    };
    let owner = plugin_reset_owner(&database).await;
    seed(&database, "key", "10", "20").await;
    let store = PgAdminClientKeyStore::new(database.pool.clone());
    let before = status(&database, "key").await;
    // 读取和配置赋值都不能开启未使用 Key 的窗口。
    assert_eq!(before.daily_resets_at, None);
    let mut stale_owner = owner.clone();
    stale_owner.revision = Revision::new(owner.revision.get() + 1).unwrap();
    assert_eq!(
        store
            .update_client_key_budget_limits(
                budget_update("key", Some("2"), None),
                ClientKeyBudgetMutationOrigin::Plugin(stale_owner),
                &context()
            )
            .await
            .unwrap_err()
            .kind(),
        AdminStoreErrorKind::Conflict
    );
    assert_eq!(
        store
            .update_client_key_budget_limits(
                budget_update("missing", Some("2"), None),
                ClientKeyBudgetMutationOrigin::Plugin(owner.clone()),
                &context()
            )
            .await
            .unwrap_err()
            .kind(),
        AdminStoreErrorKind::NotFound
    );
    let revision: i64 =
        sqlx::query_scalar("select config_revision from runtime_settings where id=1")
            .fetch_one(&database.pool)
            .await
            .unwrap();
    sqlx::raw_sql("create function reject_limit_audit() returns trigger language plpgsql as $$ begin raise exception 'test rollback'; end $$; create trigger reject_limit_audit before insert on admin_audit_events for each row execute function reject_limit_audit()")
        .execute(&database.pool).await.unwrap();
    assert!(
        store
            .update_client_key_budget_limits(
                budget_update("key", Some("2"), None),
                ClientKeyBudgetMutationOrigin::Plugin(owner.clone()),
                &context()
            )
            .await
            .is_err()
    );
    assert_eq!(status(&database, "key").await, before);
    let after_revision: i64 =
        sqlx::query_scalar("select config_revision from runtime_settings where id=1")
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(revision, after_revision);
    sqlx::query("drop trigger reject_limit_audit on admin_audit_events")
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("update client_api_keys set enabled=false where id='key'")
        .execute(&database.pool)
        .await
        .unwrap();
    store
        .update_client_key_budget_limits(
            budget_update("key", Some("2"), None),
            ClientKeyBudgetMutationOrigin::Plugin(owner),
            &context(),
        )
        .await
        .unwrap();
    let after = status(&database, "key").await;
    assert_eq!(after.limits.daily_usd.canonical(), "2");
    assert_eq!(after.daily_resets_at, None);
    assert_eq!(after.weekly_resets_at, None);
    database.close().await;
}

fn micros(time: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::from_timestamp_micros(time.timestamp_micros()).unwrap()
}

fn in_hours(hours: i64) -> DateTime<Utc> {
    micros(Utc::now() + chrono::Duration::hours(hours))
}

fn weekly_change(
    key: &str,
    expected_revision: u64,
    action: ClientKeyWeeklyWindowAction,
) -> ChangeClientKeyWeeklyWindow {
    ChangeClientKeyWeeklyWindow {
        id: key_id(key),
        expected_revision,
        action,
    }
}

async fn weekly_audits(database: &TestDatabase) -> i64 {
    sqlx::query_scalar(
        "select count(*) from admin_audit_events where action = 'weekly_control' and config_revision is null",
    )
    .fetch_one(&database.pool)
    .await
    .unwrap()
}

async fn window(database: &TestDatabase, key: &str) -> (Option<String>, i64, DateTime<Utc>) {
    sqlx::query_as(
        "select weekly_controller, weekly_control_revision, weekly_end
        from client_key_budget_windows where client_api_key_id = $1",
    )
    .bind(key)
    .fetch_one(&database.pool)
    .await
    .unwrap()
}

async fn apply_weekly(
    store: &PgAdminClientKeyStore,
    owner: &PluginResourceOwner,
    command: ChangeClientKeyWeeklyWindow,
) -> gateway_admin::ports::store::AdminStoreResult<ClientKeyWeeklyControl> {
    store
        .change_client_key_weekly_control(owner, command, &context())
        .await
}

fn assert_control(
    control: &ClientKeyWeeklyControl,
    revision: u64,
    owner: Option<&PluginResourceOwner>,
    expires_at: Option<DateTime<Utc>>,
) {
    assert_eq!(control.revision, revision);
    assert_eq!(
        control.controller.as_deref(),
        owner.map(|owner| owner.instance_id.as_str())
    );
    assert_eq!(control.expires_at, expires_at);
    assert_eq!(control.accounting_start.is_some(), owner.is_some());
    assert!(!control.waiting);
}

#[tokio::test]
async fn weekly_control_actions_follow_owner_revision_and_lost_reply_retry() {
    let Some(database) = TestDatabase::create("weekly_control_actions").await else {
        return;
    };
    let owner = plugin_reset_owner(&database).await;
    seed(&database, "key", "10", "20").await;
    let budgets = PgClientBudgetStore::new(database.pool.clone());
    budgets.settle(charge("key", "before", "3")).await.unwrap();
    let store = PgAdminClientKeyStore::new(database.pool.clone());
    let initial = store
        .client_key_weekly_control(&key_id("key"))
        .await
        .unwrap();
    assert_control(&initial, 0, None, None);
    let config_revision: i64 =
        sqlx::query_scalar("select config_revision from runtime_settings where id=1")
            .fetch_one(&database.pool)
            .await
            .unwrap();
    let change = |command| apply_weekly(&store, &owner, command);

    // claim 默认保留已用金额，并把原生窗口过期时间交给接管者。
    let claim_until = in_hours(2);
    let claim = weekly_change(
        "key",
        0,
        ClientKeyWeeklyWindowAction::Claim {
            expires_at: claim_until,
            clear_used: false,
        },
    );
    let claimed = change(claim.clone()).await.unwrap();
    assert_control(&claimed, 1, Some(&owner), Some(claim_until));
    let after_claim = status(&database, "key").await;
    assert_eq!(after_claim.weekly_used_usd.canonical(), "3");
    assert_eq!(after_claim.weekly_resets_at, Some(claim_until.into()));
    assert_eq!(
        store
            .client_key_weekly_control(&key_id("key"))
            .await
            .unwrap()
            .expires_at,
        Some(claim_until)
    );

    // 提交后回包丢失的原样重试返回同一结果，不再追加审计或改动窗口。
    let retried = change(claim).await.unwrap();
    assert_eq!(retried.revision, 1);
    assert_eq!(retried.expires_at, Some(claim_until));
    assert_eq!(retried.accounting_start, claimed.accounting_start);
    assert_eq!(weekly_audits(&database).await, 1);

    // 相同版本上的不同操作、未来版本和重复 claim 都不能生效。
    for command in [
        weekly_change(
            "key",
            0,
            ClientKeyWeeklyWindowAction::Claim {
                expires_at: in_hours(5),
                clear_used: false,
            },
        ),
        weekly_change(
            "key",
            9,
            ClientKeyWeeklyWindowAction::Align {
                expires_at: in_hours(5),
            },
        ),
    ] {
        assert_eq!(
            change(command).await.unwrap_err().kind(),
            AdminStoreErrorKind::StaleRevision
        );
    }
    assert_eq!(
        change(weekly_change(
            "key",
            1,
            ClientKeyWeeklyWindowAction::Claim {
                expires_at: in_hours(5),
                clear_used: true,
            },
        ))
        .await
        .unwrap_err()
        .kind(),
        AdminStoreErrorKind::Conflict
    );
    assert_eq!(
        change(weekly_change(
            "key",
            1,
            ClientKeyWeeklyWindowAction::Align {
                expires_at: Utc::now() - chrono::Duration::seconds(1),
            },
        ))
        .await
        .unwrap_err()
        .kind(),
        AdminStoreErrorKind::Invalid
    );
    assert_eq!(
        status(&database, "key").await.weekly_used_usd.canonical(),
        "3"
    );

    // align 只改到期时间，保留已用金额与计费起点。
    let align_until = in_hours(3);
    let aligned = change(weekly_change(
        "key",
        1,
        ClientKeyWeeklyWindowAction::Align {
            expires_at: align_until,
        },
    ))
    .await
    .unwrap();
    assert_control(&aligned, 2, Some(&owner), Some(align_until));
    assert_eq!(aligned.accounting_start, claimed.accounting_start);
    assert_eq!(
        status(&database, "key").await.weekly_used_usd.canonical(),
        "3"
    );

    // sync 在执行时清零并重新开始计费，同时对齐到期时间。
    let sync_until = in_hours(4);
    let synced = change(weekly_change(
        "key",
        2,
        ClientKeyWeeklyWindowAction::Sync {
            expires_at: sync_until,
        },
    ))
    .await
    .unwrap();
    assert_control(&synced, 3, Some(&owner), Some(sync_until));
    assert!(synced.accounting_start >= aligned.accounting_start);
    assert_eq!(
        status(&database, "key").await.weekly_used_usd.canonical(),
        "0"
    );
    budgets
        .settle(charge("key", "after-sync", "2"))
        .await
        .unwrap();
    assert_eq!(
        status(&database, "key").await.weekly_used_usd.canonical(),
        "2"
    );

    // 管理员的周重置在受控窗口上清零并以当下为计费起点，保留接管者与到期时间。
    store
        .reset_client_key_budget(
            ResetClientKeyBudget {
                id: key_id("key"),
                period: ClientKeyBudgetPeriod::Weekly,
            },
            ClientKeyBudgetMutationOrigin::Admin,
            &context(),
        )
        .await
        .unwrap();
    let reset = store
        .client_key_weekly_control(&key_id("key"))
        .await
        .unwrap();
    assert_eq!(
        status(&database, "key").await.weekly_used_usd.canonical(),
        "0"
    );
    assert_eq!(
        reset.controller.as_deref(),
        Some(owner.instance_id.as_str())
    );
    assert_eq!(reset.expires_at, Some(sync_until));
    assert!(reset.accounting_start > synced.accounting_start);
    assert_eq!(reset.revision, 3);
    budgets
        .settle(charge("key", "after-reset", "2"))
        .await
        .unwrap();

    // 非接管者不能同步、对齐或释放。
    sqlx::query("update client_key_budget_windows set weekly_controller = 'other-instance'")
        .execute(&database.pool)
        .await
        .unwrap();
    for action in [
        ClientKeyWeeklyWindowAction::Sync {
            expires_at: in_hours(6),
        },
        ClientKeyWeeklyWindowAction::Align {
            expires_at: in_hours(6),
        },
        ClientKeyWeeklyWindowAction::Release,
    ] {
        assert_eq!(
            change(weekly_change("key", 3, action))
                .await
                .unwrap_err()
                .kind(),
            AdminStoreErrorKind::Conflict
        );
    }
    sqlx::query("update client_key_budget_windows set weekly_controller = $1")
        .bind(&owner.instance_id)
        .execute(&database.pool)
        .await
        .unwrap();

    // release 只撤销接管者，窗口和已用金额保持原状，由原生规则到期后滚动。
    let released = change(weekly_change(
        "key",
        3,
        ClientKeyWeeklyWindowAction::Release,
    ))
    .await
    .unwrap();
    assert_control(&released, 4, None, None);
    let after_release = status(&database, "key").await;
    assert_eq!(after_release.weekly_used_usd.canonical(), "2");
    assert_eq!(after_release.weekly_resets_at, Some(sync_until.into()));
    // 释放的原样重试同样只返回已提交结果。
    let released_again = change(weekly_change(
        "key",
        3,
        ClientKeyWeeklyWindowAction::Release,
    ))
    .await
    .unwrap();
    assert_eq!(released_again.revision, 4);
    assert_eq!(weekly_audits(&database).await, 4);

    // 再次接管时可显式清零已用金额。
    let reclaimed = change(weekly_change(
        "key",
        4,
        ClientKeyWeeklyWindowAction::Claim {
            expires_at: in_hours(8),
            clear_used: true,
        },
    ))
    .await
    .unwrap();
    assert_eq!(reclaimed.revision, 5);
    assert_eq!(
        status(&database, "key").await.weekly_used_usd.canonical(),
        "0"
    );

    assert_eq!(
        change(weekly_change(
            "missing",
            0,
            ClientKeyWeeklyWindowAction::Release
        ))
        .await
        .unwrap_err()
        .kind(),
        AdminStoreErrorKind::NotFound
    );
    // 周窗口操作只写审计，不推进配置版本。
    let current_revision: i64 =
        sqlx::query_scalar("select config_revision from runtime_settings where id=1")
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(current_revision, config_revision);
    assert_eq!(weekly_audits(&database).await, 5);
    database.close().await;
}

#[tokio::test]
async fn controlled_expired_window_waits_and_settles_by_completion_time() {
    let Some(database) = TestDatabase::create("weekly_control_waiting").await else {
        return;
    };
    let owner = plugin_reset_owner(&database).await;
    seed(&database, "key", "0", "5").await;
    let budgets = PgClientBudgetStore::new(database.pool.clone());
    let store = PgAdminClientKeyStore::new(database.pool.clone());
    budgets.settle(charge("key", "first", "1")).await.unwrap();
    let claimed = store
        .change_client_key_weekly_control(
            &owner,
            weekly_change(
                "key",
                0,
                ClientKeyWeeklyWindowAction::Claim {
                    expires_at: in_hours(1),
                    clear_used: false,
                },
            ),
            &context(),
        )
        .await
        .unwrap();
    budgets.admit(key_id("key")).await.unwrap();

    sqlx::query("update client_key_budget_windows set weekly_end = now() - interval '1 second'")
        .execute(&database.pool)
        .await
        .unwrap();
    let expired = window(&database, "key").await.2;
    let control = store
        .client_key_weekly_control(&key_id("key"))
        .await
        .unwrap();
    assert!(control.waiting);
    assert_eq!(
        control.controller.as_deref(),
        Some(owner.instance_id.as_str())
    );

    // 等待接管者期间不自动滚动，也不放行；普通额度即使未用尽也一样。
    let error = budgets.admit(key_id("key")).await.unwrap_err();
    assert_eq!(error.kind(), GatewayErrorKind::RateLimited);
    assert_eq!(error.client_error_code(), Some("key_weekly_window_waiting"));
    assert_eq!(window(&database, "key").await.2, expired);
    assert_eq!(
        status(&database, "key").await.weekly_used_usd.canonical(),
        "1"
    );

    // 迟到结算按完成时间归属：接管窗口起点之后的费用仍计入，之前的费用被排除。
    budgets.settle(charge("key", "late", "0.5")).await.unwrap();
    assert_eq!(
        status(&database, "key").await.weekly_used_usd.canonical(),
        "1.5"
    );
    let before_start = ClientBudgetCharge {
        completed_at: (claimed.accounting_start.unwrap() - chrono::Duration::seconds(1)).into(),
        ..charge("key", "too-old", "4")
    };
    budgets.settle(before_start).await.unwrap();
    assert_eq!(
        status(&database, "key").await.weekly_used_usd.canonical(),
        "1.5"
    );

    // 接管者同步后恢复放行，并从执行时重新计费。
    let synced = store
        .change_client_key_weekly_control(
            &owner,
            weekly_change(
                "key",
                1,
                ClientKeyWeeklyWindowAction::Sync {
                    expires_at: in_hours(2),
                },
            ),
            &context(),
        )
        .await
        .unwrap();
    assert!(!synced.waiting);
    budgets.admit(key_id("key")).await.unwrap();
    assert_eq!(
        status(&database, "key").await.weekly_used_usd.canonical(),
        "0"
    );

    // 释放后回到原生规则：过期窗口在下一次准入滚动。
    sqlx::query("update client_key_budget_windows set weekly_end = now() - interval '1 second'")
        .execute(&database.pool)
        .await
        .unwrap();
    store
        .change_client_key_weekly_control(
            &owner,
            weekly_change("key", 2, ClientKeyWeeklyWindowAction::Release),
            &context(),
        )
        .await
        .unwrap();
    budgets.admit(key_id("key")).await.unwrap();
    assert!(window(&database, "key").await.2 > Utc::now());
    database.close().await;
}

#[tokio::test]
async fn weekly_control_change_rolls_back_with_audit_and_revalidates_authority() {
    let Some(database) = TestDatabase::create("weekly_control_rollback").await else {
        return;
    };
    let owner = plugin_reset_owner(&database).await;
    seed(&database, "key", "10", "20").await;
    let store = PgAdminClientKeyStore::new(database.pool.clone());
    let claim = weekly_change(
        "key",
        0,
        ClientKeyWeeklyWindowAction::Claim {
            expires_at: in_hours(2),
            clear_used: true,
        },
    );
    let mut stale_owner = owner.clone();
    stale_owner.revision = Revision::new(owner.revision.get() + 1).unwrap();
    assert_eq!(
        store
            .change_client_key_weekly_control(&stale_owner, claim.clone(), &context())
            .await
            .unwrap_err()
            .kind(),
        AdminStoreErrorKind::Conflict
    );
    sqlx::raw_sql("create function reject_weekly_audit() returns trigger language plpgsql as $$ begin raise exception 'test rollback'; end $$; create trigger reject_weekly_audit before insert on admin_audit_events for each row execute function reject_weekly_audit()")
        .execute(&database.pool)
        .await
        .unwrap();
    assert!(
        store
            .change_client_key_weekly_control(&owner, claim.clone(), &context())
            .await
            .is_err()
    );
    let (controller, revision): (Option<String>, i64) = sqlx::query_as(
        "select weekly_controller, weekly_control_revision from client_key_budget_windows where client_api_key_id = 'key'",
    )
    .fetch_optional(&database.pool)
    .await
    .unwrap()
    .unwrap_or((None, 0));
    assert_eq!((controller, revision), (None, 0));
    assert_eq!(weekly_audits(&database).await, 0);
    sqlx::query("drop trigger reject_weekly_audit on admin_audit_events")
        .execute(&database.pool)
        .await
        .unwrap();
    store
        .change_client_key_weekly_control(&owner, claim, &context())
        .await
        .unwrap();
    assert_eq!(weekly_audits(&database).await, 1);
    // 实例被停用后，同一 owner 的写入不再通过当前授权校验。
    sqlx::query("update plugin_instances set enabled = false")
        .execute(&database.pool)
        .await
        .unwrap();
    assert_eq!(
        store
            .change_client_key_weekly_control(
                &owner,
                weekly_change("key", 1, ClientKeyWeeklyWindowAction::Release),
                &context()
            )
            .await
            .unwrap_err()
            .kind(),
        AdminStoreErrorKind::Conflict
    );
    database.close().await;
}

fn owned_instance(artifact_sha256: &str, enabled: bool) -> PluginInstance {
    PluginInstance {
        id: uuid::Uuid::now_v7().to_string(),
        name: "weekly owner".into(),
        artifact_sha256: artifact_sha256.to_owned(),
        enabled,
        trusted_process: true,
        configuration: serde_json::json!({}),
        secrets: BTreeMap::new(),
        bindings: vec![],
        revision: Revision::new(1).unwrap(),
    }
}

fn owner_of(instance: &PluginInstance) -> PluginResourceOwner {
    PluginResourceOwner {
        instance_id: instance.id.clone(),
        artifact_sha256: instance.artifact_sha256.clone(),
        revision: instance.revision,
    }
}

fn no_state() -> PluginStateCommit {
    PluginStateCommit {
        configuration: PluginStateConfiguration { namespaces: vec![] },
        transition_id: None,
    }
}

/// 安装并接受制品，返回制品摘要和当前配置版本。
async fn accepted_artifact(
    database: &TestDatabase,
    marker: char,
) -> (PgPluginStore, String, Revision) {
    super::plugins::artifacts::initialize_revision(database).await;
    let store = PgPluginStore::new(database.pool.clone());
    let installed = store
        .install_artifact(
            super::plugins::artifacts::artifact(marker, &["linux-x86_64"]),
            PluginSource::Upload,
            &context(),
        )
        .await
        .unwrap();
    let sha = installed.artifact.metadata.sha256;
    let accepted = store.accept_artifact(&sha, &context()).await.unwrap();
    (store, sha, accepted.config_revision)
}

async fn claim(database: &TestDatabase, owner: &PluginResourceOwner, key: &str) {
    let store = PgAdminClientKeyStore::new(database.pool.clone());
    let expected = store
        .client_key_weekly_control(&key_id(key))
        .await
        .unwrap()
        .revision;
    let claimed = store
        .change_client_key_weekly_control(
            owner,
            weekly_change(
                key,
                expected,
                ClientKeyWeeklyWindowAction::Claim {
                    expires_at: in_hours(6),
                    clear_used: false,
                },
            ),
            &context(),
        )
        .await
        .unwrap();
    assert_eq!(
        claimed.controller.as_deref(),
        Some(owner.instance_id.as_str())
    );
}

async fn assert_released(database: &TestDatabase, keys: &[&str], kept_used: &str) {
    let store = PgAdminClientKeyStore::new(database.pool.clone());
    for key in keys {
        let control = store.client_key_weekly_control(&key_id(key)).await.unwrap();
        assert_eq!(control.controller, None, "{key} should be released");
        assert!(control.expires_at.is_none());
        // 释放不改窗口内容。
        assert_eq!(
            status(database, key).await.weekly_used_usd.canonical(),
            kept_used
        );
    }
}

#[tokio::test]
async fn disabling_replacing_or_deleting_an_instance_releases_its_weekly_windows_atomically() {
    let Some(database) = TestDatabase::create("weekly_control_release").await else {
        return;
    };
    let (plugins, sha, config_revision) = accepted_artifact(&database, 'c').await;
    seed(&database, "one", "10", "20").await;
    seed(&database, "two", "10", "20").await;
    let budgets = PgClientBudgetStore::new(database.pool.clone());
    for key in ["one", "two"] {
        budgets.settle(charge(key, key, "1")).await.unwrap();
    }

    // 多实例停用入口。
    let saved = plugins
        .save_instance(owned_instance(&sha, true), config_revision, &context())
        .await
        .unwrap();
    let instance = saved.instance.clone();
    claim(&database, &owner_of(&instance), "one").await;
    claim(&database, &owner_of(&instance), "two").await;
    let audits = weekly_audits(&database).await;
    let disabled = plugins
        .disable_instances(
            std::slice::from_ref(&instance.id),
            saved.config_revision,
            &context(),
        )
        .await
        .unwrap();
    assert_released(&database, &["one", "two"], "1").await;
    assert_eq!(weekly_audits(&database).await, audits + 2);

    // 普通保存为停用同样释放。
    let mut enabled = plugins.load_instances().await.unwrap().instances.remove(0);
    enabled.enabled = true;
    let saved = plugins
        .save_instance(enabled, disabled, &context())
        .await
        .unwrap();
    claim(&database, &owner_of(&saved.instance), "one").await;
    let mut off = saved.instance.clone();
    off.enabled = false;
    let saved = plugins
        .save_instance(off, saved.config_revision, &context())
        .await
        .unwrap();
    assert_released(&database, &["one"], "1").await;

    // 删除只允许已停用实例；被技术暂停保留了接管的实例被删除时，先释放再删除行。
    let mut enabled = saved.instance.clone();
    enabled.enabled = true;
    let saved = plugins
        .save_instance(enabled, saved.config_revision, &context())
        .await
        .unwrap();
    claim(&database, &owner_of(&saved.instance), "two").await;
    let mut paused = saved.instance.clone();
    paused.enabled = false;
    let paused = plugins
        .pause_instance_for_state_transition(paused, saved.config_revision, no_state(), &context())
        .await
        .unwrap();
    assert!(
        PgAdminClientKeyStore::new(database.pool.clone())
            .client_key_weekly_control(&key_id("two"))
            .await
            .unwrap()
            .controller
            .is_some()
    );
    plugins
        .delete_instance(&paused.instance.id, paused.config_revision, &context())
        .await
        .unwrap();
    assert_released(&database, &["two"], "1").await;

    // 被同一事务替换的实例也释放。
    let old = plugins
        .save_instance(
            owned_instance(&sha, true),
            plugins.load_instances().await.unwrap().config_revision,
            &context(),
        )
        .await
        .unwrap();
    claim(&database, &owner_of(&old.instance), "one").await;
    let target = owned_instance(&sha, true);
    plugins
        .save_instance_replacing(
            target,
            old.config_revision,
            no_state(),
            &[PluginInstanceReplacement {
                id: old.instance.id.clone(),
                expected_revision: old.instance.revision.get(),
            }],
            &context(),
        )
        .await
        .unwrap();
    assert_released(&database, &["one"], "1").await;
    database.close().await;
}

#[tokio::test]
async fn state_transition_pause_retains_weekly_windows_and_is_not_a_user_disable() {
    let Some(database) = TestDatabase::create("weekly_control_pause").await else {
        return;
    };
    let (plugins, sha, config_revision) = accepted_artifact(&database, 'd').await;
    seed(&database, "key", "10", "20").await;
    let saved = plugins
        .save_instance(owned_instance(&sha, true), config_revision, &context())
        .await
        .unwrap();
    let owner = owner_of(&saved.instance);
    claim(&database, &owner, "key").await;
    let held = window(&database, "key").await;
    assert_eq!(held.0.as_deref(), Some(owner.instance_id.as_str()));

    // 已停用的实例不能再走技术暂停入口。
    let mut off = saved.instance.clone();
    off.enabled = false;
    let mut other_artifact = saved.instance.clone();
    other_artifact.artifact_sha256 = "0".repeat(64);
    other_artifact.enabled = false;
    assert!(
        plugins
            .pause_instance_for_state_transition(
                other_artifact,
                saved.config_revision,
                no_state(),
                &context(),
            )
            .await
            .is_err()
    );
    let mut still_enabled = saved.instance.clone();
    still_enabled.enabled = true;
    assert_eq!(
        plugins
            .pause_instance_for_state_transition(
                still_enabled,
                saved.config_revision,
                no_state(),
                &context()
            )
            .await
            .map(|_| ())
            .unwrap_err()
            .kind(),
        AdminStoreErrorKind::Conflict
    );
    assert_eq!(window(&database, "key").await, held);

    let paused = plugins
        .pause_instance_for_state_transition(off, saved.config_revision, no_state(), &context())
        .await
        .unwrap();
    assert!(!paused.instance.enabled);
    assert_eq!(window(&database, "key").await, held);

    // 迁移失败后恢复原版本重新启用，接管者仍是同一实例。
    let mut restored = paused.instance.clone();
    restored.enabled = true;
    plugins
        .save_instance_with_state(restored, paused.config_revision, no_state(), &context())
        .await
        .unwrap();
    assert_eq!(window(&database, "key").await, held);

    database.close().await;
}
