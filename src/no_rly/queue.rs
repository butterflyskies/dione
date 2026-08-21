//! The hold queue — bounced messages parked under single-use handles.
//!
//! The queue is generic over its payload so the machinery survives judge
//! swaps: the contradictionary holds [`super::consent::ReplyRequest`]s today,
//! and a future judge can hold whatever its send path needs.
//!
//! # Invariants
//!
//! - **Single use**: [`HoldQueue::settle`] removes the entry; a settled
//!   handle can never be claimed again. There is no way to read a payload
//!   out of the queue without going through claim/settle.
//! - **No pre-emption**: handles are minted by [`HoldQueue::hold`] at bounce
//!   time — a handle for a message that has not bounced does not exist.
//! - **Expiry is time-based, not sweep-based**: [`HoldQueue::claim`] checks
//!   the deadline itself, so an entry past its TTL is dead even if the
//!   background sweep has not run yet.
//!
//! Time is passed in explicitly (`now: Instant`) so every invariant is
//! testable without sleeping — the same convention as `rate_limiter`.

use crate::{no_rly::judge::RejectReason, timestamp::Timestamp};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt,
    hash::{BuildHasher, Hasher, RandomState},
    time::{Duration, Instant},
};

/// A single-use claim ticket for one bounced message.
///
/// This is a consent token, not a security token: it only has meaning inside
/// the process-local queue that minted it, so uniqueness within the process
/// is sufficient. The queue does not survive a restart — but the journal
/// does, and shutdown drains every pending handle into it as expired.
///
/// **Trust boundary.** Handles are predictable (`nr-{16-bit nonce}-{seq}`) and
/// unscoped — anyone who can call the gate's verbs can guess and act on any
/// live handle. That is sound here because the gate is single-principal: the
/// one construct that bounced a message is the only caller of `no_rly` /
/// `rephrase`. If this gate is ever shared across principals, handles must
/// become unguessable and be scoped to their minter before then.
///
/// Serializes as its string form (e.g. `"nr-3f92-7"`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HoldHandle(String);

impl HoldHandle {
    /// Wrap a caller-supplied string for lookup. Constructing a handle grants
    /// nothing — it must match a live queue entry to mean anything.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// The handle's string form.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for HoldHandle {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for HoldHandle {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl fmt::Display for HoldHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One bounced message waiting on the queue.
#[derive(Debug, Clone)]
pub struct Held<T> {
    /// The queued payload — release sends exactly this.
    pub payload: T,
    /// Why it bounced.
    pub reason: RejectReason,
    /// The handle this bounce chains from, when a rephrase re-bounced.
    pub parent: Option<HoldHandle>,
    /// Wall-clock bounce time, recorded in the journal.
    pub bounced_at: Timestamp,
    /// Monotonic bounce time, for latency measurement.
    pub bounced_instant: Instant,
    /// Monotonic expiry deadline (`bounced_instant + ttl`).
    deadline: Instant,
    /// True while a delivery attempt for this entry is in flight — the queue
    /// lock is released across the Discord send, so a reserved entry must be
    /// invisible to sweep/drain/evict (it cannot be resolved out from under
    /// its in-flight send) and to a second concurrent claim (which sees it as
    /// unavailable). Set under the lock by [`HoldQueue::reserve`]; cleared by
    /// [`HoldQueue::settle`] (success) or [`HoldQueue::release_reservation`] /
    /// [`HoldQueue::record_partial`] (failure).
    in_flight: bool,
    /// Chunks already posted to Discord in a prior partial delivery. A retry
    /// resumes from `payload` (the undelivered remainder) so no chunk is sent
    /// twice; these IDs are folded into the final released set.
    pub sent_ids: Vec<u64>,
    /// The full held text before a partial delivery shrank `payload` to the
    /// undelivered remainder — recorded as the journal `message` so the audit
    /// row is the whole message, not just the trailing chunk.
    pub original_message: Option<String>,
    /// When a judged-clear rephrase replacement failed delivery, the original
    /// (withdrawn) text is retained here so a later resolution journals the
    /// (original, reason, sent-replacement) triple instead of losing it.
    pub withdrawn_original: Option<String>,
}

impl<T> Held<T> {
    /// Time elapsed since the bounce.
    pub fn latency(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.bounced_instant)
    }

    /// Time remaining before the entry expires (zero if already past).
    pub fn expires_in(&self, now: Instant) -> Duration {
        self.deadline.saturating_duration_since(now)
    }

    /// Whether the entry's deadline has passed.
    pub fn is_expired(&self, now: Instant) -> bool {
        now >= self.deadline
    }
}

/// Why [`HoldQueue::claim`] refused to hand back an entry.
#[derive(Debug)]
pub enum ClaimError<T> {
    /// The handle never existed, was already used, or expired and was swept.
    Unknown,
    /// The entry's deadline passed before the claim. The entry has been
    /// removed; it is returned so the caller can journal the expiry.
    Expired(Box<Held<T>>),
}

/// In-memory queue of bounced messages keyed by single-use handle.
#[derive(Debug)]
pub struct HoldQueue<T> {
    entries: HashMap<HoldHandle, Held<T>>,
    /// Random per-instance disambiguator baked into every handle, so handles
    /// from a previous process are visibly foreign to this one.
    nonce: u16,
    /// Monotonic mint counter — guarantees uniqueness within the process.
    seq: u64,
}

impl<T> HoldQueue<T> {
    /// An empty queue with a fresh random nonce.
    pub fn new() -> Self {
        let nonce = RandomState::new().build_hasher().finish() as u16;
        Self {
            entries: HashMap::new(),
            nonce,
            seq: 0,
        }
    }

    /// Park a bounced payload and mint its handle. `ttl` is captured per
    /// entry so a live config reload only affects future bounces.
    pub fn hold(
        &mut self,
        payload: T,
        reason: RejectReason,
        parent: Option<HoldHandle>,
        ttl: Duration,
        now: Instant,
    ) -> HoldHandle {
        self.seq += 1;
        let handle = HoldHandle(format!("nr-{:04x}-{}", self.nonce, self.seq));
        self.entries.insert(
            handle.clone(),
            Held {
                payload,
                reason,
                parent,
                bounced_at: Timestamp::now(),
                bounced_instant: now,
                // A pathological `ttl` (a mis-set `hold_ttl_secs` in the
                // billions of years) would overflow `now + ttl` and panic at
                // the first bounce; saturate to a far-future deadline instead
                // so a bad config never kills the request loop. The config
                // layer also caps `hold_ttl_secs`, so this is defense in depth.
                deadline: now
                    .checked_add(ttl)
                    .or_else(|| now.checked_add(Duration::from_secs(365 * 24 * 3600)))
                    .unwrap_or(now),
                in_flight: false,
                sent_ids: Vec::new(),
                original_message: None,
                withdrawn_original: None,
            },
        );
        handle
    }

    /// Look up a live entry, returning a snapshot of it. The entry stays
    /// queued — callers that act on the payload call [`HoldQueue::settle`]
    /// once the action succeeds, so a failed send does not burn the handle.
    ///
    /// A claim past the deadline removes the entry and reports it via
    /// [`ClaimError::Expired`] — expiry does not wait for the sweep.
    pub fn claim(&mut self, handle: &HoldHandle, now: Instant) -> Result<Held<T>, ClaimError<T>>
    where
        T: Clone,
    {
        match self.entries.get(handle) {
            None => Err(ClaimError::Unknown),
            Some(entry) if entry.is_expired(now) => {
                let entry = self
                    .entries
                    .remove(handle)
                    .expect("entry present under held map key");
                Err(ClaimError::Expired(Box::new(entry)))
            }
            Some(entry) => Ok(entry.clone()),
        }
    }

    /// Reserve a live entry for an in-flight delivery: hand back a snapshot
    /// and mark the stored entry `in_flight` so the queue lock can be released
    /// across the (slow) Discord send without the entry being swept, drained,
    /// evicted, or claimed by a second concurrent caller. The reservation is
    /// resolved by [`settle`](Self::settle) on success or
    /// [`release_reservation`](Self::release_reservation) /
    /// [`record_partial`](Self::record_partial) on failure.
    ///
    /// A handle already in flight reports [`ClaimError::Unknown`] — the second
    /// caller loses exactly as it would against a settled handle. Expiry is
    /// still enforced at reserve time for entries not in flight.
    pub fn reserve(&mut self, handle: &HoldHandle, now: Instant) -> Result<Held<T>, ClaimError<T>>
    where
        T: Clone,
    {
        match self.entries.get_mut(handle) {
            None => Err(ClaimError::Unknown),
            // Busy: a delivery is already in flight for this handle.
            Some(entry) if entry.in_flight => Err(ClaimError::Unknown),
            Some(entry) if entry.is_expired(now) => {
                let entry = self
                    .entries
                    .remove(handle)
                    .expect("entry present under held map key");
                Err(ClaimError::Expired(Box::new(entry)))
            }
            Some(entry) => {
                entry.in_flight = true;
                Ok(entry.clone())
            }
        }
    }

    /// Clear the in-flight mark set by [`reserve`](Self::reserve), returning
    /// the entry to the live pool for a later retry. No-op if the handle is
    /// gone. Used when a delivery attempt fails with nothing recoverable to
    /// resume from.
    pub fn release_reservation(&mut self, handle: &HoldHandle) {
        if let Some(entry) = self.entries.get_mut(handle) {
            entry.in_flight = false;
        }
    }

    /// Record a partial multi-chunk delivery so a retry resumes instead of
    /// restarting: replace the payload with the undelivered `remainder`,
    /// accumulate the `newly_sent` chunk IDs, preserve the `full_message` for
    /// the eventual journal record (set once), and clear the in-flight mark.
    /// No-op if the handle is gone.
    pub fn record_partial(
        &mut self,
        handle: &HoldHandle,
        remainder: T,
        newly_sent: Vec<u64>,
        full_message: String,
    ) {
        if let Some(entry) = self.entries.get_mut(handle) {
            if entry.original_message.is_none() {
                entry.original_message = Some(full_message);
            }
            entry.sent_ids.extend(newly_sent);
            entry.payload = remainder;
            entry.in_flight = false;
        }
    }

    /// Consume a handle after its payload was acted on. Returns the entry,
    /// or `None` if the handle was not live. After settling, the handle is
    /// dead: no claim, release, or rephrase can ever see it again.
    pub fn settle(&mut self, handle: &HoldHandle) -> Option<Held<T>> {
        self.entries.remove(handle)
    }

    /// Replace a live entry's payload in place, keeping its handle, reason,
    /// chain link, and deadline. Returns `false` when the handle is not
    /// live.
    ///
    /// Used by the rephrase path when a judged replacement fails to deliver:
    /// the entry must hold the replacement so a retry operates on the text
    /// the construct consented to and can never silently revert to the
    /// original.
    pub fn update_payload(&mut self, handle: &HoldHandle, payload: T) -> bool {
        match self.entries.get_mut(handle) {
            Some(entry) => {
                entry.payload = payload;
                true
            }
            None => false,
        }
    }

    /// Refresh a live entry's expiry deadline. No-op if the handle is gone.
    /// Used to grant a fresh decision window when a judged-clear rephrase
    /// merely failed delivery — symmetric with the fresh TTL a re-bounce mints.
    pub fn refresh_deadline(&mut self, handle: &HoldHandle, deadline: Instant) {
        if let Some(entry) = self.entries.get_mut(handle) {
            entry.deadline = deadline;
        }
    }

    /// Record (once) the original text withdrawn when a rephrase replacement
    /// was accepted, so a later resolution journals the (original, reason,
    /// replacement) triple rather than losing the original. No-op if the
    /// handle is gone or an original is already retained.
    pub fn set_withdrawn_original(&mut self, handle: &HoldHandle, original: String) {
        if let Some(entry) = self.entries.get_mut(handle)
            && entry.withdrawn_original.is_none()
        {
            entry.withdrawn_original = Some(original);
        }
    }

    /// Remove and return the entry closest to expiry, or `None` when no
    /// evictable entry exists. In-flight entries are skipped — an entry mid
    /// delivery is owned by its send and must not be evicted from under it.
    /// Capacity enforcement uses this: the entry nearest its deadline is the
    /// one whose eviction forfeits the least remaining decision window.
    pub fn evict_next_expiring(&mut self) -> Option<(HoldHandle, Held<T>)> {
        let handle = self
            .entries
            .iter()
            .filter(|(_, e)| !e.in_flight)
            .min_by_key(|(_, e)| e.deadline)
            .map(|(h, _)| h.clone())?;
        let entry = self
            .entries
            .remove(&handle)
            .expect("entry present under held map key");
        Some((handle, entry))
    }

    /// Remove and return every entry past its deadline. In-flight entries are
    /// left in place: they are owned by their delivery, which settles or
    /// releases the reservation itself.
    pub fn sweep_expired(&mut self, now: Instant) -> Vec<(HoldHandle, Held<T>)> {
        let expired: Vec<HoldHandle> = self
            .entries
            .iter()
            .filter(|(_, e)| !e.in_flight && e.is_expired(now))
            .map(|(h, _)| h.clone())
            .collect();
        expired
            .into_iter()
            .filter_map(|h| self.entries.remove(&h).map(|e| (h, e)))
            .collect()
    }

    /// Remove and return every entry not currently in flight, expired or not.
    /// Used at shutdown so pending bounces are journaled as expired instead of
    /// vanishing. In-flight entries are owned by their delivery task and are
    /// skipped, so the drain can never double-journal an entry whose send is
    /// about to settle it.
    pub fn drain(&mut self) -> Vec<(HoldHandle, Held<T>)> {
        let handles: Vec<HoldHandle> = self
            .entries
            .iter()
            .filter(|(_, e)| !e.in_flight)
            .map(|(h, _)| h.clone())
            .collect();
        handles
            .into_iter()
            .filter_map(|h| self.entries.remove(&h).map(|e| (h, e)))
            .collect()
    }

    /// Number of live entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the queue has no live entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl<T> Default for HoldQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const TTL: Duration = Duration::from_secs(180);

    fn reason() -> RejectReason {
        RejectReason {
            matches: vec![crate::no_rly::judge::ReasonEntry {
                pattern: "straightforward".into(),
                reason: None,
            }],
        }
    }

    #[test]
    fn hold_then_claim_returns_payload() {
        let mut q: HoldQueue<String> = HoldQueue::new();
        let now = Instant::now();
        let handle = q.hold("held text".into(), reason(), None, TTL, now);
        let entry = q.claim(&handle, now).expect("live entry");
        assert_eq!(entry.payload, "held text");
        assert_eq!(entry.parent, None);
    }

    #[test]
    fn claim_does_not_consume_settle_does() {
        let mut q: HoldQueue<String> = HoldQueue::new();
        let now = Instant::now();
        let handle = q.hold("msg".into(), reason(), None, TTL, now);
        assert!(q.claim(&handle, now).is_ok());
        assert!(q.claim(&handle, now).is_ok(), "claim is not consumption");
        assert!(q.settle(&handle).is_some());
        assert!(
            matches!(q.claim(&handle, now), Err(ClaimError::Unknown)),
            "settled handle must be dead"
        );
        assert!(q.settle(&handle).is_none(), "double settle yields nothing");
    }

    #[test]
    fn update_payload_replaces_in_place_and_keeps_the_handle_live() {
        let mut q: HoldQueue<String> = HoldQueue::new();
        let now = Instant::now();
        let handle = q.hold("original".into(), reason(), None, TTL, now);
        assert!(q.update_payload(&handle, "replacement".into()));
        let entry = q.claim(&handle, now).expect("handle stays live");
        assert_eq!(entry.payload, "replacement");
        assert_eq!(entry.expires_in(now), TTL, "deadline is untouched");
        assert!(
            !q.update_payload(&HoldHandle::new("nr-0000-9"), "x".into()),
            "a dead handle takes no payload"
        );
    }

    #[test]
    fn unknown_handle_is_unknown() {
        let mut q: HoldQueue<String> = HoldQueue::new();
        let bogus = HoldHandle::new("nr-0000-999");
        assert!(matches!(
            q.claim(&bogus, Instant::now()),
            Err(ClaimError::Unknown)
        ));
    }

    #[test]
    fn claim_past_deadline_expires_even_without_sweep() {
        let mut q: HoldQueue<String> = HoldQueue::new();
        let now = Instant::now();
        let handle = q.hold("msg".into(), reason(), None, TTL, now);
        let later = now + TTL + Duration::from_secs(1);
        match q.claim(&handle, later) {
            Err(ClaimError::Expired(entry)) => assert_eq!(entry.payload, "msg"),
            other => panic!("expected Expired, got {other:?}"),
        }
        assert!(
            matches!(q.claim(&handle, later), Err(ClaimError::Unknown)),
            "expired entry must be removed by the failed claim"
        );
    }

    #[test]
    fn claim_at_exact_deadline_is_expired() {
        let mut q: HoldQueue<String> = HoldQueue::new();
        let now = Instant::now();
        let handle = q.hold("msg".into(), reason(), None, TTL, now);
        assert!(matches!(
            q.claim(&handle, now + TTL),
            Err(ClaimError::Expired(_))
        ));
    }

    #[test]
    fn sweep_removes_only_expired() {
        let mut q: HoldQueue<String> = HoldQueue::new();
        let now = Instant::now();
        let old = q.hold("old".into(), reason(), None, Duration::from_secs(10), now);
        let fresh = q.hold("fresh".into(), reason(), None, TTL, now);

        let swept = q.sweep_expired(now + Duration::from_secs(11));
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].0, old);
        assert_eq!(q.len(), 1);
        assert!(q.claim(&fresh, now + Duration::from_secs(11)).is_ok());
    }

    #[test]
    fn evict_next_expiring_takes_earliest_deadline() {
        let mut q: HoldQueue<String> = HoldQueue::new();
        let now = Instant::now();
        q.hold("late".into(), reason(), None, TTL, now);
        let soonest = q.hold("soon".into(), reason(), None, Duration::from_secs(5), now);
        q.hold("later".into(), reason(), None, TTL, now);

        let (handle, entry) = q.evict_next_expiring().expect("non-empty queue evicts");
        assert_eq!(handle, soonest);
        assert_eq!(entry.payload, "soon");
        assert_eq!(q.len(), 2);

        let mut empty: HoldQueue<String> = HoldQueue::new();
        assert!(empty.evict_next_expiring().is_none());
    }

    #[test]
    fn drain_empties_everything() {
        let mut q: HoldQueue<String> = HoldQueue::new();
        let now = Instant::now();
        q.hold("a".into(), reason(), None, TTL, now);
        q.hold("b".into(), reason(), None, TTL, now);
        assert_eq!(q.drain().len(), 2);
        assert!(q.is_empty());
    }

    #[test]
    fn chained_hold_records_parent() {
        let mut q: HoldQueue<String> = HoldQueue::new();
        let now = Instant::now();
        let first = q.hold("first".into(), reason(), None, TTL, now);
        let second = q.hold("second".into(), reason(), Some(first.clone()), TTL, now);
        let entry = q.claim(&second, now).unwrap();
        assert_eq!(entry.parent, Some(first));
    }

    #[test]
    fn expires_in_counts_down_and_saturates() {
        let mut q: HoldQueue<String> = HoldQueue::new();
        let now = Instant::now();
        let handle = q.hold("msg".into(), reason(), None, TTL, now);
        let entry = q.claim(&handle, now).unwrap();
        assert_eq!(entry.expires_in(now), TTL);
        assert_eq!(
            entry.expires_in(now + Duration::from_secs(60)),
            TTL - Duration::from_secs(60)
        );
        assert_eq!(
            entry.expires_in(now + TTL + Duration::from_secs(5)),
            Duration::ZERO
        );
    }

    #[test]
    fn handle_serializes_as_plain_string() {
        let handle = HoldHandle::new("nr-abcd-1");
        assert_eq!(
            serde_json::to_value(&handle).unwrap(),
            serde_json::json!("nr-abcd-1")
        );
    }

    // ── Property tests: the invariants hold under arbitrary op sequences ──

    #[derive(Debug, Clone)]
    enum Op {
        Hold,
        /// Claim + settle-on-success — models a release attempt where the
        /// send succeeds.
        ReleaseOk(usize),
        /// Claim without settle — models a release attempt whose send fails.
        ReleaseFailed(usize),
        /// Replace a live entry's payload — models a rephrase that shifted the
        /// held text (v2 mutation).
        UpdatePayload(usize),
        /// Evict the entry nearest its deadline — models capacity enforcement
        /// (v2 mutation).
        Evict,
        Sweep,
        Advance(u64),
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            3 => Just(Op::Hold),
            3 => (0usize..12).prop_map(Op::ReleaseOk),
            2 => (0usize..12).prop_map(Op::ReleaseFailed),
            2 => (0usize..12).prop_map(Op::UpdatePayload),
            1 => Just(Op::Evict),
            1 => Just(Op::Sweep),
            3 => (1u64..400).prop_map(Op::Advance),
        ]
    }

    proptest! {
        /// Under any interleaving of holds, release attempts, payload updates,
        /// evictions, sweeps, and clock advances: handles are unique, each
        /// handle produces at most one successful release, nothing succeeds
        /// past its deadline, a successful release is never followed by another
        /// success or a live claim, and entries are conserved — every minted
        /// handle is either still in the queue or was removed by exactly one of
        /// release/sweep/evict/expiry.
        #[test]
        fn queue_invariants_hold(ops in proptest::collection::vec(op_strategy(), 1..60)) {
            let base = Instant::now();
            let mut now = base;
            let mut q: HoldQueue<u32> = HoldQueue::new();
            let mut minted: Vec<(HoldHandle, Instant)> = Vec::new();
            let mut released: Vec<bool> = Vec::new();
            // Independent oracle: has this handle left the queue (by release,
            // sweep, evict, or expiry-at-claim)? Conservation is checked at the
            // end against the physical queue length.
            let mut gone: Vec<bool> = Vec::new();
            // Shadow of the current payload for each still-present handle.
            let mut payload: Vec<u32> = Vec::new();

            for op in ops {
                match op {
                    Op::Hold => {
                        let val = minted.len() as u32;
                        let handle = q.hold(val, reason(), None, TTL, now);
                        prop_assert!(
                            !minted.iter().any(|(h, _)| *h == handle),
                            "minted handles must be unique"
                        );
                        minted.push((handle, now + TTL));
                        released.push(false);
                        gone.push(false);
                        payload.push(val);
                    }
                    Op::ReleaseOk(i) | Op::ReleaseFailed(i) if i < minted.len() => {
                        let settle = matches!(op, Op::ReleaseOk(_));
                        let (handle, deadline) = minted[i].clone();
                        match q.claim(&handle, now) {
                            Ok(entry) => {
                                prop_assert!(now < deadline, "claim must not succeed past deadline");
                                prop_assert!(!released[i], "released handle must never be claimable");
                                prop_assert_eq!(entry.payload, payload[i], "payload follows its handle");
                                if settle {
                                    prop_assert!(q.settle(&handle).is_some());
                                    released[i] = true;
                                    gone[i] = true;
                                }
                            }
                            Err(ClaimError::Expired(_)) => {
                                prop_assert!(now >= deadline, "expiry only at or past deadline");
                                prop_assert!(!released[i], "released handles are Unknown, not Expired");
                                gone[i] = true;
                            }
                            Err(ClaimError::Unknown) => {
                                prop_assert!(
                                    gone[i] || now >= deadline,
                                    "live unreleased handle within deadline must be claimable"
                                );
                            }
                        }
                    }
                    Op::ReleaseOk(_) | Op::ReleaseFailed(_) => {}
                    Op::UpdatePayload(i) if i < minted.len() => {
                        let handle = minted[i].0.clone();
                        let new_val = 1000 + i as u32;
                        let updated = q.update_payload(&handle, new_val);
                        // update_payload uses the map directly and ignores the
                        // deadline, so it succeeds iff the entry has not yet
                        // been removed (an expired-but-unswept entry is still
                        // physically present and updatable).
                        prop_assert_eq!(
                            updated, !gone[i],
                            "update_payload succeeds iff the handle is still in the map"
                        );
                        if updated {
                            payload[i] = new_val;
                        }
                    }
                    Op::UpdatePayload(_) => {}
                    Op::Evict => {
                        if let Some((handle, _)) = q.evict_next_expiring() {
                            let idx = minted.iter().position(|(h, _)| *h == handle).unwrap();
                            prop_assert!(!released[idx], "evict must never take a released handle");
                            prop_assert!(!gone[idx], "evict must never take an already-removed handle");
                            gone[idx] = true;
                        }
                    }
                    Op::Sweep => {
                        for (handle, entry) in q.sweep_expired(now) {
                            prop_assert!(entry.is_expired(now));
                            let idx = minted.iter().position(|(h, _)| *h == handle).unwrap();
                            prop_assert!(!released[idx], "sweep must never evict a released handle");
                            gone[idx] = true;
                        }
                    }
                    Op::Advance(secs) => {
                        now += Duration::from_secs(secs);
                    }
                }
            }

            // Conservation: the physical queue holds exactly the minted handles
            // that have not left it.
            let still_present = gone.iter().filter(|g| !**g).count();
            prop_assert_eq!(
                q.len(),
                still_present,
                "queue length must equal minted handles that never left"
            );
            let release_count = released.iter().filter(|r| **r).count();
            prop_assert!(release_count <= minted.len());
        }
    }
}
