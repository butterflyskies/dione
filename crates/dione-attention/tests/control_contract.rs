use dione_attention::{
    config::AttentionConfig,
    control::{AttentionCommand, schema},
};
use serde_json::{Value, json};

fn commands() -> Vec<(Value, bool)> {
    let policy = json!({"wanted":0.5,"prompt":0.5,"participation":0.5,"change":0.5});
    vec![
        (json!({"operation":"status"}), false),
        (
            json!({"operation":"configure","settings":AttentionConfig::default()}),
            true,
        ),
        (
            json!({"operation":"retrieve","record_id":"fixture-record-42"}),
            false,
        ),
        (json!({"operation":"review","requested":2,"seed":7}), true),
        (
            json!({"operation":"feedback","record_id":"fixture-record-42","label":"wanted_later"}),
            true,
        ),
        (
            json!({"operation":"partitions","spec":{"training_end_ms":10,"validation_end_ms":20}}),
            false,
        ),
        (
            json!({"operation":"replay","record_ids":["fixture-record-42"],"policy":policy}),
            false,
        ),
        (
            json!({"operation":"fit","record_ids":["fixture-record-42"],"options":{
                "minimum_labels":2,"minimum_wanted":1,"minimum_not_needed":1,"minimum_promptly":1,
                "minimum_later":1,"regularization":0.1,"iterations":2,"learning_rate":0.1,"seed":7,
                "environment":"fixture-orbit-42","wanted_threshold":0.5,"timely_threshold":0.5,"candidate_ttl_ms":1000
            }}),
            true,
        ),
        (
            json!({"operation":"open_evaluation","plan":{
                "id":"fixture-evaluation-42","record_ids":["fixture-record-42"],"fixed_baseline":policy,
                "limits":{"minimum_labeled":2,"minimum_wanted":1,"minimum_promptly":1,
                    "minimum_wanted_recall":0.5,"minimum_timely_recall":0.5,"minimum_volume_reduction":0.1,
                    "maximum_unwanted_delivery_rate":0.5,"maximum_standard_error":0.5},"opened_at_ms":10
            }}),
            true,
        ),
        (
            json!({"operation":"evaluate","evaluation_id":"fixture-evaluation-42","digest":"fixture-digest-42"}),
            true,
        ),
        (
            json!({"operation":"promote","evaluation_id":"fixture-evaluation-42","digest":"fixture-digest-42"}),
            true,
        ),
        (
            json!({"operation":"rollback","digest":"fixture-digest-42"}),
            true,
        ),
    ]
}

#[test]
fn all_twelve_commands_decode_and_classify_without_granting_authority() {
    let values = commands();
    assert_eq!(values.len(), 12);
    for (input, mutating) in values {
        let command: AttentionCommand = serde_json::from_value(input.clone()).unwrap();
        assert_eq!(command.is_mutating(), mutating, "{input}");
    }
}

#[test]
fn payload_commands_reject_forged_annotator_and_status_keeps_unit_shape() {
    for (mut input, _) in commands() {
        input["annotator"] = json!("fixture-other-recipient-42");
        if input["operation"] == "status" {
            let command: AttentionCommand = serde_json::from_value(input).unwrap();
            assert!(matches!(command, AttentionCommand::Status));
            assert!(!command.is_mutating());
            continue;
        }
        let error = serde_json::from_value::<AttentionCommand>(input).unwrap_err();
        assert!(
            error.to_string().starts_with("unknown field `annotator`"),
            "{error}"
        );
    }
    let allowed: AttentionCommand = serde_json::from_value(json!({"operation":"status"})).unwrap();
    assert!(!allowed.is_mutating());
}

#[test]
fn required_payloads_keep_their_literal_error_contract() {
    let error = serde_json::from_value::<AttentionCommand>(
        json!({"operation":"feedback","record_id":"fixture-record-42"}),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "missing field `label`");
    let command: AttentionCommand = serde_json::from_value(
        json!({"operation":"feedback","record_id":"fixture-record-42","label":"wanted_later"}),
    )
    .unwrap();
    match command {
        AttentionCommand::Feedback { record_id, label } => {
            assert_eq!(record_id.as_str(), "fixture-record-42");
            assert_eq!(serde_json::to_value(label).unwrap(), json!("wanted_later"));
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[test]
fn discovery_schema_matches_the_preserved_literal_contract() {
    let expected: Value = serde_json::from_str(include_str!("control-schema.json")).unwrap();
    assert_eq!(schema(), expected);
}
