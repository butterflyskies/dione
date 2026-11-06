use dione_bell_rings::{BellEvaluator, BellStatus};
use dione_config::{BellMode, BellRingsConfig, Config, LoadedConfig};
use dione_discord_bells::{evaluate_native, prepare_bells};
use dione_discord_events::{MessageEvent, MessageTargeting, NotificationEvent};
use dione_messages::timestamp::Timestamp;
use serenity::model::id::{ChannelId, MessageId, UserId};
use std::sync::Arc;

#[tokio::test]
async fn empty_provider_and_nonmessage_paths_preserve_observed_metadata_in_both_modes() {
    let message = MessageEvent {
        chat_id: ChannelId::new(1),
        message_id: MessageId::new(2),
        user: "fixture".into(),
        user_id: UserId::new(3),
        author_kind: dione_attention::types::SourceAuthorKind::DirectHuman,
        content: "original content".into(),
        targeting: MessageTargeting::DirectMessage,
        timestamp: Timestamp::parse("2026-11-06T00:00:00Z").unwrap(),
        attachments: vec![],
        is_voice_message: false,
        thread_parent_id: None,
        reply_to_message_id: None,
        reply_to_user_id: None,
        reply_to_user: None,
        reply_to_content_preview: None,
        bells: Some("retained".into()),
        bells_status: Some(BellStatus::PartialError),
    };
    let settings = BellRingsConfig {
        enabled: true,
        providers: vec![],
        ..BellRingsConfig::default()
    };
    let evaluator = Arc::new(BellEvaluator::new());
    let (event, bells, status) =
        evaluate_native(NotificationEvent::Message(message), &settings, &evaluator).await;
    assert!(bells.is_empty());
    assert_eq!(status, None);
    let NotificationEvent::Message(message) = event else {
        panic!("message retained")
    };
    assert_eq!(message.content, "original content");
    assert_eq!(message.bells.as_deref(), Some("retained"));
    assert_eq!(message.bells_status, Some(BellStatus::PartialError));
    for mode in [BellMode::Live, BellMode::Shadow] {
        let raw = Config {
            bell_rings: BellRingsConfig {
                mode,
                ..BellRingsConfig::default()
            },
            ..Config::default()
        };
        let loaded = LoadedConfig::try_from_raw(raw).unwrap();
        let result = prepare_bells(
            NotificationEvent::Message(message.clone()),
            &loaded,
            &evaluator,
        )
        .await;
        let NotificationEvent::Message(result) = result else {
            panic!("message retained")
        };
        assert_eq!(result.bells.as_deref(), Some("retained"));
        assert_eq!(result.bells_status, Some(BellStatus::PartialError));
        let result = prepare_bells(
            NotificationEvent::ConfigError {
                error: "original error".into(),
            },
            &loaded,
            &evaluator,
        )
        .await;
        assert!(matches!(result, NotificationEvent::ConfigError{error}if error=="original error"));
    }
}
