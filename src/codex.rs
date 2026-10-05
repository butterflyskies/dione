//! Durable delivery for Codex.
//!
//! Codex cannot turn unsolicited MCP notifications into new turns. In Codex
//! mode, Dione therefore persists accepted Discord events. Codex conversations
//! may pull them explicitly, or a live app-server worker may inject them into
//! one exact thread. Consumers lease an event, handle it, then acknowledge the
//! lease. Expired leases become eligible for redelivery.

mod app_server;

use crate::attention::types::RecordId;
pub use app_server::{
    AttentionDeliveryGuard, AttentionDeliveryReceipt, AttentionGuardFailure, CodexDeliveryConfig,
    CodexDeliveryError, run_delivery_worker,
};
use camino::{Utf8Path, Utf8PathBuf};
use chrono::{DateTime, TimeDelta, Utc};
use clap::ValueEnum;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use serenity::model::id::MessageId;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{File, OpenOptions},
    io::{self, Write},
    sync::Arc,
    time::Duration,
};
use thiserror::Error;
use tokio::sync::{Mutex, Notify};

const INBOX_FILE_NAME: &str = "codex-inbox.json";
const LOCK_FILE_NAME: &str = "codex-inbox.lock";
const MAX_WAIT: Duration = Duration::from_secs(55);
const DEFAULT_WAIT: Duration = Duration::from_secs(45);
const MAX_LEASE: Duration = Duration::from_secs(60 * 60);
const DEFAULT_LEASE: Duration = Duration::from_secs(2 * 60);
const MAX_CONSUMER_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const DEFAULT_CONSUMER_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_PROCESSED_MESSAGE_IDS: usize = 10_000;
// Local replay window after acknowledgement. This bounds durable state; it
// does not promise suppression of a provider replay after the window expires.
const PROCESSED_TEAMS_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const MAX_RETAINED_TEAMS_EVENT_KEYS: usize = 10_000;
const LIVE_CONSUMER_LABEL: &str = "dione-live-app-server";
const MAX_ATTENTION_RECORD_ID_BYTES: usize = 512;
const MIN_ATTENTION_DEFER: Duration = Duration::from_millis(100);
const MAX_ATTENTION_DEFER: Duration = Duration::from_secs(30);

/// Determines how inbound Discord events are delivered to an agent harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum TransportMode {
    /// Emit Claude Code channel notifications on MCP stdout.
    #[default]
    ClaudeCode,
    /// Persist events for explicit pull through MCP tools.
    Codex,
}

/// Opaque acknowledgement token for one active event lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeliveryToken(String);

impl DeliveryToken {
    pub fn parse(value: &str) -> Result<Self, CodexQueueError> {
        let value = value.trim();
        if value.is_empty() || value.len() > 128 {
            return Err(CodexQueueError::InvalidDeliveryToken);
        }
        Ok(Self(value.to_owned()))
    }

    fn new(event_id: EventId, generation: u64) -> Self {
        Self(format!("dione-{event_id}-{generation}"))
    }
}

/// Monotonic identifier for one event in the durable Codex inbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventId(u64);

impl EventId {
    pub fn new(value: u64) -> Self {
        Self(value)
    }
}

impl std::fmt::Display for EventId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Validate an ASCII identifier: non-empty, max `max_len` bytes, `[A-Za-z0-9_-]` only.
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
    pub fn parse(value: &str) -> Result<Self, CodexQueueError> {
        let value = value.trim();
        if !validate_ascii_id(value, 128) {
            return Err(CodexQueueError::InvalidThreadId);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for CodexThreadId {
    type Err = CodexQueueError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl std::fmt::Display for CodexThreadId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl<'de> Deserialize<'de> for CodexThreadId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

/// Discord snowflake cached for durable event deduplication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct DiscordMessageId(MessageId);

/// Identity from an authenticated Teams Activity, independent of reply handle
/// and notification timestamp. The provider tag prevents cross-transport IDs
/// from sharing a deduplication namespace.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "provider", content = "id", rename_all = "snake_case")]
pub(crate) enum ProviderEventKey {
    Teams {
        tenant_id: String,
        bot_id: String,
        conversation_id: String,
        activity_id: String,
    },
}

impl ProviderEventKey {
    pub(crate) fn teams(
        tenant_id: &str,
        bot_id: &str,
        conversation_id: &str,
        activity_id: &str,
    ) -> Result<Self, LiveQueueError> {
        if [tenant_id, bot_id, conversation_id, activity_id]
            .into_iter()
            .any(|value| {
                value.is_empty() || value.len() > 512 || value.chars().any(char::is_control)
            })
        {
            return Err(LiveQueueError::InvalidProviderEventId);
        }
        Ok(Self::Teams {
            tenant_id: tenant_id.to_owned(),
            bot_id: bot_id.to_owned(),
            conversation_id: conversation_id.to_owned(),
            activity_id: activity_id.to_owned(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProcessedProviderEvent {
    key: ProviderEventKey,
    acknowledged_at: DateTime<Utc>,
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

impl Serialize for DiscordMessageId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for DiscordMessageId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let raw = value.parse::<u64>().map_err(serde::de::Error::custom)?;
        let id = crate::mcp::ids::Snowflake::new(raw)
            .map(crate::mcp::ids::Snowflake::message)
            .ok_or_else(|| serde::de::Error::custom("Discord message ID must be non-zero"))?;
        Ok(Self(id))
    }
}

/// Opaque identifier for one Codex conversation consuming inbound events.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConsumerId(String);

impl ConsumerId {
    pub fn parse(value: &str) -> Result<Self, CodexQueueError> {
        let value = value.trim();
        if !validate_ascii_id(value, 128) {
            return Err(CodexQueueError::InvalidConsumerId);
        }
        Ok(Self(value.to_owned()))
    }

    fn new(generation: u64) -> Self {
        Self(format!("codex-consumer-{generation}"))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ConsumerKind {
    #[default]
    Pull,
    Live,
}

impl ConsumerKind {
    fn is_pull(&self) -> bool {
        *self == Self::Pull
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConsumerRegistration {
    id: ConsumerId,
    label: String,
    /// Missing on older inboxes: a label alone never confers live authority.
    #[serde(default, skip_serializing_if = "ConsumerKind::is_pull")]
    kind: ConsumerKind,
    expires_at: DateTime<Utc>,
    #[serde(default = "default_consumer_ttl_seconds")]
    ttl_seconds: u64,
}

impl ConsumerRegistration {
    fn ttl(&self) -> Duration {
        Duration::from_secs(self.ttl_seconds).min(MAX_CONSUMER_TTL)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Lease {
    token: DeliveryToken,
    #[serde(default)]
    consumer_id: Option<ConsumerId>,
    expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueuedEvent {
    id: EventId,
    payload: Value,
    #[serde(default)]
    discord_message_id: Option<DiscordMessageId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider_event_key: Option<ProviderEventKey>,
    #[serde(default)]
    consumer_id: Option<ConsumerId>,
    /// Exact live thread binding at ingress. Pull consumers leave this unset.
    #[serde(default)]
    live_thread_id: Option<CodexThreadId>,
    /// Parsed and persisted at ingress so managed notifications can never
    /// silently fall back to ordinary delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    attention: Option<AttentionScheduling>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deferred_until: Option<DateTime<Utc>>,
    #[serde(default)]
    lease: Option<Lease>,
}

impl QueuedEvent {
    /// A persisted route or provider key remains private even after its live
    /// registration expires or a legacy inbox loses its consumer kind.
    fn is_live_bound(&self) -> bool {
        self.live_thread_id.is_some() || self.provider_event_key.is_some()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct InboxState {
    next_id: u64,
    #[serde(default)]
    next_lease_generation: u64,
    #[serde(default)]
    next_consumer_generation: u64,
    #[serde(default)]
    primary_consumer: Option<ConsumerId>,
    #[serde(default)]
    live_thread_id: Option<CodexThreadId>,
    #[serde(default)]
    consumers: Vec<ConsumerRegistration>,
    #[serde(default)]
    processed_message_ids: VecDeque<DiscordMessageId>,
    #[serde(default)]
    processed_provider_event_keys: VecDeque<ProcessedProviderEvent>,
    entries: VecDeque<QueuedEvent>,
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum PersistFailure {
    BeforeWrite,
    BeforeRename,
    AfterRename,
}

struct DurableInbox {
    path: Utf8PathBuf,
    temporary_path: Utf8PathBuf,
    directory_sync_pending: bool,
    _lock_file: File,
    #[cfg(test)]
    persist_failure: Option<PersistFailure>,
    #[cfg(test)]
    directory_sync_attempts: usize,
    state: InboxState,
    message_ids: HashSet<DiscordMessageId>,
    processed_message_ids: HashSet<DiscordMessageId>,
    provider_event_keys: HashSet<ProviderEventKey>,
    processed_provider_event_keys: HashMap<ProviderEventKey, DateTime<Utc>>,
}

/// A single-owner, durable queue shared by Discord ingress and MCP pull tools.
#[derive(Clone)]
pub struct CodexEventQueue {
    inbox: Arc<Mutex<DurableInbox>>,
    changed: Arc<Notify>,
    #[cfg(test)]
    binding_publish_gate: Arc<std::sync::Mutex<Option<BindingPublishGate>>>,
}

#[cfg(test)]
struct BindingPublishGate {
    reached: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}

/// Outcome of admitting one notification to the exact live route.
/// An `Err` from `enqueue_live` always means this notification was not inserted.
#[derive(Debug)]
pub(crate) enum LiveEnqueueReceipt {
    Committed,
    CommittedDurabilityUncertain(CodexQueueError),
    Duplicate,
}

/// Event returned to a Codex consumer under a time-bounded lease.
#[derive(Debug, Clone, Serialize)]
pub struct LeasedEvent {
    pub event_id: EventId,
    pub delivery_token: DeliveryToken,
    pub lease_expires_at: DateTime<Utc>,
    pub consumer_id: ConsumerId,
    /// Structured MCP notification. User-authored content remains data.
    pub event: Value,
    #[serde(skip)]
    pub(crate) attention: Option<AttentionScheduling>,
}

#[derive(Debug, Clone)]
struct LeasePoll {
    event: Option<LeasedEvent>,
    deferred_for: Option<Duration>,
}

/// Durable result of narrowly invalidating attention-managed pending work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AttentionInvalidationResult {
    pub removed: usize,
    pub invalidated_leases: usize,
}

/// Queue status for diagnostics and operational checks.
#[derive(Debug, Clone, Serialize)]
pub struct QueueStatus {
    pub queued: usize,
    pub leased: usize,
    pub next_event_id: EventId,
    pub primary_consumer: Option<ConsumerId>,
    pub consumers: Vec<ConsumerStatus>,
    pub unassigned: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConsumerStatus {
    pub consumer_id: ConsumerId,
    pub label: String,
    pub expires_at: DateTime<Utc>,
    pub primary: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConsumerRegistrationResult {
    pub consumer_id: ConsumerId,
    pub primary: bool,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HandoffResult {
    pub previous_consumer_id: ConsumerId,
    pub primary_consumer_id: ConsumerId,
    pub moved_pending: usize,
    pub invalidated_leases: usize,
}

#[derive(Debug, Error)]
pub enum CodexQueueError {
    #[error("failed to access Codex inbox `{path}`")]
    InboxIo {
        path: Utf8PathBuf,
        #[source]
        source: io::Error,
    },
    /// A visible commit remains in memory, but its crash durability is unconfirmed.
    ///
    /// Further mutations and duplicate checks retry the directory sync first.
    /// This error can also reject a new operation before mutation while a
    /// previous commit still needs its directory sync.
    /// Retrying an acknowledgement can then return `UnknownDeliveryToken` because
    /// its original removal committed. Inspect queue state before retrying a
    /// non-idempotent operation such as consumer registration.
    #[error("visible Codex inbox commit at `{path}` has unconfirmed crash durability")]
    InboxDurabilityUncertain {
        path: Utf8PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to decode Codex inbox `{path}`")]
    InboxDecode {
        path: Utf8PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("another Dione process owns Codex inbox `{path}`")]
    InboxLocked { path: Utf8PathBuf },
    #[error("invalid delivery token")]
    InvalidDeliveryToken,
    #[error("invalid consumer id")]
    InvalidConsumerId,
    #[error("invalid Codex thread id")]
    InvalidThreadId,
    #[error("consumer is unknown or expired")]
    UnknownConsumer,
    #[error("consumer is not the active primary")]
    NotPrimaryConsumer,
    #[error("an active primary consumer already exists")]
    PrimaryConsumerExists,
    #[error("delivery token is unknown or its lease expired")]
    UnknownDeliveryToken,
    #[error("malformed attention scheduling metadata: {reason}")]
    MalformedAttentionMetadata { reason: &'static str },
    #[error("attention invalidation requires a source message id or record id")]
    AttentionSelectorRequired,
    #[error("delivery token does not identify an attention-managed event")]
    NotAttentionManaged,
}

/// Teams-only route and replay failures stay crate-private so the public
/// exhaustive Codex queue error remains stable across this feature release.
#[derive(Debug, Error)]
pub(crate) enum LiveQueueError {
    #[error(transparent)]
    Queue(#[from] CodexQueueError),
    #[error("invalid provider event id")]
    InvalidProviderEventId,
    #[error("no exact live resident route is ready")]
    ResidentUnavailable,
    #[error("teams replay-identity capacity is full")]
    ReplayCapacity,
}

impl DurableInbox {
    fn load(state_dir: &Utf8Path) -> Result<Self, CodexQueueError> {
        std::fs::create_dir_all(state_dir.as_std_path()).map_err(|source| {
            CodexQueueError::InboxIo {
                path: state_dir.to_owned(),
                source,
            }
        })?;

        let lock_path = state_dir.join(LOCK_FILE_NAME);
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path.as_std_path())
            .map_err(|source| CodexQueueError::InboxIo {
                path: lock_path.clone(),
                source,
            })?;
        lock_file
            .try_lock_exclusive()
            .map_err(|_| CodexQueueError::InboxLocked {
                path: lock_path.clone(),
            })?;

        let path = state_dir.join(INBOX_FILE_NAME);
        let temporary_path = state_dir.join(format!("{INBOX_FILE_NAME}.tmp"));
        let (mut state, directory_sync_pending): (InboxState, bool) =
            match std::fs::read(path.as_std_path()) {
                Ok(bytes) => (
                    serde_json::from_slice(&bytes).map_err(|source| {
                        CodexQueueError::InboxDecode {
                            path: path.clone(),
                            source,
                        }
                    })?,
                    true,
                ),
                Err(source) if source.kind() == io::ErrorKind::NotFound => {
                    (InboxState::default(), false)
                }
                Err(source) => {
                    return Err(CodexQueueError::InboxIo {
                        path: path.clone(),
                        source,
                    });
                }
            };
        for event in &mut state.entries {
            if event.discord_message_id.is_none() && event.provider_event_key.is_none() {
                event.discord_message_id = discord_message_id(&event.payload);
            }
            event.attention = parse_attention_scheduling(&event.payload)?;
            migrate_legacy_evidence_projection(&mut event.payload);
        }
        let message_ids = state
            .entries
            .iter()
            .filter_map(|event| event.discord_message_id)
            .collect();
        let processed_message_ids = state.processed_message_ids.iter().cloned().collect();
        let provider_event_keys = state
            .entries
            .iter()
            .filter_map(|event| event.provider_event_key.clone())
            .collect();
        let processed_provider_event_keys = state
            .processed_provider_event_keys
            .iter()
            .map(|event| (event.key.clone(), event.acknowledged_at))
            .collect();
        Ok(Self {
            path,
            temporary_path,
            directory_sync_pending,
            _lock_file: lock_file,
            #[cfg(test)]
            persist_failure: None,
            #[cfg(test)]
            directory_sync_attempts: 0,
            state,
            message_ids,
            processed_message_ids,
            provider_event_keys,
            processed_provider_event_keys,
        })
    }

    fn enqueue(&mut self, payload: Value) -> Result<bool, CodexQueueError> {
        self.sync_directory()?;
        let now = Utc::now();
        let discord_message_id = discord_message_id(&payload);
        let attention = parse_attention_scheduling(&payload)?;
        if discord_message_id.as_ref().is_some_and(|id| {
            self.message_ids.contains(id) || self.processed_message_ids.contains(id)
        }) {
            return Ok(false);
        }

        self.transaction(move |inbox| {
            inbox.expire_consumers(now);
            let id = EventId::new(inbox.state.next_id);
            inbox.state.next_id = inbox.state.next_id.saturating_add(1);
            if let Some(message_id) = &discord_message_id {
                inbox.message_ids.insert(*message_id);
            }
            inbox.state.entries.push_back(QueuedEvent {
                id,
                payload,
                discord_message_id,
                provider_event_key: None,
                consumer_id: inbox.state.primary_consumer.clone(),
                live_thread_id: inbox.live_event_thread_id(),
                attention,
                deferred_until: None,
                lease: None,
            });
            Ok(true)
        })
    }

    /// Admit only to the resident's current consumer and thread in one mutation.
    /// The sync preflight separates a prior uncertain commit from uncertainty
    /// caused by this insertion.
    fn enqueue_live(
        &mut self,
        payload: Value,
        provider_event_key: ProviderEventKey,
    ) -> Result<LiveEnqueueReceipt, LiveQueueError> {
        self.sync_directory()?;
        let now = Utc::now();
        if self.contains_provider_event(&provider_event_key, now) {
            return Ok(LiveEnqueueReceipt::Duplicate);
        }
        let attention = parse_attention_scheduling(&payload)?;
        let result = self.transaction(move |inbox| {
            inbox.prune_processed_provider_events(now);
            if inbox.teams_event_capacity_used() >= MAX_RETAINED_TEAMS_EVENT_KEYS {
                return Err(LiveQueueError::ReplayCapacity);
            }
            inbox.expire_consumers(now);
            let Some(consumer_id) = inbox.state.primary_consumer.clone() else {
                return Err(LiveQueueError::ResidentUnavailable);
            };
            let Some(consumer) = inbox
                .state
                .consumers
                .iter()
                .find(|consumer| consumer.id == consumer_id)
            else {
                return Err(LiveQueueError::ResidentUnavailable);
            };
            if consumer.kind != ConsumerKind::Live || consumer.label != LIVE_CONSUMER_LABEL {
                return Err(LiveQueueError::ResidentUnavailable);
            }
            let Some(live_thread_id) = inbox.state.live_thread_id.clone() else {
                return Err(LiveQueueError::ResidentUnavailable);
            };

            let id = EventId::new(inbox.state.next_id);
            inbox.state.next_id = inbox.state.next_id.saturating_add(1);
            inbox.provider_event_keys.insert(provider_event_key.clone());
            inbox.state.entries.push_back(QueuedEvent {
                id,
                payload,
                discord_message_id: None,
                provider_event_key: Some(provider_event_key),
                consumer_id: Some(consumer_id),
                live_thread_id: Some(live_thread_id),
                attention,
                deferred_until: None,
                lease: None,
            });
            Ok(())
        });
        match result {
            Ok(()) => Ok(LiveEnqueueReceipt::Committed),
            Err(LiveQueueError::Queue(
                error @ CodexQueueError::InboxDurabilityUncertain { .. },
            )) => Ok(LiveEnqueueReceipt::CommittedDurabilityUncertain(error)),
            Err(error) => Err(error),
        }
    }

    fn lease_next(
        &mut self,
        consumer_id: &ConsumerId,
        now: DateTime<Utc>,
        lease_duration: Duration,
        live_thread_id: Option<&CodexThreadId>,
    ) -> Result<LeasePoll, CodexQueueError> {
        let mut retained_lease = None;
        let result = self.transaction(|inbox| {
            inbox.ensure_consumer_access(consumer_id, live_thread_id.is_some())?;
            inbox.touch_consumer(consumer_id, now)?;
            for event in &mut inbox.state.entries {
                if event
                    .lease
                    .as_ref()
                    .is_some_and(|lease| lease.expires_at <= now)
                {
                    event.lease = None;
                }
            }

            let matches_consumer = |event: &QueuedEvent| {
                event.lease.is_none()
                    && event.consumer_id.as_ref() == Some(consumer_id)
                    && (live_thread_id.is_some() || !event.is_live_bound())
                    && live_thread_id
                        .is_none_or(|thread_id| event.live_thread_id.as_ref() == Some(thread_id))
            };
            let index = inbox.state.entries.iter().position(|event| {
                matches_consumer(event)
                    && event
                        .deferred_until
                        .is_none_or(|available| available <= now)
            });
            let Some(index) = index else {
                let deferred_for = inbox
                    .state
                    .entries
                    .iter()
                    .filter(|event| matches_consumer(event))
                    .filter_map(|event| event.deferred_until)
                    .filter(|available| *available > now)
                    .min()
                    .and_then(|available| available.signed_duration_since(now).to_std().ok());
                return Ok(LeasePoll {
                    event: None,
                    deferred_for,
                });
            };

            let generation = inbox.state.next_lease_generation;
            inbox.state.next_lease_generation = generation.saturating_add(1);
            let event = &mut inbox.state.entries[index];
            event.deferred_until = None;
            let token = DeliveryToken::new(event.id, generation);
            let expires_at = now + duration_delta(lease_duration);
            event.lease = Some(Lease {
                token: token.clone(),
                consumer_id: Some(consumer_id.clone()),
                expires_at,
            });
            let poll = LeasePoll {
                event: Some(LeasedEvent {
                    event_id: event.id,
                    delivery_token: token,
                    lease_expires_at: expires_at,
                    consumer_id: consumer_id.clone(),
                    event: event.payload.clone(),
                    attention: event.attention.clone(),
                }),
                deferred_for: None,
            };
            retained_lease = Some(poll.clone());
            Ok(poll)
        });
        match result {
            Err(error @ CodexQueueError::InboxDurabilityUncertain { .. }) => {
                let Some(poll) = retained_lease else {
                    // A pending prior directory sync failed before this
                    // operation could mutate the inbox.
                    return Err(error);
                };
                if let Some(event) = &poll.event {
                    tracing::warn!(
                        event_id = %event.event_id,
                        error = %error,
                        "Codex lease is visible with unconfirmed crash durability; delivery may replay after a crash"
                    );
                }
                Ok(poll)
            }
            other => other,
        }
    }

    fn bind_live_thread(
        &mut self,
        thread_id: Option<CodexThreadId>,
    ) -> Result<(), CodexQueueError> {
        self.transaction(move |inbox| {
            inbox.state.live_thread_id = thread_id;
            Ok(())
        })
    }

    fn live_event_thread_id(&self) -> Option<CodexThreadId> {
        let primary = self.state.primary_consumer.as_ref()?;
        self.state.consumers.iter().find(|consumer| {
            consumer.id == *primary
                && consumer.kind == ConsumerKind::Live
                && consumer.label == LIVE_CONSUMER_LABEL
        })?;
        self.state.live_thread_id.clone()
    }

    fn acknowledge(
        &mut self,
        consumer_id: &ConsumerId,
        token: &DeliveryToken,
        now: DateTime<Utc>,
        allow_live: bool,
    ) -> Result<(), CodexQueueError> {
        self.transaction(|inbox| {
            inbox.ensure_consumer_access(consumer_id, allow_live)?;
            inbox.touch_consumer(consumer_id, now)?;
            let Some(index) = inbox.state.entries.iter().position(|event| {
                event.lease.as_ref().is_some_and(|lease| {
                    lease.token == *token && lease.consumer_id.as_ref() == Some(consumer_id)
                })
            }) else {
                return Err(CodexQueueError::UnknownDeliveryToken);
            };
            if !allow_live && inbox.state.entries[index].is_live_bound() {
                return Err(CodexQueueError::UnknownConsumer);
            }
            let Some(removed) = inbox.state.entries.remove(index) else {
                return Err(CodexQueueError::UnknownDeliveryToken);
            };
            if let Some(message_id) = removed.discord_message_id {
                inbox.message_ids.remove(&message_id);
                inbox.remember_processed_message(message_id);
            }
            if let Some(key) = removed.provider_event_key {
                inbox.provider_event_keys.remove(&key);
                inbox.processed_provider_event_keys.insert(key.clone(), now);
                inbox
                    .state
                    .processed_provider_event_keys
                    .push_back(ProcessedProviderEvent {
                        key,
                        acknowledged_at: now,
                    });
            }
            Ok(())
        })
    }

    fn defer_attention(
        &mut self,
        consumer_id: &ConsumerId,
        token: &DeliveryToken,
        now: DateTime<Utc>,
        delay: Duration,
    ) -> Result<(), CodexQueueError> {
        self.transaction(|inbox| {
            inbox.touch_consumer(consumer_id, now)?;
            let event = inbox
                .state
                .entries
                .iter_mut()
                .find(|event| {
                    event.lease.as_ref().is_some_and(|lease| {
                        lease.token == *token && lease.consumer_id.as_ref() == Some(consumer_id)
                    })
                })
                .ok_or(CodexQueueError::UnknownDeliveryToken)?;
            if event.attention.is_none() {
                return Err(CodexQueueError::NotAttentionManaged);
            }
            event.lease = None;
            event.deferred_until = Some(now + duration_delta(delay));
            Ok(())
        })
    }

    fn invalidate_attention(
        &mut self,
        source_message_id: Option<DiscordMessageId>,
        record_id: Option<&RecordId>,
    ) -> Result<AttentionInvalidationResult, CodexQueueError> {
        if source_message_id.is_none() && record_id.is_none() {
            return Err(CodexQueueError::AttentionSelectorRequired);
        }
        self.transaction(|inbox| {
            let matches = |event: &QueuedEvent| {
                event.attention.as_ref().is_some_and(|attention| {
                    source_message_id.is_some_and(|id| event.discord_message_id == Some(id))
                        || record_id.is_some_and(|id| &attention.record_id == id)
                })
            };
            let mut invalidated_leases = 0;
            for event in &mut inbox.state.entries {
                if matches(event) && event.lease.take().is_some() {
                    invalidated_leases += 1;
                }
            }
            let removed_message_ids: Vec<_> = inbox
                .state
                .entries
                .iter()
                .filter(|event| matches(event))
                .filter_map(|event| event.discord_message_id)
                .collect();
            let removed_provider_keys: Vec<_> = inbox
                .state
                .entries
                .iter()
                .filter(|event| matches(event))
                .filter_map(|event| event.provider_event_key.clone())
                .collect();
            let before = inbox.state.entries.len();
            inbox.state.entries.retain(|event| !matches(event));
            for message_id in removed_message_ids {
                inbox.message_ids.remove(&message_id);
            }
            for key in removed_provider_keys {
                inbox.provider_event_keys.remove(&key);
            }
            Ok(AttentionInvalidationResult {
                removed: before - inbox.state.entries.len(),
                invalidated_leases,
            })
        })
    }

    fn status(&mut self, now: DateTime<Utc>) -> QueueStatus {
        let active_consumers: Vec<_> = self
            .state
            .consumers
            .iter()
            .filter(|consumer| consumer.expires_at > now)
            .collect();
        let primary_consumer = self.state.primary_consumer.clone().filter(|primary| {
            active_consumers
                .iter()
                .any(|consumer| consumer.id == *primary)
        });
        QueueStatus {
            queued: self.state.entries.len(),
            leased: self
                .state
                .entries
                .iter()
                .filter(|event| {
                    event
                        .lease
                        .as_ref()
                        .is_some_and(|lease| lease.expires_at > now)
                })
                .count(),
            next_event_id: EventId::new(self.state.next_id),
            primary_consumer: primary_consumer.clone(),
            consumers: active_consumers
                .into_iter()
                .map(|consumer| ConsumerStatus {
                    consumer_id: consumer.id.clone(),
                    label: consumer.label.clone(),
                    expires_at: consumer.expires_at,
                    primary: primary_consumer.as_ref() == Some(&consumer.id),
                })
                .collect(),
            unassigned: self
                .state
                .entries
                .iter()
                .filter(|event| event.consumer_id.is_none())
                .count(),
        }
    }

    fn register_consumer(
        &mut self,
        label: String,
        now: DateTime<Utc>,
        ttl: Duration,
        make_primary: bool,
        claim_unassigned: bool,
    ) -> Result<ConsumerRegistrationResult, CodexQueueError> {
        self.transaction(move |inbox| {
            if label.trim() == LIVE_CONSUMER_LABEL {
                return Err(CodexQueueError::InvalidConsumerId);
            }
            inbox.expire_consumers(now);
            if make_primary && inbox.state.primary_consumer.is_some() {
                return Err(CodexQueueError::PrimaryConsumerExists);
            }
            let generation = inbox.state.next_consumer_generation;
            inbox.state.next_consumer_generation = generation.saturating_add(1);
            let consumer_id = ConsumerId::new(generation);
            let expires_at = now + duration_delta(ttl);
            inbox.state.consumers.push(ConsumerRegistration {
                id: consumer_id.clone(),
                label,
                kind: ConsumerKind::Pull,
                expires_at,
                ttl_seconds: ttl.as_secs(),
            });
            if make_primary {
                inbox.state.primary_consumer = Some(consumer_id.clone());
                if claim_unassigned {
                    for event in &mut inbox.state.entries {
                        if event.consumer_id.is_none() && !event.is_live_bound() {
                            event.consumer_id = Some(consumer_id.clone());
                        }
                    }
                }
            }
            Ok(ConsumerRegistrationResult {
                consumer_id,
                primary: make_primary,
                expires_at,
            })
        })
    }

    fn register_live_consumer(
        &mut self,
        now: DateTime<Utc>,
    ) -> Result<ConsumerId, CodexQueueError> {
        self.transaction(|inbox| {
            if let Some(primary) = inbox.state.primary_consumer.clone()
                && let Some(consumer) = inbox
                    .state
                    .consumers
                    .iter_mut()
                    .find(|consumer| consumer.id == primary)
                && consumer.kind == ConsumerKind::Live
                && consumer.label == LIVE_CONSUMER_LABEL
            {
                // The inbox lock proves the previous Dione process is gone,
                // so the live worker may resume its durable identity even
                // after a long outage. Keeping the identity also keeps its
                // already-routed events deliverable.
                consumer.expires_at = now + duration_delta(MAX_CONSUMER_TTL);
                consumer.ttl_seconds = MAX_CONSUMER_TTL.as_secs();
                return Ok(primary);
            }
            inbox.expire_consumers(now);
            if inbox.state.primary_consumer.is_some() {
                return Err(CodexQueueError::PrimaryConsumerExists);
            }
            let generation = inbox.state.next_consumer_generation;
            inbox.state.next_consumer_generation = generation.saturating_add(1);
            let consumer_id = ConsumerId::new(generation);
            inbox.state.consumers.push(ConsumerRegistration {
                id: consumer_id.clone(),
                label: LIVE_CONSUMER_LABEL.to_owned(),
                kind: ConsumerKind::Live,
                expires_at: now + duration_delta(MAX_CONSUMER_TTL),
                ttl_seconds: MAX_CONSUMER_TTL.as_secs(),
            });
            // Existing unassigned/orphaned events are intentionally not moved.
            // Enabling live delivery must not replay an arbitrary old backlog.
            inbox.state.primary_consumer = Some(consumer_id.clone());
            Ok(consumer_id)
        })
    }

    fn handoff(
        &mut self,
        from: &ConsumerId,
        to: &ConsumerId,
        now: DateTime<Utc>,
        move_pending: bool,
    ) -> Result<HandoffResult, CodexQueueError> {
        self.transaction(|inbox| {
            inbox.ensure_consumer_access(from, false)?;
            inbox.ensure_consumer_access(to, false)?;
            inbox.expire_consumers(now);
            if inbox.state.primary_consumer.as_ref() != Some(from) {
                return Err(CodexQueueError::NotPrimaryConsumer);
            }
            inbox.touch_consumer(to, now)?;
            let mut moved_pending = 0;
            let mut invalidated_leases = 0;
            if move_pending {
                for event in &mut inbox.state.entries {
                    if event.consumer_id.as_ref() == Some(from) && !event.is_live_bound() {
                        if event.lease.take().is_some() {
                            invalidated_leases += 1;
                        }
                        event.consumer_id = Some(to.clone());
                        moved_pending += 1;
                    }
                }
            }
            inbox.state.primary_consumer = Some(to.clone());
            Ok(HandoffResult {
                previous_consumer_id: from.clone(),
                primary_consumer_id: to.clone(),
                moved_pending,
                invalidated_leases,
            })
        })
    }

    fn claim_primary(
        &mut self,
        consumer_id: &ConsumerId,
        now: DateTime<Utc>,
        claim_orphaned: bool,
    ) -> Result<usize, CodexQueueError> {
        self.transaction(|inbox| {
            inbox.ensure_consumer_access(consumer_id, false)?;
            inbox.expire_consumers(now);
            if inbox.state.primary_consumer.is_some() {
                return Err(CodexQueueError::PrimaryConsumerExists);
            }
            inbox.touch_consumer(consumer_id, now)?;
            let active_consumers: HashSet<_> = inbox
                .state
                .consumers
                .iter()
                .map(|consumer| consumer.id.clone())
                .collect();
            let mut claimed = 0;
            if claim_orphaned {
                for event in &mut inbox.state.entries {
                    if !event.is_live_bound()
                        && event
                            .consumer_id
                            .as_ref()
                            .is_none_or(|owner| !active_consumers.contains(owner))
                    {
                        event.lease = None;
                        event.consumer_id = Some(consumer_id.clone());
                        claimed += 1;
                    }
                }
            }
            inbox.state.primary_consumer = Some(consumer_id.clone());
            Ok(claimed)
        })
    }

    fn touch_consumer(
        &mut self,
        consumer_id: &ConsumerId,
        now: DateTime<Utc>,
    ) -> Result<(), CodexQueueError> {
        self.expire_consumers(now);
        let Some(consumer) = self
            .state
            .consumers
            .iter_mut()
            .find(|consumer| consumer.id == *consumer_id)
        else {
            return Err(CodexQueueError::UnknownConsumer);
        };
        consumer.expires_at = now + duration_delta(consumer.ttl());
        Ok(())
    }

    fn ensure_consumer_access(
        &self,
        consumer_id: &ConsumerId,
        live_call: bool,
    ) -> Result<(), CodexQueueError> {
        if let Some(consumer) = self
            .state
            .consumers
            .iter()
            .find(|consumer| consumer.id == *consumer_id)
        {
            if live_call {
                if consumer.kind != ConsumerKind::Live || consumer.label != LIVE_CONSUMER_LABEL {
                    return Err(CodexQueueError::UnknownConsumer);
                }
            } else if consumer.kind == ConsumerKind::Live {
                return Err(CodexQueueError::UnknownConsumer);
            }
        }
        Ok(())
    }

    fn expire_consumers(&mut self, now: DateTime<Utc>) {
        self.state
            .consumers
            .retain(|consumer| consumer.expires_at > now);
        if self.state.primary_consumer.as_ref().is_some_and(|primary| {
            !self
                .state
                .consumers
                .iter()
                .any(|consumer| consumer.id == *primary)
        }) {
            self.state.primary_consumer = None;
        }
    }

    fn remember_processed_message(&mut self, message_id: DiscordMessageId) {
        self.processed_message_ids.insert(message_id);
        self.state.processed_message_ids.push_back(message_id);
        while self.state.processed_message_ids.len() > MAX_PROCESSED_MESSAGE_IDS {
            if let Some(expired) = self.state.processed_message_ids.pop_front() {
                self.processed_message_ids.remove(&expired);
            }
        }
    }

    fn contains_provider_event(&self, key: &ProviderEventKey, now: DateTime<Utc>) -> bool {
        let cutoff = now - duration_delta(PROCESSED_TEAMS_RETENTION);
        self.provider_event_keys.contains(key)
            || self
                .processed_provider_event_keys
                .get(key)
                .is_some_and(|acknowledged_at| *acknowledged_at > cutoff)
    }

    fn teams_event_capacity_used(&self) -> usize {
        self.provider_event_keys.len() + self.processed_provider_event_keys.len()
    }

    fn prune_processed_provider_events(&mut self, now: DateTime<Utc>) {
        let cutoff = now - duration_delta(PROCESSED_TEAMS_RETENTION);
        self.state
            .processed_provider_event_keys
            .retain(|event| event.acknowledged_at > cutoff);
        self.processed_provider_event_keys
            .retain(|_, acknowledged_at| *acknowledged_at > cutoff);
    }

    fn transaction<T, E>(&mut self, mutate: impl FnOnce(&mut Self) -> Result<T, E>) -> Result<T, E>
    where
        E: From<CodexQueueError>,
    {
        self.sync_directory().map_err(E::from)?;
        let state = self.state.clone();
        let message_ids = self.message_ids.clone();
        let processed_message_ids = self.processed_message_ids.clone();
        let provider_event_keys = self.provider_event_keys.clone();
        let processed_provider_event_keys = self.processed_provider_event_keys.clone();
        let result = mutate(self);
        match result {
            Ok(value) => match self.persist() {
                Ok(()) => Ok(value),
                Err(error @ CodexQueueError::InboxDurabilityUncertain { .. }) => {
                    Err(E::from(error))
                }
                Err(error) => {
                    self.state = state;
                    self.message_ids = message_ids;
                    self.processed_message_ids = processed_message_ids;
                    self.provider_event_keys = provider_event_keys;
                    self.processed_provider_event_keys = processed_provider_event_keys;
                    Err(E::from(error))
                }
            },
            Err(error) => {
                self.state = state;
                self.message_ids = message_ids;
                self.processed_message_ids = processed_message_ids;
                self.provider_event_keys = provider_event_keys;
                self.processed_provider_event_keys = processed_provider_event_keys;
                Err(error)
            }
        }
    }

    #[cfg(test)]
    fn fail_persist_at(&self, stage: PersistFailure) -> io::Result<()> {
        if self.persist_failure == Some(stage) {
            return Err(io::Error::other("injected persistence failure"));
        }
        Ok(())
    }

    fn persist(&mut self) -> Result<(), CodexQueueError> {
        let bytes = serde_json::to_vec_pretty(&self.state).map_err(|source| {
            CodexQueueError::InboxDecode {
                path: self.path.clone(),
                source,
            }
        })?;
        #[cfg(test)]
        self.fail_persist_at(PersistFailure::BeforeWrite)
            .map_err(|source| CodexQueueError::InboxIo {
                path: self.temporary_path.clone(),
                source,
            })?;
        let mut temporary = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(self.temporary_path.as_std_path())
            .map_err(|source| CodexQueueError::InboxIo {
                path: self.temporary_path.clone(),
                source,
            })?;
        temporary
            .write_all(&bytes)
            .and_then(|()| temporary.sync_all())
            .map_err(|source| CodexQueueError::InboxIo {
                path: self.temporary_path.clone(),
                source,
            })?;
        #[cfg(test)]
        self.fail_persist_at(PersistFailure::BeforeRename)
            .map_err(|source| CodexQueueError::InboxIo {
                path: self.path.clone(),
                source,
            })?;
        std::fs::rename(self.temporary_path.as_std_path(), self.path.as_std_path()).map_err(
            |source| CodexQueueError::InboxIo {
                path: self.path.clone(),
                source,
            },
        )?;
        self.directory_sync_pending = true;
        self.sync_directory()
    }

    fn sync_directory(&mut self) -> Result<(), CodexQueueError> {
        if !self.directory_sync_pending {
            return Ok(());
        }
        #[cfg(test)]
        {
            self.directory_sync_attempts += 1;
        }
        let Some(parent) = self.path.parent() else {
            return Ok(());
        };
        let sync = || File::open(parent.as_std_path()).and_then(|directory| directory.sync_all());
        #[cfg(test)]
        let sync = || {
            self.fail_persist_at(PersistFailure::AfterRename)
                .and_then(|()| sync())
        };
        sync().map_err(|source| CodexQueueError::InboxDurabilityUncertain {
            path: self.path.clone(),
            source,
        })?;
        self.directory_sync_pending = false;
        Ok(())
    }
}

impl CodexEventQueue {
    pub fn load(state_dir: &Utf8Path) -> Result<Self, CodexQueueError> {
        Ok(Self {
            inbox: Arc::new(Mutex::new(DurableInbox::load(state_dir)?)),
            changed: Arc::new(Notify::new()),
            #[cfg(test)]
            binding_publish_gate: Arc::new(std::sync::Mutex::new(None)),
        })
    }

    /// Persist an event before making it visible to consumers.
    ///
    /// Returns `false` when the Discord message id is already queued or processed.
    /// A durability-uncertain error retains the visible commit and requires a
    /// successful directory sync before another mutation or duplicate result.
    /// Retrying events without a deduplicated Discord message id, including
    /// lifecycle and reaction events, can insert a second entry after uncertainty.
    pub async fn enqueue(&self, payload: Value) -> Result<bool, CodexQueueError> {
        let result = self.inbox.lock().await.enqueue(payload);
        if matches!(
            result,
            Ok(true) | Err(CodexQueueError::InboxDurabilityUncertain { .. })
        ) {
            self.changed.notify_waiters();
        }
        result
    }

    /// Persist an event for the exact resident route or reject it without insertion.
    /// Both receipt variants describe a visible insertion and wake live pollers.
    pub(crate) async fn enqueue_live(
        &self,
        payload: Value,
        provider_event_key: ProviderEventKey,
    ) -> Result<LiveEnqueueReceipt, LiveQueueError> {
        let mut inbox = self.inbox.lock().await;
        let result = inbox.enqueue_live(payload, provider_event_key);
        if matches!(
            &result,
            Ok(LiveEnqueueReceipt::Committed)
                | Ok(LiveEnqueueReceipt::CommittedDurabilityUncertain(_))
        ) {
            self.changed.notify_waiters();
        }
        result
    }

    pub async fn next_event(
        &self,
        consumer_id: &ConsumerId,
        wait: Duration,
        lease: Duration,
    ) -> Result<Option<LeasedEvent>, CodexQueueError> {
        self.next_matching_event(consumer_id, None, wait, lease)
            .await
    }

    pub(crate) async fn next_live_event(
        &self,
        consumer_id: &ConsumerId,
        thread_id: &CodexThreadId,
        wait: Duration,
        lease: Duration,
    ) -> Result<Option<LeasedEvent>, CodexQueueError> {
        self.next_matching_event(consumer_id, Some(thread_id), wait, lease)
            .await
    }

    async fn next_matching_event(
        &self,
        consumer_id: &ConsumerId,
        thread_id: Option<&CodexThreadId>,
        wait: Duration,
        lease: Duration,
    ) -> Result<Option<LeasedEvent>, CodexQueueError> {
        let wait = wait.min(MAX_WAIT);
        let lease = lease.min(MAX_LEASE);
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            // Register before checking. Notify does not retain a permit when
            // no waiter exists, so registering later creates a lost-wake gap.
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let LeasePoll {
                event,
                deferred_for,
            } = self
                .inbox
                .lock()
                .await
                .lease_next(consumer_id, Utc::now(), lease, thread_id)?;
            if event.is_some() {
                return Ok(event);
            }

            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let wake_after = deferred_for.unwrap_or(remaining).min(remaining);
            if tokio::time::timeout(wake_after, notified).await.is_err() && wake_after == remaining
            {
                return Ok(None);
            }
        }
    }

    pub async fn bind_live_thread(
        &self,
        thread_id: Option<CodexThreadId>,
    ) -> Result<(), CodexQueueError> {
        self.inbox.lock().await.bind_live_thread(thread_id)?;
        self.changed.notify_waiters();
        Ok(())
    }

    pub(crate) async fn bind_live_thread_and_publish(
        &self,
        thread_id: Option<CodexThreadId>,
        binding: &tokio::sync::watch::Sender<Option<CodexThreadId>>,
    ) -> Result<(), CodexQueueError> {
        let mut inbox = self.inbox.lock().await;
        let result = inbox.bind_live_thread(thread_id);
        #[cfg(test)]
        let gate = self.binding_publish_gate.lock().unwrap().take();
        #[cfg(test)]
        if let Some(BindingPublishGate { reached, release }) = gate {
            let _ = reached.send(());
            let _ = release.await;
        }
        binding.send_if_modified(|current| {
            #[cfg(test)]
            assert!(
                self.inbox.try_lock().is_err(),
                "watch publication must still hold the inbox guard"
            );
            if *current == inbox.state.live_thread_id {
                false
            } else {
                *current = inbox.state.live_thread_id.clone();
                true
            }
        });
        drop(inbox);
        // The watch publication wakes the live worker when the binding changes.
        // The queue notification also wakes waiting pollers after a retained commit.
        if matches!(
            result,
            Ok(()) | Err(CodexQueueError::InboxDurabilityUncertain { .. })
        ) {
            self.changed.notify_waiters();
        }
        result
    }

    pub async fn acknowledge(
        &self,
        consumer_id: &ConsumerId,
        token: &DeliveryToken,
    ) -> Result<(), CodexQueueError> {
        self.inbox
            .lock()
            .await
            .acknowledge(consumer_id, token, Utc::now(), false)?;
        self.changed.notify_waiters();
        Ok(())
    }

    pub(crate) async fn acknowledge_live(
        &self,
        consumer_id: &ConsumerId,
        token: &DeliveryToken,
    ) -> Result<(), CodexQueueError> {
        self.inbox
            .lock()
            .await
            .acknowledge(consumer_id, token, Utc::now(), true)?;
        self.changed.notify_waiters();
        Ok(())
    }

    pub(crate) async fn defer_attention(
        &self,
        consumer_id: &ConsumerId,
        token: &DeliveryToken,
        delay: Duration,
    ) -> Result<(), CodexQueueError> {
        self.inbox.lock().await.defer_attention(
            consumer_id,
            token,
            Utc::now(),
            delay.clamp(MIN_ATTENTION_DEFER, MAX_ATTENTION_DEFER),
        )?;
        self.changed.notify_waiters();
        Ok(())
    }

    /// Remove only attention-managed pending work matching either selector.
    /// Active leases are invalidated transactionally before entries disappear.
    pub async fn invalidate_attention(
        &self,
        source_message_id: Option<MessageId>,
        record_id: Option<&RecordId>,
    ) -> Result<AttentionInvalidationResult, CodexQueueError> {
        if let Some(record_id) = record_id
            && (record_id.as_str().trim().is_empty()
                || record_id.as_str().len() > MAX_ATTENTION_RECORD_ID_BYTES)
        {
            return Err(CodexQueueError::MalformedAttentionMetadata {
                reason: "attention_record must be a non-empty bounded string",
            });
        }
        let result = self
            .inbox
            .lock()
            .await
            .invalidate_attention(source_message_id.map(DiscordMessageId), record_id)?;
        self.changed.notify_waiters();
        Ok(result)
    }

    pub async fn status(&self) -> QueueStatus {
        self.inbox.lock().await.status(Utc::now())
    }

    pub async fn register_consumer(
        &self,
        label: String,
        ttl: Duration,
        make_primary: bool,
        claim_unassigned: bool,
    ) -> Result<ConsumerRegistrationResult, CodexQueueError> {
        let result = self.inbox.lock().await.register_consumer(
            label,
            Utc::now(),
            ttl.min(MAX_CONSUMER_TTL),
            make_primary,
            claim_unassigned,
        )?;
        self.changed.notify_waiters();
        Ok(result)
    }

    pub(crate) async fn register_live_consumer(&self) -> Result<ConsumerId, CodexQueueError> {
        let consumer_id = self.inbox.lock().await.register_live_consumer(Utc::now())?;
        self.changed.notify_waiters();
        Ok(consumer_id)
    }

    pub async fn handoff(
        &self,
        from: &ConsumerId,
        to: &ConsumerId,
        move_pending: bool,
    ) -> Result<HandoffResult, CodexQueueError> {
        let result = self
            .inbox
            .lock()
            .await
            .handoff(from, to, Utc::now(), move_pending)?;
        self.changed.notify_waiters();
        Ok(result)
    }

    pub async fn claim_primary(
        &self,
        consumer_id: &ConsumerId,
        claim_orphaned: bool,
    ) -> Result<usize, CodexQueueError> {
        let claimed =
            self.inbox
                .lock()
                .await
                .claim_primary(consumer_id, Utc::now(), claim_orphaned)?;
        self.changed.notify_waiters();
        Ok(claimed)
    }
}

pub fn wait_duration(seconds: Option<u64>) -> Duration {
    seconds
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_WAIT)
        .min(MAX_WAIT)
}

pub fn lease_duration(seconds: Option<u64>) -> Duration {
    seconds
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_LEASE)
        .clamp(Duration::from_secs(1), MAX_LEASE)
}

pub fn consumer_ttl(seconds: Option<u64>) -> Duration {
    seconds
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_CONSUMER_TTL)
        .clamp(Duration::from_secs(60), MAX_CONSUMER_TTL)
}

const fn default_consumer_ttl_seconds() -> u64 {
    DEFAULT_CONSUMER_TTL.as_secs()
}

fn duration_delta(duration: Duration) -> TimeDelta {
    TimeDelta::from_std(duration).unwrap_or_else(|_| TimeDelta::hours(1))
}

fn discord_message_id(payload: &Value) -> Option<DiscordMessageId> {
    // A lifecycle event or reaction references a source; it is not another create.
    if payload
        .pointer("/params/meta/type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind != "message")
    {
        return None;
    }
    payload
        .pointer("/params/meta/message_id")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<u64>().ok())
        .and_then(crate::mcp::ids::Snowflake::new)
        .map(crate::mcp::ids::Snowflake::message)
        .map(DiscordMessageId)
}

fn parse_attention_scheduling(
    notification: &Value,
) -> Result<Option<AttentionScheduling>, CodexQueueError> {
    let Some(meta) = notification
        .pointer("/params/meta")
        .and_then(Value::as_object)
    else {
        return Ok(None);
    };
    let delivery = meta.get("attention_delivery");
    let record_id = meta.get("attention_record");
    if delivery.is_none() && record_id.is_none() {
        return Ok(None);
    }

    let delivery =
        delivery
            .and_then(Value::as_str)
            .ok_or(CodexQueueError::MalformedAttentionMetadata {
                reason: "attention_delivery must be prompt or next_turn",
            })?;
    let delivery = match delivery {
        "prompt" => AttentionDelivery::Prompt,
        "next_turn" => AttentionDelivery::NextTurn,
        _ => {
            return Err(CodexQueueError::MalformedAttentionMetadata {
                reason: "attention_delivery must be prompt or next_turn",
            });
        }
    };
    let record_id =
        record_id
            .and_then(Value::as_str)
            .ok_or(CodexQueueError::MalformedAttentionMetadata {
                reason: "attention_record must be a non-empty bounded string",
            })?;
    if record_id.trim().is_empty() || record_id.len() > MAX_ATTENTION_RECORD_ID_BYTES {
        return Err(CodexQueueError::MalformedAttentionMetadata {
            reason: "attention_record must be a non-empty bounded string",
        });
    }

    Ok(Some(AttentionScheduling {
        delivery,
        record_id: record_id.into(),
    }))
}

/// Renames the pre-v2 structured projection on queued notifications.
///
/// Existing v2 citations keep their order. Any distinct legacy citations are
/// appended, so a mixed-version payload loses neither representation while the
/// retired field name disappears from consumer-visible delivery.
fn migrate_legacy_evidence_projection(payload: &mut Value) -> bool {
    let Some(meta) = payload
        .pointer_mut("/params/meta")
        .and_then(Value::as_object_mut)
    else {
        return false;
    };
    let Some(legacy) = meta.remove("evidence") else {
        return false;
    };

    match meta.entry("citation_locators") {
        serde_json::map::Entry::Vacant(entry) => {
            entry.insert(legacy);
        }
        serde_json::map::Entry::Occupied(mut entry) => {
            if let (Some(current), Some(legacy)) =
                (entry.get_mut().as_array_mut(), legacy.as_array())
            {
                for citation in legacy {
                    if !current.contains(citation) {
                        current.push(citation.clone());
                    }
                }
            }
        }
    }
    true
}

pub fn timeout_response() -> Value {
    json!({ "event": null, "timed_out": true })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_path(dir: &TempDir) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap()
    }

    fn message(id: &str, content: &str) -> Value {
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/claude/channel",
            "params": {
                "content": content,
                "meta": { "message_id": id, "chat_id": "123" }
            }
        })
    }

    fn teams_key(activity_id: &str) -> ProviderEventKey {
        ProviderEventKey::teams("tenant-a", "bot-a", "conversation-a", activity_id).unwrap()
    }

    fn tool_result(response: Value) -> Value {
        let text = response["content"][0]["text"]
            .as_str()
            .expect("MCP tool result contains JSON text");
        serde_json::from_str(text).expect("MCP tool result text is JSON")
    }

    fn managed_message(id: &str, record_id: &str, delivery: &str, content: &str) -> Value {
        let mut notification = message(id, content);
        notification["params"]["meta"]["attention_delivery"] = json!(delivery);
        notification["params"]["meta"]["attention_record"] = json!(record_id);
        notification
    }

    async fn primary_consumer(queue: &CodexEventQueue) -> ConsumerId {
        queue
            .register_consumer("test thread".to_owned(), DEFAULT_CONSUMER_TTL, true, true)
            .await
            .unwrap()
            .consumer_id
    }

    #[tokio::test]
    async fn durable_payload_migrates_legacy_evidence_projection_to_citations() {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).unwrap();
        let consumer = primary_consumer(&queue).await;
        let mut payload = message("1", "claim [🔍=v1:AAAAAAAAAAw]");
        payload["params"]["meta"]["evidence"] = json!([{
            "locator": "v1:AAAAAAAAAAw",
            "author_id": "300"
        }]);

        assert!(queue.enqueue(payload).await.unwrap());
        drop(queue);

        let reloaded = CodexEventQueue::load(&path).unwrap();
        let leased = reloaded
            .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
            .await
            .unwrap()
            .expect("leased event");
        assert!(leased.event["params"]["meta"].get("evidence").is_none());
        assert_eq!(
            leased.event["params"]["meta"]["citation_locators"],
            json!([{
                "locator": "v1:AAAAAAAAAAw",
                "author_id": "300"
            }])
        );
        assert!(
            leased.event["params"]["meta"]
                .get("claim_locators")
                .is_none()
        );

        drop(reloaded);
        let persisted: Value =
            serde_json::from_slice(&std::fs::read(path.join(INBOX_FILE_NAME)).unwrap()).unwrap();
        assert!(
            persisted["entries"][0]["payload"]["params"]["meta"]
                .get("evidence")
                .is_none()
        );
        assert_eq!(
            persisted["entries"][0]["payload"]["params"]["meta"]["citation_locators"],
            leased.event["params"]["meta"]["citation_locators"]
        );
    }

    #[test]
    fn legacy_evidence_projection_merges_without_overwriting_v2_citations() {
        let mut payload = message("1", "mixed");
        payload["params"]["meta"]["citation_locators"] = json!([
            { "locator": "v2:citation:AAAAAAAAAAI", "author_id": "300" },
            { "locator": "v1:AAAAAAAAAAw", "author_id": "300" }
        ]);
        payload["params"]["meta"]["evidence"] = json!([
            { "locator": "v1:AAAAAAAAAAw", "author_id": "300" },
            { "locator": "v1:AAAAAAAAAAQ", "author_id": "300" }
        ]);

        assert!(migrate_legacy_evidence_projection(&mut payload));
        assert!(payload["params"]["meta"].get("evidence").is_none());
        assert_eq!(
            payload["params"]["meta"]["citation_locators"],
            json!([
                { "locator": "v2:citation:AAAAAAAAAAI", "author_id": "300" },
                { "locator": "v1:AAAAAAAAAAw", "author_id": "300" },
                { "locator": "v1:AAAAAAAAAAQ", "author_id": "300" }
            ])
        );
    }

    #[tokio::test]
    async fn lease_ack_removes_event_durably() {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).unwrap();
        let consumer = primary_consumer(&queue).await;
        queue.enqueue(message("1", "hello")).await.unwrap();
        let event = queue
            .next_event(&consumer, Duration::ZERO, Duration::from_secs(60))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.event["params"]["content"], "hello");
        queue
            .acknowledge(&consumer, &event.delivery_token)
            .await
            .unwrap();
        assert_eq!(queue.status().await.queued, 0);
    }

    #[tokio::test]
    async fn lease_and_ack_refresh_the_registered_consumer_ttl() {
        let dir = TempDir::new().unwrap();
        let queue = CodexEventQueue::load(&temp_path(&dir)).unwrap();
        let registered_at = Utc::now();
        let ttl = Duration::from_secs(60);
        let consumer = queue
            .inbox
            .lock()
            .await
            .register_consumer("short lived".to_owned(), registered_at, ttl, true, true)
            .unwrap()
            .consumer_id;
        queue.enqueue(message("1", "hello")).await.unwrap();

        let leased_at = registered_at + TimeDelta::seconds(30);
        let event = queue
            .inbox
            .lock()
            .await
            .lease_next(&consumer, leased_at, DEFAULT_LEASE, None)
            .unwrap()
            .event
            .unwrap();
        let lease_refresh = queue
            .inbox
            .lock()
            .await
            .state
            .consumers
            .iter()
            .find(|registration| registration.id == consumer)
            .unwrap()
            .expires_at;
        assert_eq!(lease_refresh, leased_at + TimeDelta::seconds(60));

        let acknowledged_at = registered_at + TimeDelta::seconds(40);
        queue
            .inbox
            .lock()
            .await
            .acknowledge(&consumer, &event.delivery_token, acknowledged_at, false)
            .unwrap();
        let ack_refresh = queue
            .inbox
            .lock()
            .await
            .state
            .consumers
            .iter()
            .find(|registration| registration.id == consumer)
            .unwrap()
            .expires_at;
        assert_eq!(ack_refresh, acknowledged_at + TimeDelta::seconds(60));
    }

    #[tokio::test]
    async fn expired_lease_is_redelivered_with_new_token() {
        let dir = TempDir::new().unwrap();
        let queue = CodexEventQueue::load(&temp_path(&dir)).unwrap();
        let consumer = primary_consumer(&queue).await;
        queue.enqueue(message("1", "hello")).await.unwrap();
        let first = queue
            .next_event(&consumer, Duration::ZERO, Duration::from_millis(1))
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        let second = queue
            .next_event(&consumer, Duration::ZERO, Duration::from_secs(60))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.event_id, second.event_id);
        assert_ne!(first.delivery_token, second.delivery_token);
    }

    #[tokio::test]
    async fn duplicate_discord_message_is_not_enqueued_twice() {
        let dir = TempDir::new().unwrap();
        let queue = CodexEventQueue::load(&temp_path(&dir)).unwrap();
        primary_consumer(&queue).await;
        assert!(queue.enqueue(message("1", "first")).await.unwrap());
        assert!(!queue.enqueue(message("1", "duplicate")).await.unwrap());
        assert_eq!(queue.status().await.queued, 1);
    }

    #[tokio::test]
    async fn durable_lifecycle_events_are_not_deduplicated_as_duplicate_creates() {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).unwrap();
        let consumer = primary_consumer(&queue).await;
        let mut edit = message("1", "edited");
        edit["params"]["meta"]["type"] = json!("message_edit");
        let mut delete = message("1", "deleted");
        delete["params"]["meta"]["type"] = json!("message_delete");

        assert!(queue.enqueue(message("1", "original")).await.unwrap());
        assert!(queue.enqueue(edit).await.unwrap());
        assert!(queue.enqueue(delete).await.unwrap());
        assert!(
            !queue
                .enqueue(message("1", "duplicate create"))
                .await
                .unwrap()
        );
        drop(queue);

        let queue = CodexEventQueue::load(&path).unwrap();
        let mut delivered_kinds = Vec::new();
        while let Some(event) = queue
            .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
            .await
            .unwrap()
        {
            delivered_kinds.push(
                event
                    .event
                    .pointer("/params/meta/type")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            );
            queue
                .acknowledge(&consumer, &event.delivery_token)
                .await
                .unwrap();
        }

        assert_eq!(
            delivered_kinds,
            [
                None,
                Some("message_edit".to_owned()),
                Some("message_delete".to_owned()),
            ]
        );
    }

    #[test]
    fn second_process_owner_is_rejected() {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let _first = CodexEventQueue::load(&path).unwrap();
        let second = CodexEventQueue::load(&path);
        assert!(matches!(second, Err(CodexQueueError::InboxLocked { .. })));
    }

    #[tokio::test]
    async fn existing_v1_inbox_is_backward_compatible() {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        std::fs::write(
            path.join(INBOX_FILE_NAME),
            serde_json::to_vec(&json!({
                "next_id": 2,
                "entries": [{ "id": 1, "payload": message("1", "legacy") }]
            }))
            .unwrap(),
        )
        .unwrap();
        let queue = CodexEventQueue::load(&path).unwrap();
        queue
            .bind_live_thread(Some(CodexThreadId::parse("legacy-thread").unwrap()))
            .await
            .unwrap();
        drop(queue);

        let persisted: Value =
            serde_json::from_slice(&std::fs::read(path.join(INBOX_FILE_NAME)).unwrap()).unwrap();
        assert_eq!(persisted["entries"][0]["id"], 1);
        assert_eq!(persisted["entries"][0]["discord_message_id"], "1");
        assert_eq!(persisted["live_thread_id"], "legacy-thread");

        let reloaded = CodexEventQueue::load(&path).unwrap();
        let inbox = reloaded.inbox.lock().await;
        assert_eq!(inbox.state.entries[0].id, EventId::new(1));
        assert_eq!(
            inbox.state.entries[0].discord_message_id,
            Some(DiscordMessageId(MessageId::new(1)))
        );
        assert_eq!(
            inbox
                .state
                .live_thread_id
                .as_ref()
                .map(CodexThreadId::as_str),
            Some("legacy-thread")
        );
    }

    #[tokio::test]
    async fn legacy_consumer_without_stored_ttl_uses_the_default() {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let now = Utc::now();
        std::fs::write(
            path.join(INBOX_FILE_NAME),
            serde_json::to_vec(&json!({
                "next_id": 1,
                "primary_consumer": "codex-consumer-0",
                "consumers": [{
                    "id": "codex-consumer-0",
                    "label": "legacy consumer",
                    "expires_at": now + TimeDelta::hours(1)
                }],
                "entries": []
            }))
            .unwrap(),
        )
        .unwrap();

        let queue = CodexEventQueue::load(&path).unwrap();
        let consumer = ConsumerId::parse("codex-consumer-0").unwrap();
        let refreshed_at = now + TimeDelta::minutes(1);
        let mut inbox = queue.inbox.lock().await;
        inbox.touch_consumer(&consumer, refreshed_at).unwrap();
        let registration = inbox
            .state
            .consumers
            .iter()
            .find(|registration| registration.id == consumer)
            .unwrap();
        assert_eq!(registration.ttl_seconds, DEFAULT_CONSUMER_TTL.as_secs());
        assert_eq!(
            registration.expires_at,
            refreshed_at + duration_delta(DEFAULT_CONSUMER_TTL)
        );
    }

    #[tokio::test]
    async fn registered_consumer_ttl_survives_restart() {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let registered_at = Utc::now();
        let consumer = {
            let queue = CodexEventQueue::load(&path).unwrap();
            queue
                .inbox
                .lock()
                .await
                .register_consumer(
                    "short lived".to_owned(),
                    registered_at,
                    Duration::from_secs(60),
                    true,
                    true,
                )
                .unwrap()
                .consumer_id
        };

        let queue = CodexEventQueue::load(&path).unwrap();
        let refreshed_at = registered_at + TimeDelta::seconds(30);
        let mut inbox = queue.inbox.lock().await;
        inbox.touch_consumer(&consumer, refreshed_at).unwrap();
        let registration = inbox
            .state
            .consumers
            .iter()
            .find(|registration| registration.id == consumer)
            .unwrap();
        assert_eq!(registration.ttl_seconds, 60);
        assert_eq!(
            registration.expires_at,
            refreshed_at + TimeDelta::seconds(60)
        );
    }

    #[tokio::test]
    async fn handoff_moves_future_and_pending_events_to_new_consumer() {
        let dir = TempDir::new().unwrap();
        let queue = CodexEventQueue::load(&temp_path(&dir)).unwrap();
        let first = primary_consumer(&queue).await;
        queue.enqueue(message("1", "pending")).await.unwrap();
        let second = queue
            .register_consumer(
                "replacement thread".to_owned(),
                DEFAULT_CONSUMER_TTL,
                false,
                false,
            )
            .await
            .unwrap()
            .consumer_id;
        let handoff = queue.handoff(&first, &second, true).await.unwrap();
        assert_eq!(handoff.moved_pending, 1);
        assert!(
            queue
                .next_event(&first, Duration::ZERO, Duration::from_secs(60))
                .await
                .unwrap()
                .is_none()
        );
        let event = queue
            .next_event(&second, Duration::ZERO, Duration::from_secs(60))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.event["params"]["content"], "pending");
        queue.enqueue(message("2", "future")).await.unwrap();
        queue
            .acknowledge(&second, &event.delivery_token)
            .await
            .unwrap();
        let future = queue
            .next_event(&second, Duration::ZERO, Duration::from_secs(60))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(future.event["params"]["content"], "future");
    }

    #[tokio::test]
    async fn acknowledged_discord_message_remains_deduplicated() {
        let dir = TempDir::new().unwrap();
        let queue = CodexEventQueue::load(&temp_path(&dir)).unwrap();
        let consumer = primary_consumer(&queue).await;
        queue.enqueue(message("1", "first")).await.unwrap();
        let event = queue
            .next_event(&consumer, Duration::ZERO, Duration::from_secs(60))
            .await
            .unwrap()
            .unwrap();
        queue
            .acknowledge(&consumer, &event.delivery_token)
            .await
            .unwrap();
        assert!(!queue.enqueue(message("1", "duplicate")).await.unwrap());
    }

    #[tokio::test]
    async fn registered_consumer_can_claim_after_primary_expires() {
        let dir = TempDir::new().unwrap();
        let queue = CodexEventQueue::load(&temp_path(&dir)).unwrap();
        let first = queue
            .register_consumer(
                "short lived".to_owned(),
                Duration::from_secs(60),
                true,
                true,
            )
            .await
            .unwrap()
            .consumer_id;
        let second = queue
            .register_consumer(
                "waiting thread".to_owned(),
                Duration::from_secs(60 * 60),
                false,
                false,
            )
            .await
            .unwrap()
            .consumer_id;
        queue.enqueue(message("1", "orphaned")).await.unwrap();

        let claimed = queue
            .inbox
            .lock()
            .await
            .claim_primary(&second, Utc::now() + TimeDelta::minutes(2), true)
            .unwrap();
        assert_eq!(claimed, 1);
        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn live_consumer_is_reused_after_process_restart() {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).unwrap();

        let first = queue.register_live_consumer().await.unwrap();
        drop(queue);
        let reloaded = CodexEventQueue::load(&path).unwrap();
        let resumed = reloaded.register_live_consumer().await.unwrap();

        assert_eq!(resumed, first);
        let status = reloaded.status().await;
        assert_eq!(status.primary_consumer.as_ref(), Some(&first));
        assert_eq!(status.consumers.len(), 1);
    }

    #[tokio::test]
    async fn expired_live_consumer_keeps_routed_events_after_long_outage() {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).unwrap();
        let first = queue.register_live_consumer().await.unwrap();
        let thread = CodexThreadId::parse("thread-outage").unwrap();
        queue.bind_live_thread(Some(thread.clone())).await.unwrap();
        queue.enqueue(message("1", "still pending")).await.unwrap();
        {
            let mut inbox = queue.inbox.lock().await;
            inbox
                .state
                .consumers
                .iter_mut()
                .find(|consumer| consumer.id == first)
                .unwrap()
                .expires_at = Utc::now() - TimeDelta::seconds(1);
            inbox.persist().unwrap();
        }
        drop(queue);
        let reloaded = CodexEventQueue::load(&path).unwrap();
        let resumed = reloaded.register_live_consumer().await.unwrap();

        assert_eq!(resumed, first);
        assert!(
            reloaded
                .next_live_event(&resumed, &thread, Duration::ZERO, DEFAULT_LEASE)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn live_rebind_does_not_replay_unleased_event_into_new_thread() {
        let dir = TempDir::new().unwrap();
        let queue = CodexEventQueue::load(&temp_path(&dir)).unwrap();
        queue
            .bind_live_thread(Some(CodexThreadId::parse("thread-a").unwrap()))
            .await
            .unwrap();
        let consumer = queue.register_live_consumer().await.unwrap();
        queue.enqueue(message("1", "for a")).await.unwrap();

        queue
            .bind_live_thread(Some(CodexThreadId::parse("thread-b").unwrap()))
            .await
            .unwrap();
        queue.enqueue(message("2", "for b")).await.unwrap();

        let for_b = queue
            .next_live_event(
                &consumer,
                &CodexThreadId::parse("thread-b").unwrap(),
                Duration::ZERO,
                DEFAULT_LEASE,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(for_b.event["params"]["content"], "for b");
        assert!(
            queue
                .next_live_event(
                    &consumer,
                    &CodexThreadId::parse("thread-a").unwrap(),
                    Duration::ZERO,
                    DEFAULT_LEASE,
                )
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn live_rebind_does_not_replay_leased_event_into_new_thread() {
        let dir = TempDir::new().unwrap();
        let queue = CodexEventQueue::load(&temp_path(&dir)).unwrap();
        queue
            .bind_live_thread(Some(CodexThreadId::parse("thread-a").unwrap()))
            .await
            .unwrap();
        let consumer = queue.register_live_consumer().await.unwrap();
        queue.enqueue(message("1", "leased for a")).await.unwrap();
        let _leased = queue
            .next_live_event(
                &consumer,
                &CodexThreadId::parse("thread-a").unwrap(),
                Duration::ZERO,
                DEFAULT_LEASE,
            )
            .await
            .unwrap()
            .unwrap();

        queue
            .bind_live_thread(Some(CodexThreadId::parse("thread-b").unwrap()))
            .await
            .unwrap();

        assert!(
            queue
                .next_live_event(
                    &consumer,
                    &CodexThreadId::parse("thread-b").unwrap(),
                    Duration::ZERO,
                    DEFAULT_LEASE,
                )
                .await
                .unwrap()
                .is_none()
        );
    }

    async fn binding_server(
        path: &Utf8Path,
        queue: &CodexEventQueue,
    ) -> (
        crate::mcp::server::DioneServer,
        tokio::sync::watch::Receiver<Option<CodexThreadId>>,
    ) {
        let (notification_tx, _notification_rx) = tokio::sync::mpsc::channel(1);
        let (binding_tx, binding_rx) = tokio::sync::watch::channel(None);
        let server = crate::mcp::server::DioneServer::new(
            crate::state::new_state(),
            Arc::new(Mutex::new(crate::queue::AccessQueue::load(path))),
            Arc::new(serenity::http::Http::new("fake")),
            path.to_owned(),
            notification_tx,
            crate::tracing_channel::TraceLevelController::noop(),
            TransportMode::Codex,
            Arc::new(crate::no_rly::consent::ConsentGate::new(path)),
            Arc::new(crate::ingress_ledger::IngressLedger::new()),
        )
        .await
        .with_codex_queue(Some(queue.clone()))
        .with_codex_thread_binding(Some(binding_tx));
        (server, binding_rx)
    }

    async fn dispatch_binding(
        server: &crate::mcp::server::DioneServer,
        thread_id: &str,
    ) -> Result<Value, String> {
        crate::mcp::dispatch::call_tool(
            server,
            "bind_codex_thread",
            json!({ "thread_id": thread_id }),
        )
        .await
    }

    #[tokio::test]
    async fn dispatch_binding_publishes_retained_commit_on_failure() {
        for stage in [
            PersistFailure::BeforeWrite,
            PersistFailure::BeforeRename,
            PersistFailure::AfterRename,
        ] {
            let dir = TempDir::new().unwrap();
            let path = temp_path(&dir);
            let queue = CodexEventQueue::load(&path).unwrap();
            let (server, binding_rx) = binding_server(&path, &queue).await;
            dispatch_binding(&server, "thread-a").await.unwrap();
            let consumer = queue.register_live_consumer().await.unwrap();
            queue.inbox.lock().await.persist_failure = Some(stage);

            let error = dispatch_binding(&server, "thread-b").await.unwrap_err();
            let committed = stage == PersistFailure::AfterRename;
            assert_eq!(error.contains("unconfirmed crash durability"), committed);
            let expected =
                CodexThreadId::parse(if committed { "thread-b" } else { "thread-a" }).unwrap();
            assert_eq!(binding_rx.borrow().as_ref(), Some(&expected));
            assert_eq!(
                queue.inbox.lock().await.state.live_thread_id.as_ref(),
                Some(&expected)
            );
            let disk: InboxState =
                serde_json::from_slice(&std::fs::read(path.join(INBOX_FILE_NAME)).unwrap())
                    .unwrap();
            assert_eq!(disk.live_thread_id.as_ref(), Some(&expected));
            if committed {
                let error = dispatch_binding(&server, "thread-c").await.unwrap_err();
                assert!(error.contains("unconfirmed crash durability"));
                assert_eq!(binding_rx.borrow().as_ref(), Some(&expected));
                assert_eq!(
                    queue.inbox.lock().await.state.live_thread_id.as_ref(),
                    Some(&expected)
                );
            }

            queue.inbox.lock().await.persist_failure = None;
            queue
                .enqueue(message("1", "retained binding"))
                .await
                .unwrap();
            let watched = binding_rx.borrow().clone().unwrap();
            let event = queue
                .next_live_event(&consumer, &watched, Duration::ZERO, DEFAULT_LEASE)
                .await
                .unwrap()
                .expect("delivery without another bind or enqueue");
            assert_eq!(event.event["params"]["content"], "retained binding");
        }
    }

    #[tokio::test]
    async fn dispatch_binding_recovery_barrier_publishes_actual_binding() {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).unwrap();
        let (server, binding_rx) = binding_server(&path, &queue).await;
        dispatch_binding(&server, "thread-a").await.unwrap();
        let consumer = queue.register_live_consumer().await.unwrap();
        queue.inbox.lock().await.persist_failure = Some(PersistFailure::AfterRename);
        assert!(queue.enqueue(message("1", "already queued")).await.is_err());
        let before = std::fs::read(path.join(INBOX_FILE_NAME)).unwrap();

        let error = dispatch_binding(&server, "thread-b").await.unwrap_err();
        assert!(error.contains("unconfirmed crash durability"));
        let expected = CodexThreadId::parse("thread-a").unwrap();
        assert_eq!(binding_rx.borrow().as_ref(), Some(&expected));
        assert_eq!(std::fs::read(path.join(INBOX_FILE_NAME)).unwrap(), before);
        queue.inbox.lock().await.persist_failure = None;
        let watched = binding_rx.borrow().clone().unwrap();
        let event = queue
            .next_live_event(&consumer, &watched, Duration::ZERO, DEFAULT_LEASE)
            .await
            .unwrap()
            .expect("recovery delivers the existing event without another mutation");
        assert_eq!(event.event["params"]["content"], "already queued");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dispatch_binding_concurrent_publication_matches_commit_order() {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).unwrap();
        let (server, binding_rx) = binding_server(&path, &queue).await;
        dispatch_binding(&server, "thread-initial").await.unwrap();
        let server = Arc::new(server);
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        *queue.binding_publish_gate.lock().unwrap() = Some(BindingPublishGate {
            reached: reached_tx,
            release: release_rx,
        });

        let first_server = Arc::clone(&server);
        let first =
            tokio::spawn(async move { dispatch_binding(&first_server, "thread-first").await });
        tokio::time::timeout(Duration::from_secs(2), reached_rx)
            .await
            .expect("first bind reaches committed but unpublished state")
            .unwrap();
        let first_id = CodexThreadId::parse("thread-first").unwrap();
        assert!(queue.inbox.try_lock().is_err());
        assert_eq!(
            binding_rx.borrow().as_ref(),
            Some(&CodexThreadId::parse("thread-initial").unwrap())
        );
        let committed: InboxState =
            serde_json::from_slice(&std::fs::read(path.join(INBOX_FILE_NAME)).unwrap()).unwrap();
        assert_eq!(committed.live_thread_id.as_ref(), Some(&first_id));

        let second_server = Arc::clone(&server);
        let mut second =
            tokio::spawn(async move { dispatch_binding(&second_server, "thread-second").await });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut second)
                .await
                .is_err(),
            "second bind must not complete before first watch publication"
        );
        release_tx.send(()).unwrap();
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        let second_id = CodexThreadId::parse("thread-second").unwrap();
        assert_eq!(binding_rx.borrow().as_ref(), Some(&second_id));
        let disk: InboxState =
            serde_json::from_slice(&std::fs::read(path.join(INBOX_FILE_NAME)).unwrap()).unwrap();
        assert_eq!(disk.live_thread_id.as_ref(), Some(&second_id));
    }

    #[tokio::test]
    async fn post_rename_enqueue_keeps_memory_consistent_with_disk() {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).unwrap();
        queue.inbox.lock().await.persist_failure = Some(PersistFailure::AfterRename);
        assert!(queue.enqueue(message("1", "committed")).await.is_err());
        let persisted: Value =
            serde_json::from_slice(&std::fs::read(path.join(INBOX_FILE_NAME)).unwrap()).unwrap();
        assert_eq!(persisted["entries"].as_array().unwrap().len(), 1);
        assert_eq!(queue.status().await.queued, 1);
        queue.inbox.lock().await.persist_failure = None;
        assert!(queue.enqueue(message("2", "later")).await.unwrap());
        drop(queue);
        let queue = CodexEventQueue::load(&path).unwrap();
        assert_eq!(queue.status().await.queued, 2);
        assert!(!queue.enqueue(message("1", "duplicate")).await.unwrap());
    }

    #[tokio::test]
    async fn uncertain_lease_does_not_hide_first_event_behind_second() {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).unwrap();
        let consumer = primary_consumer(&queue).await;
        queue.enqueue(message("1", "first")).await.unwrap();
        queue.enqueue(message("2", "second")).await.unwrap();

        queue.inbox.lock().await.persist_failure = Some(PersistFailure::AfterRename);
        let first_attempt = queue
            .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
            .await;
        queue.inbox.lock().await.persist_failure = None;
        let first = match first_attempt {
            Ok(Some(event)) => event,
            Err(CodexQueueError::InboxDurabilityUncertain { .. }) => queue
                .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
                .await
                .unwrap()
                .expect("first event after directory sync recovery"),
            other => panic!("unexpected first lease outcome: {other:?}"),
        };
        assert_eq!(
            first.event["params"]["content"], "first",
            "a retained uncertain lease must not let a later event pass first"
        );

        let visible: InboxState =
            serde_json::from_slice(&std::fs::read(path.join(INBOX_FILE_NAME)).unwrap()).unwrap();
        let visible_first = visible.entries.front().unwrap();
        assert_eq!(
            visible_first.lease.as_ref().unwrap().token,
            first.delivery_token
        );
        drop(queue);
        let restarted = CodexEventQueue::load(&path).unwrap();
        restarted
            .acknowledge(&consumer, &first.delivery_token)
            .await
            .unwrap();
        let second = restarted
            .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
            .await
            .unwrap()
            .expect("second event after first is settled");
        assert_eq!(second.event["params"]["content"], "second");
    }

    #[tokio::test]
    async fn pending_directory_sync_rejects_lease_before_mutation() {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).unwrap();
        let consumer = primary_consumer(&queue).await;
        queue.enqueue(message("1", "first")).await.unwrap();
        queue.inbox.lock().await.persist_failure = Some(PersistFailure::AfterRename);
        assert!(matches!(
            queue.enqueue(message("2", "second")).await,
            Err(CodexQueueError::InboxDurabilityUncertain { .. })
        ));
        let before = std::fs::read(path.join(INBOX_FILE_NAME)).unwrap();
        let generation = queue.inbox.lock().await.state.next_lease_generation;
        assert!(matches!(
            queue
                .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
                .await,
            Err(CodexQueueError::InboxDurabilityUncertain { .. })
        ));
        let inbox = queue.inbox.lock().await;
        assert_eq!(inbox.state.next_lease_generation, generation);
        assert!(
            inbox
                .state
                .entries
                .iter()
                .all(|event| event.lease.is_none())
        );
        drop(inbox);
        assert_eq!(std::fs::read(path.join(INBOX_FILE_NAME)).unwrap(), before);
        queue.inbox.lock().await.persist_failure = None;
        let first = queue
            .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.event["params"]["content"], "first");
    }

    #[tokio::test]
    async fn enqueue_persistence_failure_outcomes() {
        for stage in [
            PersistFailure::BeforeWrite,
            PersistFailure::BeforeRename,
            PersistFailure::AfterRename,
        ] {
            let dir = TempDir::new().unwrap();
            let path = temp_path(&dir);
            let queue = CodexEventQueue::load(&path).unwrap();
            let consumer = primary_consumer(&queue).await;
            queue.enqueue(message("1", "existing")).await.unwrap();
            let before = std::fs::read(path.join(INBOX_FILE_NAME)).unwrap();
            queue.inbox.lock().await.persist_failure = Some(stage);
            let result = queue.enqueue(message("2", "candidate")).await;
            let committed = stage == PersistFailure::AfterRename;
            if committed {
                assert!(matches!(
                    result,
                    Err(CodexQueueError::InboxDurabilityUncertain { .. })
                ));
            } else {
                assert!(matches!(result, Err(CodexQueueError::InboxIo { .. })));
            }
            assert_eq!(queue.status().await.queued, if committed { 2 } else { 1 });
            let visible = std::fs::read(path.join(INBOX_FILE_NAME)).unwrap();
            let inbox = queue.inbox.lock().await;
            assert_eq!(
                serde_json::to_value(&inbox.state).unwrap(),
                serde_json::from_slice::<Value>(&visible).unwrap()
            );
            assert_eq!(
                inbox
                    .message_ids
                    .contains(&DiscordMessageId(MessageId::new(2))),
                committed
            );
            drop(inbox);
            if committed {
                assert!(matches!(
                    queue.enqueue(message("2", "duplicate")).await,
                    Err(CodexQueueError::InboxDurabilityUncertain { .. })
                ));
                assert!(matches!(
                    queue.enqueue(message("3", "blocked")).await,
                    Err(CodexQueueError::InboxDurabilityUncertain { .. })
                ));
                assert!(matches!(
                    queue
                        .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
                        .await,
                    Err(CodexQueueError::InboxDurabilityUncertain { .. })
                ));
                assert_eq!(std::fs::read(path.join(INBOX_FILE_NAME)).unwrap(), visible);
                assert_eq!(queue.status().await.queued, 2);
            } else {
                assert_eq!(visible, before);
            }
            drop(queue);
            let queue = CodexEventQueue::load(&path).unwrap();
            assert_eq!(queue.status().await.queued, if committed { 2 } else { 1 });
            if committed {
                queue.inbox.lock().await.persist_failure = Some(PersistFailure::AfterRename);
                assert!(matches!(
                    queue.enqueue(message("2", "duplicate after reopen")).await,
                    Err(CodexQueueError::InboxDurabilityUncertain { .. })
                ));
                queue.inbox.lock().await.persist_failure = None;
            }
            assert_eq!(
                queue.enqueue(message("2", "retry")).await.unwrap(),
                !committed
            );
            assert!(queue.enqueue(message("3", "later")).await.unwrap());
            drop(queue);
            let queue = CodexEventQueue::load(&path).unwrap();
            assert_eq!(queue.status().await.queued, 3);
            for id in ["1", "2", "3"] {
                assert!(!queue.enqueue(message(id, "duplicate")).await.unwrap());
            }
        }
    }

    #[tokio::test]
    async fn acknowledgement_persistence_failure_outcomes() {
        for stage in [
            PersistFailure::BeforeWrite,
            PersistFailure::BeforeRename,
            PersistFailure::AfterRename,
        ] {
            let dir = TempDir::new().unwrap();
            let path = temp_path(&dir);
            let queue = CodexEventQueue::load(&path).unwrap();
            let consumer = primary_consumer(&queue).await;
            queue.enqueue(message("1", "handled")).await.unwrap();
            let event = queue
                .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
                .await
                .unwrap()
                .unwrap();
            let before = std::fs::read(path.join(INBOX_FILE_NAME)).unwrap();
            queue.inbox.lock().await.persist_failure = Some(stage);
            let result = queue.acknowledge(&consumer, &event.delivery_token).await;
            let committed = stage == PersistFailure::AfterRename;
            if committed {
                assert!(matches!(
                    result,
                    Err(CodexQueueError::InboxDurabilityUncertain { .. })
                ));
            } else {
                assert!(matches!(result, Err(CodexQueueError::InboxIo { .. })));
            }
            assert_eq!(queue.status().await.queued, if committed { 0 } else { 1 });
            let visible = std::fs::read(path.join(INBOX_FILE_NAME)).unwrap();
            let inbox = queue.inbox.lock().await;
            assert_eq!(
                serde_json::to_value(&inbox.state).unwrap(),
                serde_json::from_slice::<Value>(&visible).unwrap()
            );
            assert_eq!(
                inbox
                    .processed_message_ids
                    .contains(&DiscordMessageId(MessageId::new(1))),
                committed
            );
            assert_eq!(
                inbox
                    .message_ids
                    .contains(&DiscordMessageId(MessageId::new(1))),
                !committed
            );
            drop(inbox);
            if committed {
                assert!(matches!(
                    queue.acknowledge(&consumer, &event.delivery_token).await,
                    Err(CodexQueueError::InboxDurabilityUncertain { .. })
                ));
                assert!(matches!(
                    queue.enqueue(message("1", "duplicate")).await,
                    Err(CodexQueueError::InboxDurabilityUncertain { .. })
                ));
                assert!(matches!(
                    queue.enqueue(message("2", "blocked")).await,
                    Err(CodexQueueError::InboxDurabilityUncertain { .. })
                ));
                assert_eq!(std::fs::read(path.join(INBOX_FILE_NAME)).unwrap(), visible);
            } else {
                assert_eq!(visible, before);
            }
            drop(queue);
            let queue = CodexEventQueue::load(&path).unwrap();
            assert_eq!(queue.status().await.queued, if committed { 0 } else { 1 });
            let retry = queue.acknowledge(&consumer, &event.delivery_token).await;
            if committed {
                assert!(matches!(retry, Err(CodexQueueError::UnknownDeliveryToken)));
            } else {
                retry.unwrap();
            }
            assert!(!queue.enqueue(message("1", "duplicate")).await.unwrap());
            assert!(queue.enqueue(message("2", "later")).await.unwrap());
            drop(queue);
            let queue = CodexEventQueue::load(&path).unwrap();
            assert_eq!(queue.status().await.queued, 1);
            assert!(!queue.enqueue(message("1", "duplicate")).await.unwrap());
        }
    }

    #[tokio::test]
    async fn live_enqueue_requires_exact_resident_and_preserves_failed_route_state() {
        let dir = TempDir::new().unwrap();
        let queue = CodexEventQueue::load(&temp_path(&dir)).unwrap();
        assert!(matches!(
            queue
                .enqueue_live(message("1", "no resident"), teams_key("1"))
                .await,
            Err(LiveQueueError::ResidentUnavailable)
        ));
        let consumer = queue.register_live_consumer().await.unwrap();
        assert!(matches!(
            queue
                .enqueue_live(message("2", "no thread"), teams_key("2"))
                .await,
            Err(LiveQueueError::ResidentUnavailable)
        ));
        assert_eq!(queue.status().await.queued, 0);

        let thread = CodexThreadId::parse("thread-a").unwrap();
        queue.bind_live_thread(Some(thread.clone())).await.unwrap();
        assert!(matches!(
            queue
                .enqueue_live(message("3", "routed"), teams_key("3"))
                .await,
            Ok(LiveEnqueueReceipt::Committed)
        ));
        let event = queue
            .next_live_event(&consumer, &thread, Duration::ZERO, DEFAULT_LEASE)
            .await
            .unwrap()
            .expect("exact resident event");
        assert_eq!(event.event["params"]["content"], "routed");

        {
            let mut inbox = queue.inbox.lock().await;
            inbox.state.consumers[0].expires_at = Utc::now() - TimeDelta::seconds(1);
        }
        assert!(matches!(
            queue
                .enqueue_live(message("4", "expired route"), teams_key("4"))
                .await,
            Err(LiveQueueError::ResidentUnavailable)
        ));
        let inbox = queue.inbox.lock().await;
        assert_eq!(inbox.state.primary_consumer.as_ref(), Some(&consumer));
        assert_eq!(inbox.state.consumers.len(), 1);
    }

    #[tokio::test]
    async fn dispatch_forged_live_label_cannot_admit_or_receive_teams_event() {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).unwrap();
        let (server, binding_rx) = binding_server(&path, &queue).await;
        dispatch_binding(&server, "thread-a").await.unwrap();
        let forged = crate::mcp::dispatch::call_tool(
            &server,
            "register_event_consumer",
            json!({
                "label": LIVE_CONSUMER_LABEL,
                "make_primary": true,
                "claim_unassigned": true
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(forged, CodexQueueError::InvalidConsumerId.to_string());
        assert!(queue.status().await.primary_consumer.is_none());

        let resident = queue.register_live_consumer().await.unwrap();
        assert!(matches!(
            queue
                .enqueue_live(message("1", "for resident"), teams_key("activity-1"))
                .await,
            Ok(LiveEnqueueReceipt::Committed)
        ));
        let thread = binding_rx.borrow().clone().unwrap();
        let event = queue
            .next_live_event(&resident, &thread, Duration::ZERO, DEFAULT_LEASE)
            .await
            .unwrap()
            .expect("only dedicated live consumer receives Teams event");
        assert_eq!(event.event["params"]["content"], "for resident");
    }

    #[tokio::test]
    async fn dispatch_public_poll_cannot_lease_status_disclosed_live_consumer() {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).unwrap();
        let (server, binding_rx) = binding_server(&path, &queue).await;
        dispatch_binding(&server, "thread-a").await.unwrap();
        let resident = queue.register_live_consumer().await.unwrap();
        queue
            .enqueue_live(
                message("1", "private Teams activity"),
                teams_key("activity-1"),
            )
            .await
            .unwrap();

        let status = crate::mcp::dispatch::call_tool(&server, "event_queue_status", json!({}))
            .await
            .unwrap();
        let status = tool_result(status);
        let disclosed = status["primary_consumer"].as_str().unwrap();
        assert_eq!(disclosed, resident.0.as_str());
        let public_poll = crate::mcp::dispatch::call_tool(
            &server,
            "next_event",
            json!({ "consumer_id": disclosed, "wait_seconds": 0 }),
        )
        .await;
        assert!(
            public_poll.is_err(),
            "public poll leased live event: {public_poll:?}"
        );

        let thread = binding_rx.borrow().clone().unwrap();
        let event = queue
            .next_live_event(&resident, &thread, Duration::ZERO, DEFAULT_LEASE)
            .await
            .unwrap()
            .expect("resident still receives private activity");
        assert_eq!(event.event["params"]["content"], "private Teams activity");
    }

    #[tokio::test]
    async fn dispatch_public_ack_cannot_remove_resident_lease() {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).unwrap();
        let (server, binding_rx) = binding_server(&path, &queue).await;
        dispatch_binding(&server, "thread-a").await.unwrap();
        let resident = queue.register_live_consumer().await.unwrap();
        queue
            .enqueue_live(
                message("1", "private Teams activity"),
                teams_key("activity-1"),
            )
            .await
            .unwrap();
        let thread = binding_rx.borrow().clone().unwrap();
        let leased = queue
            .next_live_event(&resident, &thread, Duration::ZERO, DEFAULT_LEASE)
            .await
            .unwrap()
            .unwrap();

        let public_ack = crate::mcp::dispatch::call_tool(
            &server,
            "ack_event",
            json!({
                "consumer_id": resident.0.as_str(),
                "delivery_token": leased.delivery_token.0.as_str()
            }),
        )
        .await;
        assert!(
            public_ack.is_err(),
            "public ack removed resident lease: {public_ack:?}"
        );
        assert_eq!(queue.status().await.queued, 1);
        assert_eq!(queue.status().await.leased, 1);
    }

    #[tokio::test]
    async fn dispatch_public_handoff_cannot_move_live_primary_or_pending_activity() {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).unwrap();
        let (server, binding_rx) = binding_server(&path, &queue).await;
        dispatch_binding(&server, "thread-a").await.unwrap();
        let resident = queue.register_live_consumer().await.unwrap();
        queue
            .enqueue_live(
                message("1", "private Teams activity"),
                teams_key("activity-1"),
            )
            .await
            .unwrap();
        let pull = crate::mcp::dispatch::call_tool(
            &server,
            "register_event_consumer",
            json!({ "label": "forged pull" }),
        )
        .await
        .unwrap();
        let pull = tool_result(pull);
        let pull_id = pull["consumer_id"].as_str().unwrap();

        let handoff = crate::mcp::dispatch::call_tool(
            &server,
            "handoff_event_consumer",
            json!({
                "from_consumer_id": resident.0.as_str(),
                "to_consumer_id": pull_id,
                "move_pending": true
            }),
        )
        .await;
        assert!(
            handoff.is_err(),
            "public handoff moved live route: {handoff:?}"
        );
        assert_eq!(
            queue.status().await.primary_consumer.as_ref(),
            Some(&resident)
        );

        let thread = binding_rx.borrow().clone().unwrap();
        let event = queue
            .next_live_event(&resident, &thread, Duration::ZERO, DEFAULT_LEASE)
            .await
            .unwrap()
            .expect("live event remains with resident");
        assert_eq!(event.event["params"]["content"], "private Teams activity");
    }

    #[tokio::test]
    async fn dispatch_public_claim_cannot_take_expired_live_activity_after_reload() {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).unwrap();
        let (server, _binding_rx) = binding_server(&path, &queue).await;
        dispatch_binding(&server, "thread-a").await.unwrap();
        queue.register_live_consumer().await.unwrap();
        queue
            .enqueue_live(
                message("1", "private Teams activity"),
                teams_key("activity-1"),
            )
            .await
            .unwrap();
        {
            let mut inbox = queue.inbox.lock().await;
            inbox.state.consumers[0].expires_at = Utc::now() - TimeDelta::seconds(1);
            inbox.persist().unwrap();
        }
        drop(server);
        drop(queue);

        let reloaded = CodexEventQueue::load(&path).unwrap();
        let (server, _) = binding_server(&path, &reloaded).await;
        let pull = crate::mcp::dispatch::call_tool(
            &server,
            "register_event_consumer",
            json!({ "label": "ordinary pull" }),
        )
        .await
        .unwrap();
        let pull = tool_result(pull);
        let pull_id = pull["consumer_id"].as_str().unwrap();
        let claimed = crate::mcp::dispatch::call_tool(
            &server,
            "claim_event_consumer",
            json!({ "consumer_id": pull_id, "claim_orphaned": true }),
        )
        .await
        .unwrap();
        let claimed = tool_result(claimed);
        assert_eq!(claimed["claimed_events"], 0);
        let poll = crate::mcp::dispatch::call_tool(
            &server,
            "next_event",
            json!({ "consumer_id": pull_id, "wait_seconds": 0 }),
        )
        .await
        .unwrap();
        let poll = tool_result(poll);
        assert!(poll["event"].is_null());
        assert_eq!(reloaded.status().await.queued, 1);
    }

    #[tokio::test]
    async fn dispatch_public_registration_cannot_claim_unassigned_live_activity() {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).unwrap();
        let (server, _binding_rx) = binding_server(&path, &queue).await;
        dispatch_binding(&server, "thread-a").await.unwrap();
        queue.register_live_consumer().await.unwrap();
        queue
            .enqueue_live(
                message("1", "private Teams activity"),
                teams_key("activity-1"),
            )
            .await
            .unwrap();
        {
            let mut inbox = queue.inbox.lock().await;
            inbox.state.primary_consumer = None;
            inbox.state.consumers.clear();
            inbox.state.entries[0].consumer_id = None;
            inbox.persist().unwrap();
        }

        let pull = crate::mcp::dispatch::call_tool(
            &server,
            "register_event_consumer",
            json!({
                "label": "ordinary pull",
                "make_primary": true,
                "claim_unassigned": true
            }),
        )
        .await
        .unwrap();
        let pull = tool_result(pull);
        let pull_id = pull["consumer_id"].as_str().unwrap();
        let poll = crate::mcp::dispatch::call_tool(
            &server,
            "next_event",
            json!({ "consumer_id": pull_id, "wait_seconds": 0 }),
        )
        .await
        .unwrap();
        let poll = tool_result(poll);
        assert!(poll["event"].is_null());
        assert_eq!(queue.status().await.queued, 1);
        assert_eq!(queue.status().await.unassigned, 1);
    }

    #[tokio::test]
    async fn public_live_consumer_guard_waits_for_prior_sync_barrier() {
        let dir = TempDir::new().unwrap();
        let queue = CodexEventQueue::load(&temp_path(&dir)).unwrap();
        let resident = queue.register_live_consumer().await.unwrap();
        queue.inbox.lock().await.persist_failure = Some(PersistFailure::AfterRename);
        assert!(matches!(
            queue.enqueue(message("1", "committed before guard")).await,
            Err(CodexQueueError::InboxDurabilityUncertain { .. })
        ));
        assert!(matches!(
            queue
                .next_event(&resident, Duration::ZERO, DEFAULT_LEASE)
                .await,
            Err(CodexQueueError::InboxDurabilityUncertain { .. })
        ));
        queue.inbox.lock().await.persist_failure = None;
        assert!(matches!(
            queue
                .next_event(&resident, Duration::ZERO, DEFAULT_LEASE)
                .await,
            Err(CodexQueueError::UnknownConsumer)
        ));
        assert_eq!(queue.status().await.queued, 1);
    }

    #[tokio::test]
    async fn legacy_same_label_pull_record_never_becomes_live_after_reload_or_expiry() {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).unwrap();
        let thread = CodexThreadId::parse("thread-a").unwrap();
        queue.bind_live_thread(Some(thread.clone())).await.unwrap();
        drop(queue);

        // A pre-kind inbox could persist a public pull registration with the
        // live label. Missing kind must not upgrade that record to a resident.
        let inbox_path = path.join(INBOX_FILE_NAME);
        let mut disk: Value = serde_json::from_slice(&std::fs::read(&inbox_path).unwrap()).unwrap();
        disk["next_consumer_generation"] = json!(1);
        disk["primary_consumer"] = json!("codex-consumer-0");
        disk["consumers"] = json!([{
            "id": "codex-consumer-0",
            "label": LIVE_CONSUMER_LABEL,
            "expires_at": Utc::now() + TimeDelta::hours(1),
            "ttl_seconds": 3600
        }]);
        std::fs::write(&inbox_path, serde_json::to_vec_pretty(&disk).unwrap()).unwrap();

        let reloaded = CodexEventQueue::load(&path).unwrap();
        assert!(matches!(
            reloaded
                .enqueue_live(message("1", "must reject"), teams_key("activity-1"))
                .await,
            Err(LiveQueueError::ResidentUnavailable)
        ));
        assert!(matches!(
            reloaded.register_live_consumer().await,
            Err(CodexQueueError::PrimaryConsumerExists)
        ));
        {
            let mut inbox = reloaded.inbox.lock().await;
            inbox.state.consumers[0].expires_at = Utc::now() - TimeDelta::seconds(1);
            inbox.persist().unwrap();
        }
        drop(reloaded);

        let recovered = CodexEventQueue::load(&path).unwrap();
        let resident = recovered.register_live_consumer().await.unwrap();
        assert_ne!(resident, ConsumerId::parse("codex-consumer-0").unwrap());
        assert!(matches!(
            recovered
                .enqueue_live(message("2", "for real resident"), teams_key("activity-2"))
                .await,
            Ok(LiveEnqueueReceipt::Committed)
        ));
        let event = recovered
            .next_live_event(&resident, &thread, Duration::ZERO, DEFAULT_LEASE)
            .await
            .unwrap()
            .expect("recovered resident receives only fresh event");
        assert_eq!(event.event["params"]["content"], "for real resident");
    }

    #[tokio::test]
    async fn reserved_live_label_rejection_waits_for_prior_sync_barrier() {
        let dir = TempDir::new().unwrap();
        let queue = CodexEventQueue::load(&temp_path(&dir)).unwrap();
        queue.register_live_consumer().await.unwrap();
        queue.inbox.lock().await.persist_failure = Some(PersistFailure::AfterRename);
        assert!(matches!(
            queue.enqueue(message("1", "already committed")).await,
            Err(CodexQueueError::InboxDurabilityUncertain { .. })
        ));
        assert!(matches!(
            queue
                .register_consumer(
                    LIVE_CONSUMER_LABEL.to_owned(),
                    DEFAULT_CONSUMER_TTL,
                    true,
                    true,
                )
                .await,
            Err(CodexQueueError::InboxDurabilityUncertain { .. })
        ));
        queue.inbox.lock().await.persist_failure = None;
        assert!(matches!(
            queue
                .register_consumer(
                    format!(" {LIVE_CONSUMER_LABEL} "),
                    DEFAULT_CONSUMER_TTL,
                    true,
                    true,
                )
                .await,
            Err(CodexQueueError::InvalidConsumerId)
        ));
        assert_eq!(queue.status().await.consumers.len(), 1);
    }

    #[tokio::test]
    async fn live_enqueue_distinguishes_precommit_from_visible_uncertainty_and_wakes() {
        for stage in [
            PersistFailure::BeforeWrite,
            PersistFailure::BeforeRename,
            PersistFailure::AfterRename,
        ] {
            let dir = TempDir::new().unwrap();
            let queue = CodexEventQueue::load(&temp_path(&dir)).unwrap();
            let consumer = queue.register_live_consumer().await.unwrap();
            let thread = CodexThreadId::parse("thread-a").unwrap();
            queue.bind_live_thread(Some(thread.clone())).await.unwrap();
            queue.inbox.lock().await.persist_failure = Some(stage);
            let notified = queue.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let result = queue
                .enqueue_live(message("1", "for resident"), teams_key("1"))
                .await;
            let committed = stage == PersistFailure::AfterRename;
            if committed {
                assert!(matches!(
                    result,
                    Ok(LiveEnqueueReceipt::CommittedDurabilityUncertain(
                        CodexQueueError::InboxDurabilityUncertain { .. }
                    ))
                ));
            } else {
                assert!(matches!(
                    result,
                    Err(LiveQueueError::Queue(CodexQueueError::InboxIo { .. }))
                ));
            }
            assert_eq!(queue.status().await.queued, usize::from(committed));
            if committed {
                tokio::time::timeout(Duration::from_millis(100), notified)
                    .await
                    .expect("committed event wakes resident");
                assert!(matches!(
                    queue
                        .enqueue_live(message("1", "replay while unsynced"), teams_key("1"))
                        .await,
                    Err(LiveQueueError::Queue(
                        CodexQueueError::InboxDurabilityUncertain { .. }
                    ))
                ));
            } else {
                assert!(
                    tokio::time::timeout(Duration::from_millis(20), notified)
                        .await
                        .is_err()
                );
            }
            queue.inbox.lock().await.persist_failure = None;
            if committed {
                assert!(matches!(
                    queue
                        .enqueue_live(message("1", "replay after recovery"), teams_key("1"))
                        .await,
                    Ok(LiveEnqueueReceipt::Duplicate)
                ));
            } else {
                assert!(matches!(
                    queue
                        .enqueue_live(message("1", "retry after rollback"), teams_key("1"))
                        .await,
                    Ok(LiveEnqueueReceipt::Committed)
                ));
            }
            let event = queue
                .next_live_event(&consumer, &thread, Duration::ZERO, DEFAULT_LEASE)
                .await
                .unwrap();
            assert!(event.is_some());
        }
    }

    #[tokio::test]
    async fn live_enqueue_prior_sync_barrier_rejects_this_event_before_mutation() {
        let dir = TempDir::new().unwrap();
        let queue = CodexEventQueue::load(&temp_path(&dir)).unwrap();
        let consumer = queue.register_live_consumer().await.unwrap();
        let thread = CodexThreadId::parse("thread-a").unwrap();
        queue.bind_live_thread(Some(thread.clone())).await.unwrap();
        queue.inbox.lock().await.persist_failure = Some(PersistFailure::AfterRename);
        assert!(matches!(
            queue.enqueue(message("1", "prior event")).await,
            Err(CodexQueueError::InboxDurabilityUncertain { .. })
        ));
        let before = std::fs::read(queue.inbox.lock().await.path.as_std_path()).unwrap();
        // Even an unavailable route must not bypass an older sync barrier.
        queue.inbox.lock().await.state.live_thread_id = None;
        assert!(matches!(
            queue
                .enqueue_live(message("2", "must not insert"), teams_key("2"))
                .await,
            Err(LiveQueueError::Queue(
                CodexQueueError::InboxDurabilityUncertain { .. }
            ))
        ));
        assert_eq!(
            std::fs::read(queue.inbox.lock().await.path.as_std_path()).unwrap(),
            before
        );
        queue.inbox.lock().await.state.live_thread_id = Some(thread.clone());
        queue.inbox.lock().await.persist_failure = None;
        let event = queue
            .next_live_event(&consumer, &thread, Duration::ZERO, DEFAULT_LEASE)
            .await
            .unwrap()
            .expect("prior event remains deliverable after recovery");
        assert_eq!(event.event["params"]["content"], "prior event");
        let status = queue.status().await;
        assert_eq!(status.queued, 1);
        assert_eq!(status.leased, 1);
    }

    #[tokio::test]
    async fn live_enqueue_reloaded_inbox_clears_barrier_before_admission() {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).unwrap();
        let consumer = queue.register_live_consumer().await.unwrap();
        let thread = CodexThreadId::parse("thread-a").unwrap();
        queue.bind_live_thread(Some(thread.clone())).await.unwrap();
        drop(queue);

        let reloaded = CodexEventQueue::load(&path).unwrap();
        reloaded.inbox.lock().await.persist_failure = Some(PersistFailure::AfterRename);
        let before = std::fs::read(path.join(INBOX_FILE_NAME)).unwrap();
        assert!(matches!(
            reloaded
                .enqueue_live(message("1", "not inserted"), teams_key("1"))
                .await,
            Err(LiveQueueError::Queue(
                CodexQueueError::InboxDurabilityUncertain { .. }
            ))
        ));
        assert_eq!(std::fs::read(path.join(INBOX_FILE_NAME)).unwrap(), before);
        assert_eq!(reloaded.status().await.queued, 0);

        reloaded.inbox.lock().await.persist_failure = None;
        assert!(matches!(
            reloaded
                .enqueue_live(message("2", "after recovery"), teams_key("2"))
                .await,
            Ok(LiveEnqueueReceipt::Committed)
        ));
        let event = reloaded
            .next_live_event(&consumer, &thread, Duration::ZERO, DEFAULT_LEASE)
            .await
            .unwrap()
            .expect("recovered exact route");
        assert_eq!(event.event["params"]["content"], "after recovery");
    }

    #[tokio::test]
    async fn live_enqueue_suppresses_queued_and_acknowledged_replays_across_restart() {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).unwrap();
        let consumer = queue.register_live_consumer().await.unwrap();
        let thread = CodexThreadId::parse("thread-a").unwrap();
        queue.bind_live_thread(Some(thread.clone())).await.unwrap();
        let key = teams_key("activity/a");
        assert!(matches!(
            queue
                .enqueue_live(message("1", "original"), key.clone())
                .await,
            Ok(LiveEnqueueReceipt::Committed)
        ));
        {
            let notified = queue.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            assert!(matches!(
                queue
                    .enqueue_live(message("1", "replayed with new handle"), key.clone())
                    .await,
                Ok(LiveEnqueueReceipt::Duplicate)
            ));
            assert!(
                tokio::time::timeout(Duration::from_millis(20), notified)
                    .await
                    .is_err()
            );
        }
        assert_eq!(queue.status().await.queued, 1);
        drop(queue);
        let queue = CodexEventQueue::load(&path).unwrap();
        assert!(matches!(
            queue
                .enqueue_live(message("1", "replayed pending after restart"), key.clone())
                .await,
            Ok(LiveEnqueueReceipt::Duplicate)
        ));
        let leased = queue
            .next_live_event(&consumer, &thread, Duration::ZERO, DEFAULT_LEASE)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(leased.event["params"]["content"], "original");
        queue
            .acknowledge_live(&consumer, &leased.delivery_token)
            .await
            .unwrap();
        assert!(matches!(
            queue
                .enqueue_live(message("1", "after ack"), key.clone())
                .await,
            Ok(LiveEnqueueReceipt::Duplicate)
        ));
        drop(queue);

        let reloaded = CodexEventQueue::load(&path).unwrap();
        assert!(matches!(
            reloaded
                .enqueue_live(message("1", "after restart"), key.clone())
                .await,
            Ok(LiveEnqueueReceipt::Duplicate)
        ));
        assert_eq!(reloaded.status().await.queued, 0);
        let other_conversation =
            ProviderEventKey::teams("tenant-a", "bot-a", "conversation-b", "activity/a").unwrap();
        assert!(matches!(
            reloaded
                .enqueue_live(message("1", "other conversation"), other_conversation)
                .await,
            Ok(LiveEnqueueReceipt::Committed)
        ));
    }

    #[tokio::test]
    async fn live_enqueue_replay_key_validates_identity_and_expires_after_ack_window() {
        assert!(matches!(
            ProviderEventKey::teams("", "bot", "conversation", "activity"),
            Err(LiveQueueError::InvalidProviderEventId)
        ));
        assert!(matches!(
            ProviderEventKey::teams("tenant", "bot", "conversation", "x\n"),
            Err(LiveQueueError::InvalidProviderEventId)
        ));
        assert!(matches!(
            ProviderEventKey::teams("tenant", "bot", "conversation", &"x".repeat(513)),
            Err(LiveQueueError::InvalidProviderEventId)
        ));
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).unwrap();
        let consumer = queue.register_live_consumer().await.unwrap();
        let thread = CodexThreadId::parse("thread-a").unwrap();
        queue.bind_live_thread(Some(thread.clone())).await.unwrap();
        let key = teams_key("activity/a");
        queue
            .enqueue_live(message("1", "original"), key.clone())
            .await
            .unwrap();
        let leased = queue
            .next_live_event(&consumer, &thread, Duration::ZERO, DEFAULT_LEASE)
            .await
            .unwrap()
            .unwrap();
        queue
            .acknowledge_live(&consumer, &leased.delivery_token)
            .await
            .unwrap();
        {
            let mut inbox = queue.inbox.lock().await;
            let expired =
                Utc::now() - duration_delta(PROCESSED_TEAMS_RETENTION) - TimeDelta::seconds(1);
            inbox.state.processed_provider_event_keys[0].acknowledged_at = expired;
            inbox
                .processed_provider_event_keys
                .insert(key.clone(), expired);
            inbox.persist().unwrap();
        }
        drop(queue);

        let reloaded = CodexEventQueue::load(&path).unwrap();
        assert!(matches!(
            reloaded
                .enqueue_live(message("1", "fresh after expiry"), key.clone())
                .await,
            Ok(LiveEnqueueReceipt::Committed)
        ));
        let inbox = reloaded.inbox.lock().await;
        assert!(inbox.state.processed_provider_event_keys.is_empty());
        assert_eq!(inbox.provider_event_keys.len(), 1);
    }

    #[tokio::test]
    async fn live_enqueue_acknowledgement_uncertainty_retains_processed_replay_key() {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).unwrap();
        let consumer = queue.register_live_consumer().await.unwrap();
        let thread = CodexThreadId::parse("thread-a").unwrap();
        queue.bind_live_thread(Some(thread.clone())).await.unwrap();
        let key = teams_key("activity/a");
        queue
            .enqueue_live(message("1", "original"), key.clone())
            .await
            .unwrap();
        let leased = queue
            .next_live_event(&consumer, &thread, Duration::ZERO, DEFAULT_LEASE)
            .await
            .unwrap()
            .unwrap();
        queue.inbox.lock().await.persist_failure = Some(PersistFailure::AfterRename);
        assert!(matches!(
            queue
                .acknowledge_live(&consumer, &leased.delivery_token)
                .await,
            Err(CodexQueueError::InboxDurabilityUncertain { .. })
        ));
        assert_eq!(queue.status().await.queued, 0);
        assert!(matches!(
            queue
                .enqueue_live(message("1", "before sync"), key.clone())
                .await,
            Err(LiveQueueError::Queue(
                CodexQueueError::InboxDurabilityUncertain { .. }
            ))
        ));
        queue.inbox.lock().await.persist_failure = None;
        assert!(matches!(
            queue
                .enqueue_live(message("1", "after sync"), key.clone())
                .await,
            Ok(LiveEnqueueReceipt::Duplicate)
        ));
        drop(queue);
        let reloaded = CodexEventQueue::load(&path).unwrap();
        assert!(matches!(
            reloaded
                .enqueue_live(message("1", "after reload"), key)
                .await,
            Ok(LiveEnqueueReceipt::Duplicate)
        ));
    }

    #[tokio::test]
    async fn live_enqueue_replay_capacity_fails_closed_but_allows_duplicates() {
        let dir = TempDir::new().unwrap();
        let queue = CodexEventQueue::load(&temp_path(&dir)).unwrap();
        queue.register_live_consumer().await.unwrap();
        queue
            .bind_live_thread(Some(CodexThreadId::parse("thread-a").unwrap()))
            .await
            .unwrap();
        {
            let mut inbox = queue.inbox.lock().await;
            let now = Utc::now();
            for index in 0..MAX_RETAINED_TEAMS_EVENT_KEYS {
                let key = teams_key(&format!("activity-{index}"));
                inbox.processed_provider_event_keys.insert(key.clone(), now);
                inbox
                    .state
                    .processed_provider_event_keys
                    .push_back(ProcessedProviderEvent {
                        key,
                        acknowledged_at: now,
                    });
            }
            inbox.persist().unwrap();
        }
        assert!(matches!(
            queue
                .enqueue_live(message("1", "duplicate"), teams_key("activity-0"))
                .await,
            Ok(LiveEnqueueReceipt::Duplicate)
        ));
        let before = std::fs::read(queue.inbox.lock().await.path.as_std_path()).unwrap();
        assert!(matches!(
            queue
                .enqueue_live(message("2", "new"), teams_key("activity-new"))
                .await,
            Err(LiveQueueError::ReplayCapacity)
        ));
        assert_eq!(
            std::fs::read(queue.inbox.lock().await.path.as_std_path()).unwrap(),
            before
        );
        assert_eq!(queue.status().await.queued, 0);
        {
            let mut inbox = queue.inbox.lock().await;
            let expired =
                Utc::now() - duration_delta(PROCESSED_TEAMS_RETENTION) - TimeDelta::seconds(1);
            inbox.state.processed_provider_event_keys[0].acknowledged_at = expired;
            inbox
                .processed_provider_event_keys
                .insert(teams_key("activity-0"), expired);
            inbox.persist().unwrap();
        }
        let path = temp_path(&dir);
        drop(queue);
        let reloaded = CodexEventQueue::load(&path).unwrap();
        assert!(matches!(
            reloaded
                .enqueue_live(message("2", "after expiry"), teams_key("activity-new"))
                .await,
            Ok(LiveEnqueueReceipt::Committed)
        ));
        let inbox = reloaded.inbox.lock().await;
        assert_eq!(inbox.provider_event_keys.len(), 1);
        assert_eq!(
            inbox.processed_provider_event_keys.len(),
            MAX_RETAINED_TEAMS_EVENT_KEYS - 1
        );
    }

    #[tokio::test]
    async fn failed_ack_persistence_rolls_back_in_memory_state() {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).unwrap();
        let consumer = primary_consumer(&queue).await;
        queue.enqueue(message("1", "keep me")).await.unwrap();
        let event = queue
            .next_event(&consumer, Duration::ZERO, Duration::from_secs(60))
            .await
            .unwrap()
            .unwrap();

        let valid_temporary_path = {
            let mut inbox = queue.inbox.lock().await;
            let valid = inbox.temporary_path.clone();
            inbox.temporary_path = path.join("missing").join("inbox.tmp");
            valid
        };
        assert!(
            queue
                .acknowledge(&consumer, &event.delivery_token)
                .await
                .is_err()
        );
        assert_eq!(queue.status().await.queued, 1);

        queue.inbox.lock().await.temporary_path = valid_temporary_path;
        queue
            .acknowledge(&consumer, &event.delivery_token)
            .await
            .unwrap();
        assert_eq!(queue.status().await.queued, 0);
    }

    #[tokio::test]
    async fn malformed_attention_markers_are_rejected_at_enqueue() {
        let dir = TempDir::new().unwrap();
        let queue = CodexEventQueue::load(&temp_path(&dir)).unwrap();
        for malformed in [
            json!({ "params": { "meta": { "attention_delivery": "prompt" } } }),
            json!({ "params": { "meta": { "attention_record": "record-a" } } }),
            json!({ "params": { "meta": {
                "attention_delivery": "immediate",
                "attention_record": "record-a"
            } } }),
        ] {
            assert!(matches!(
                queue.enqueue(malformed).await,
                Err(CodexQueueError::MalformedAttentionMetadata { .. })
            ));
        }
        assert!(queue.enqueue(message("10", "ordinary")).await.unwrap());
        assert_eq!(queue.status().await.queued, 1);
    }

    #[tokio::test]
    async fn attention_invalidation_revokes_leases_and_preserves_unrelated_queue() {
        let dir = TempDir::new().unwrap();
        let queue = CodexEventQueue::load(&temp_path(&dir)).unwrap();
        let consumer = primary_consumer(&queue).await;
        queue
            .enqueue(managed_message("101", "record-a", "prompt", "managed a"))
            .await
            .unwrap();
        queue.enqueue(message("102", "ordinary")).await.unwrap();
        queue
            .enqueue(managed_message("103", "record-b", "next_turn", "managed b"))
            .await
            .unwrap();

        let leased_managed = queue
            .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(leased_managed.event["params"]["content"], "managed a");
        let invalidated = queue
            .invalidate_attention(None, Some(&"record-a".into()))
            .await
            .unwrap();
        assert_eq!(
            invalidated,
            AttentionInvalidationResult {
                removed: 1,
                invalidated_leases: 1,
            }
        );
        assert!(matches!(
            queue
                .defer_attention(
                    &consumer,
                    &leased_managed.delivery_token,
                    Duration::from_secs(1),
                )
                .await,
            Err(CodexQueueError::UnknownDeliveryToken)
        ));
        let ordinary = queue
            .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ordinary.event["params"]["content"], "ordinary");
        let invalidated = queue
            .invalidate_attention(Some(MessageId::new(103)), None)
            .await
            .unwrap();
        assert_eq!(invalidated.removed, 1);
        assert_eq!(invalidated.invalidated_leases, 0);
        queue
            .acknowledge(&consumer, &ordinary.delivery_token)
            .await
            .unwrap();
        assert_eq!(queue.status().await.queued, 0);
    }
}
