//! Compile-time contracts share one trybuild workspace and one runner.

#[test]
fn contract_misuse_is_rejected() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/ui/verified_action/*.rs");
    tests.compile_fail("tests/ui/module_identity/swapped_components.rs");
    tests.pass("tests/ui/module_identity/valid_components.rs");
}
