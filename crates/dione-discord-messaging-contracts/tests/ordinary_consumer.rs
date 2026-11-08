use dione_discord_messaging_contracts::{
    ChannelRef, DeliveryReceipt, DmDeliveryRoute, MessagingErrorKind, PartialReceipt, ReplyOutcome,
    Report, TargetError, TargetOperation, wire,
};
use dione_outbound_consent::delivery::{DeliveryCause, PostedMessages};
use serde_json::json;
use serenity::model::id::{ChannelId, MessageId};

#[test]
fn ordinary_metadata_preserves_unknown_prefix_complete_dm_and_mismatch_contracts() {
    let unknown = wire::error(
        MessagingErrorKind::DeliveryUnknown {
            cause: DeliveryCause::ChunkSend {
                index: 3,
                source: Box::new(serenity::Error::Other("fixture interrupted")),
            },
            confirmed: vec![MessageId::new(103), MessageId::new(101)],
        }
        .into(),
    );
    assert_eq!(
        unknown,
        json!({"error":"delivery completion unknown: failed to send chunk 3: fixture interrupted; reconcile before resending","delivery_ambiguous":true,"retryable":false,"message_ids":[103,101]})
    );
    let receipt = DeliveryReceipt::new(vec![MessageId::new(101)], vec![]);
    assert_eq!(receipt.message_ids(), [MessageId::new(101)]);
    assert!(receipt.locators().is_empty());
    let dm = wire::dm(Ok(Report {
        outcome: ReplyOutcome::Delivered(receipt),
        auto_rewrites: vec![],
    }
    .dm(DmDeliveryRoute::Direct(ChannelId::new(42)))));
    assert_eq!(dm, json!({"ok":true,"message_ids":[101],"channel_id":"42"}));
    let partial = wire::reply(Ok(Report {
        outcome: ReplyOutcome::PartiallyDelivered(PartialReceipt {
            sent: PostedMessages::new(MessageId::new(103), vec![MessageId::new(101)]),
            cause: DeliveryCause::ChunkSend {
                index: 3,
                source: Box::new(serenity::Error::Other("fixture interrupted")),
            },
        }),
        auto_rewrites: vec![],
    }));
    assert_eq!(partial["message_ids"], json!([103, 101]));
    assert_eq!(partial["partially_delivered"], true);
    assert_eq!(ChannelRef::new(0).get(), 0);
    assert_eq!(ChannelRef::new(42).to_string(), "42");
    assert_eq!(
        wire::target_error(TargetError::ChannelMismatch {
            message: MessageId::new(7),
            admitted: ChannelRef::new(42),
            claimed: ChannelRef::new(43),
            operation: TargetOperation::EditMessage,
        }),
        json!({"error":"message 7 was admitted in channel 42, not claimed channel 43; edit_message blocked","reason":"channel_mismatch","message_id":"7","admitted_channel_id":"42","claimed_channel_id":"43","operation":"edit_message","blocked":true})
    );
}
