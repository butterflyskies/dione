#[path = "../../../src/discord/verified_action.rs"]
mod verified_action;

use serenity::model::id::MessageId;
use verified_action::EventBinding;

fn binding() -> EventBinding {
    loop {}
}

fn main() {
    let mut binding = binding();
    binding.message_id = MessageId::new(2);
}
