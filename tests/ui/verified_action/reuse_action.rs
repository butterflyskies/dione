#[path = "../../../src/discord/verified_action.rs"]
mod verified_action;

use verified_action::{FreshPolicySnapshot, Resolved, VerifiedActionGate, VerifiedAppAction};

fn action() -> VerifiedAppAction<Resolved> {
    loop {}
}

fn policy() -> FreshPolicySnapshot {
    loop {}
}

fn main() {
    let action = action();
    VerifiedActionGate::evaluate(action, policy());
    VerifiedActionGate::evaluate(action, policy());
}
