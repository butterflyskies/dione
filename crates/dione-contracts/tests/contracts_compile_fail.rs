//! Provider and slot identities remain distinct at the public boundary.

#[test]
fn module_identity_components_remain_distinct() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/ui/module_identity/swapped_components.rs");
    tests.pass("tests/ui/module_identity/valid_components.rs");
}
