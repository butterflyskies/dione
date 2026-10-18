//! Versioned resident-only OMP inbox schema, separate from the Codex inbox.
use super::types::{OmpPayload, SavedSessionId};
use crate::{
    delivery::ProviderEventKey,
    durable::{
        CodecError, CodecErrorKind, ConsumerId, DeliveryToken, EndpointId, EntryVisibility,
        EventId, QueueCodec, QueueSnapshot, state,
    },
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use thiserror::Error;

pub(super) struct OmpCodec;

#[derive(Debug, Error)]
enum FormatError {
    #[error("invalid OMP inbox JSON")]
    Decode(#[source] serde_path_to_error::Error<serde_json::Error>),
    #[error("trailing OMP inbox JSON")]
    Trailing(#[source] serde_json::Error),
    #[error("invalid OMP resident ownership")]
    Ownership,
    #[error("invalid OMP event replay identity")]
    Replay,
    #[error("invalid OMP notification payload")]
    Payload(#[source] super::types::OmpSessionError),
    #[error("invalid saved OMP session")]
    Session,
}
fn invalid(error: FormatError) -> CodecError {
    CodecError {
        kind: CodecErrorKind::Decode(Box::new(error)),
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u32,
    next_id: u64,
    next_lease_generation: u64,
    next_consumer_generation: u64,
    primary_consumer: Option<ConsumerId>,
    endpoint: Option<SavedSessionId>,
    consumers: Vec<Registration>,
    acknowledged: VecDeque<Receipt>,
    entries: VecDeque<Entry>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    id: ConsumerId,
    label: String,
    expires_at: DateTime<Utc>,
    ttl_seconds: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    key: ProviderEventKey,
    acknowledged_at: DateTime<Utc>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Lease {
    token: DeliveryToken,
    consumer_id: ConsumerId,
    expires_at: DateTime<Utc>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: EventId,
    payload: OmpPayload,
    replay_key: ProviderEventKey,
    consumer_id: ConsumerId,
    endpoint: SavedSessionId,
    lease: Option<Lease>,
}
fn session(value: &EndpointId) -> Result<SavedSessionId, CodecError> {
    SavedSessionId::parse(value.as_str()).map_err(|_| invalid(FormatError::Session))
}
impl QueueCodec for OmpCodec {
    type Payload = OmpPayload;
    type Key = ProviderEventKey;

    fn decode(
        &self,
        bytes: &[u8],
    ) -> Result<QueueSnapshot<OmpPayload, ProviderEventKey>, CodecError> {
        let mut decoder = serde_json::Deserializer::from_slice(bytes);
        let snapshot: Snapshot = serde_path_to_error::deserialize(&mut decoder)
            .map_err(|error| invalid(FormatError::Decode(error)))?;
        decoder
            .end()
            .map_err(|error| invalid(FormatError::Trailing(error)))?;
        if snapshot.version != 1 {
            return Err(CodecError {
                kind: CodecErrorKind::UnsupportedVersion,
            });
        }
        let mut entries = VecDeque::new();
        for entry in snapshot.entries {
            entry
                .payload
                .validate()
                .map_err(|error| invalid(FormatError::Payload(error)))?;
            if entry.payload.key() != entry.replay_key {
                return Err(invalid(FormatError::Replay));
            }
            if entry
                .lease
                .as_ref()
                .is_some_and(|lease| lease.consumer_id != entry.consumer_id)
            {
                return Err(invalid(FormatError::Ownership));
            }
            entries.push_back(state::Entry {
                id: entry.id,
                payload: entry.payload,
                replay_keys: vec![entry.replay_key],
                consumer_id: Some(entry.consumer_id),
                endpoint: Some(entry.endpoint.endpoint().clone()),
                visibility: EntryVisibility::ResidentOnly,
                deferrable: false,
                deferred_until: None,
                lease: entry.lease.map(|lease| state::Lease {
                    token: lease.token,
                    consumer_id: Some(lease.consumer_id),
                    expires_at: lease.expires_at,
                }),
            });
        }
        if snapshot
            .primary_consumer
            .as_ref()
            .is_some_and(|id| !snapshot.consumers.iter().any(|consumer| &consumer.id == id))
        {
            return Err(invalid(FormatError::Ownership));
        }
        Ok(QueueSnapshot {
            next_id: snapshot.next_id,
            next_lease_generation: snapshot.next_lease_generation,
            next_consumer_generation: snapshot.next_consumer_generation,
            primary_consumer: snapshot.primary_consumer,
            endpoint: snapshot.endpoint.map(|value| value.endpoint().clone()),
            consumers: snapshot
                .consumers
                .into_iter()
                .map(|consumer| state::ConsumerRegistration {
                    id: consumer.id,
                    label: consumer.label,
                    kind: state::ConsumerKind::Resident,
                    expires_at: consumer.expires_at,
                    ttl_seconds: consumer.ttl_seconds,
                })
                .collect(),
            acknowledged: snapshot
                .acknowledged
                .into_iter()
                .map(|receipt| state::ReplayReceipt {
                    key: receipt.key,
                    acknowledged_at: receipt.acknowledged_at,
                })
                .collect(),
            entries,
        })
    }
    fn encode(
        &self,
        snapshot: &QueueSnapshot<OmpPayload, ProviderEventKey>,
    ) -> Result<Vec<u8>, CodecError> {
        let consumers = snapshot
            .consumers
            .iter()
            .map(|consumer| {
                if consumer.kind != state::ConsumerKind::Resident {
                    return Err(invalid(FormatError::Ownership));
                }
                Ok(Registration {
                    id: consumer.id.clone(),
                    label: consumer.label.clone(),
                    expires_at: consumer.expires_at,
                    ttl_seconds: consumer.ttl_seconds,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let entries = snapshot
            .entries
            .iter()
            .map(|entry| {
                entry
                    .payload
                    .validate()
                    .map_err(|error| invalid(FormatError::Payload(error)))?;
                if entry.visibility != EntryVisibility::ResidentOnly
                    || entry.deferrable
                    || entry.deferred_until.is_some()
                {
                    return Err(invalid(FormatError::Ownership));
                }
                let [key] = entry.replay_keys.as_slice() else {
                    return Err(invalid(FormatError::Replay));
                };
                if *key != entry.payload.key() {
                    return Err(invalid(FormatError::Replay));
                }
                let consumer_id = entry
                    .consumer_id
                    .clone()
                    .ok_or_else(|| invalid(FormatError::Ownership))?;
                let endpoint = session(
                    entry
                        .endpoint
                        .as_ref()
                        .ok_or_else(|| invalid(FormatError::Ownership))?,
                )?;
                let lease = entry
                    .lease
                    .as_ref()
                    .map(|lease| {
                        if lease.consumer_id.as_ref() != Some(&consumer_id) {
                            return Err(invalid(FormatError::Ownership));
                        }
                        Ok(Lease {
                            token: lease.token.clone(),
                            consumer_id: consumer_id.clone(),
                            expires_at: lease.expires_at,
                        })
                    })
                    .transpose()?;
                Ok(Entry {
                    id: entry.id,
                    payload: entry.payload.clone(),
                    replay_key: key.clone(),
                    consumer_id,
                    endpoint,
                    lease,
                })
            })
            .collect::<Result<VecDeque<_>, CodecError>>()?;
        serde_json::to_vec(&Snapshot {
            version: 1,
            next_id: snapshot.next_id,
            next_lease_generation: snapshot.next_lease_generation,
            next_consumer_generation: snapshot.next_consumer_generation,
            primary_consumer: snapshot.primary_consumer.clone(),
            endpoint: snapshot.endpoint.as_ref().map(session).transpose()?,
            consumers,
            entries,
            acknowledged: snapshot
                .acknowledged
                .iter()
                .map(|receipt| Receipt {
                    key: receipt.key.clone(),
                    acknowledged_at: receipt.acknowledged_at,
                })
                .collect(),
        })
        .map_err(|error| CodecError {
            kind: CodecErrorKind::Encode(Box::new(error)),
        })
    }
}
