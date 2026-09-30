use super::*;
use gateway_admin::model::client_keys::{
    ChangeClientKeyBudgetWindow, ClientKeyBudgetWindow, ClientKeyBudgetWindowMode as Mode,
    ClientKeyBudgetWindowPeriod as Period, ClientKeyBudgetWindowUpdate as Update,
};

fn future() -> DateTime<Utc> {
    DateTime::from_timestamp_millis((Utc::now() + chrono::Duration::hours(3)).timestamp_millis())
        .unwrap()
}

fn command(
    key: &str,
    period: Period,
    revision: u64,
    update: Update,
) -> ChangeClientKeyBudgetWindow {
    ChangeClientKeyBudgetWindow {
        id: key_id(key),
        period,
        expected_revision: revision,
        update,
    }
}

async fn read(database: &TestDatabase, key: &str, period: Period) -> ClientKeyBudgetWindow {
    PgAdminClientKeyStore::new(database.pool.clone())
        .client_key_budget_window(&key_id(key), period)
        .await
        .unwrap()
}

fn used(budget: &ClientBudgetStatus, period: Period) -> String {
    match period {
        Period::Daily => budget.daily_used_usd,
        Period::Weekly => budget.weekly_used_usd,
    }
    .canonical()
}

#[tokio::test]
async fn window_policy_migration_preserves_existing_automatic_balances_and_dates() {
    let Some(database) = TestDatabase::create_through("window_upgrade", 20).await else {
        return;
    };
    seed(&database, "key", "10", "20").await;
    let end = future();
    sqlx::query("insert into client_key_budget_windows (client_api_key_id,daily_start,daily_end,daily_used_usd,weekly_start,weekly_end,weekly_used_usd) values ('key',now(),$1,2,now(),$1,3)")
        .bind(end).execute(&database.pool).await.unwrap();
    super::super::TEST_MIGRATOR
        .run(&database.pool)
        .await
        .unwrap();
    super::super::TEST_MIGRATOR
        .run(&database.pool)
        .await
        .unwrap();
    let budget = status(&database, "key").await;
    assert_eq!(budget.daily_used_usd.canonical(), "2");
    assert_eq!(budget.weekly_used_usd.canonical(), "3");
    for period in [Period::Daily, Period::Weekly] {
        let window = read(&database, "key", period).await;
        assert_eq!(window.mode, Mode::Automatic);
        assert_eq!(window.revision, 0);
        assert_eq!(window.expires_at, Some(end));
    }
    let budgets = PgClientBudgetStore::new(database.pool.clone());
    budgets.admit(key_id("key")).await.unwrap();
    budgets
        .settle(charge("key", "after-upgrade", "1"))
        .await
        .unwrap();
    let budget = status(&database, "key").await;
    assert_eq!(budget.daily_used_usd.canonical(), "3");
    assert_eq!(budget.weekly_used_usd.canonical(), "4");
    database.close().await;
}

#[tokio::test]
async fn native_window_commands_preserve_usage_and_retry_only_the_last_committed_change() {
    let Some(database) = TestDatabase::create("window_commands").await else {
        return;
    };
    let owner = plugin_reset_owner(&database).await;
    let store = PgAdminClientKeyStore::new(database.pool.clone());
    let budgets = PgClientBudgetStore::new(database.pool.clone());
    for (key, period, other) in [
        ("daily", Period::Daily, Period::Weekly),
        ("weekly", Period::Weekly, Period::Daily),
    ] {
        seed(&database, key, "10", "20").await;
        budgets.settle(charge(key, key, "3")).await.unwrap();
        let before = read(&database, key, period).await;
        let other_before = read(&database, key, other).await;
        let fixed = command(
            key,
            period,
            before.revision,
            Update::Fixed {
                expires_at: future(),
                clear_used: false,
            },
        );
        let first = store
            .change_client_key_budget_window(
                fixed.clone(),
                ClientKeyBudgetMutationOrigin::Admin,
                &context(),
            )
            .await
            .unwrap();
        assert_eq!(first.mode, Mode::Fixed);
        assert_eq!(first.accounting_start, before.accounting_start);
        assert_eq!(used(&status(&database, key).await, period), "3");
        assert_eq!(read(&database, key, other).await, other_before);
        budgets
            .settle(charge(key, &format!("{key}-after"), "2"))
            .await
            .unwrap();
        assert_eq!(
            store
                .change_client_key_budget_window(
                    fixed.clone(),
                    ClientKeyBudgetMutationOrigin::Admin,
                    &context()
                )
                .await
                .unwrap(),
            first
        );
        assert_eq!(used(&status(&database, key).await, period), "5");
        let mut other_actor = context();
        other_actor.actor = MutationActor::AdminSession {
            admin_user_id: "another-admin".into(),
        };
        assert_eq!(
            store
                .change_client_key_budget_window(
                    fixed.clone(),
                    ClientKeyBudgetMutationOrigin::Admin,
                    &other_actor
                )
                .await
                .unwrap_err()
                .kind(),
            AdminStoreErrorKind::StaleRevision
        );
        assert_eq!(
            store
                .change_client_key_budget_window(
                    fixed.clone(),
                    ClientKeyBudgetMutationOrigin::Plugin(owner.clone()),
                    &context(),
                )
                .await
                .unwrap_err()
                .kind(),
            AdminStoreErrorKind::StaleRevision
        );
        // 没有接管者：其他已授权调用方可按当前版本写入，同一版本只有一个写入成功。
        let reset = command(
            key,
            period,
            first.revision,
            Update::Fixed {
                expires_at: future(),
                clear_used: true,
            },
        );
        let next = store
            .change_client_key_budget_window(
                reset.clone(),
                ClientKeyBudgetMutationOrigin::Plugin(owner.clone()),
                &context(),
            )
            .await
            .unwrap();
        assert!(next.accounting_start > first.accounting_start);
        assert_eq!(used(&status(&database, key).await, period), "0");
        budgets
            .settle(charge(key, &format!("{key}-new"), "1"))
            .await
            .unwrap();
        assert_eq!(
            store
                .change_client_key_budget_window(
                    reset,
                    ClientKeyBudgetMutationOrigin::Plugin(owner.clone()),
                    &context()
                )
                .await
                .unwrap(),
            next
        );
        assert_eq!(used(&status(&database, key).await, period), "1");
        assert_eq!(
            store
                .change_client_key_budget_window(
                    fixed,
                    ClientKeyBudgetMutationOrigin::Admin,
                    &context()
                )
                .await
                .unwrap_err()
                .kind(),
            AdminStoreErrorKind::StaleRevision
        );
        let align = command(
            key,
            period,
            next.revision,
            Update::Fixed {
                expires_at: future() + chrono::Duration::hours(1),
                clear_used: false,
            },
        );
        let aligned = store
            .change_client_key_budget_window(
                align,
                ClientKeyBudgetMutationOrigin::Admin,
                &context(),
            )
            .await
            .unwrap();
        assert_eq!(aligned.accounting_start, next.accounting_start);
        assert_eq!(used(&status(&database, key).await, period), "1");
        let automatic = store
            .change_client_key_budget_window(
                command(key, period, aligned.revision, Update::Automatic),
                ClientKeyBudgetMutationOrigin::Admin,
                &context(),
            )
            .await
            .unwrap();
        assert_eq!(automatic.mode, Mode::Automatic);
        assert_eq!(automatic.expires_at, aligned.expires_at);
        assert_eq!(used(&status(&database, key).await, period), "1");
    }
    let audits: i64 = sqlx::query_scalar("select count(*) from admin_audit_events where action='change_budget_window' and config_revision is null").fetch_one(&database.pool).await.unwrap();
    assert_eq!(audits, 8);
    database.close().await;
}

#[tokio::test]
async fn fixed_expiry_blocks_admission_but_settles_until_an_explicit_new_window() {
    let Some(database) = TestDatabase::create("window_expiry").await else {
        return;
    };
    let store = PgAdminClientKeyStore::new(database.pool.clone());
    let budgets = PgClientBudgetStore::new(database.pool.clone());
    for (key, period) in [("daily", Period::Daily), ("weekly", Period::Weekly)] {
        seed(&database, key, "0", "0").await;
        budgets.settle(charge(key, key, "3")).await.unwrap();
        let before = read(&database, key, period).await;
        let fixed = store
            .change_client_key_budget_window(
                command(
                    key,
                    period,
                    before.revision,
                    Update::Fixed {
                        expires_at: future(),
                        clear_used: false,
                    },
                ),
                ClientKeyBudgetMutationOrigin::Admin,
                &context(),
            )
            .await
            .unwrap();
        // 直接推进测试窗口的截止边界，模拟时间流逝，不依赖运行速度或时区零点。
        sqlx::query("update client_key_budget_windows set daily_start=case when $2 then now()-interval '1 hour' else daily_start end, weekly_start=case when not $2 then now()-interval '1 hour' else weekly_start end, daily_end=case when $2 then now()-interval '1 second' else daily_end end, weekly_end=case when not $2 then now()-interval '1 second' else weekly_end end where client_api_key_id=$1")
            .bind(key).bind(period == Period::Daily).execute(&database.pool).await.unwrap();
        let expired = read(&database, key, period).await;
        assert_eq!(
            budgets
                .admit(key_id(key))
                .await
                .unwrap_err()
                .client_error_code(),
            Some("key_budget_window_expired")
        );
        budgets
            .settle(charge(key, &format!("{key}-late"), "2"))
            .await
            .unwrap();
        assert_eq!(used(&status(&database, key).await, period), "5");
        assert_eq!(read(&database, key, period).await, expired);
        let late = charge(key, &format!("{key}-old-completion"), "4");
        let next = store
            .change_client_key_budget_window(
                command(
                    key,
                    period,
                    fixed.revision,
                    Update::Fixed {
                        expires_at: future(),
                        clear_used: true,
                    },
                ),
                ClientKeyBudgetMutationOrigin::Admin,
                &context(),
            )
            .await
            .unwrap();
        budgets.settle(late).await.unwrap();
        assert_eq!(used(&status(&database, key).await, period), "0");
        budgets.admit(key_id(key)).await.unwrap();
        budgets
            .settle(charge(key, &format!("{key}-new"), "1"))
            .await
            .unwrap();
        assert_eq!(used(&status(&database, key).await, period), "1");
        assert_eq!(read(&database, key, period).await, next);
    }
    database.close().await;
}

#[tokio::test]
async fn native_reset_closes_fixed_windows_and_invalidates_pending_writes_even_for_unused_keys() {
    let Some(database) = TestDatabase::create("window_native_reset").await else {
        return;
    };
    let store = PgAdminClientKeyStore::new(database.pool.clone());
    let budgets = PgClientBudgetStore::new(database.pool.clone());
    for (key, period) in [
        ("daily", ClientKeyBudgetPeriod::Daily),
        ("weekly", ClientKeyBudgetPeriod::Weekly),
        ("all", ClientKeyBudgetPeriod::All),
    ] {
        seed(&database, key, "10", "20").await;
        budgets.settle(charge(key, key, "3")).await.unwrap();
        let mut commands = Vec::new();
        for window in [Period::Daily, Period::Weekly] {
            let before = read(&database, key, window).await;
            let change = command(
                key,
                window,
                before.revision,
                Update::Fixed {
                    expires_at: future(),
                    clear_used: false,
                },
            );
            store
                .change_client_key_budget_window(
                    change.clone(),
                    ClientKeyBudgetMutationOrigin::Admin,
                    &context(),
                )
                .await
                .unwrap();
            commands.push(change);
        }
        let late = charge(key, &format!("{key}-late"), "1");
        store
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
        budgets.settle(late).await.unwrap();
        for change in commands {
            let selected = period == ClientKeyBudgetPeriod::All
                || matches!(
                    (period, change.period),
                    (ClientKeyBudgetPeriod::Daily, Period::Daily)
                        | (ClientKeyBudgetPeriod::Weekly, Period::Weekly)
                );
            let window = read(&database, key, change.period).await;
            if selected {
                assert_eq!(window.mode, Mode::Automatic);
                assert_eq!(window.expires_at, None);
                assert_eq!(used(&status(&database, key).await, change.period), "0");
                assert_eq!(
                    store
                        .change_client_key_budget_window(
                            change,
                            ClientKeyBudgetMutationOrigin::Admin,
                            &context()
                        )
                        .await
                        .unwrap_err()
                        .kind(),
                    AdminStoreErrorKind::StaleRevision
                );
            } else {
                assert_eq!(window.mode, Mode::Fixed);
                assert_eq!(used(&status(&database, key).await, change.period), "4");
            }
        }
        budgets.admit(key_id(key)).await.unwrap();
        let after = status(&database, key).await;
        assert!(after.daily_resets_at.is_some());
        assert!(after.weekly_resets_at.is_some());
    }
    // 未开窗的 Key 只重置一侧，另一侧仍须接纳重置前完成、之后才入账的请求。
    for (key, reset, selected, other) in [
        (
            "unused_daily",
            ClientKeyBudgetPeriod::Daily,
            Period::Daily,
            Period::Weekly,
        ),
        (
            "unused_weekly",
            ClientKeyBudgetPeriod::Weekly,
            Period::Weekly,
            Period::Daily,
        ),
    ] {
        seed(&database, key, "10", "20").await;
        let late = charge(key, key, "2");
        store
            .reset_client_key_budget(
                ResetClientKeyBudget {
                    id: key_id(key),
                    period: reset,
                },
                ClientKeyBudgetMutationOrigin::Admin,
                &context(),
            )
            .await
            .unwrap();
        budgets.settle(late).await.unwrap();
        let budget = status(&database, key).await;
        assert_eq!(used(&budget, selected), "0");
        assert_eq!(used(&budget, other), "2");
        assert_eq!(read(&database, key, selected).await.expires_at, None);
        budgets
            .settle(charge(key, &format!("{key}-next"), "1"))
            .await
            .unwrap();
        assert_eq!(used(&status(&database, key).await, selected), "1");
    }
    seed(&database, "unused", "10", "20").await;
    store
        .reset_client_key_budget(
            ResetClientKeyBudget {
                id: key_id("unused"),
                period: ClientKeyBudgetPeriod::All,
            },
            ClientKeyBudgetMutationOrigin::Admin,
            &context(),
        )
        .await
        .unwrap();
    assert_eq!(read(&database, "unused", Period::Weekly).await.revision, 1);
    assert_eq!(
        read(&database, "unused", Period::Weekly).await.expires_at,
        None
    );
    assert_eq!(
        store
            .change_client_key_budget_window(
                command(
                    "unused",
                    Period::Weekly,
                    0,
                    Update::Fixed {
                        expires_at: future(),
                        clear_used: false
                    }
                ),
                ClientKeyBudgetMutationOrigin::Admin,
                &context()
            )
            .await
            .unwrap_err()
            .kind(),
        AdminStoreErrorKind::StaleRevision
    );
    database.close().await;
}

#[tokio::test]
async fn window_changes_revalidate_authority_and_rollback_with_audit() {
    let Some(database) = TestDatabase::create("window_atomicity").await else {
        return;
    };
    let owner = plugin_reset_owner(&database).await;
    seed(&database, "key", "10", "20").await;
    let store = PgAdminClientKeyStore::new(database.pool.clone());
    let change = command(
        "key",
        Period::Weekly,
        0,
        Update::Fixed {
            expires_at: future(),
            clear_used: true,
        },
    );
    let before = read(&database, "key", Period::Weekly).await;
    sqlx::raw_sql("create function reject_window_audit() returns trigger language plpgsql as $$ begin raise exception 'test rollback'; end $$; create trigger reject_window_audit before insert on admin_audit_events for each row execute function reject_window_audit()")
        .execute(&database.pool).await.unwrap();
    assert!(
        store
            .change_client_key_budget_window(
                change.clone(),
                ClientKeyBudgetMutationOrigin::Plugin(owner.clone()),
                &context()
            )
            .await
            .is_err()
    );
    assert_eq!(read(&database, "key", Period::Weekly).await, before);
    sqlx::query("drop trigger reject_window_audit on admin_audit_events")
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("update plugin_instances set enabled=false")
        .execute(&database.pool)
        .await
        .unwrap();
    assert_eq!(
        store
            .change_client_key_budget_window(
                change.clone(),
                ClientKeyBudgetMutationOrigin::Plugin(owner.clone()),
                &context()
            )
            .await
            .unwrap_err()
            .kind(),
        AdminStoreErrorKind::Conflict
    );
    assert_eq!(read(&database, "key", Period::Weekly).await, before);
    let mutation = context();
    let (first, second) = tokio::join!(
        store.change_client_key_budget_window(
            change.clone(),
            ClientKeyBudgetMutationOrigin::Admin,
            &mutation
        ),
        store.change_client_key_budget_window(
            command(
                "key",
                Period::Weekly,
                0,
                Update::Fixed {
                    expires_at: future() + chrono::Duration::hours(1),
                    clear_used: true
                }
            ),
            ClientKeyBudgetMutationOrigin::Admin,
            &mutation
        ),
    );
    assert!(first.is_ok() ^ second.is_ok());
    let error = first.err().or_else(|| second.err()).unwrap();
    assert_eq!(error.kind(), AdminStoreErrorKind::StaleRevision);
    sqlx::query("update plugin_instances set enabled=true")
        .execute(&database.pool)
        .await
        .unwrap();
    let current = read(&database, "key", Period::Weekly).await;
    let pending = command(
        "key",
        Period::Weekly,
        current.revision,
        Update::Fixed {
            expires_at: future(),
            clear_used: true,
        },
    );
    let committed = store
        .change_client_key_budget_window(
            pending.clone(),
            ClientKeyBudgetMutationOrigin::Plugin(owner.clone()),
            &context(),
        )
        .await
        .unwrap();
    sqlx::query("update plugin_instances set enabled=false")
        .execute(&database.pool)
        .await
        .unwrap();
    assert_eq!(
        store
            .change_client_key_budget_window(
                pending,
                ClientKeyBudgetMutationOrigin::Plugin(owner),
                &context()
            )
            .await
            .unwrap_err()
            .kind(),
        AdminStoreErrorKind::Conflict
    );
    assert_eq!(read(&database, "key", Period::Weekly).await, committed);
    database.close().await;
}
