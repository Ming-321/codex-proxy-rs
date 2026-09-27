use std::{collections::BTreeMap, time::SystemTime};

use chrono::{DateTime, Utc};
use gateway_admin::{
    model::{
        MutationActor, MutationContext, Revision,
        client_keys::{
            ClientKeyBudgetMutationOrigin, ClientKeyBudgetPeriod, ResetClientKeyBudget,
            UpdateClientKey, UpdateClientKeyBudgetLimits,
        },
        plugin_resources::PluginResourceOwner,
        plugins::{
            PluginSource,
            instances::{PluginInstance, PluginPermissionGrant},
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

#[tokio::test]
async fn weekly_control_preserves_claim_deduplicates_sync_and_waits_at_expiry() {
    use gateway_admin::model::weekly_budget::{ChangeWeeklyBudget, WeeklyBudgetAction};
    let Some(database) = TestDatabase::create("weekly_control").await else {
        return;
    };
    seed(&database, "key", "100", "100").await;
    let owner = plugin_reset_owner(&database).await;
    let origin = ClientKeyBudgetMutationOrigin::Plugin(owner.clone());
    let admin = PgAdminClientKeyStore::new(database.pool.clone());
    let budget = PgClientBudgetStore::new(database.pool.clone());
    budget
        .settle(charge("key", "before_claim", "3"))
        .await
        .unwrap();
    let before = status(&database, "key").await;
    let claim = ChangeWeeklyBudget {
        id: key_id("key"),
        expected_revision: 0,
        action: WeeklyBudgetAction::Claim {
            expires_at: Utc::now() + chrono::Duration::days(2),
            clear_used: false,
        },
    };
    admin
        .change_weekly_budget(claim.clone(), origin.clone(), &context())
        .await
        .unwrap();
    let after = status(&database, "key").await;
    assert_eq!(after.weekly_used_usd, before.weekly_used_usd);
    assert_eq!(after.daily_used_usd, before.daily_used_usd);
    assert_eq!(after.limits, before.limits);
    assert_eq!(
        after.weekly_controller.as_deref(),
        Some(owner.instance_id.as_str())
    );
    budget
        .settle(charge("key", "after_claim", "1"))
        .await
        .unwrap();
    admin
        .change_weekly_budget(claim, origin.clone(), &context())
        .await
        .unwrap();
    assert_eq!(
        status(&database, "key").await.weekly_used_usd.canonical(),
        "4"
    );
    let late = charge("key", "late", "7");
    // 数据库时间推进夹具模拟账号窗口到期，不等待实际两天。
    sqlx::query("update client_key_budget_windows set weekly_end=now()-interval '1 second' where client_api_key_id='key'").execute(&database.pool).await.unwrap();
    assert_eq!(
        budget
            .admit(key_id("key"))
            .await
            .unwrap_err()
            .client_error_code(),
        Some("key_weekly_window_waiting")
    );
    budget.settle(charge("key", "inflight", "2")).await.unwrap();
    let waiting = status(&database, "key").await;
    assert!(waiting.weekly_waiting);
    assert_eq!(waiting.weekly_used_usd.canonical(), "6");
    let sync = ChangeWeeklyBudget {
        id: key_id("key"),
        expected_revision: 1,
        action: WeeklyBudgetAction::Sync {
            expires_at: Utc::now() + chrono::Duration::days(7),
        },
    };
    admin
        .change_weekly_budget(sync.clone(), origin.clone(), &context())
        .await
        .unwrap();
    budget.settle(late).await.unwrap();
    assert_eq!(
        status(&database, "key").await.weekly_used_usd.canonical(),
        "0"
    );
    budget
        .settle(charge("key", "after_sync", "5"))
        .await
        .unwrap();
    // 用新的 Store 实例重试，模拟提交响应丢失后进程重启。
    let restarted = PgAdminClientKeyStore::new(database.pool.clone());
    restarted
        .change_weekly_budget(sync.clone(), origin.clone(), &context())
        .await
        .unwrap();
    assert_eq!(
        status(&database, "key").await.weekly_used_usd.canonical(),
        "5"
    );
    budget.admit(key_id("key")).await.unwrap();
    let early = ChangeWeeklyBudget {
        expected_revision: 2,
        action: WeeklyBudgetAction::Sync {
            expires_at: Utc::now() + chrono::Duration::days(3),
        },
        ..sync.clone()
    };
    admin
        .change_weekly_budget(early, origin.clone(), &context())
        .await
        .unwrap();
    assert_eq!(
        admin
            .change_weekly_budget(sync, origin, &context())
            .await
            .unwrap_err()
            .kind(),
        AdminStoreErrorKind::StaleRevision
    );
    assert_eq!(
        status(&database, "key").await.weekly_used_usd.canonical(),
        "0"
    );
    sqlx::query("update client_api_keys set daily_limit_usd=1 where id='key'")
        .execute(&database.pool)
        .await
        .unwrap();
    assert_eq!(
        budget
            .admit(key_id("key"))
            .await
            .unwrap_err()
            .client_error_code(),
        Some("key_daily_budget_exceeded")
    );
    sqlx::query("update client_api_keys set enabled=false where id='key'")
        .execute(&database.pool)
        .await
        .unwrap();
    assert_eq!(
        budget.admit(key_id("key")).await.unwrap_err().kind(),
        GatewayErrorKind::PolicyDenied
    );
    let count: i64 = sqlx::query_scalar(
        "select count(*) from client_key_charge_events where client_api_key_id='key'",
    )
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(count, 5);
    database.close().await;
}

#[tokio::test]
async fn weekly_control_initializes_unused_keys_and_release_preserves_spending() {
    use gateway_admin::model::weekly_budget::{ChangeWeeklyBudget, WeeklyBudgetAction};
    let Some(database) = TestDatabase::create("weekly_release").await else {
        return;
    };
    seed(&database, "key", "0", "10").await;
    let owner = plugin_reset_owner(&database).await;
    let origin = ClientKeyBudgetMutationOrigin::Plugin(owner.clone());
    let admin = PgAdminClientKeyStore::new(database.pool.clone());
    let budget = PgClientBudgetStore::new(database.pool.clone());
    assert_eq!(
        admin
            .weekly_budget_control(&key_id("key"))
            .await
            .unwrap()
            .revision,
        0
    );
    let claim = ChangeWeeklyBudget {
        id: key_id("key"),
        expected_revision: 0,
        action: WeeklyBudgetAction::Claim {
            expires_at: Utc::now() + chrono::Duration::days(2),
            clear_used: true,
        },
    };
    admin
        .change_weekly_budget(claim.clone(), origin.clone(), &context())
        .await
        .unwrap();
    budget.admit(key_id("key")).await.unwrap();
    budget.settle(charge("key", "new_key", "2")).await.unwrap();
    admin
        .change_weekly_budget(claim.clone(), origin.clone(), &context())
        .await
        .unwrap();
    assert_eq!(
        status(&database, "key").await.weekly_used_usd.canonical(),
        "2"
    );
    let release = ChangeWeeklyBudget {
        id: key_id("key"),
        expected_revision: 1,
        action: WeeklyBudgetAction::Release,
    };
    admin
        .change_weekly_budget(
            release.clone(),
            ClientKeyBudgetMutationOrigin::Admin,
            &context(),
        )
        .await
        .unwrap();
    let after = status(&database, "key").await;
    assert!(after.weekly_controller.is_none());
    assert_eq!(after.weekly_used_usd.canonical(), "2");
    assert_eq!(after.limits.weekly_usd.canonical(), "10");
    let native_end: DateTime<Utc> = sqlx::query_scalar("select (date_trunc('day',now() at time zone 'Asia/Shanghai') at time zone 'Asia/Shanghai')+interval '168 hours'").fetch_one(&database.pool).await.unwrap();
    assert_eq!(after.weekly_resets_at, Some(native_end.into()));
    assert_eq!(
        admin
            .change_weekly_budget(claim, origin.clone(), &context())
            .await
            .unwrap_err()
            .kind(),
        AdminStoreErrorKind::StaleRevision
    );
    admin
        .change_weekly_budget(release, ClientKeyBudgetMutationOrigin::Admin, &context())
        .await
        .unwrap();
    // 重新接管保留金额，不重放首次清零；明确停用由宿主解除。
    admin
        .change_weekly_budget(
            ChangeWeeklyBudget {
                id: key_id("key"),
                expected_revision: 2,
                action: WeeklyBudgetAction::Claim {
                    expires_at: Utc::now() + chrono::Duration::days(1),
                    clear_used: false,
                },
            },
            origin.clone(),
            &context(),
        )
        .await
        .unwrap();
    let plugins = PgPluginStore::new(database.pool.clone());
    let snapshot = plugins.load_instances().await.unwrap();
    let mut instance = snapshot.instances[0].clone();
    instance.enabled = false;
    plugins
        .save_instance(instance, snapshot.config_revision, &context())
        .await
        .unwrap();
    let stopped = status(&database, "key").await;
    assert!(stopped.weekly_controller.is_none());
    assert_eq!(stopped.weekly_used_usd.canonical(), "2");
    assert_eq!(stopped.weekly_control_revision, 4);
    let old = ChangeWeeklyBudget {
        id: key_id("key"),
        expected_revision: 4,
        action: WeeklyBudgetAction::Sync {
            expires_at: Utc::now() + chrono::Duration::days(3),
        },
    };
    assert!(
        admin
            .change_weekly_budget(old, origin, &context())
            .await
            .is_err()
    );
    sqlx::query("update client_key_budget_windows set weekly_end=now()-interval '1 second' where client_api_key_id='key'").execute(&database.pool).await.unwrap();
    budget.admit(key_id("key")).await.unwrap();
    assert_eq!(
        status(&database, "key").await.weekly_used_usd.canonical(),
        "0"
    );
    database.close().await;
}

#[tokio::test]
async fn weekly_control_partial_progress_and_audit_failure_are_recoverable() {
    use gateway_admin::model::weekly_budget::{ChangeWeeklyBudget, WeeklyBudgetAction};
    let Some(database) = TestDatabase::create("weekly_partial").await else {
        return;
    };
    for key in ["first", "second"] {
        seed(&database, key, "0", "10").await;
    }
    let owner = plugin_reset_owner(&database).await;
    let origin = ClientKeyBudgetMutationOrigin::Plugin(owner);
    let admin = PgAdminClientKeyStore::new(database.pool.clone());
    let budget = PgClientBudgetStore::new(database.pool.clone());
    let first = ChangeWeeklyBudget {
        id: key_id("first"),
        expected_revision: 0,
        action: WeeklyBudgetAction::Claim {
            expires_at: Utc::now() + chrono::Duration::days(2),
            clear_used: true,
        },
    };
    admin
        .change_weekly_budget(first.clone(), origin.clone(), &context())
        .await
        .unwrap();
    let invalid = ChangeWeeklyBudget {
        id: key_id("second"),
        expected_revision: 0,
        action: WeeklyBudgetAction::Claim {
            expires_at: Utc::now() - chrono::Duration::days(1),
            clear_used: true,
        },
    };
    assert_eq!(
        admin
            .change_weekly_budget(invalid, origin.clone(), &context())
            .await
            .unwrap_err()
            .kind(),
        AdminStoreErrorKind::Invalid
    );
    budget
        .settle(charge("first", "partial", "2"))
        .await
        .unwrap();
    admin
        .change_weekly_budget(first.clone(), origin.clone(), &context())
        .await
        .unwrap();
    admin
        .change_weekly_budget(
            ChangeWeeklyBudget {
                id: key_id("second"),
                ..first.clone()
            },
            origin.clone(),
            &context(),
        )
        .await
        .unwrap();
    assert_eq!(
        status(&database, "first").await.weekly_used_usd.canonical(),
        "2"
    );
    assert_eq!(
        admin
            .weekly_budget_control(&key_id("second"))
            .await
            .unwrap()
            .revision,
        1
    );
    sqlx::raw_sql("create function reject_weekly_audit() returns trigger language plpgsql as $$ begin if NEW.action='weekly_control' then raise exception 'fixture audit failure'; end if; return NEW; end $$; create trigger reject_weekly_audit before insert on admin_audit_events for each row execute function reject_weekly_audit()")
        .execute(&database.pool).await.unwrap();
    let sync = ChangeWeeklyBudget {
        expected_revision: 1,
        action: WeeklyBudgetAction::Sync {
            expires_at: Utc::now() + chrono::Duration::days(3),
        },
        ..first
    };
    assert!(
        admin
            .change_weekly_budget(sync.clone(), origin.clone(), &context())
            .await
            .is_err()
    );
    assert_eq!(
        admin
            .weekly_budget_control(&key_id("first"))
            .await
            .unwrap()
            .revision,
        1
    );
    assert_eq!(
        status(&database, "first").await.weekly_used_usd.canonical(),
        "2"
    );
    sqlx::query("drop trigger reject_weekly_audit on admin_audit_events")
        .execute(&database.pool)
        .await
        .unwrap();
    let mutation = context();
    let (left, right) = tokio::join!(
        admin.change_weekly_budget(sync.clone(), origin.clone(), &mutation),
        admin.change_weekly_budget(sync, origin, &mutation)
    );
    assert_eq!(left.unwrap().revision, 2);
    assert_eq!(right.unwrap().revision, 2);
    database.close().await;
}

#[tokio::test]
async fn weekly_control_rejects_competing_owners_and_releases_when_permission_is_removed() {
    use gateway_admin::model::weekly_budget::{ChangeWeeklyBudget, WeeklyBudgetAction};
    let Some(database) = TestDatabase::create("weekly_owner").await else {
        return;
    };
    seed(&database, "key", "0", "10").await;
    let owner = plugin_reset_owner(&database).await;
    let admin = PgAdminClientKeyStore::new(database.pool.clone());
    let plugins = PgPluginStore::new(database.pool.clone());
    let claim = ChangeWeeklyBudget {
        id: key_id("key"),
        expected_revision: 0,
        action: WeeklyBudgetAction::Claim {
            expires_at: Utc::now() + chrono::Duration::days(2),
            clear_used: false,
        },
    };
    admin
        .change_weekly_budget(
            claim.clone(),
            ClientKeyBudgetMutationOrigin::Plugin(owner.clone()),
            &context(),
        )
        .await
        .unwrap();
    let snapshot = plugins.load_instances().await.unwrap();
    let original = snapshot.instances[0].clone();
    let mut competing = original.clone();
    competing.id = uuid::Uuid::now_v7().to_string();
    competing.name = "competing controller".into();
    let competing = plugins
        .save_instance(competing, snapshot.config_revision, &context())
        .await
        .unwrap()
        .instance;
    let other = PluginResourceOwner {
        instance_id: competing.id,
        artifact_sha256: competing.artifact_sha256,
        revision: competing.revision,
    };
    assert_eq!(
        admin
            .change_weekly_budget(
                ChangeWeeklyBudget {
                    expected_revision: 1,
                    ..claim.clone()
                },
                ClientKeyBudgetMutationOrigin::Plugin(other),
                &context()
            )
            .await
            .unwrap_err()
            .kind(),
        AdminStoreErrorKind::Conflict
    );
    let budget = PgClientBudgetStore::new(database.pool.clone());
    budget
        .settle(charge("key", "before_revoke", "3"))
        .await
        .unwrap();
    let mut replacement = super::plugins::artifacts::artifact('c', &["linux-x86_64"]);
    replacement.metadata.version = "1.1.0".into();
    let installed = plugins
        .install_artifact(replacement, PluginSource::Upload, &context())
        .await
        .unwrap();
    let accepted = plugins
        .accept_artifact(&installed.artifact.metadata.sha256, &context())
        .await
        .unwrap();
    let mut without_permission = original;
    without_permission.artifact_sha256 = installed.artifact.metadata.sha256;
    without_permission.grants.clear();
    let saved = plugins
        .save_instance(without_permission, accepted.config_revision, &context())
        .await
        .unwrap();
    let state = status(&database, "key").await;
    assert!(state.weekly_controller.is_none());
    assert_eq!(state.weekly_used_usd.canonical(), "3");
    assert_eq!(state.weekly_control_revision, 2);
    assert!(
        admin
            .change_weekly_budget(
                ChangeWeeklyBudget {
                    expected_revision: 2,
                    ..claim
                },
                ClientKeyBudgetMutationOrigin::Plugin(owner.clone()),
                &context()
            )
            .await
            .is_err()
    );
    let mut disabled = saved.instance;
    disabled.enabled = false;
    let stopped = plugins
        .save_instance(disabled, saved.config_revision, &context())
        .await
        .unwrap();
    plugins
        .delete_instance(&owner.instance_id, stopped.config_revision, &context())
        .await
        .unwrap();
    assert_eq!(
        status(&database, "key").await.weekly_used_usd.canonical(),
        "3"
    );
    database.close().await;
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
    let mut package = super::plugins::artifacts::artifact('b', &["linux-x86_64"]);
    package.metadata.requested_permissions = vec!["key_budgets".into()];
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
                grants: vec![PluginPermissionGrant {
                    permission: "key_budgets".into(),
                }],
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
async fn plugin_reset_revalidates_current_authorization_without_changing_the_ledger() {
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
        "update plugin_artifacts set accepted_at=now(), metadata_json=jsonb_set(metadata_json,'{requestedPermissions}','[]'::jsonb)",
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
    sqlx::query("update plugin_artifacts set metadata_json=jsonb_set(metadata_json,'{requestedPermissions}','[\"key_budgets\"]'::jsonb)")
        .execute(&database.pool).await.unwrap();
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
