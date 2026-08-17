#[test]
fn verified_action_misuse_is_rejected() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/ui/verified_action/*.rs");
}
