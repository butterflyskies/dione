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
pub enum ConsumerKind {
    /// Registration available to pull consumers.
    Pull,
    /// Registration usable only through an owner-created resident capability.
    Resident,
}
/// Detached consumer registration passed to and from an adapter codec.
#[derive(Debug, Clone)]
pub struct ConsumerRegistration {
    /// Exact retained registration identity.
    pub id: ConsumerId,
    /// Diagnostic label, never a credential.
    pub label: String,
    /// Retained registration authority class.
    pub kind: ConsumerKind,
    /// Registration expiration.
    pub expires_at: DateTime<Utc>,
    /// Renewal lifetime in seconds.
    pub ttl_seconds: u64,
}
/// Detached lease passed to and from an adapter codec.
#[derive(Debug, Clone)]
pub struct Lease {
    /// Exact token required to settle this lease.
    pub token: DeliveryToken,
    /// Registration assigned to the lease, if retained by the format.
    pub consumer_id: Option<ConsumerId>,
    /// Expiration after which the lease may be replaced.
    pub expires_at: DateTime<Utc>,
}
/// Detached queued entry passed to and from an adapter codec.
#[derive(Debug, Clone)]
pub struct Entry<P, K> {
    /// Stable event identity.
    pub id: EventId,
    /// Adapter-owned immutable payload.
    pub payload: P,
    /// Replay identities retained for this entry.
    pub replay_keys: Vec<K>,
    /// Registration currently assigned to the entry.
    pub consumer_id: Option<ConsumerId>,
    /// Exact retained delivery endpoint.
    pub endpoint: Option<EndpointId>,
    /// Whether a pull consumer may acquire the entry.
    pub visibility: EntryVisibility,
    /// Whether admission permits temporary delivery deferral.
    pub deferrable: bool,
    /// Earliest permitted delivery time when deferred.
    pub deferred_until: Option<DateTime<Utc>>,
    /// Current lease, if one is retained.
    pub lease: Option<Lease>,
}
/// Detached acknowledged replay fence passed to and from an adapter codec.
#[derive(Debug, Clone)]
pub struct ReplayReceipt<K> {
    /// Exact replay identity.
    pub key: K,
    /// Time the owner acknowledged this identity.
    pub acknowledged_at: DateTime<Utc>,
}
/// Detached codec transfer value; decoded snapshots are validated before installation.
///
/// Public fields let adapter codecs preserve their own complete disk schemas.
/// Editing this value does not mutate a live queue.
#[derive(Debug, Clone)]
pub struct QueueSnapshot<P, K> {
    /// Next event allocation counter, including the exhausted sentinel.
    pub next_id: u64,
    /// Next lease allocation counter, including the exhausted sentinel.
    pub next_lease_generation: u64,
    /// Next registration allocation counter, including the exhausted sentinel.
    pub next_consumer_generation: u64,
    /// Retained primary registration.
    pub primary_consumer: Option<ConsumerId>,
    /// Retained resident delivery endpoint.
    pub endpoint: Option<EndpointId>,
    /// Retained registrations.
    pub consumers: Vec<ConsumerRegistration>,
    /// Retained acknowledged replay fences in insertion order.
    pub acknowledged: VecDeque<ReplayReceipt<K>>,
    /// Retained queue entries in admission order.
    pub entries: VecDeque<Entry<P, K>>,
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
