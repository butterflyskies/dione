#[path = "../../../src/discord/verified_action.rs"]
mod verified_action;

use serenity::model::id::{ChannelId, MessageId, WebhookId};
use verified_action::{EventBinding, EventContext};

fn main() {
    let _ = EventBinding {
        message_id: MessageId::new(1),
        channel_id: ChannelId::new(2),
        context: EventContext::DirectMessage,
        webhook_id: WebhookId::new(3),
    };
}
