//! Durable delivery for Codex.
//!
//! Codex cannot turn unsolicited MCP notifications into new turns. In Codex
//! mode, Dione therefore persists accepted Discord events. Codex conversations
//! may pull them explicitly, or a live app-server worker may inject them into
//! one exact thread. Consumers lease an event, handle it, then acknowledge the
//! lease. Expired leases become eligible for redelivery.

mod app_server;

pub use app_server::{CodexDeliveryConfig, CodexDeliveryError, run_delivery_worker};

use camino::{Utf8Path, Utf8PathBuf};
use chrono::{DateTime, TimeDelta, Utc};
use clap::ValueEnum;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use serenity::model::id::MessageId;
use std::{
    collections::{HashSet, VecDeque},
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
const LIVE_CONSUMER_LABEL: &str = "dione-live-app-server";

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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConsumerRegistration {
    id: ConsumerId,
    label: String,
    expires_at: DateTime<Utc>,
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
    #[serde(default)]
    consumer_id: Option<ConsumerId>,
    /// Exact live thread binding at ingress. Pull consumers leave this unset.
    #[serde(default)]
    live_thread_id: Option<CodexThreadId>,
    #[serde(default)]
    lease: Option<Lease>,
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
    entries: VecDeque<QueuedEvent>,
}

struct DurableInbox {
    path: Utf8PathBuf,
    temporary_path: Utf8PathBuf,
    _lock_file: File,
    state: InboxState,
    message_ids: HashSet<DiscordMessageId>,
    processed_message_ids: HashSet<DiscordMessageId>,
}

/// A single-owner, durable queue shared by Discord ingress and MCP pull tools.
#[derive(Clone)]
pub struct CodexEventQueue {
    inbox: Arc<Mutex<DurableInbox>>,
    changed: Arc<Notify>,
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
#[non_exhaustive]
pub enum CodexQueueError {
    #[error("failed to access Codex inbox `{path}`")]
    InboxIo {
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
        let mut state: InboxState = match std::fs::read(path.as_std_path()) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|source| CodexQueueError::InboxDecode {
                    path: path.clone(),
                    source,
                })?
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => InboxState::default(),
            Err(source) => {
                return Err(CodexQueueError::InboxIo {
                    path: path.clone(),
                    source,
                });
            }
        };
        for event in &mut state.entries {
            if event.discord_message_id.is_none() {
                event.discord_message_id = discord_message_id(&event.payload);
            }
        }
        let message_ids = state
            .entries
            .iter()
            .filter_map(|event| event.discord_message_id)
            .collect();
        let processed_message_ids = state.processed_message_ids.iter().cloned().collect();
        Ok(Self {
            path,
            temporary_path,
            _lock_file: lock_file,
            state,
            message_ids,
            processed_message_ids,
        })
    }

    fn enqueue(&mut self, payload: Value) -> Result<bool, CodexQueueError> {
        let now = Utc::now();
        self.expire_consumers(now);
        let discord_message_id = discord_message_id(&payload);
        if discord_message_id.as_ref().is_some_and(|id| {
            self.message_ids.contains(id) || self.processed_message_ids.contains(id)
        }) {
            return Ok(false);
        }

        self.transaction(move |inbox| {
            let id = EventId::new(inbox.state.next_id);
            inbox.state.next_id = inbox.state.next_id.saturating_add(1);
            if let Some(message_id) = &discord_message_id {
                inbox.message_ids.insert(*message_id);
            }
            inbox.state.entries.push_back(QueuedEvent {
                id,
                payload,
                discord_message_id,
                consumer_id: inbox.state.primary_consumer.clone(),
                live_thread_id: inbox.live_event_thread_id(),
                lease: None,
            });
            Ok(true)
        })
    }

    fn lease_next(
        &mut self,
        consumer_id: &ConsumerId,
        now: DateTime<Utc>,
        lease_duration: Duration,
        live_thread_id: Option<&CodexThreadId>,
    ) -> Result<Option<LeasedEvent>, CodexQueueError> {
        self.transaction(|inbox| {
            inbox.touch_consumer(consumer_id, now, DEFAULT_CONSUMER_TTL)?;
            for event in &mut inbox.state.entries {
                if event
                    .lease
                    .as_ref()
                    .is_some_and(|lease| lease.expires_at <= now)
                {
                    event.lease = None;
                }
            }

            let Some(index) = inbox.state.entries.iter().position(|event| {
                event.lease.is_none()
                    && event.consumer_id.as_ref() == Some(consumer_id)
                    && live_thread_id
                        .is_none_or(|thread_id| event.live_thread_id.as_ref() == Some(thread_id))
            }) else {
                return Ok(None);
            };

            let generation = inbox.state.next_lease_generation;
            inbox.state.next_lease_generation = generation.saturating_add(1);
            let event = &mut inbox.state.entries[index];
            let token = DeliveryToken::new(event.id, generation);
            let expires_at = now + duration_delta(lease_duration);
            event.lease = Some(Lease {
                token: token.clone(),
                consumer_id: Some(consumer_id.clone()),
                expires_at,
            });
            Ok(Some(LeasedEvent {
                event_id: event.id,
                delivery_token: token,
                lease_expires_at: expires_at,
                consumer_id: consumer_id.clone(),
                event: event.payload.clone(),
            }))
        })
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
        self.state
            .consumers
            .iter()
            .find(|consumer| consumer.id == *primary && consumer.label == LIVE_CONSUMER_LABEL)?;
        self.state.live_thread_id.clone()
    }

    fn acknowledge(
        &mut self,
        consumer_id: &ConsumerId,
        token: &DeliveryToken,
    ) -> Result<(), CodexQueueError> {
        self.transaction(|inbox| {
            inbox.touch_consumer(consumer_id, Utc::now(), DEFAULT_CONSUMER_TTL)?;
            let Some(index) = inbox.state.entries.iter().position(|event| {
                event.lease.as_ref().is_some_and(|lease| {
                    lease.token == *token && lease.consumer_id.as_ref() == Some(consumer_id)
                })
            }) else {
                return Err(CodexQueueError::UnknownDeliveryToken);
            };
            let Some(removed) = inbox.state.entries.remove(index) else {
                return Err(CodexQueueError::UnknownDeliveryToken);
            };
            if let Some(message_id) = removed.discord_message_id {
                inbox.message_ids.remove(&message_id);
                inbox.remember_processed_message(message_id);
            }
            Ok(())
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
                expires_at,
            });
            if make_primary {
                inbox.state.primary_consumer = Some(consumer_id.clone());
                if claim_unassigned {
                    for event in &mut inbox.state.entries {
                        if event.consumer_id.is_none() {
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
                && consumer.label == LIVE_CONSUMER_LABEL
            {
                // The inbox lock proves the previous Dione process is gone,
                // so the live worker may resume its durable identity even
                // after a long outage. Keeping the identity also keeps its
                // already-routed events deliverable.
                consumer.expires_at = now + duration_delta(MAX_CONSUMER_TTL);
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
                expires_at: now + duration_delta(MAX_CONSUMER_TTL),
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
            inbox.expire_consumers(now);
            if inbox.state.primary_consumer.as_ref() != Some(from) {
                return Err(CodexQueueError::NotPrimaryConsumer);
            }
            inbox.touch_consumer(to, now, DEFAULT_CONSUMER_TTL)?;
            let mut moved_pending = 0;
            let mut invalidated_leases = 0;
            if move_pending {
                for event in &mut inbox.state.entries {
                    if event.consumer_id.as_ref() == Some(from) {
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
            inbox.expire_consumers(now);
            if inbox.state.primary_consumer.is_some() {
                return Err(CodexQueueError::PrimaryConsumerExists);
            }
            inbox.touch_consumer(consumer_id, now, DEFAULT_CONSUMER_TTL)?;
            let active_consumers: HashSet<_> = inbox
                .state
                .consumers
                .iter()
                .map(|consumer| consumer.id.clone())
                .collect();
            let mut claimed = 0;
            if claim_orphaned {
                for event in &mut inbox.state.entries {
                    if event
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
        ttl: Duration,
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
        consumer.expires_at = now + duration_delta(ttl);
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

    fn transaction<T>(
        &mut self,
        mutate: impl FnOnce(&mut Self) -> Result<T, CodexQueueError>,
    ) -> Result<T, CodexQueueError> {
        let state = self.state.clone();
        let message_ids = self.message_ids.clone();
        let processed_message_ids = self.processed_message_ids.clone();
        let result = mutate(self);
        match result {
            Ok(value) => match self.persist() {
                Ok(()) => Ok(value),
                Err(error) => {
                    self.state = state;
                    self.message_ids = message_ids;
                    self.processed_message_ids = processed_message_ids;
                    Err(error)
                }
            },
            Err(error) => {
                self.state = state;
                self.message_ids = message_ids;
                self.processed_message_ids = processed_message_ids;
                Err(error)
            }
        }
    }

    fn persist(&self) -> Result<(), CodexQueueError> {
        let bytes = serde_json::to_vec_pretty(&self.state).map_err(|source| {
            CodexQueueError::InboxDecode {
                path: self.path.clone(),
                source,
            }
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
        std::fs::rename(self.temporary_path.as_std_path(), self.path.as_std_path()).map_err(
            |source| CodexQueueError::InboxIo {
                path: self.path.clone(),
                source,
            },
        )?;
        let Some(parent) = self.path.parent() else {
            return Ok(());
        };
        File::open(parent.as_std_path())
            .and_then(|directory| directory.sync_all())
            .map_err(|source| CodexQueueError::InboxIo {
                path: parent.to_owned(),
                source,
            })
    }
}

impl CodexEventQueue {
    pub fn load(state_dir: &Utf8Path) -> Result<Self, CodexQueueError> {
        Ok(Self {
            inbox: Arc::new(Mutex::new(DurableInbox::load(state_dir)?)),
            changed: Arc::new(Notify::new()),
        })
    }

    /// Persist an event before making it visible to consumers.
    ///
    /// Returns `false` when the Discord message id is already queued.
    pub async fn enqueue(&self, payload: Value) -> Result<bool, CodexQueueError> {
        let inserted = self.inbox.lock().await.enqueue(payload)?;
        if inserted {
            self.changed.notify_waiters();
        }
        Ok(inserted)
    }

    pub async fn next_event(
        &self,
        consumer_id: &ConsumerId,
        wait: Duration,
        lease: Duration,
    ) -> Result<Option<LeasedEvent>, CodexQueueError> {
        let wait = wait.min(MAX_WAIT);
        let lease = lease.min(MAX_LEASE);
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            // Register the waiter before checking the queue. `notify_waiters`
            // does not retain a permit when no waiter is registered, so doing
            // this after `lease_next` leaves a lost-wake window.
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(event) =
                self.inbox
                    .lock()
                    .await
                    .lease_next(consumer_id, Utc::now(), lease, None)?
            {
                return Ok(Some(event));
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() || tokio::time::timeout(remaining, notified).await.is_err() {
                return Ok(None);
            }
        }
    }

    pub(crate) async fn next_live_event(
        &self,
        consumer_id: &ConsumerId,
        thread_id: &CodexThreadId,
        wait: Duration,
        lease: Duration,
    ) -> Result<Option<LeasedEvent>, CodexQueueError> {
        let wait = wait.min(MAX_WAIT);
        let lease = lease.min(MAX_LEASE);
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(event) = self.inbox.lock().await.lease_next(
                consumer_id,
                Utc::now(),
                lease,
                Some(thread_id),
            )? {
                return Ok(Some(event));
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() || tokio::time::timeout(remaining, notified).await.is_err() {
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

    pub async fn acknowledge(
        &self,
        consumer_id: &ConsumerId,
        token: &DeliveryToken,
    ) -> Result<(), CodexQueueError> {
        self.inbox.lock().await.acknowledge(consumer_id, token)?;
        self.changed.notify_waiters();
        Ok(())
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

fn duration_delta(duration: Duration) -> TimeDelta {
    TimeDelta::from_std(duration).unwrap_or_else(|_| TimeDelta::hours(1))
}

fn discord_message_id(payload: &Value) -> Option<DiscordMessageId> {
    payload
        .pointer("/params/meta/message_id")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<u64>().ok())
        .and_then(crate::mcp::ids::Snowflake::new)
        .map(crate::mcp::ids::Snowflake::message)
        .map(DiscordMessageId)
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

    async fn primary_consumer(queue: &CodexEventQueue) -> ConsumerId {
        queue
            .register_consumer("test thread".to_owned(), DEFAULT_CONSUMER_TTL, true, true)
            .await
            .unwrap()
            .consumer_id
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
        let queue = CodexEventQueue::load(&temp_path(&dir)).unwrap();

        let first = queue.register_live_consumer().await.unwrap();
        let resumed = queue.register_live_consumer().await.unwrap();

        assert_eq!(resumed, first);
        let status = queue.status().await;
        assert_eq!(status.primary_consumer.as_ref(), Some(&first));
        assert_eq!(status.consumers.len(), 1);
    }

    #[tokio::test]
    async fn expired_live_consumer_keeps_routed_events_after_long_outage() {
        let dir = TempDir::new().unwrap();
        let queue = CodexEventQueue::load(&temp_path(&dir)).unwrap();
        let first = queue.register_live_consumer().await.unwrap();
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
        }

        let resumed = queue.register_live_consumer().await.unwrap();

        assert_eq!(resumed, first);
        assert!(
            queue
                .next_event(&resumed, Duration::ZERO, DEFAULT_LEASE)
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
}
