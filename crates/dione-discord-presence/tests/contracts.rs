#[test]
fn requested_state_and_authority_representations_remain_private() {
    let cases = trybuild::TestCases::new();
    cases.pass("tests/ui-pass/public_request.rs");
    cases.compile_fail("tests/ui/*.rs");
}
