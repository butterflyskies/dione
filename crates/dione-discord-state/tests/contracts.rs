#[test]
fn creator_cache_and_raw_fact_producers_remain_private() {
    let cases = trybuild::TestCases::new();
    cases.pass("tests/ui-pass/public_state.rs");
    cases.compile_fail("tests/ui/*.rs");
}
