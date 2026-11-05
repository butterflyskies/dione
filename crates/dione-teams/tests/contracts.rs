#[test]
fn native_authority_privacy_is_enforced_for_external_consumers() {
    let cases = trybuild::TestCases::new();
    cases.pass("tests/ui-pass/public_services.rs");
    cases.compile_fail("tests/ui/*.rs");
}
