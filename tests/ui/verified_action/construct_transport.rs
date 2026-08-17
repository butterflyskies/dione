#[path = "../../../src/discord/verified_action.rs"]
mod verified_action;

use verified_action::{TransportProvider, VerifiedTransport};

fn main() {
    let _ = VerifiedTransport {
        provider: TransportProvider::PluralKit,
    };
}
