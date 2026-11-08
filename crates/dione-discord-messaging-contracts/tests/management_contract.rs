use dione_discord_messaging_contracts::{
    TargetError,
    management::{CreatedThread, ManagementError, ManagementErrorKind, wire},
};
use serde_json::json;
use serenity::model::id::ChannelId;
use std::error::Error as _;

#[test]
fn ordinary_success_retains_returned_thread_metadata() {
    assert_eq!(wire::mutation(Ok(())), json!({"ok":true}));
    assert_eq!(
        wire::thread(Ok(CreatedThread {
            id: ChannelId::new(200),
            name: "Discord returned name".into()
        })),
        json!({"ok":true,"thread_id":"200","name":"Discord returned name"})
    );
}

#[test]
fn ordinary_denial_keeps_requested_channel_display_and_source_category() {
    let error = ManagementError {
        channel: ChannelId::new(42),
        kind: ManagementErrorKind::OutboundDenied,
    };
    assert_eq!(error.channel, ChannelId::new(42));
    assert_eq!(
        error.to_string(),
        "management operation failed in channel 42"
    );
    assert!(
        error
            .source()
            .unwrap()
            .downcast_ref::<ManagementErrorKind>()
            .is_some()
    );
    assert_eq!(
        error.source().unwrap().to_string(),
        "channel not in allowlist"
    );
    assert_eq!(
        wire::mutation(Err(error)),
        json!({"error":"channel not in allowlist"})
    );
}

#[test]
fn ordinary_target_failure_retains_reason_and_thread_error_projection() {
    let error = ManagementError {
        channel: ChannelId::new(42),
        kind: ManagementErrorKind::Target(TargetError::IngressRequired),
    };
    assert_eq!(
        error.source().unwrap().to_string(),
        "message target requires an ingress receipt or own send"
    );
    assert_eq!(
        wire::thread(Err(error)),
        json!({"error":"message target requires an ingress receipt or own send","reason":"ingress_required"})
    );
}
