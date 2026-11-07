//! Application-owned request origins and reserved resident controls.
pub(crate) const PRIVATE_CONTROL_TOOLS: &[&str] = &[
    "teams_bind_session",
    "teams_check_session",
    "teams_recover_session",
    "teams_unbind_session",
    "teams_ack_event",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RequestOrigin {
    Stdio,
    ResidentControl,
}
