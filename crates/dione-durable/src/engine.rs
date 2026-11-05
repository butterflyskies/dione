//! Pure transitions over an owned generic snapshot.
use super::{
    Admission, AllocationKind, ConsumerId, ConsumerRegistrationResult, ConsumerStatus,
    DeliveryToken, EndpointId, EntryVisibility, EventId, HandoffResult, IdentityError,
    IdentityErrorKind, LeasedEvent, QueueError, QueueErrorKind, QueueOptions, QueueSnapshot,
    QueueStatus, RemovalResult, ReplayClass, SnapshotIssue,
    state::{ConsumerKind, ConsumerRegistration, Entry, Lease, LeasePoll, ReplayReceipt},
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::{collections::HashSet, hash::Hash, time::Duration};

pub(super) enum Access<'a> {
    Pull(&'a ConsumerId),
    Resident(&'a ConsumerId),
}
impl Access<'_> {
    fn id(&self) -> &ConsumerId {
        match self {
            Self::Pull(id) | Self::Resident(id) => id,
        }
    }
    fn permits(&self, entry_visibility: EntryVisibility) -> bool {
        matches!(self, Self::Resident(_)) || entry_visibility == EntryVisibility::Public
    }
}
fn error(kind: QueueErrorKind) -> QueueError {
    QueueError::new("durable queue transition", kind)
}
fn integrity(issue: SnapshotIssue) -> QueueError {
    error(QueueErrorKind::Integrity(issue))
}
fn canonical_number(text: &str) -> Option<u64> {
    let value = text.parse::<u64>().ok()?;
    (value.to_string() == text).then_some(value)
}
fn token_generation(token: &DeliveryToken) -> Option<u64> {
    let tail = token.as_str().strip_prefix("dione-")?;
    let (event, generation) = tail.split_once('-')?;
    canonical_number(event)?;
    canonical_number(generation)
}
pub(super) fn validate_snapshot<P, K: Eq + Hash>(
    state: &QueueSnapshot<P, K>,
    options: &QueueOptions<K>,
) -> Result<(), QueueError> {
    let mut events = HashSet::new();
    let mut consumers = HashSet::new();
    let mut tokens = HashSet::new();
    let mut keys = HashSet::new();
    for consumer in &state.consumers {
        if !consumers.insert(&consumer.id) {
            return Err(integrity(SnapshotIssue::DuplicateConsumer));
        }
    }
    for id in state
        .consumers
        .iter()
        .map(|consumer| &consumer.id)
        .chain(state.primary_consumer.iter())
        .chain(
            state
                .entries
                .iter()
                .filter_map(|entry| entry.consumer_id.as_ref()),
        )
        .chain(
            state
                .entries
                .iter()
                .filter_map(|entry| entry.lease.as_ref())
                .filter_map(|lease| lease.consumer_id.as_ref()),
        )
    {
        if id
            .as_str()
            .strip_prefix(options.consumer_prefix)
            .and_then(canonical_number)
            .is_some_and(|generation| generation >= state.next_consumer_generation)
        {
            return Err(integrity(SnapshotIssue::StaleCounter));
        }
    }
    for entry in &state.entries {
        if !events.insert(entry.id) {
            return Err(integrity(SnapshotIssue::DuplicateEvent));
        }
        if entry.id.get() >= state.next_id {
            return Err(integrity(SnapshotIssue::StaleCounter));
        }
        let mut local_keys = HashSet::new();
        for key in &entry.replay_keys {
            if !local_keys.insert(key) {
                return Err(integrity(SnapshotIssue::RepeatedReplay));
            }
            if !keys.insert(key) {
                return Err(integrity(SnapshotIssue::DuplicateReplay));
            }
        }
        if let Some(lease) = &entry.lease {
            if !tokens.insert(&lease.token) {
                return Err(integrity(SnapshotIssue::DuplicateLease));
            }
            if token_generation(&lease.token)
                .is_some_and(|generation| generation >= state.next_lease_generation)
            {
                return Err(integrity(SnapshotIssue::StaleCounter));
            }
        }
    }
    Ok(())
}
fn allocate(counter: &mut u64, allocation: AllocationKind) -> Result<u64, QueueError> {
    let next = counter
        .checked_add(1)
        .ok_or_else(|| error(QueueErrorKind::AllocationExhausted { allocation }))?;
    let allocated = *counter;
    *counter = next;
    Ok(allocated)
}
fn expiry(now: DateTime<Utc>, duration: Duration) -> DateTime<Utc> {
    now + ChronoDuration::from_std(duration).unwrap_or_else(|_| ChronoDuration::hours(1))
}
fn expire<P, K>(state: &mut QueueSnapshot<P, K>, now: DateTime<Utc>) {
    state.consumers.retain(|consumer| consumer.expires_at > now);
    if state.primary_consumer.as_ref().is_some_and(|primary| {
        !state
            .consumers
            .iter()
            .any(|consumer| &consumer.id == primary)
    }) {
        state.primary_consumer = None;
    }
}
fn touch<P, K>(
    state: &mut QueueSnapshot<P, K>,
    access: &Access<'_>,
    now: DateTime<Utc>,
) -> Result<(), QueueError> {
    expire(state, now);
    let consumer = state
        .consumers
        .iter_mut()
        .find(|consumer| &consumer.id == access.id())
        .ok_or_else(|| error(QueueErrorKind::UnknownConsumer))?;
    let authorized = matches!(
        (access, consumer.kind),
        (Access::Pull(_), ConsumerKind::Pull) | (Access::Resident(_), ConsumerKind::Resident)
    );
    if !authorized {
        return Err(error(QueueErrorKind::UnknownConsumer));
    }
    consumer.expires_at = expiry(now, Duration::from_secs(consumer.ttl_seconds.min(86_400)));
    Ok(())
}
pub(super) fn duplicate<P, K: Eq>(
    state: &QueueSnapshot<P, K>,
    options: &QueueOptions<K>,
    key: &K,
    now: DateTime<Utc>,
) -> bool {
    state
        .entries
        .iter()
        .any(|entry| entry.replay_keys.contains(key))
        || state.acknowledged.iter().any(|receipt| {
            &receipt.key == key
                && ((options.replay_class)(key) == ReplayClass::Rolling
                    || receipt.acknowledged_at > now - options.timed_retention)
        })
}
pub(super) enum AdmissionRoute {
    Public,
    Resident,
}
pub(super) struct RegistrationRequest {
    pub label: String,
    pub ttl: Duration,
    pub make_primary: bool,
    pub claim_unassigned: bool,
    pub kind: ConsumerKind,
}
pub(super) fn enqueue<P, K: Clone + Eq + Hash>(
    state: &mut QueueSnapshot<P, K>,
    options: &QueueOptions<K>,
    admission: Admission<P, K>,
    route: AdmissionRoute,
    now: DateTime<Utc>,
) -> Result<bool, QueueError> {
    let Admission {
        payload,
        replay_key: key,
        deferrable,
    } = admission;
    let resident = matches!(route, AdmissionRoute::Resident);
    if key
        .as_ref()
        .is_some_and(|key| duplicate(state, options, key, now))
    {
        return Ok(false);
    }
    state.acknowledged.retain(|receipt| {
        (options.replay_class)(&receipt.key) == ReplayClass::Rolling
            || receipt.acknowledged_at > now - options.timed_retention
    });
    if key
        .as_ref()
        .is_some_and(|key| (options.replay_class)(key) == ReplayClass::Timed)
    {
        let retained: HashSet<&K> = state
            .entries
            .iter()
            .flat_map(|entry| &entry.replay_keys)
            .chain(state.acknowledged.iter().map(|receipt| &receipt.key))
            .filter(|key| (options.replay_class)(key) == ReplayClass::Timed)
            .collect();
        if retained.len() >= options.timed_limit {
            return Err(error(QueueErrorKind::ReplayCapacity));
        }
    }
    expire(state, now);
    let resident_primary = state
        .primary_consumer
        .as_ref()
        .and_then(|primary| {
            state
                .consumers
                .iter()
                .find(|consumer| &consumer.id == primary)
        })
        .is_some_and(|consumer| {
            consumer.kind == ConsumerKind::Resident && consumer.label == options.resident_label
        });
    let endpoint = if resident_primary {
        state.endpoint.clone()
    } else {
        None
    };
    if resident && (!resident_primary || endpoint.is_none()) {
        return Err(error(QueueErrorKind::ResidentUnavailable));
    }
    let id = EventId::new(allocate(&mut state.next_id, AllocationKind::Event)?);
    if state.entries.iter().any(|entry| entry.id == id) {
        return Err(integrity(SnapshotIssue::IdentityCollision));
    }
    let visibility = if resident || endpoint.is_some() {
        EntryVisibility::ResidentOnly
    } else {
        EntryVisibility::Public
    };
    state.entries.push_back(Entry {
        id,
        payload,
        replay_keys: key.into_iter().collect(),
        consumer_id: state.primary_consumer.clone(),
        endpoint,
        visibility,
        deferrable,
        deferred_until: None,
        lease: None,
    });
    Ok(true)
}
pub(super) fn lease<P: Clone, K>(
    state: &mut QueueSnapshot<P, K>,
    access: Access<'_>,
    endpoint: Option<&EndpointId>,
    now: DateTime<Utc>,
    duration: Duration,
) -> Result<LeasePoll<P>, QueueError> {
    touch(state, &access, now)?;
    for entry in &mut state.entries {
        if entry
            .lease
            .as_ref()
            .is_some_and(|lease| lease.expires_at <= now)
        {
            entry.lease = None;
        }
    }
    let eligible = |entry: &Entry<P, K>| {
        entry.lease.is_none()
            && entry.consumer_id.as_ref() == Some(access.id())
            && access.permits(entry.visibility)
            && match endpoint {
                Some(endpoint) => entry.endpoint.as_ref() == Some(endpoint),
                None => entry.visibility == EntryVisibility::Public,
            }
    };
    let selected = state
        .entries
        .iter()
        .position(|entry| eligible(entry) && entry.deferred_until.is_none_or(|until| until <= now));
    let Some(index) = selected else {
        let deferred_for = state
            .entries
            .iter()
            .filter(|entry| eligible(entry))
            .filter_map(|entry| entry.deferred_until)
            .filter(|until| *until > now)
            .min()
            .and_then(|until| (until - now).to_std().ok());
        return Ok(LeasePoll {
            event: None,
            deferred_for,
        });
    };
    let generation = allocate(&mut state.next_lease_generation, AllocationKind::Lease)?;
    let token = DeliveryToken::generated(state.entries[index].id, generation);
    if state
        .entries
        .iter()
        .filter_map(|entry| entry.lease.as_ref())
        .any(|lease| lease.token == token)
    {
        return Err(integrity(SnapshotIssue::IdentityCollision));
    }
    let entry = &mut state.entries[index];
    let expires_at = expiry(now, duration.min(Duration::from_secs(3600)));
    entry.deferred_until = None;
    entry.lease = Some(Lease {
        token: token.clone(),
        consumer_id: Some(access.id().clone()),
        expires_at,
    });
    Ok(LeasePoll {
        event: Some(LeasedEvent {
            event_id: entry.id,
            delivery_token: token,
            lease_expires_at: expires_at,
            consumer_id: access.id().clone(),
            payload: entry.payload.clone(),
        }),
        deferred_for: None,
    })
}
pub(super) fn ack_checked<P, K: Clone + Eq>(
    state: &mut QueueSnapshot<P, K>,
    options: &QueueOptions<K>,
    consumer: &ConsumerId,
    endpoint: &EndpointId,
    token: &DeliveryToken,
    now: DateTime<Utc>,
) -> Result<bool, QueueError> {
    let valid = state.entries.iter().any(|entry| {
        entry.visibility == EntryVisibility::ResidentOnly
            && entry.consumer_id.as_ref() == Some(consumer)
            && entry.endpoint.as_ref() == Some(endpoint)
            && entry.lease.as_ref().is_some_and(|lease| {
                &lease.token == token
                    && lease.consumer_id.as_ref() == Some(consumer)
                    && lease.expires_at > now
            })
    });
    if !valid {
        return Err(error(QueueErrorKind::UnknownDeliveryToken));
    }
    ack(state, options, Access::Resident(consumer), token, now)
}

pub(super) fn renew_resident<P, K>(
    state: &mut QueueSnapshot<P, K>,
    options: &QueueOptions<K>,
    endpoint: Option<&EndpointId>,
    now: DateTime<Utc>,
) -> Result<ConsumerRegistrationResult, QueueError> {
    let registration = register(
        state,
        options,
        RegistrationRequest {
            label: options.resident_label.to_owned(),
            ttl: Duration::from_secs(86_400),
            make_primary: true,
            claim_unassigned: false,
            kind: ConsumerKind::Resident,
        },
        now,
    )?;
    for entry in &mut state.entries {
        let orphaned = entry.consumer_id.as_ref().is_none_or(|id| {
            !state
                .consumers
                .iter()
                .any(|consumer| &consumer.id == id && consumer.expires_at > now)
        });
        if entry.visibility == EntryVisibility::ResidentOnly
            && endpoint.is_some()
            && entry.endpoint.as_ref() == endpoint
            && orphaned
        {
            entry.consumer_id = Some(registration.consumer_id.clone());
            entry.lease = None;
        }
    }
    Ok(registration)
}

pub(super) fn rebind_resident<P, K>(
    state: &mut QueueSnapshot<P, K>,
    options: &QueueOptions<K>,
    endpoint: EndpointId,
    now: DateTime<Utc>,
) -> Result<ConsumerRegistrationResult, QueueError> {
    let registration = renew_resident(state, options, Some(&endpoint), now)?;
    for entry in &mut state.entries {
        if entry.consumer_id.as_ref() == Some(&registration.consumer_id)
            && entry.endpoint.as_ref() == Some(&endpoint)
            && entry.visibility == EntryVisibility::ResidentOnly
        {
            entry.lease = None;
        }
    }
    state.endpoint = Some(endpoint);
    Ok(registration)
}

pub(super) fn ack<P, K: Clone + Eq>(
    state: &mut QueueSnapshot<P, K>,
    options: &QueueOptions<K>,
    access: Access<'_>,
    token: &DeliveryToken,
    now: DateTime<Utc>,
) -> Result<bool, QueueError> {
    touch(state, &access, now)?;
    let index = state
        .entries
        .iter()
        .position(|entry| {
            entry.lease.as_ref().is_some_and(|lease| {
                &lease.token == token && lease.consumer_id.as_ref() == Some(access.id())
            })
        })
        .ok_or_else(|| error(QueueErrorKind::UnknownDeliveryToken))?;
    if !access.permits(state.entries[index].visibility) {
        return Err(error(QueueErrorKind::UnknownConsumer));
    }
    let Some(entry) = state.entries.remove(index) else {
        return Err(error(QueueErrorKind::UnknownDeliveryToken));
    };
    for key in entry.replay_keys {
        state.acknowledged.retain(|receipt| receipt.key != key);
        state.acknowledged.push_back(ReplayReceipt {
            key,
            acknowledged_at: now,
        });
    }
    let mut rolling = state
        .acknowledged
        .iter()
        .filter(|receipt| (options.replay_class)(&receipt.key) == ReplayClass::Rolling)
        .count();
    state.acknowledged.retain(|receipt| {
        if (options.replay_class)(&receipt.key) == ReplayClass::Rolling
            && rolling > options.rolling_limit
        {
            rolling -= 1;
            false
        } else {
            true
        }
    });
    Ok(true)
}
pub(super) fn defer<P, K>(
    state: &mut QueueSnapshot<P, K>,
    consumer: &ConsumerId,
    token: &DeliveryToken,
    now: DateTime<Utc>,
    delay: Duration,
) -> Result<bool, QueueError> {
    touch(state, &Access::Resident(consumer), now)?;
    let entry = state
        .entries
        .iter_mut()
        .find(|entry| {
            entry.lease.as_ref().is_some_and(|lease| {
                &lease.token == token && lease.consumer_id.as_ref() == Some(consumer)
            })
        })
        .ok_or_else(|| error(QueueErrorKind::UnknownDeliveryToken))?;
    if !entry.deferrable {
        return Err(error(QueueErrorKind::NotDeferrable));
    }
    entry.lease = None;
    entry.deferred_until = Some(expiry(
        now,
        delay.clamp(Duration::from_millis(100), Duration::from_secs(30)),
    ));
    Ok(true)
}
pub(super) fn invalidate<P, K>(
    state: &mut QueueSnapshot<P, K>,
    predicate: impl Fn(&P) -> bool,
) -> RemovalResult {
    let mut result = RemovalResult {
        removed: 0,
        invalidated_leases: 0,
    };
    state.entries.retain(|entry| {
        if predicate(&entry.payload) {
            result.removed += 1;
            result.invalidated_leases += usize::from(entry.lease.is_some());
            false
        } else {
            true
        }
    });
    result
}
pub(super) fn status<P, K>(state: &QueueSnapshot<P, K>, now: DateTime<Utc>) -> QueueStatus {
    let primary = state
        .primary_consumer
        .as_ref()
        .filter(|id| {
            state
                .consumers
                .iter()
                .any(|consumer| &consumer.id == *id && consumer.expires_at > now)
        })
        .cloned();
    QueueStatus {
        queued: state.entries.len(),
        leased: state
            .entries
            .iter()
            .filter(|entry| {
                entry
                    .lease
                    .as_ref()
                    .is_some_and(|lease| lease.expires_at > now)
            })
            .count(),
        next_event_id: EventId::new(state.next_id),
        primary_consumer: primary.clone(),
        consumers: state
            .consumers
            .iter()
            .filter(|consumer| consumer.expires_at > now)
            .map(|consumer| ConsumerStatus {
                consumer_id: consumer.id.clone(),
                label: consumer.label.clone(),
                expires_at: consumer.expires_at,
                primary: primary.as_ref() == Some(&consumer.id),
            })
            .collect(),
        unassigned: state
            .entries
            .iter()
            .filter(|entry| entry.consumer_id.is_none())
            .count(),
    }
}
pub(super) fn register<P, K>(
    state: &mut QueueSnapshot<P, K>,
    options: &QueueOptions<K>,
    request: RegistrationRequest,
    now: DateTime<Utc>,
) -> Result<ConsumerRegistrationResult, QueueError> {
    let RegistrationRequest {
        label,
        ttl,
        make_primary,
        claim_unassigned,
        kind,
    } = request;
    let resident = kind == ConsumerKind::Resident;
    if resident
        && let Some(consumer) = state.primary_consumer.as_ref().and_then(|primary| {
            state.consumers.iter_mut().find(|consumer| {
                &consumer.id == primary
                    && consumer.kind == ConsumerKind::Resident
                    && consumer.label == options.resident_label
            })
        })
    {
        consumer.ttl_seconds = 86_400;
        consumer.expires_at = expiry(now, Duration::from_secs(86_400));
        return Ok(ConsumerRegistrationResult {
            consumer_id: consumer.id.clone(),
            primary: true,
            expires_at: consumer.expires_at,
        });
    } else if !resident && label.trim() == options.resident_label {
        return Err(error(QueueErrorKind::Identity(IdentityError {
            kind: IdentityErrorKind::Consumer,
        })));
    }
    expire(state, now);
    if make_primary && state.primary_consumer.is_some() {
        return Err(error(QueueErrorKind::PrimaryConsumerExists));
    }
    let generation = allocate(
        &mut state.next_consumer_generation,
        AllocationKind::Consumer,
    )?;
    let id = ConsumerId::from_stored(&format!("{}{generation}", options.consumer_prefix))
        .map_err(|cause| error(QueueErrorKind::Identity(cause)))?;
    if state.consumers.iter().any(|consumer| consumer.id == id)
        || state.entries.iter().any(|entry| {
            entry.consumer_id.as_ref() == Some(&id)
                || entry
                    .lease
                    .as_ref()
                    .is_some_and(|lease| lease.consumer_id.as_ref() == Some(&id))
        })
    {
        return Err(integrity(SnapshotIssue::IdentityCollision));
    }
    let ttl = ttl.min(Duration::from_secs(86_400));
    let ttl_seconds = ttl.as_secs();
    let expires_at = expiry(now, ttl);
    state.consumers.push(ConsumerRegistration {
        id: id.clone(),
        label,
        kind: if resident {
            ConsumerKind::Resident
        } else {
            ConsumerKind::Pull
        },
        expires_at,
        ttl_seconds,
    });
    if make_primary {
        state.primary_consumer = Some(id.clone());
        if claim_unassigned && !resident {
            for entry in &mut state.entries {
                if entry.consumer_id.is_none() && entry.visibility == EntryVisibility::Public {
                    entry.consumer_id = Some(id.clone());
                }
            }
        }
    }
    Ok(ConsumerRegistrationResult {
        consumer_id: id,
        primary: make_primary,
        expires_at,
    })
}
pub(super) fn handoff<P, K>(
    state: &mut QueueSnapshot<P, K>,
    from: &ConsumerId,
    to: &ConsumerId,
    move_pending: bool,
    now: DateTime<Utc>,
) -> Result<HandoffResult, QueueError> {
    for id in [from, to] {
        if state
            .consumers
            .iter()
            .any(|consumer| &consumer.id == id && consumer.kind == ConsumerKind::Resident)
        {
            return Err(error(QueueErrorKind::UnknownConsumer));
        }
    }
    expire(state, now);
    if state.primary_consumer.as_ref() != Some(from) {
        return Err(error(QueueErrorKind::NotPrimaryConsumer));
    }
    touch(state, &Access::Pull(to), now)?;
    let mut moved_pending = 0;
    let mut invalidated_leases = 0;
    if move_pending {
        for entry in &mut state.entries {
            if entry.consumer_id.as_ref() == Some(from)
                && entry.visibility == EntryVisibility::Public
            {
                entry.consumer_id = Some(to.clone());
                moved_pending += 1;
                invalidated_leases += usize::from(entry.lease.take().is_some());
            }
        }
    }
    state.primary_consumer = Some(to.clone());
    Ok(HandoffResult {
        previous_consumer_id: from.clone(),
        primary_consumer_id: to.clone(),
        moved_pending,
        invalidated_leases,
    })
}
pub(super) fn claim<P, K>(
    state: &mut QueueSnapshot<P, K>,
    consumer: &ConsumerId,
    claim_orphaned: bool,
    now: DateTime<Utc>,
) -> Result<usize, QueueError> {
    touch(state, &Access::Pull(consumer), now)?;
    if state.primary_consumer.is_some() {
        return Err(error(QueueErrorKind::PrimaryConsumerExists));
    }
    let active: HashSet<&ConsumerId> = state
        .consumers
        .iter()
        .map(|registration| &registration.id)
        .collect();
    let mut moved = 0;
    if claim_orphaned {
        for entry in &mut state.entries {
            if entry.visibility == EntryVisibility::Public
                && entry
                    .consumer_id
                    .as_ref()
                    .is_none_or(|id| !active.contains(id))
            {
                entry.consumer_id = Some(consumer.clone());
                entry.lease = None;
                moved += 1;
            }
        }
    }
    state.primary_consumer = Some(consumer.clone());
    Ok(moved)
}
