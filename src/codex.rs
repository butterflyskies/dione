//! Legacy Codex delivery projection backed by the shared durable state machine.
mod app_server;
mod error;
pub(crate) mod legacy_delivery;
mod queue;
mod queue_codec;
#[cfg(test)]
pub(crate) use crate::durable::PersistFailure;
#[cfg(test)]
pub(crate) use queue_codec::LegacyReplayIdentity;
#[cfg(test)]
mod legacy_tests;
#[cfg(test)]
mod owned_tests;

use crate::attention::types::RecordId;
pub use crate::durable::{
    AllocationKind, CodecError, CodecErrorKind, ConsumerId, ConsumerRegistrationResult,
    ConsumerStatus, DeliveryToken, EndpointId, EventId, HandoffResult, IdentityError,
    IdentityErrorKind, QueueError, QueueErrorKind, QueueStatus,
    RemovalResult as AttentionInvalidationResult, SnapshotIssue,
};
pub use app_server::{
    CodexDeliveryConfig, CodexDeliveryError, CodexDeliveryErrorKind, run_delivery_worker,
};
use chrono::{DateTime, Utc};
pub use error::{CodexQueueError, CodexQueueErrorKind};
pub(crate) use error::{LiveQueueError, LiveQueueErrorKind};
#[cfg(test)]
pub(crate) use legacy_delivery::decode_attention as decode_legacy_attention_for_test;
pub use legacy_delivery::{LegacyDeliveryError, LegacyDiscordScope};
pub use queue::CodexEventQueue;
pub(crate) use queue::{CodexBindingController, CodexResidentHandle, LiveAdmissionOutcome};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fmt, str::FromStr, time::Duration};

const MAX_WAIT: Duration = Duration::from_secs(55);
const DEFAULT_WAIT: Duration = Duration::from_secs(45);
const MAX_LEASE: Duration = Duration::from_secs(3600);
const DEFAULT_LEASE: Duration = Duration::from_secs(120);
const MAX_CONSUMER_TTL: Duration = Duration::from_secs(86400);
const DEFAULT_CONSUMER_TTL: Duration = Duration::from_secs(900);
const MIN_ATTENTION_DEFER: Duration = Duration::from_millis(100);
const MAX_ATTENTION_DEFER: Duration = Duration::from_secs(30);

fn validate_ascii_id(value: &str, max_len: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}
/// Exact Codex conversation receiving live inbound delivery.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct CodexThreadId(String);

impl CodexThreadId {
    /// Parses an external Codex thread ID using the historical trim policy.
    pub fn parse(value: &str) -> Result<Self, CodexQueueError> {
        let value = value.trim();
        if !validate_ascii_id(value, 128) {
            return Err(CodexQueueErrorKind::InvalidThreadId.into());
        }
        Ok(Self(value.to_owned()))
    }

    /// Borrows the validated host thread spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for CodexThreadId {
    type Err = CodexQueueError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl fmt::Display for CodexThreadId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl<'de> Deserialize<'de> for CodexThreadId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.trim() != value {
            return Err(serde::de::Error::custom("invalid stored Codex thread id"));
        }
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

/// Safe-turn scheduling requested by the attention admission layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AttentionDelivery {
    Prompt,
    NextTurn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AttentionScheduling {
    pub(crate) delivery: AttentionDelivery,
    pub(crate) record_id: RecordId,
}

/// Event returned to a Codex consumer under a time-bounded lease.
#[derive(Debug, Clone, Serialize)]
pub struct LeasedEvent {
    /// Stable durable event identifier.
    pub event_id: EventId,
    /// Exact acknowledgement token for this lease.
    pub delivery_token: DeliveryToken,
    /// Time at which this lease becomes eligible for redelivery.
    pub lease_expires_at: DateTime<Utc>,
    /// Registration that owns this lease.
    pub consumer_id: ConsumerId,
    /// Structured MCP notification. User-authored content remains data.
    pub event: Value,
    #[serde(skip)]
    pub(crate) attention: Option<AttentionScheduling>,
}

/// One required selector for invalidating managed pending work.
#[derive(Debug, Clone, Copy)]
pub enum AttentionInvalidation<'a> {
    /// Match the complete configured module, conversation, and message identity.
    Source(&'a crate::core::MessageRef),
    /// Match a validated decision identity.
    Record(&'a RecordId),
}

/// Converts an optional RPC wait duration to its bounded value.
pub fn wait_duration(seconds: Option<u64>) -> Duration {
    seconds
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_WAIT)
        .min(MAX_WAIT)
}

/// Converts an optional RPC lease duration to its bounded value.
pub fn lease_duration(seconds: Option<u64>) -> Duration {
    seconds
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_LEASE)
        .clamp(Duration::from_secs(1), MAX_LEASE)
}

/// Converts an optional RPC registration TTL to its bounded value.
pub fn consumer_ttl(seconds: Option<u64>) -> Duration {
    seconds
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_CONSUMER_TTL)
        .clamp(Duration::from_secs(60), MAX_CONSUMER_TTL)
}

/// The stable pull-protocol response when no event was leased.
pub fn timeout_response() -> Value {
    json!({"event":null,"timed_out":true})
}
