//! Independent workshop queue format used by the common owner's integration tests.

use crate::durable::{
    codec::QueueCodec,
    error::{CodecError, CodecErrorKind},
    identity::{ConsumerId, DeliveryToken, EndpointId, EventId},
    state::{
        ConsumerKind, ConsumerRegistration, Entry, EntryVisibility, Lease, QueueSnapshot,
        ReplayReceipt,
    },
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WorkItem {
    pub station: String,
    pub quantity: u16,
}
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum WorkKey {
    Batch(u64),
    Reservation(u64),
}
pub(super) struct WorkshopCodec;

// Decimal strings preserve the full u64 domain independently of TOML's signed integers.
#[derive(Serialize, Deserialize)]
struct WorkshopFile {
    workshop_format: u8,
    next_ticket: String,
    next_checkout: String,
    next_worker: String,
    foreman: Option<ConsumerId>,
    destination: Option<EndpointId>,
    workers: Vec<Worker>,
    receipts: Vec<Receipt>,
    jobs: Vec<Job>,
}
#[derive(Serialize, Deserialize)]
struct Worker {
    id: ConsumerId,
    label: String,
    role: ConsumerKind,
    until: DateTime<Utc>,
    ttl: String,
}
#[derive(Serialize, Deserialize)]
struct Receipt {
    key: Key,
    at: DateTime<Utc>,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "family", content = "number")]
enum Key {
    Batch(String),
    Reservation(String),
}
#[derive(Serialize, Deserialize)]
struct Job {
    ticket: String,
    station: String,
    quantity: u16,
    keys: Vec<Key>,
    worker: Option<ConsumerId>,
    destination: Option<EndpointId>,
    visibility: EntryVisibility,
    deferrable: bool,
    deferred_until: Option<DateTime<Utc>>,
    checkout: Option<Checkout>,
}
#[derive(Serialize, Deserialize)]
struct Checkout {
    token: DeliveryToken,
    worker: Option<ConsumerId>,
    until: DateTime<Utc>,
}
fn decode_error(error: impl std::error::Error + Send + Sync + 'static) -> CodecError {
    CodecError {
        kind: CodecErrorKind::Decode(Box::new(error)),
    }
}
fn number(value: &str) -> Result<u64, CodecError> {
    value.parse().map_err(decode_error)
}
impl From<&WorkKey> for Key {
    fn from(value: &WorkKey) -> Self {
        match value {
            WorkKey::Batch(n) => Self::Batch(n.to_string()),
            WorkKey::Reservation(n) => Self::Reservation(n.to_string()),
        }
    }
}
impl Key {
    fn into_domain(self) -> Result<WorkKey, CodecError> {
        match self {
            Self::Batch(n) => Ok(WorkKey::Batch(number(&n)?)),
            Self::Reservation(n) => Ok(WorkKey::Reservation(number(&n)?)),
        }
    }
}
impl QueueCodec for WorkshopCodec {
    type Payload = WorkItem;
    type Key = WorkKey;
    fn decode(&self, bytes: &[u8]) -> Result<QueueSnapshot<WorkItem, WorkKey>, CodecError> {
        let text = std::str::from_utf8(bytes).map_err(decode_error)?;
        let file: WorkshopFile = serde_path_to_error::deserialize(
            toml::Deserializer::parse(text).map_err(decode_error)?,
        )
        .map_err(decode_error)?;
        if file.workshop_format != 7 {
            return Err(CodecError {
                kind: CodecErrorKind::UnsupportedVersion,
            });
        }
        Ok(QueueSnapshot {
            next_id: number(&file.next_ticket)?,
            next_lease_generation: number(&file.next_checkout)?,
            next_consumer_generation: number(&file.next_worker)?,
            primary_consumer: file.foreman,
            endpoint: file.destination,
            consumers: file
                .workers
                .into_iter()
                .map(|w| {
                    Ok(ConsumerRegistration {
                        id: w.id,
                        label: w.label,
                        kind: w.role,
                        expires_at: w.until,
                        ttl_seconds: number(&w.ttl)?,
                    })
                })
                .collect::<Result<_, CodecError>>()?,
            acknowledged: file
                .receipts
                .into_iter()
                .map(|r| {
                    Ok(ReplayReceipt {
                        key: r.key.into_domain()?,
                        acknowledged_at: r.at,
                    })
                })
                .collect::<Result<_, CodecError>>()?,
            entries: file
                .jobs
                .into_iter()
                .map(|j| {
                    Ok(Entry {
                        id: EventId::new(number(&j.ticket)?),
                        payload: WorkItem {
                            station: j.station,
                            quantity: j.quantity,
                        },
                        replay_keys: j
                            .keys
                            .into_iter()
                            .map(Key::into_domain)
                            .collect::<Result<_, _>>()?,
                        consumer_id: j.worker,
                        endpoint: j.destination,
                        visibility: j.visibility,
                        deferrable: j.deferrable,
                        deferred_until: j.deferred_until,
                        lease: j.checkout.map(|c| Lease {
                            token: c.token,
                            consumer_id: c.worker,
                            expires_at: c.until,
                        }),
                    })
                })
                .collect::<Result<_, CodecError>>()?,
        })
    }
    fn encode(&self, state: &QueueSnapshot<WorkItem, WorkKey>) -> Result<Vec<u8>, CodecError> {
        let file = WorkshopFile {
            workshop_format: 7,
            next_ticket: state.next_id.to_string(),
            next_checkout: state.next_lease_generation.to_string(),
            next_worker: state.next_consumer_generation.to_string(),
            foreman: state.primary_consumer.clone(),
            destination: state.endpoint.clone(),
            workers: state
                .consumers
                .iter()
                .map(|w| Worker {
                    id: w.id.clone(),
                    label: w.label.clone(),
                    role: w.kind,
                    until: w.expires_at,
                    ttl: w.ttl_seconds.to_string(),
                })
                .collect(),
            receipts: state
                .acknowledged
                .iter()
                .map(|r| Receipt {
                    key: Key::from(&r.key),
                    at: r.acknowledged_at,
                })
                .collect(),
            jobs: state
                .entries
                .iter()
                .map(|j| Job {
                    ticket: j.id.get().to_string(),
                    station: j.payload.station.clone(),
                    quantity: j.payload.quantity,
                    keys: j.replay_keys.iter().map(Key::from).collect(),
                    worker: j.consumer_id.clone(),
                    destination: j.endpoint.clone(),
                    visibility: j.visibility,
                    deferrable: j.deferrable,
                    deferred_until: j.deferred_until,
                    checkout: j.lease.as_ref().map(|c| Checkout {
                        token: c.token.clone(),
                        worker: c.consumer_id.clone(),
                        until: c.expires_at,
                    }),
                })
                .collect(),
        };
        toml::to_string(&file)
            .map(String::into_bytes)
            .map_err(|error| CodecError {
                kind: CodecErrorKind::Encode(Box::new(error)),
            })
    }
}
