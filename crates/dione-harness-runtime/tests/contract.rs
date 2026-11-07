use dione_harness::AttentionPolicy;
use dione_harness_runtime::{
    HarnessSelection, HarnessSession, HarnessSessionErrorKind, StdioExitPolicy,
};

#[test]
fn external_selection_and_nonresident_capability_are_explicit() {
    assert_eq!(
        HarnessSelection::OhMyPi.attention_policy(),
        AttentionPolicy::Direct
    );
    assert_eq!(
        HarnessSelection::Codex.stdio_exit_policy(true),
        StdioExitPolicy::DetachUntilCancellation
    );
    assert_eq!(
        HarnessSelection::Codex.stdio_exit_policy(false),
        StdioExitPolicy::ShutdownProcess
    );
    for selected in [HarnessSelection::ClaudeCode, HarnessSelection::OhMyPi] {
        for resident in [false, true] {
            assert_eq!(
                selected.stdio_exit_policy(resident),
                StdioExitPolicy::ShutdownProcess
            );
        }
    }
    let session = HarnessSession::oh_my_pi();
    assert_eq!(
        session.connection().attention_policy(),
        AttentionPolicy::Direct
    );
    let error = session.resident_admission().err().unwrap();
    assert!(matches!(
        error.kind,
        HarnessSessionErrorKind::TeamsUnavailable
    ));
    assert_eq!(
        error.to_string(),
        "resident Teams ingress requires a prepared Codex or OMP session"
    );
}
