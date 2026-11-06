use dione_contracts::{ModuleInstanceId, ModuleSlotId, ProviderId};
use dione_delivery_scheduler::BufferResult;
use dione_discord_events::NotificationEvent;
use dione_discord_scheduling::DeliveryBuffer;
use serenity::model::id::{ChannelId, MessageId, UserId};
use std::time::Duration;

fn reaction(id: u64) -> NotificationEvent {
    NotificationEvent::Reaction {
        chat_id: ChannelId::new(42),
        message_id: MessageId::new(id),
        user: "reactor".into(),
        user_id: UserId::new(7),
        emoji: "retained".into(),
        self_react: false,
    }
}

#[test]
fn external_delay_predecessor_and_requeue_preserve_payload_order() {
    let module = ModuleInstanceId::new(
        ProviderId::new("discord").unwrap(),
        ModuleSlotId::new("external").unwrap(),
    );
    let mut buffer = DeliveryBuffer::new(module.clone());
    assert_eq!(buffer.module(), &module);
    assert!(matches!(
        buffer.buffer_event(reaction(10), Duration::from_secs(5)),
        BufferResult::Buffered
    ));
    assert!(buffer.next_flush_deadline().is_some());
    let BufferResult::FlushThenImmediate {
        mut preceding,
        event,
    } = buffer.buffer_event(reaction(11), Duration::ZERO)
    else {
        panic!("same-channel predecessor retained before immediate event")
    };
    assert_eq!(preceding.len(), 1);
    preceding.push(*event);
    buffer.requeue(preceding);
    let payloads = buffer.flush_ready(tokio::time::Instant::now());
    let mut ids = Vec::new();
    for payload in payloads {
        let NotificationEvent::Reaction {
            chat_id,
            message_id,
            user,
            emoji,
            ..
        } = payload
        else {
            panic!("original reaction payload")
        };
        assert_eq!(chat_id, ChannelId::new(42));
        assert_eq!(user, "reactor");
        assert_eq!(emoji, "retained");
        ids.push(message_id.get());
    }
    assert_eq!(ids, vec![10, 11]);
    assert!(buffer.flush_all().is_empty());
    assert!(buffer.next_flush_deadline().is_none());
}
