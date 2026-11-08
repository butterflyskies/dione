use super::*;
use proptest::prelude::*;
proptest! {
        /// The parsed limit always lands in Discord's accepted 1..=100 window
        /// (a limit of 0 would make `has_more: count == limit` hold vacuously
        /// on an empty page), and an absent limit yields the default.
        #[test]
        fn parse_limit_always_in_range(
            // Edge-weighted for the same reason as id_strategy: uniform u64
            // almost never lands on 0 (clamp-to-1 branch) or in 1..=100
            // (exact passthrough branch), leaving both untested.
            limit in proptest::option::of(prop_oneof![
                1 => Just(0u64),
                5 => 1u64..=100,
                4 => 101u64..,
            ]),
            default in 1u8..=100,
        ) {
            let args = match limit {
                Some(l) => json!({ "limit": l }),
                None => json!({}),
            };
            let parsed = parse_limit(&args, default);
            prop_assert!((1..=100).contains(&parsed));
            match limit {
                None => prop_assert_eq!(parsed, default),
                Some(l) => prop_assert_eq!(u64::from(parsed), l.clamp(1, 100)),
            }
        }
}
#[test]
fn strict_optional_id_absent_yields_none() {
    assert_eq!(
        parse_strict_optional_id(&json!({}), "id")
            .map(|o| o.map(|s| s.message().get()))
            .map_err(|error| error.to_string()),
        Ok(None)
    );
}

#[test]
fn strict_optional_id_valid_snowflake() {
    let result = parse_strict_optional_id(&json!({"id": "1234567890"}), "id");
    assert_eq!(
        result
            .map(|o| o.map(|s| s.message().get()))
            .map_err(|error| error.to_string()),
        Ok(Some(1234567890))
    );

    let result = parse_strict_optional_id(&json!({"id": 1234567890u64}), "id");
    assert_eq!(
        result
            .map(|o| o.map(|s| s.message().get()))
            .map_err(|error| error.to_string()),
        Ok(Some(1234567890))
    );
}

#[test]
fn strict_optional_id_rejects_null() {
    assert!(parse_strict_optional_id(&json!({"id": null}), "id").is_err());
}

#[test]
fn strict_optional_id_rejects_bool() {
    assert!(parse_strict_optional_id(&json!({"id": true}), "id").is_err());
}

#[test]
fn strict_optional_id_rejects_object() {
    assert!(parse_strict_optional_id(&json!({"id": {}}), "id").is_err());
}

#[test]
fn strict_optional_id_rejects_array() {
    assert!(parse_strict_optional_id(&json!({"id": []}), "id").is_err());
}

#[test]
fn strict_optional_id_rejects_empty_string() {
    assert!(parse_strict_optional_id(&json!({"id": ""}), "id").is_err());
}

#[test]
fn strict_optional_id_rejects_zero() {
    assert!(parse_strict_optional_id(&json!({"id": 0}), "id").is_err());
    assert!(parse_strict_optional_id(&json!({"id": "0"}), "id").is_err());
}

#[test]
fn strict_optional_id_rejects_non_numeric_string() {
    assert!(parse_strict_optional_id(&json!({"id": "abc"}), "id").is_err());
}

#[test]
fn wrong_type_optional_limit_keeps_legacy_default() {
    for limit in [
        Value::Null,
        json!(true),
        json!("9"),
        json!(-1),
        json!(1.5),
        json!([]),
        json!({}),
    ] {
        assert_eq!(parse_limit(&json!({"limit":limit}), 20), 20);
    }
}

#[test]
fn malformed_search_retains_field_path_and_concrete_cause() {
    let error = search_wire::parse_request(
        serenity::model::id::GuildId::new(7),
        &json!({"limit":"bad"}),
    )
    .unwrap_err();
    match error.kind {
        SearchInputErrorKind::Deserialize(source) => {
            assert_eq!(source.path().to_string(), "limit");
            assert!(source.inner().is_data());
        }
        SearchInputErrorKind::Preparation(error) => {
            panic!("unexpected preparation failure: {error}")
        }
    }
    let cursor = parse_strict_optional_id(&json!({"before":"bad"}), "before").unwrap_err();
    assert_eq!(cursor.to_string(), "invalid before: not a valid u64");
    assert!(matches!(cursor.kind, CursorErrorKind::Decimal(_)));
}
