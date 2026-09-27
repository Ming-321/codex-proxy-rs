use super::*;
use gateway_admin::model::{
    plugins::state::{PluginStateCommit, PluginStateConfiguration},
    weekly_budget::{ChangeWeeklyBudget, WeeklyBudgetAction},
};

#[tokio::test]
async fn waiting_manual_reset_excludes_late_charges_without_releasing_waiting() {
    let Some(database) = TestDatabase::create("weekly_waiting_reset").await else {
        return;
    };
    let owner = plugin_reset_owner(&database).await;
    let admin = PgAdminClientKeyStore::new(database.pool.clone());
    let budget = PgClientBudgetStore::new(database.pool.clone());
    for (id, period) in [
        ("weekly", ClientKeyBudgetPeriod::Weekly),
        ("all", ClientKeyBudgetPeriod::All),
    ] {
        seed(&database, id, "100", "100").await;
        admin
            .change_weekly_budget(
                ChangeWeeklyBudget {
                    id: key_id(id),
                    expected_revision: 0,
                    action: WeeklyBudgetAction::Claim {
                        expires_at: Utc::now() + chrono::Duration::days(2),
                        clear_used: false,
                    },
                },
                ClientKeyBudgetMutationOrigin::Plugin(owner.clone()),
                &context(),
            )
            .await
            .unwrap();
        budget
            .settle(charge(id, &format!("{id}_before"), "3"))
            .await
            .unwrap();
        sqlx::query("update client_key_budget_windows set weekly_end=now()-interval '1 second' where client_api_key_id=$1")
            .bind(id).execute(&database.pool).await.unwrap();
        let before = admin.weekly_budget_control(&key_id(id)).await.unwrap();
        let delayed = charge(id, &format!("{id}_late"), "2");
        admin
            .reset_client_key_budget(
                ResetClientKeyBudget {
                    id: key_id(id),
                    period,
                },
                ClientKeyBudgetMutationOrigin::Admin,
                &context(),
            )
            .await
            .unwrap();
        budget.settle(delayed).await.unwrap();
        assert_eq!(status(&database, id).await.weekly_used_usd.canonical(), "0");
        let after = admin.weekly_budget_control(&key_id(id)).await.unwrap();
        assert_eq!(after.controller, before.controller);
        assert_eq!(after.expires_at, before.expires_at);
        assert_eq!(after.revision, before.revision);
        assert!(after.accounting_start > before.accounting_start);
        assert!(after.waiting);
        assert_eq!(
            budget
                .admit(key_id(id))
                .await
                .unwrap_err()
                .client_error_code(),
            Some("key_weekly_window_waiting")
        );
        budget
            .settle(charge(id, &format!("{id}_after"), "1"))
            .await
            .unwrap();
        assert_eq!(status(&database, id).await.weekly_used_usd.canonical(), "1");
    }
    database.close().await;
}

#[tokio::test]
async fn align_preserves_accounting_start_spending_and_retry_identity() {
    let Some(database) = TestDatabase::create("weekly_align").await else {
        return;
    };
    seed(&database, "key", "100", "100").await;
    let owner = plugin_reset_owner(&database).await;
    let origin = ClientKeyBudgetMutationOrigin::Plugin(owner);
    let admin = PgAdminClientKeyStore::new(database.pool.clone());
    let budget = PgClientBudgetStore::new(database.pool.clone());
    let claimed = admin
        .change_weekly_budget(
            ChangeWeeklyBudget {
                id: key_id("key"),
                expected_revision: 0,
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
    budget.settle(charge("key", "used", "3")).await.unwrap();
    sqlx::query("update client_key_budget_windows set weekly_end=now()-interval '1 second' where client_api_key_id='key'").execute(&database.pool).await.unwrap();
    let pending = ChangeWeeklyBudget {
        id: key_id("key"),
        expected_revision: 1,
        action: WeeklyBudgetAction::Align {
            expires_at: Utc::now() + chrono::Duration::days(3),
        },
    };
    assert!(
        admin
            .change_weekly_budget(
                pending.clone(),
                ClientKeyBudgetMutationOrigin::Admin,
                &context()
            )
            .await
            .is_err()
    );
    let aligned = admin
        .change_weekly_budget(pending.clone(), origin.clone(), &context())
        .await
        .unwrap();
    assert!(!aligned.waiting);
    assert_eq!(aligned.accounting_start, claimed.accounting_start);
    assert_eq!(aligned.controller, claimed.controller);
    assert_eq!(
        status(&database, "key").await.weekly_used_usd.canonical(),
        "3"
    );
    budget.settle(charge("key", "after", "2")).await.unwrap();
    assert_eq!(
        admin
            .change_weekly_budget(pending, origin, &context())
            .await
            .unwrap(),
        aligned
    );
    assert_eq!(
        status(&database, "key").await.weekly_used_usd.canonical(),
        "5"
    );
    database.close().await;
}

#[tokio::test]
async fn migration_pause_preserves_control_and_old_process_loses_write_authority() {
    let Some(database) = TestDatabase::create("weekly_pause").await else {
        return;
    };
    seed(&database, "key", "0", "100").await;
    let owner = plugin_reset_owner(&database).await;
    let admin = PgAdminClientKeyStore::new(database.pool.clone());
    let plugins = PgPluginStore::new(database.pool.clone());
    let request = ChangeWeeklyBudget {
        id: key_id("key"),
        expected_revision: 0,
        action: WeeklyBudgetAction::Claim {
            expires_at: Utc::now() + chrono::Duration::days(2),
            clear_used: true,
        },
    };
    let control = admin
        .change_weekly_budget(
            request.clone(),
            ClientKeyBudgetMutationOrigin::Plugin(owner.clone()),
            &context(),
        )
        .await
        .unwrap();
    PgClientBudgetStore::new(database.pool.clone())
        .settle(charge("key", "during", "4"))
        .await
        .unwrap();
    let snapshot = plugins.load_instances().await.unwrap();
    let mut paused = snapshot.instances[0].clone();
    paused.enabled = false;
    let saved = plugins
        .pause_instance_for_state_transition(
            paused,
            snapshot.config_revision,
            PluginStateCommit {
                configuration: PluginStateConfiguration { namespaces: vec![] },
                transition_id: None,
            },
            &context(),
        )
        .await
        .unwrap();
    assert_eq!(
        admin.weekly_budget_control(&key_id("key")).await.unwrap(),
        control
    );
    assert!(
        admin
            .change_weekly_budget(
                request.clone(),
                ClientKeyBudgetMutationOrigin::Plugin(owner),
                &context()
            )
            .await
            .is_err()
    );
    let mut resumed = saved.instance;
    resumed.enabled = true;
    let saved = plugins
        .save_instance(resumed, saved.config_revision, &context())
        .await
        .unwrap();
    let current = PluginResourceOwner {
        instance_id: saved.instance.id,
        artifact_sha256: saved.instance.artifact_sha256,
        revision: saved.instance.revision,
    };
    admin
        .change_weekly_budget(
            request,
            ClientKeyBudgetMutationOrigin::Plugin(current),
            &context(),
        )
        .await
        .unwrap();
    assert_eq!(
        status(&database, "key").await.weekly_used_usd.canonical(),
        "4"
    );
    database.close().await;
}
