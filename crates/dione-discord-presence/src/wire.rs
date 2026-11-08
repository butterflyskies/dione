//! MCP projection of typed Discord presence results.

use crate::{
    InvalidActivityType, InvalidOnlineStatus, PresenceAcknowledgement, PresenceActivity,
    PresenceRequest, PresenceSetError, PresenceSnapshot, PresenceUnavailable,
};
use serde_json::{Value, json};
use thiserror::Error;

/// Projects local acknowledgement without claiming remote Discord delivery.
pub fn set(result: Result<PresenceAcknowledgement, PresenceSetError>) -> Value {
    match result {
        Ok(acknowledgement) => {
            json!({"ok": true, "status": match acknowledgement { PresenceAcknowledgement::Applied => "applied", PresenceAcknowledgement::Accepted => "accepted" }})
        }
        Err(error) => json!({"error": error.to_string()}),
    }
}
/// Projects requested state and installation history without probing Discord.
pub fn get(result: Result<PresenceSnapshot, PresenceUnavailable>) -> Value {
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

/// A typed MCP presence input failure preserving the legacy display and source behavior.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct PresenceInputError {
    /// The rejected input category.
    pub kind: PresenceInputErrorKind,
}

/// Which part of a presence request could not be decoded.
#[derive(Debug, Error)]
pub enum PresenceInputErrorKind {
    /// The status token is not supported.
    #[error(transparent)]
    Status(InvalidOnlineStatus),
    /// The activity token is not supported.
    #[error(transparent)]
    Activity(InvalidActivityType),
    /// An activity kind requires string display text.
    #[error("activity_name is required when activity_type is set")]
    MissingActivityName,
}

impl From<InvalidOnlineStatus> for PresenceInputError {
    fn from(error: InvalidOnlineStatus) -> Self {
        Self {
            kind: PresenceInputErrorKind::Status(error),
        }
    }
}
impl From<InvalidActivityType> for PresenceInputError {
    fn from(error: InvalidActivityType) -> Self {
        Self {
            kind: PresenceInputErrorKind::Activity(error),
        }
    }
}

/// Decodes the existing lenient MCP presence request shape into ordinary domain values.
pub fn parse_request(args: &Value) -> Result<PresenceRequest, PresenceInputError> {
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
                .ok_or(PresenceInputError {
                    kind: PresenceInputErrorKind::MissingActivityName,
                })?
                .to_owned(),
        )),
        None => None,
    };
    Ok(PresenceRequest::new(status, activity))
}
