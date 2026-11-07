#[test]
fn private_state_is_not_an_external_control_surface() {
    let cases = trybuild::TestCases::new();
    cases.pass("tests/ui-pass/public_session.rs");
    cases.compile_fail("tests/ui/*.rs");
}
