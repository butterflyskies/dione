use dione_discord_tool_arguments::{
    ArgumentErrorKind, parse_enabled_sentex_handles, parse_hook_overrides, parse_id,
    parse_optional_bool, parse_optional_id, sentex_handles_schema,
};
use dione_evidence::{EvidenceMarkerVersion, append_markers};
use serde_json::{Value, json};
use std::error::Error;

#[test]
fn ordinary_ids_preserve_nonzero_errors_sources_and_optional_shapes() {
    for value in [json!(42), json!("42"), json!("00042")] {
        assert_eq!(
            parse_id(&json!({"id":value}), "id")
                .unwrap()
                .message()
                .get(),
            42
        );
    }
    for value in [json!(0), json!("0")] {
        let error = parse_id(&json!({"id":value}), "id").unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid id: must be a nonzero Discord snowflake"
        );
        assert!(matches!(error.kind(), ArgumentErrorKind::ZeroId("id")));
    }
    assert_eq!(
        parse_id(&json!({}), "id").unwrap_err().to_string(),
        "missing id"
    );
    assert_eq!(
        parse_id(&json!({"id":false}), "id")
            .unwrap_err()
            .to_string(),
        "invalid id: expected a numeric or string ID"
    );
    let error = parse_id(&json!({"id":"not-decimal"}), "id").unwrap_err();
    assert_eq!(error.to_string(), "invalid id: not a valid u64");
    let mut cause: Option<&(dyn Error + 'static)> = Some(&error);
    let mut found = false;
    while let Some(current) = cause {
        found |= current.downcast_ref::<std::num::ParseIntError>().is_some();
        cause = current.source();
    }
    assert!(
        found,
        "the original integer parse cause survives the public boundary"
    );
    for args in [
        json!({}),
        json!({"id":""}),
        json!({"id":null}),
        json!({"id":false}),
        json!({"id":[]}),
        json!({"id":{}}),
        json!({"id":1.5}),
    ] {
        assert_eq!(parse_optional_id(&args, "id").unwrap(), None);
    }
    assert_eq!(
        parse_optional_id(&json!({"id":"42"}), "id")
            .unwrap()
            .unwrap()
            .message()
            .get(),
        42
    );
    assert!(parse_optional_id(&json!({"id":"0"}), "id").is_err());
    assert!(parse_optional_id(&json!({"id":"bad"}), "id").is_err());
}

#[test]
fn ordinary_hook_and_boolean_contracts_preserve_order_and_literal_errors() {
    let hooks =
        parse_hook_overrides(&json!({"no_rly_hooks":["tier-1","contradictionary"]})).unwrap();
    assert_eq!(
        hooks.iter().map(|hook| hook.as_str()).collect::<Vec<_>>(),
        ["tier-1", "contradictionary"]
    );
    assert!(parse_hook_overrides(&json!({})).unwrap().is_empty());
    assert_eq!(
        parse_hook_overrides(&json!({"no_rly_hooks":true}))
            .unwrap_err()
            .to_string(),
        "no_rly_hooks must be an array of hook-name strings"
    );
    assert_eq!(
        parse_hook_overrides(&json!({"no_rly_hooks":[1]}))
            .unwrap_err()
            .to_string(),
        "no_rly_hooks must contain only strings"
    );
    assert!(matches!(
        parse_hook_overrides(&json!({"no_rly_hooks":["Not Valid"]}))
            .unwrap_err()
            .kind(),
        ArgumentErrorKind::Hook(_)
    ));
    assert_eq!(
        parse_optional_bool(&json!({}), "suppress_embeds").unwrap(),
        None
    );
    for value in [true, false] {
        assert_eq!(
            parse_optional_bool(&json!({"suppress_embeds":value}), "suppress_embeds").unwrap(),
            Some(value)
        );
    }
    for value in [Value::Null, json!("true"), json!(1), json!([]), json!({})] {
        assert_eq!(
            parse_optional_bool(&json!({"suppress_embeds":value}), "suppress_embeds")
                .unwrap_err()
                .to_string(),
            "suppress_embeds must be a boolean"
        );
    }
}

#[test]
fn ordinary_evidence_enable_and_version_controls_preserve_roles_and_parse_order() {
    let args = json!({"claim_handles":["12"],"citation_handles":["1"]});
    for (version, expected) in [
        (
            EvidenceMarkerVersion::V2,
            "body [🔍=v2:claim:AAAAAAAAAAw] [🔍=v2:citation:AAAAAAAAAAE]",
        ),
        (
            EvidenceMarkerVersion::V3,
            "body [🔍=v3:claim:DA] [🔍=v3:citation:AQ]",
        ),
    ] {
        let handles = parse_enabled_sentex_handles(&args, true, version).unwrap();
        assert_eq!(append_markers("body", &handles, version), expected);
        let error = parse_enabled_sentex_handles(&args, false, version).unwrap_err();
        assert_eq!(
            error.to_string(),
            "claim_handles and citation_handles require evidence_markers_enabled"
        );
        assert!(matches!(error.kind(), ArgumentErrorKind::EvidenceDisabled));
        assert!(
            parse_enabled_sentex_handles(&json!({}), false, version)
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            parse_enabled_sentex_handles(&json!({"claim_handles":[]}), false, version)
                .unwrap_err()
                .kind(),
            ArgumentErrorKind::EvidenceDisabled
        ));
        // Malformed fields are rejected by the existing evidence owner before the disabled-field check.
        assert!(matches!(
            parse_enabled_sentex_handles(&json!({"claim_handles":[1]}), false, version)
                .unwrap_err()
                .kind(),
            ArgumentErrorKind::Evidence(_)
        ));
        let schema = sentex_handles_schema("Claim handles", version);
        assert_eq!(schema["items"]["pattern"], "^[1-9][0-9]*$");
        assert_eq!(schema["items"]["maxLength"], 20);
    }
}
