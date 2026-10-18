//! MCP projection of typed Discord presence and typing results.

use crate::discord::{
    presence::{
        InvalidActivityType, InvalidOnlineStatus, PresenceAcknowledgement, PresenceActivity,
        PresenceRequest, PresenceSetError, PresenceSnapshot, PresenceUnavailable,
    },
    typing::TypingError,
};
use serde_json::{Value, json};
use thiserror::Error;

pub(super) fn set(result: Result<PresenceAcknowledgement, PresenceSetError>) -> Value {
    match result {
        Ok(acknowledgement) => {
            json!({"ok": true, "status": match acknowledgement { PresenceAcknowledgement::Applied => "applied", PresenceAcknowledgement::Accepted => "accepted" }})
        }
        Err(error) => json!({"error": error.to_string()}),
    }
}
pub(super) fn get(result: Result<PresenceSnapshot, PresenceUnavailable>) -> Value {
    let snapshot = match result {
        Ok(snapshot) => snapshot,
        Err(error) => return json!({"error": error.to_string()}),
    };
    match snapshot.desired() {
        None => json!({
            "set_this_process": false,
            "sink_installed": snapshot.sink_installed(),
            "note": "no presence has been requested since this process started; Discord is showing the gateway default. sink_installed reports whether a shard messenger has been installed since the last gateway ready — disconnects are not tracked"
        }),
        Some(desired) => json!({
            "set_this_process": true,
            "online_status": desired.request().status().to_string(),
            "activity_type": desired.request().activity().map(|activity| activity.kind().to_string()),
            "activity_name": desired.request().activity().map(|activity| activity.name()),
            "set_at": desired.set_at().to_rfc3339(),
            "sink_installed": snapshot.sink_installed(),
            "source": "last requested presence (replayed on reconnect), not a Discord read-back; sink_installed is not a live connection probe"
        }),
    }
}
pub(super) fn typing(result: Result<(), TypingError>) -> Value {
    match result {
        Ok(()) => json!({"ok": true}),
        Err(error) => json!({"error": error.to_string()}),
    }
}

#[derive(Debug, Error)]
pub(super) enum PresenceInputError {
    #[error(transparent)]
    Status(#[from] InvalidOnlineStatus),
    #[error(transparent)]
    Activity(#[from] InvalidActivityType),
    #[error("activity_name is required when activity_type is set")]
    MissingActivityName,
}

pub(super) fn parse_request(args: &Value) -> Result<PresenceRequest, PresenceInputError> {
    let status = args
        .get("online_status")
        .and_then(Value::as_str)
        .unwrap_or("online")
        .parse()?;
    let kind = args
        .get("activity_type")
        .and_then(Value::as_str)
        .map(str::parse)
        .transpose()?;
    let activity = match kind {
        Some(kind) => Some(PresenceActivity::new(
            kind,
            args.get("activity_name")
                .and_then(Value::as_str)
                .ok_or(PresenceInputError::MissingActivityName)?
                .to_owned(),
        )),
        None => None,
    };
    Ok(PresenceRequest::new(status, activity))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_projects_success_and_denial() {
        assert_eq!(typing(Ok(())), json!({"ok": true}));
        assert_eq!(
            typing(Err(TypingError::Denied)),
            json!({"error": "channel not in allowlist"})
        );
    }
}
