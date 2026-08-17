#[path = "../../../src/discord/verified_action.rs"]
mod verified_action;

use verified_action::{
    FreshPolicySnapshot, Resolved, Unresolved, VerifiedActionGate, VerifiedAppAction,
};

fn unresolved() -> VerifiedAppAction<Unresolved> {
    loop {}
}

fn resolved() -> VerifiedAppAction<Resolved> {
    loop {}
}

fn policy() -> FreshPolicySnapshot {
    loop {}
}

fn main() {
    VerifiedActionGate::evaluate(unresolved(), policy());
    VerifiedActionGate::evaluate(resolved().binding(), policy());
}
