#[path = "../../../src/discord/verified_action.rs"]
mod verified_action;

use verified_action::VerifiedTransport;

fn transport() -> VerifiedTransport {
    loop {}
}

fn main() {
    let _ = transport().clone();
}
