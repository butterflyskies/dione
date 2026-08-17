#[path = "../../../src/discord/verified_action.rs"]
mod verified_action;

use verified_action::{
    PrincipalResolver, TransportProvider, Unresolved, VerifiedAppAction,
};

fn resolver() -> PrincipalResolver {
    loop {}
}

fn action() -> VerifiedAppAction<Unresolved> {
    loop {}
}

fn main() {
    let mut session = resolver().begin(action());
    session.provider = TransportProvider::PluralKit;
}
