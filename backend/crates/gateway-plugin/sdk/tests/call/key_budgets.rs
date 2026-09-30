use gateway_plugin_sdk::call::key_budgets::{
    BudgetPeriod, ChangeWeeklyWindowRequest, ResetKeyBudgetRequest, WeeklyWindowAction,
    WeeklyWindowControl, WeeklyWindowQuery,
};
use serde_json::json;

#[test]
fn reset_requires_an_explicit_supported_period_and_rejects_extra_authority() {
    for period in [BudgetPeriod::Daily, BudgetPeriod::Weekly, BudgetPeriod::All] {
        let request = ResetKeyBudgetRequest {
            client_key_id: "key_1".into(),
            period,
        };
        assert_eq!(
            serde_json::from_value::<ResetKeyBudgetRequest>(
                serde_json::to_value(&request).unwrap()
            )
            .unwrap(),
            request
        );
    }
    for invalid in [
        json!({"client_key_id":"key_1"}),
        json!({"client_key_id":"key_1","period":"monthly"}),
        json!({"client_key_id":"key_1","period":"weekly","instance_id":"forged"}),
    ] {
        assert!(serde_json::from_value::<ResetKeyBudgetRequest>(invalid).is_err());
    }
}

#[test]
fn weekly_window_actions_keep_their_wire_shape_and_default_to_preserving_usage() {
    for (action, wire) in [
        (
            WeeklyWindowAction::Claim {
                expires_at_ms: 10,
                clear_used: true,
            },
            json!({"action":"claim","expires_at_ms":10,"clear_used":true}),
        ),
        (
            WeeklyWindowAction::Sync { expires_at_ms: 11 },
            json!({"action":"sync","expires_at_ms":11}),
        ),
        (
            WeeklyWindowAction::Align { expires_at_ms: 12 },
            json!({"action":"align","expires_at_ms":12}),
        ),
        (WeeklyWindowAction::Release, json!({"action":"release"})),
    ] {
        assert_eq!(serde_json::to_value(action).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<WeeklyWindowAction>(wire).unwrap(),
            action
        );
    }
    // `clear_used` 缺省为保留已用金额。
    assert_eq!(
        serde_json::from_value::<WeeklyWindowAction>(json!({"action":"claim","expires_at_ms":10}))
            .unwrap(),
        WeeklyWindowAction::Claim {
            expires_at_ms: 10,
            clear_used: false
        }
    );
    for invalid in [
        json!({"action":"reset"}),
        json!({"action":"sync"}),
        json!({"action":"align","expires_at_ms":1,"instance_id":"forged"}),
    ] {
        assert!(serde_json::from_value::<WeeklyWindowAction>(invalid).is_err());
    }
}

#[test]
fn weekly_window_requests_reject_forged_authority_and_missing_revision() {
    let request = ChangeWeeklyWindowRequest {
        client_key_id: "key_1".into(),
        expected_revision: 2,
        operation: WeeklyWindowAction::Release,
    };
    assert_eq!(
        serde_json::from_value::<ChangeWeeklyWindowRequest>(
            serde_json::to_value(&request).unwrap()
        )
        .unwrap(),
        request
    );
    for invalid in [
        json!({"client_key_id":"key_1","operation":{"action":"release"}}),
        json!({"client_key_id":"key_1","expected_revision":2}),
        json!({"client_key_id":"key_1","expected_revision":-1,"operation":{"action":"release"}}),
        json!({"client_key_id":"key_1","expected_revision":2,"operation":{"action":"release"},"instance_id":"forged"}),
    ] {
        assert!(serde_json::from_value::<ChangeWeeklyWindowRequest>(invalid).is_err());
    }
    assert!(
        serde_json::from_value::<WeeklyWindowQuery>(
            json!({"client_key_id":"key_1","instance_id":"forged"})
        )
        .is_err()
    );
    let control = WeeklyWindowControl {
        revision: 0,
        controller: None,
        expires_at_ms: None,
        accounting_start_at_ms: None,
        waiting: false,
    };
    assert_eq!(
        serde_json::from_value::<WeeklyWindowControl>(serde_json::to_value(&control).unwrap())
            .unwrap(),
        control
    );
}
