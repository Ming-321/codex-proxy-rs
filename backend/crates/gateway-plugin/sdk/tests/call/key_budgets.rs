use gateway_plugin_sdk::call::key_budgets::{
    BudgetPeriod, BudgetWindowQuery, BudgetWindowUpdate, ChangeBudgetWindowRequest,
    ResetKeyBudgetRequest,
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
fn window_commands_require_a_version_and_keep_authority_out_of_the_payload() {
    for period in ["daily", "weekly"] {
        for update in [
            json!({"mode":"automatic"}),
            json!({"mode":"fixed","expires_at_ms":1_900_000_000_000_i64,"clear_used":true}),
        ] {
            let value = json!({"client_key_id":"key_1","period":period,"expected_revision":7,"update":update});
            let request: ChangeBudgetWindowRequest = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(request).unwrap(), value);
        }
    }
    let fixed: BudgetWindowUpdate = serde_json::from_value(json!({
        "mode":"fixed", "expires_at_ms":1_900_000_000_000_i64,
    }))
    .unwrap();
    assert!(matches!(
        fixed,
        BudgetWindowUpdate::Fixed {
            clear_used: false,
            ..
        }
    ));
    for invalid in [
        json!({"client_key_id":"key_1","period":"all"}),
        json!({"client_key_id":"key_1","period":"weekly","owner":"forged"}),
    ] {
        assert!(serde_json::from_value::<BudgetWindowQuery>(invalid).is_err());
    }
    let valid = json!({"client_key_id":"key_1","period":"weekly","expected_revision":0,"update":{"mode":"automatic"}});
    for (field, value) in [
        ("expected_revision", json!(-1)),
        ("period", json!("all")),
        ("instance_id", json!("forged")),
        ("update", json!({"mode":"automatic","clear_used":true})),
        (
            "update",
            json!({"mode":"fixed","expires_at_ms":1000,"accounting_start_at_ms":0}),
        ),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = value;
        assert!(serde_json::from_value::<ChangeBudgetWindowRequest>(invalid).is_err());
    }
    let mut missing = valid;
    missing.as_object_mut().unwrap().remove("expected_revision");
    assert!(serde_json::from_value::<ChangeBudgetWindowRequest>(missing).is_err());
}
