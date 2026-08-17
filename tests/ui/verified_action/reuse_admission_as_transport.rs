#[path = "../../../src/discord/verified_action.rs"]
mod verified_action;

use verified_action::{AdmissionFacts, PrincipalResolver};

fn facts() -> AdmissionFacts {
    loop {}
}

fn resolver() -> PrincipalResolver {
    loop {}
}

fn main() {
    let _ = resolver().begin(facts());
}
