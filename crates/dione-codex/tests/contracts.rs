#[test]
fn private_resident_codec_and_lifecycle_state_cannot_be_forged() {
    let cases = trybuild::TestCases::new();
    cases.pass("tests/ui-pass/public_session.rs");
    cases.compile_fail("tests/ui/*.rs");
}
