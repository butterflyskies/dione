#[path = "../../../src/discord/verified_action.rs"]
mod verified_action;

use verified_action::{Resolved, Unresolved, VerifiedAppAction};

fn main() {
    let _: VerifiedAppAction<Unresolved> = VerifiedAppAction {
        transport: loop {},
        binding: loop {},
        state: loop {},
    };
    let _: VerifiedAppAction<Resolved> = VerifiedAppAction {
        transport: loop {},
        binding: loop {},
        state: loop {},
    };
}
