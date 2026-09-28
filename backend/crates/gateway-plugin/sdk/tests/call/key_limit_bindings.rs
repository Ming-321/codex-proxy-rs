use gateway_plugin_sdk::call::key_limit_bindings::{ChangeKeyLimitBindingRequest, KeyLimitBinding};
use serde_json::json;

#[test]
fn unbinding_is_explicit_and_callers_cannot_supply_an_identity() {
    let request = json!({"client_key_id":"a","source_key_id":null,"expected_revision":1});
    assert_eq!(
        serde_json::from_value::<ChangeKeyLimitBindingRequest>(request.clone())
            .unwrap()
            .source_key_id,
        None
    );
    let mut missing = request.clone();
    missing.as_object_mut().unwrap().remove("source_key_id");
    assert!(serde_json::from_value::<ChangeKeyLimitBindingRequest>(missing).is_err());
    let mut forged = request;
    forged["instance_id"] = json!("another-plugin");
    assert!(serde_json::from_value::<ChangeKeyLimitBindingRequest>(forged).is_err());
}

#[test]
fn binding_response_preserves_unknown_loading_state_and_accepts_future_fields() {
    let reply = json!({"client_key_id":"a","source_key_id":"a","revision":0,"config_revision":1,
        "binding_config_revision":null,"loaded_config_revision":null,"source_enabled":true,"future_field":{}});
    let binding: KeyLimitBinding = serde_json::from_value(reply).unwrap();
    assert_eq!(binding.loaded_config_revision, None);
    assert_eq!(binding.binding_config_revision, None);
    assert_eq!(binding.revision, 0);
}
