use dione_discord_presence::{
    ActivityType, OnlineStatus, PresenceAcknowledgement, PresenceActivity, PresenceRequest,
    PresenceSetError, PresenceUnavailable, SharedPresence,
    wire::{self, PresenceInputErrorKind},
};
use serde_json::{Value, json};
use std::error::Error;

#[test]
fn ordinary_decoder_preserves_lenient_defaults_and_empty_activity_text() {
    for input in [
        json!({}),
        Value::Null,
        json!({"online_status":17,"activity_type":false,"activity_name":[]}),
        json!({"activity_name":"ignored"}),
    ] {
        assert_eq!(
            wire::parse_request(&input).unwrap(),
            PresenceRequest::new(OnlineStatus::Online, None)
        );
    }
    assert_eq!(
        wire::parse_request(
            &json!({"online_status":"idle","activity_type":"custom","activity_name":""})
        )
        .unwrap(),
        PresenceRequest::new(
            OnlineStatus::Idle,
            Some(PresenceActivity::new(ActivityType::Custom, String::new()))
        )
    );
}

#[test]
fn status_error_precedes_activity_and_typed_categories_preserve_display_and_sources() {
    let status =
        wire::parse_request(&json!({"online_status":"bad","activity_type":"bad"})).unwrap_err();
    assert!(matches!(status.kind, PresenceInputErrorKind::Status(_)));
    assert_eq!(
        status.to_string(),
        "invalid online_status: bad; must be one of: online, idle, dnd, invisible"
    );
    assert!(status.source().is_none());
    let activity = wire::parse_request(&json!({"activity_type":"bad"})).unwrap_err();
    assert!(matches!(activity.kind, PresenceInputErrorKind::Activity(_)));
    assert_eq!(
        activity.to_string(),
        "invalid activity_type: bad; must be one of: playing, watching, listening, competing, custom"
    );
    assert!(activity.source().is_none());
    for name in [Value::Null, json!(7), json!([])] {
        let error = wire::parse_request(&json!({"activity_type":"playing","activity_name":name}))
            .unwrap_err();
        assert!(matches!(
            error.kind,
            PresenceInputErrorKind::MissingActivityName
        ));
        assert_eq!(
            error.to_string(),
            "activity_name is required when activity_type is set"
        );
        assert!(error.source().is_none());
    }
    assert_eq!(
        wire::parse_request(&json!({"online_status":"idle"}))
            .unwrap()
            .status(),
        OnlineStatus::Idle
    );
}

#[test]
fn applied_and_accepted_acknowledge_distinct_local_progress() {
    assert_eq!(
        wire::set(Ok(PresenceAcknowledgement::Applied)),
        json!({"ok":true,"status":"applied"})
    );
    assert_eq!(
        wire::set(Ok(PresenceAcknowledgement::Accepted)),
        json!({"ok":true,"status":"accepted"})
    );
    assert_eq!(
        wire::set(Err(PresenceSetError::Unavailable)),
        json!({"error":"presence updates require the discord gateway command channel"})
    );
    assert_eq!(
        wire::get(Err(PresenceUnavailable)),
        json!({"error":"presence state is not wired on this transport"})
    );
}

#[tokio::test]
async fn requested_snapshot_retains_timestamp_null_fields_and_honest_readback_notes() {
    let state = SharedPresence::new();
    assert_eq!(
        wire::get(Ok(state.snapshot().await)),
        json!({
            "set_this_process":false,"sink_installed":false,
            "note":"no presence has been requested since this process started; Discord is showing the gateway default. sink_installed reports whether a shard messenger has been installed since the last gateway ready — disconnects are not tracked"
        })
    );
    state
        .set_presence(PresenceRequest::new(OnlineStatus::Dnd, None))
        .await;
    let snapshot = state.snapshot().await;
    let stamp = snapshot.desired().unwrap().set_at().to_rfc3339();
    assert_eq!(
        wire::get(Ok(snapshot)),
        json!({
            "set_this_process":true,"online_status":"dnd","activity_type":null,"activity_name":null,
            "set_at":stamp,"sink_installed":false,
            "source":"last requested presence (replayed on reconnect), not a Discord read-back; sink_installed is not a live connection probe"
        })
    );
}
