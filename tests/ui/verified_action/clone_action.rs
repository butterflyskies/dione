#[path = "../../../src/discord/verified_action.rs"]
mod verified_action;

use verified_action::{Unresolved, VerifiedAppAction};

fn action() -> VerifiedAppAction<Unresolved> {
    loop {}
}

fn main() {
    let _ = action().clone();
}
