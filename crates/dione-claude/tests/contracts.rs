#[test]
fn projection_and_adapter_state_remain_private() {
    let cases = trybuild::TestCases::new();
    cases.pass("tests/ui-pass/public_connect.rs");
    cases.compile_fail("tests/ui/*.rs");
}
