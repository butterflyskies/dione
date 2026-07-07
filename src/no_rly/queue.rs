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

use std::{
    collections::HashMap,
    fmt,
    hash::{BuildHasher, Hasher, RandomState},
    time::{Duration, Instant},
};

use serde::Serialize;

use crate::{no_rly::judge::RejectReason, timestamp::Timestamp};

/// A single-use claim ticket for one bounced message.
///
/// This is a consent token, not a security token: it only has meaning inside
/// the process-local queue that minted it, so uniqueness within the process
/// is sufficient. The queue does not survive a restart — but the journal
/// does, and shutdown drains every pending handle into it as expired.
///
/// Serializes as its string form (e.g. `"nr-3f92-7"`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
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
                deadline: now + ttl,
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

    /// Consume a handle after its payload was acted on. Returns the entry,
    /// or `None` if the handle was not live. After settling, the handle is
    /// dead: no claim, release, or rephrase can ever see it again.
    pub fn settle(&mut self, handle: &HoldHandle) -> Option<Held<T>> {
        self.entries.remove(handle)
    }

    /// Remove and return every entry past its deadline.
    pub fn sweep_expired(&mut self, now: Instant) -> Vec<(HoldHandle, Held<T>)> {
        let expired: Vec<HoldHandle> = self
            .entries
            .iter()
            .filter(|(_, e)| e.is_expired(now))
            .map(|(h, _)| h.clone())
            .collect();
        expired
            .into_iter()
            .filter_map(|h| self.entries.remove(&h).map(|e| (h, e)))
            .collect()
    }

    /// Remove and return every entry, expired or not. Used at shutdown so
    /// pending bounces are journaled as expired instead of vanishing.
    pub fn drain(&mut self) -> Vec<(HoldHandle, Held<T>)> {
        self.entries.drain().collect()
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
    use proptest::prelude::*;

    use super::*;

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
        Sweep,
        Advance(u64),
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            3 => Just(Op::Hold),
            3 => (0usize..12).prop_map(Op::ReleaseOk),
            2 => (0usize..12).prop_map(Op::ReleaseFailed),
            1 => Just(Op::Sweep),
            3 => (1u64..400).prop_map(Op::Advance),
        ]
    }

    proptest! {
        /// Under any interleaving of holds, release attempts, sweeps, and
        /// clock advances: handles are unique, each handle produces at most
        /// one successful release, nothing succeeds past its deadline, and a
        /// successful release is never followed by another success or by a
        /// live claim.
        #[test]
        fn queue_invariants_hold(ops in proptest::collection::vec(op_strategy(), 1..60)) {
            let base = Instant::now();
            let mut now = base;
            let mut q: HoldQueue<u32> = HoldQueue::new();
            let mut minted: Vec<(HoldHandle, Instant)> = Vec::new();
            let mut released: Vec<bool> = Vec::new();

            for op in ops {
                match op {
                    Op::Hold => {
                        let handle = q.hold(minted.len() as u32, reason(), None, TTL, now);
                        prop_assert!(
                            !minted.iter().any(|(h, _)| *h == handle),
                            "minted handles must be unique"
                        );
                        minted.push((handle, now + TTL));
                        released.push(false);
                    }
                    Op::ReleaseOk(i) | Op::ReleaseFailed(i) if i < minted.len() => {
                        let settle = matches!(op, Op::ReleaseOk(_));
                        let (handle, deadline) = minted[i].clone();
                        match q.claim(&handle, now) {
                            Ok(entry) => {
                                prop_assert!(now < deadline, "claim must not succeed past deadline");
                                prop_assert!(!released[i], "released handle must never be claimable");
                                prop_assert_eq!(entry.payload, i as u32, "payload follows its handle");
                                if settle {
                                    prop_assert!(q.settle(&handle).is_some());
                                    released[i] = true;
                                }
                            }
                            Err(ClaimError::Expired(_)) => {
                                prop_assert!(now >= deadline, "expiry only at or past deadline");
                                prop_assert!(!released[i], "released handles are Unknown, not Expired");
                            }
                            Err(ClaimError::Unknown) => {
                                prop_assert!(
                                    released[i] || now >= deadline,
                                    "live unreleased handle within deadline must be claimable"
                                );
                            }
                        }
                    }
                    Op::ReleaseOk(_) | Op::ReleaseFailed(_) => {}
                    Op::Sweep => {
                        for (handle, entry) in q.sweep_expired(now) {
                            prop_assert!(entry.is_expired(now));
                            let idx = minted.iter().position(|(h, _)| *h == handle).unwrap();
                            prop_assert!(!released[idx], "sweep must never evict a released handle");
                        }
                    }
                    Op::Advance(secs) => {
                        now += Duration::from_secs(secs);
                    }
                }
            }

            let release_count = released.iter().filter(|r| **r).count();
            prop_assert!(release_count <= minted.len());
        }
    }
}
