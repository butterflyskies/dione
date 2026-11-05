use dione_pluralkit_identities::{PLURALKIT_APPLICATION_ID, PkUuid, PkUuidErrorKind};

#[test]
fn canonical_values_and_official_deployment_are_literal() {
    assert_eq!(PLURALKIT_APPLICATION_ID, 466_378_653_216_014_359);
    for (input, expected) in [
        (
            "00000000-0000-0000-0000-000000000000",
            "00000000-0000-0000-0000-000000000000",
        ),
        (
            "FFFFFFFF-FFFF-FFFF-FFFF-FFFFFFFFFFFF",
            "ffffffff-ffff-ffff-ffff-ffffffffffff",
        ),
        (
            "12345678-ABCD-1234-1234-123456789ABC",
            "12345678-abcd-1234-1234-123456789abc",
        ),
    ] {
        let id = PkUuid::parse(input).unwrap();
        assert_eq!(id.as_str(), expected);
        assert_eq!(id.to_string(), expected);
        assert_eq!(id.clone(), id);
    }
}

#[test]
fn empty_and_malformed_inputs_have_distinct_typed_errors() {
    let empty = PkUuid::parse("").unwrap_err();
    assert_eq!(empty.kind, PkUuidErrorKind::Empty);
    assert_eq!(empty.to_string(), "pk uuid must be non-empty");
    for input in [
        " ",
        "abcde",
        "12345678123412341234123456789abc",
        "12345678_1234-1234-1234-123456789abc",
        "12345678-1234_1234-1234-123456789abc",
        "12345678-1234-1234_1234-123456789abc",
        "12345678-1234-1234-1234_123456789abc",
        "12345678-1234-1234-1234-123456789abg",
        "12345678-1234-1234-1234-123456789aé",
        "12345678-1234-1234-1234-123456789abc ",
        " 12345678-1234-1234-1234-123456789abc",
    ] {
        let error = PkUuid::parse(input).unwrap_err();
        assert_eq!(error.kind, PkUuidErrorKind::InvalidFormat);
        assert_eq!(
            error.to_string(),
            "pk identifier must be a full uuid (8-4-4-4-12 hex); short ids are not accepted; use the full uuid from the pk api"
        );
        match error.kind {
            PkUuidErrorKind::Empty => panic!("nonempty malformed input classified as empty"),
            PkUuidErrorKind::InvalidFormat => {}
        }
    }
}
