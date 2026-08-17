#[path = "../../../src/discord/verified_action.rs"]
mod verified_action;

use serenity::model::id::MessageId;
use verified_action::{PrincipalResolver, Unresolved, VerifiedAppAction};

fn resolver() -> PrincipalResolver {
    loop {}
}

fn action() -> VerifiedAppAction<Unresolved> {
    loop {}
}

fn main() {
    let _ = resolver().begin(action(), MessageId::new(9));
}
