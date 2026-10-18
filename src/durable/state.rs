//! Snapshot transfer types; only the durable owner mutates a live snapshot.

use super::identity::{ConsumerId, DeliveryToken, EndpointId, EventId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::VecDeque, time::Duration};

/// Immutable replay policy family selected by the configured key classifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayClass {
    /// Queued identities plus a bounded FIFO of acknowledged identities.
    Rolling,
    /// Queued identities and time-limited acknowledged admission fences.
    Timed,
}
/// Privacy is stored independently of replay identity and current routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryVisibility {
    /// A pull consumer may claim and lease this record.
    Public,
    /// Only an owner-created resident capability may consume this record.
    ResidentOnly,
}
/// Persistent registration role; labels never grant authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ConsumerKind {
    Pull,
    Resident,
}
#[derive(Debug, Clone)]
pub(crate) struct ConsumerRegistration {
    pub id: ConsumerId,
    pub label: String,
    pub kind: ConsumerKind,
    pub expires_at: DateTime<Utc>,
    pub ttl_seconds: u64,
}
#[derive(Debug, Clone)]
pub(crate) struct Lease {
    pub token: DeliveryToken,
    pub consumer_id: Option<ConsumerId>,
    pub expires_at: DateTime<Utc>,
}
#[derive(Debug, Clone)]
pub(crate) struct Entry<P, K> {
    pub id: EventId,
    pub payload: P,
    pub replay_keys: Vec<K>,
    pub consumer_id: Option<ConsumerId>,
    pub endpoint: Option<EndpointId>,
    pub visibility: EntryVisibility,
    pub deferrable: bool,
    pub deferred_until: Option<DateTime<Utc>>,
    pub lease: Option<Lease>,
}
#[derive(Debug, Clone)]
pub(crate) struct ReplayReceipt<K> {
    pub key: K,
    pub acknowledged_at: DateTime<Utc>,
}
/// Codec transfer value. Fields are crate-private to prevent arbitrary public restoration.
#[derive(Debug, Clone)]
pub struct QueueSnapshot<P, K> {
    pub(crate) next_id: u64,
    pub(crate) next_lease_generation: u64,
    pub(crate) next_consumer_generation: u64,
    pub(crate) primary_consumer: Option<ConsumerId>,
    pub(crate) endpoint: Option<EndpointId>,
    pub(crate) consumers: Vec<ConsumerRegistration>,
    pub(crate) acknowledged: VecDeque<ReplayReceipt<K>>,
    pub(crate) entries: VecDeque<Entry<P, K>>,
}
impl<P, K> Default for QueueSnapshot<P, K> {
    fn default() -> Self {
        Self {
            next_id: 0,
            next_lease_generation: 0,
            next_consumer_generation: 0,
            primary_consumer: None,
            endpoint: None,
            consumers: Vec::new(),
            acknowledged: VecDeque::new(),
            entries: VecDeque::new(),
        }
    }
}
/// Payload and lease identity returned by the common state machine.
#[derive(Debug, Clone)]
pub struct LeasedEvent<P> {
    /// Stable queue event identity.
    pub event_id: EventId,
    /// Exact token required to settle this lease.
    pub delivery_token: DeliveryToken,
    /// Lease expiry, after which another lease may replace it.
    pub lease_expires_at: DateTime<Utc>,
    /// Registration owning this lease.
    pub consumer_id: ConsumerId,
    /// Adapter-owned payload; the engine does not interpret it.
    pub payload: P,
}
#[derive(Debug, Clone)]
pub(super) struct LeasePoll<P> {
    pub event: Option<LeasedEvent<P>>,
    pub deferred_for: Option<Duration>,
}
/// Counts from one transactional immutable selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RemovalResult {
    /// Number of removed entries.
    pub removed: usize,
    /// Number of revoked leases among those entries.
    pub invalidated_leases: usize,
}
/// Durable queue status projected without exposing mutable state.
#[derive(Debug, Clone, Serialize)]
pub struct QueueStatus {
    /// Retained queue entries.
    pub queued: usize,
    /// Unexpired leases.
    pub leased: usize,
    /// Next event counter, including an exhausted max sentinel.
    pub next_event_id: EventId,
    /// Currently live primary registration.
    pub primary_consumer: Option<ConsumerId>,
    /// Live registration diagnostics; identifiers do not grant resident access.
    pub consumers: Vec<ConsumerStatus>,
    /// Entries with no assigned consumer.
    pub unassigned: usize,
}
/// Public registration diagnostics.
#[derive(Debug, Clone, Serialize)]
pub struct ConsumerStatus {
    /// Registration identity.
    pub consumer_id: ConsumerId,
    /// Presentation label, not authority.
    pub label: String,
    /// Current expiration.
    pub expires_at: DateTime<Utc>,
    /// Whether this is the current primary.
    pub primary: bool,
}
/// Newly persisted pull registration.
#[derive(Debug, Clone, Serialize)]
pub struct ConsumerRegistrationResult {
    /// Allocated registration identity.
    pub consumer_id: ConsumerId,
    /// Whether primary ownership was requested and installed.
    pub primary: bool,
    /// Initial expiration.
    pub expires_at: DateTime<Utc>,
}
/// Atomic primary handoff result.
#[derive(Debug, Clone, Serialize)]
pub struct HandoffResult {
    /// Previously active registration.
    pub previous_consumer_id: ConsumerId,
    /// New active registration.
    pub primary_consumer_id: ConsumerId,
    /// Pending public records moved.
    pub moved_pending: usize,
    /// Leases revoked while moving pending records.
    pub invalidated_leases: usize,
}
