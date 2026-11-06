use dione_oneshot::{
    FailureClass, Outcome, Reason, Refusal, Stage, TokenSource, check_identity, retryable_for,
};

#[test]
fn standalone_identity_and_ambiguity_contract_preserves_literal_outcomes() {
    let source = TokenSource::parse("env:FIXTURE_TOKEN").unwrap();
    assert_eq!(source, TokenSource::Env("FIXTURE_TOKEN".into()));
    assert!(TokenSource::parse("env:").is_err());
    let identity = check_identity((7, "display-name".into()), 7, &source).unwrap();
    assert_eq!(identity.bot_user_id, 7);
    assert_eq!(
        check_identity((8, "display-name".into()), 7, &source)
            .unwrap_err()
            .reason,
        Reason::IdentityMismatch
    );
    for nonce in [false, true] {
        assert!(!retryable_for(
            Stage::Post,
            FailureClass::ServerError,
            nonce
        ));
        assert!(retryable_for(
            Stage::Identity,
            FailureClass::ServerError,
            nonce
        ));
    }
    let outcome = Outcome::Refused(Refusal::send_ambiguous("fixture failure"));
    assert_eq!(outcome.exit_code(), 3);
    assert_eq!(outcome.to_json()["reason"], "send_failed");
    assert_eq!(outcome.to_json()["delivery_ambiguous"], true);
    assert_eq!(outcome.to_json()["retryable"], false);
}
