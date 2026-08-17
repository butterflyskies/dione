#[path = "../../../src/discord/verified_action.rs"]
mod verified_action;

use verified_action::EventBinding;

fn main() {
    let _ = EventBinding {
        message_id: 1,
        channel_id: 2,
        context: loop {},
        webhook_id: 3,
    };
}
