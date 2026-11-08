use dione_attention::{
    config::{AttentionBrief, AttentionMode, RoomAttention},
    types::{Admission, SourceAuthorKind, SourceKey},
};
use dione_attention_admission::{AdmissionController, JudgmentWork, Submission};
use dione_config::{Config, LoadedConfig};
use dione_discord_events::{MessageEvent, MessageTargeting, NotificationEvent};
use dione_messages::timestamp::Timestamp;
use serenity::model::id::{ChannelId, MessageId, UserId};
fn config(mode: AttentionMode) -> LoadedConfig {
    let mut raw = Config::default();
    raw.attention.mode = mode;
    raw.attention.brief = Some(AttentionBrief {
        text: "Synthetic fixture-orbit-31 context".into(),
        expires_at_ms: 1_000_000,
        provider_eligible: true,
    });
    raw.attention.rooms.insert(
        ChannelId::new(42),
        RoomAttention {
            provider_eligible: true,
            ..Default::default()
        },
    );
    LoadedConfig::try_from_raw(raw).unwrap()
}
fn event(id: u64) -> NotificationEvent {
    NotificationEvent::Message(MessageEvent {
        chat_id: ChannelId::new(42),
        message_id: MessageId::new(id),
        user: "fixture-orbit-31".into(),
        user_id: UserId::new(7),
        author_kind: SourceAuthorKind::DirectHuman,
        content: format!("Synthetic event {id}"),
        targeting: MessageTargeting::Ambient,
        timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
        attachments: vec![],
        is_voice_message: false,
        thread_parent_id: None,
        reply_to_message_id: None,
        reply_to_user_id: None,
        reply_to_user: None,
        reply_to_content_preview: None,
        bells: None,
        bells_status: None,
    })
}
fn submit(
    controller: &mut AdmissionController,
    config: &LoadedConfig,
    id: u64,
) -> Box<JudgmentWork> {
    match controller.submit(event(id), config, 1_000, true) {
        Submission::Judge {
            work,
            ordinary: None,
        } => work,
        _ => panic!("expected owned enforcing judgment"),
    }
}
#[test]
fn ordinary_consumer_preserves_duplicate_ownership_and_completion_fifo() {
    let config = config(AttentionMode::On);
    let mut controller = AdmissionController::default();
    let first = submit(&mut controller, &config, 101);
    let second = submit(&mut controller, &config, 102);
    assert!(matches!(
        controller.submit(event(101), &config, 1_000, true),
        Submission::Duplicate
    ));
    assert_eq!(controller.in_flight(), 2);
    assert!(
        controller
            .complete(second.record.clone(), &config)
            .is_empty()
    );
    let ready = controller.complete(first.record.clone(), &config);
    assert_eq!(
        ready
            .iter()
            .map(|r| r.event.message_id.get())
            .collect::<Vec<_>>(),
        vec![101, 102]
    );
    assert_eq!(controller.in_flight(), 0);
    controller.defer(ready);
    assert!(controller.owns_source(&SourceKey {
        channel_id: ChannelId::new(42),
        message_id: MessageId::new(101)
    }));
    assert_eq!(
        controller
            .retry_ready()
            .iter()
            .map(|r| r.event.message_id.get())
            .collect::<Vec<_>>(),
        vec![101, 102]
    );
}
#[test]
fn ordinary_consumer_stale_incarnation_invalidation_and_drop_do_not_release_old_work() {
    let config = config(AttentionMode::On);
    let mut controller = AdmissionController::default();
    let first = submit(&mut controller, &config, 201);
    let second = submit(&mut controller, &config, 202);
    let mut stale = first.record.clone();
    stale.incarnation = AdmissionController::default().incarnation().clone();
    assert!(controller.complete(stale, &config).is_empty());
    assert!(controller.owns_work(&first.record.id));
    assert!(
        controller
            .complete(second.record.clone(), &config)
            .is_empty()
    );
    let ready = controller.invalidate(SourceKey {
        channel_id: ChannelId::new(42),
        message_id: MessageId::new(201),
    });
    assert_eq!(
        ready
            .iter()
            .map(|r| r.event.message_id.get())
            .collect::<Vec<_>>(),
        vec![202]
    );
    assert!(first.cancel.is_cancelled());
    assert!(
        controller
            .complete(first.record.clone(), &config)
            .is_empty()
    );
    let third = submit(&mut controller, &config, 203);
    drop(controller);
    assert!(third.cancel.is_cancelled());
}
#[test]
fn ordinary_consumer_off_and_unsupported_enforcement_preserve_ordinary_delivery() {
    let mut controller = AdmissionController::default();
    let off = config(AttentionMode::Off);
    assert!(matches!(
        controller.submit(event(301), &off, 1_000, true),
        Submission::Ordinary(_)
    ));
    assert_eq!(controller.in_flight(), 0);
    let on = config(AttentionMode::On);
    match controller.submit(event(302), &on, 1_000, false) {
        Submission::Judge {
            work,
            ordinary: Some(_),
        } => {
            assert_eq!(work.mode, AttentionMode::Log);
            assert_eq!(work.record.actual, Admission::Ordinary);
            controller.cancel_work();
            assert!(work.cancel.is_cancelled());
        }
        _ => panic!("unsupported enforcement must preserve ordinary event"),
    }
}
