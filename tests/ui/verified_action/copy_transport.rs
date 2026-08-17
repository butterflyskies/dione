#[path = "../../../src/discord/verified_action.rs"]
mod verified_action;

use verified_action::VerifiedTransport;

fn transport() -> VerifiedTransport {
    loop {}
}

fn consume(_: VerifiedTransport) {}

fn main() {
    let transport = transport();
    consume(transport);
    consume(transport);
}
