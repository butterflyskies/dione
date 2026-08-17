#[path = "../../../src/discord/verified_action.rs"]
mod verified_action;

use serenity::model::id::UserId;
use verified_action::RepresentedPrincipal;

fn main() {
    let _ = RepresentedPrincipal {
        discord_user_id: UserId::new(1),
        system_id: None,
        member_id: None,
    };
}
