//! Complete historical Codex JSON codec; the shared owner never reads payload fields.
use super::{
    AttentionDelivery, AttentionScheduling, CodexThreadId, LegacyDiscordScope,
    legacy_delivery::decode_attention,
};
use crate::{
    attention::delivery::{AttentionDirective, OwnedAttentionDescriptor},
    delivery::ProviderEventKey,
    durable::{
        CodecError, CodecErrorKind, ConsumerId, DeliveryToken, EndpointId, EventId, QueueCodec,
        QueueSnapshot, ReplayClass, state,
    },
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serenity::model::id::MessageId;
use std::collections::VecDeque;
use thiserror::Error;

#[derive(Debug, Clone)]
pub(crate) struct LegacyPayload {
    value: Value,
    attention: Option<OwnedAttentionDescriptor>,
}
impl LegacyPayload {
    pub(crate) fn new(
        value: Value,
        scope: &LegacyDiscordScope,
    ) -> Result<Self, super::CodexQueueError> {
        let attention =
            decode_attention(&value, scope).map_err(super::CodexQueueErrorKind::LegacyAttention)?;
        Ok(Self { value, attention })
    }
    pub(crate) fn into_value(self) -> Value {
        self.value
    }
    pub(crate) fn attention(&self) -> Option<&OwnedAttentionDescriptor> {
        self.attention.as_ref()
    }
    pub(crate) fn scheduling(&self) -> Option<AttentionScheduling> {
        self.attention.as_ref().map(|owned| {
            let descriptor = owned.descriptor();
            AttentionScheduling {
                delivery: match descriptor.directive() {
                    AttentionDirective::Prompt => AttentionDelivery::Prompt,
                    AttentionDirective::NextTurn => AttentionDelivery::NextTurn,
                },
                record_id: descriptor.record().clone(),
            }
        })
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum LegacyReplayIdentity {
    DiscordCreate(DiscordMessageId),
    Provider(ProviderEventKey),
}
impl LegacyReplayIdentity {
    pub(crate) fn from_payload(payload: &Value) -> Option<Self> {
        discord_message_id(payload).map(Self::DiscordCreate)
    }
    pub(crate) fn class(&self) -> ReplayClass {
        match self {
            Self::DiscordCreate(_) => ReplayClass::Rolling,
            Self::Provider(_) => ReplayClass::Timed,
        }
    }
}

#[derive(Debug, Error)]
#[error("{kind}")]
pub(crate) struct LegacyCodecError {
    #[source]
    pub(crate) kind: LegacyCodecErrorKind,
}
#[derive(Debug, Error)]
pub(crate) enum LegacyCodecErrorKind {
    #[error("invalid legacy inbox JSON")]
    Decode(#[source] serde_path_to_error::Error<serde_json::Error>),
    #[error("trailing legacy inbox JSON")]
    Trailing(#[source] serde_json::Error),
    #[error("invalid legacy managed payload")]
    Payload(#[source] super::CodexQueueError),
    #[error("failed to encode legacy inbox JSON")]
    Encode(#[source] serde_json::Error),
    #[error("invalid legacy endpoint")]
    Endpoint(#[source] crate::durable::IdentityError),
    #[error("legacy snapshot cannot represent repeated replay family")]
    ReplayShape,
}
fn decode_error(kind: LegacyCodecErrorKind) -> CodecError {
    CodecError {
        kind: CodecErrorKind::Decode(Box::new(LegacyCodecError { kind })),
    }
}
fn encode_error(kind: LegacyCodecErrorKind) -> CodecError {
    CodecError {
        kind: CodecErrorKind::Encode(Box::new(LegacyCodecError { kind })),
    }
}
fn default_consumer_ttl_seconds() -> u64 {
    900
}

/// Discord snowflake cached for durable event deduplication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct DiscordMessageId(pub(crate) MessageId);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProcessedProviderEvent {
    key: ProviderEventKey,
    acknowledged_at: DateTime<Utc>,
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
        let id = crate::discord::ids::Snowflake::new(raw)
            .map(crate::discord::ids::Snowflake::message)
            .ok_or_else(|| serde::de::Error::custom("Discord message ID must be non-zero"))?;
        Ok(Self(id))
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct InboxState {
    next_id: u64,
    #[serde(default, deserialize_with = "present_counter")]
    next_lease_generation: Option<u64>,
    #[serde(default, deserialize_with = "present_counter")]
    next_consumer_generation: Option<u64>,
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
        .and_then(crate::discord::ids::Snowflake::new)
        .map(crate::discord::ids::Snowflake::message)
        .map(DiscordMessageId)
}

/// Renames the pre-v2 structured projection on queued notifications.
///
/// Existing v2 citations keep their order. Any distinct legacy citations are
/// appended, so a mixed-version payload loses neither representation while the
/// retired field name disappears from consumer-visible delivery.
pub(super) fn migrate_legacy_evidence_projection(payload: &mut Value) -> bool {
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

#[derive(Deserialize)]
struct VersionEnvelope {
    #[serde(default, deserialize_with = "present_version")]
    version: Option<Value>,
}
fn present_version<'de, D: serde::Deserializer<'de>>(
    decoder: D,
) -> Result<Option<Value>, D::Error> {
    Value::deserialize(decoder).map(Some)
}

pub(crate) struct LegacyQueueCodec {
    scope: LegacyDiscordScope,
}
impl LegacyQueueCodec {
    pub(crate) fn new(scope: LegacyDiscordScope) -> Self {
        Self { scope }
    }
}
fn endpoint(thread: Option<CodexThreadId>) -> Result<Option<EndpointId>, CodecError> {
    thread
        .map(|thread| {
            EndpointId::new(thread.as_str())
                .map_err(|error| decode_error(LegacyCodecErrorKind::Endpoint(error)))
        })
        .transpose()
}
fn thread(endpoint: &Option<EndpointId>) -> Result<Option<CodexThreadId>, CodecError> {
    endpoint
        .as_ref()
        .map(|value| {
            CodexThreadId::parse(value.as_str())
                .map_err(|error| encode_error(LegacyCodecErrorKind::Payload(error)))
        })
        .transpose()
}
fn generated_consumer(value: &ConsumerId) -> Option<u64> {
    let suffix = value.as_str().strip_prefix("codex-consumer-")?;
    let number = suffix.parse::<u64>().ok()?;
    (suffix == number.to_string()).then_some(number)
}
fn generated_lease(value: &DeliveryToken) -> Option<u64> {
    let (event, generation) = value.as_str().strip_prefix("dione-")?.split_once('-')?;
    let event_number = event.parse::<u64>().ok()?;
    let generation_number = generation.parse::<u64>().ok()?;
    (event == event_number.to_string() && generation == generation_number.to_string())
        .then_some(generation_number)
}
fn present_counter<'de, D: serde::Deserializer<'de>>(decoder: D) -> Result<Option<u64>, D::Error> {
    u64::deserialize(decoder).map(Some)
}
fn reconstructed_next(values: impl Iterator<Item = u64>) -> u64 {
    values.max().map_or(0, |value| value.saturating_add(1))
}
impl QueueCodec for LegacyQueueCodec {
    type Payload = LegacyPayload;
    type Key = LegacyReplayIdentity;
    fn decode(
        &self,
        bytes: &[u8],
    ) -> Result<QueueSnapshot<LegacyPayload, LegacyReplayIdentity>, CodecError> {
        let mut decoder = serde_json::Deserializer::from_slice(bytes);
        let envelope: VersionEnvelope = serde_path_to_error::deserialize(&mut decoder)
            .map_err(|error| decode_error(LegacyCodecErrorKind::Decode(error)))?;
        decoder
            .end()
            .map_err(|error| decode_error(LegacyCodecErrorKind::Trailing(error)))?;
        if envelope
            .version
            .as_ref()
            .is_some_and(|version| version.as_u64() != Some(0))
        {
            return Err(CodecError {
                kind: CodecErrorKind::UnsupportedVersion,
            });
        }
        let mut decoder = serde_json::Deserializer::from_slice(bytes);
        let legacy: InboxState = serde_path_to_error::deserialize(&mut decoder)
            .map_err(|error| decode_error(LegacyCodecErrorKind::Decode(error)))?;
        let next_consumer_generation = legacy.next_consumer_generation.unwrap_or_else(|| {
            reconstructed_next(
                legacy
                    .consumers
                    .iter()
                    .map(|value| &value.id)
                    .chain(legacy.primary_consumer.iter())
                    .chain(legacy.entries.iter().flat_map(|entry| {
                        entry.consumer_id.iter().chain(
                            entry
                                .lease
                                .iter()
                                .flat_map(|lease| lease.consumer_id.iter()),
                        )
                    }))
                    .filter_map(generated_consumer),
            )
        });
        let next_lease_generation = legacy.next_lease_generation.unwrap_or_else(|| {
            reconstructed_next(
                legacy
                    .entries
                    .iter()
                    .filter_map(|entry| entry.lease.as_ref())
                    .filter_map(|lease| generated_lease(&lease.token)),
            )
        });
        let consumers = legacy
            .consumers
            .into_iter()
            .map(|value| state::ConsumerRegistration {
                id: value.id,
                label: value.label,
                kind: match value.kind {
                    ConsumerKind::Pull => state::ConsumerKind::Pull,
                    ConsumerKind::Live => state::ConsumerKind::Resident,
                },
                expires_at: value.expires_at,
                ttl_seconds: value.ttl_seconds,
            })
            .collect();
        let mut acknowledged: VecDeque<_> = legacy
            .processed_message_ids
            .into_iter()
            .map(|id| state::ReplayReceipt {
                key: LegacyReplayIdentity::DiscordCreate(id),
                acknowledged_at: DateTime::UNIX_EPOCH,
            })
            .collect();
        acknowledged.extend(
            legacy
                .processed_provider_event_keys
                .into_iter()
                .map(|value| state::ReplayReceipt {
                    key: LegacyReplayIdentity::Provider(value.key),
                    acknowledged_at: value.acknowledged_at,
                }),
        );
        let mut entries = VecDeque::new();
        for entry in legacy.entries {
            let visibility = if entry.live_thread_id.is_some() || entry.provider_event_key.is_some()
            {
                state::EntryVisibility::ResidentOnly
            } else {
                state::EntryVisibility::Public
            };
            let discord = entry.discord_message_id.or_else(|| {
                if entry.provider_event_key.is_none() {
                    discord_message_id(&entry.payload)
                } else {
                    None
                }
            });
            let replay_keys = discord
                .into_iter()
                .map(LegacyReplayIdentity::DiscordCreate)
                .chain(
                    entry
                        .provider_event_key
                        .into_iter()
                        .map(LegacyReplayIdentity::Provider),
                )
                .collect();
            let mut value = entry.payload;
            migrate_legacy_evidence_projection(&mut value);
            let payload = LegacyPayload::new(value, &self.scope)
                .map_err(|error| decode_error(LegacyCodecErrorKind::Payload(error)))?;
            let deferrable = payload.attention.is_some();
            entries.push_back(state::Entry {
                id: entry.id,
                payload,
                replay_keys,
                consumer_id: entry.consumer_id,
                endpoint: endpoint(entry.live_thread_id)?,
                visibility,
                deferrable,
                deferred_until: entry.deferred_until,
                lease: entry.lease.map(|value| state::Lease {
                    token: value.token,
                    consumer_id: value.consumer_id,
                    expires_at: value.expires_at,
                }),
            });
        }
        Ok(QueueSnapshot {
            next_id: legacy.next_id,
            next_lease_generation,
            next_consumer_generation,
            primary_consumer: legacy.primary_consumer,
            endpoint: endpoint(legacy.live_thread_id)?,
            consumers,
            acknowledged,
            entries,
        })
    }
    fn encode(
        &self,
        snapshot: &QueueSnapshot<LegacyPayload, LegacyReplayIdentity>,
    ) -> Result<Vec<u8>, CodecError> {
        let consumers = snapshot
            .consumers
            .iter()
            .map(|value| ConsumerRegistration {
                id: value.id.clone(),
                label: value.label.clone(),
                kind: match value.kind {
                    state::ConsumerKind::Pull => ConsumerKind::Pull,
                    state::ConsumerKind::Resident => ConsumerKind::Live,
                },
                expires_at: value.expires_at,
                ttl_seconds: value.ttl_seconds,
            })
            .collect();
        let mut processed_message_ids = VecDeque::new();
        let mut processed_provider_event_keys = VecDeque::new();
        for value in &snapshot.acknowledged {
            match &value.key {
                LegacyReplayIdentity::DiscordCreate(id) => processed_message_ids.push_back(*id),
                LegacyReplayIdentity::Provider(key) => {
                    processed_provider_event_keys.push_back(ProcessedProviderEvent {
                        key: key.clone(),
                        acknowledged_at: value.acknowledged_at,
                    })
                }
            }
        }
        let mut entries = VecDeque::new();
        for value in &snapshot.entries {
            let mut discord_message_id = None;
            let mut provider_event_key = None;
            for key in &value.replay_keys {
                match key {
                    LegacyReplayIdentity::DiscordCreate(id) => {
                        if discord_message_id.replace(*id).is_some() {
                            return Err(encode_error(LegacyCodecErrorKind::ReplayShape));
                        }
                    }
                    LegacyReplayIdentity::Provider(key) => {
                        if provider_event_key.replace(key.clone()).is_some() {
                            return Err(encode_error(LegacyCodecErrorKind::ReplayShape));
                        }
                    }
                }
            }
            entries.push_back(QueuedEvent {
                id: value.id,
                payload: value.payload.value.clone(),
                discord_message_id,
                provider_event_key,
                consumer_id: value.consumer_id.clone(),
                live_thread_id: thread(&value.endpoint)?,
                attention: value.payload.scheduling(),
                deferred_until: value.deferred_until,
                lease: value.lease.as_ref().map(|lease| Lease {
                    token: lease.token.clone(),
                    consumer_id: lease.consumer_id.clone(),
                    expires_at: lease.expires_at,
                }),
            });
        }
        let legacy = InboxState {
            next_id: snapshot.next_id,
            next_lease_generation: Some(snapshot.next_lease_generation),
            next_consumer_generation: Some(snapshot.next_consumer_generation),
            primary_consumer: snapshot.primary_consumer.clone(),
            live_thread_id: thread(&snapshot.endpoint)?,
            consumers,
            processed_message_ids,
            processed_provider_event_keys,
            entries,
        };
        serde_json::to_vec_pretty(&legacy).map_err(|error| CodecError {
            kind: CodecErrorKind::Encode(Box::new(LegacyCodecError {
                kind: LegacyCodecErrorKind::Encode(error),
            })),
        })
    }
}

#[cfg(test)]
#[path = "queue_codec_tests.rs"]
mod tests;
