//! The consent gate — orchestrates bounce → {release | rephrase | expire}.
//!
//! [`ConsentGate`] owns the hold queue and the audit journal, and is the only
//! way the three verbs touch either. Delivery goes through the
//! [`DeliverReply`] seam so every gate semantic is testable without a Discord
//! connection.
//!
//! # Consent semantics
//!
//! A handle yields **at most one successful send**, ever. A delivery
//! *reserves* its entry under the queue lock (marking it in-flight), releases
//! the lock for the Discord send, then re-acquires it to settle (consume) on
//! success — so:
//!
//! - a successful release or rephrase kills the handle (no replay);
//! - a *failed* send (outbound gate, Discord error) leaves the handle live
//!   until its deadline, because consuming it would strand the message with
//!   nothing sent and nothing retrievable;
//! - two concurrent actions on the same handle serialize on the reservation:
//!   the second sees the entry in-flight and loses with a dead handle, so only
//!   one send ever reaches the wire.
//!
//! Because the lock is not held across the send, one slow delivery cannot
//! serialize the whole gate (bounce, expiry sweep, stats, shutdown drain) or
//! eat the shutdown drain window — while an in-flight entry is invisible to
//! sweep/drain/evict, so it cannot be resolved out from under its send.
//!
//! Expiry is enforced at reserve time (see [`super::queue`]), so a handle past
//! its TTL is dead even if the background sweep has not caught it yet.

use crate::{
    contradictionary::DiaryRecord,
    discord::FenceContext,
    no_rly::{
        journal::{self, JournalHandle, Outcome},
        judge::{OutboundJudge, RejectReason, Verdict},
        queue::{ClaimError, Held, HoldHandle, HoldQueue},
    },
};
use camino::Utf8Path;
use serenity::model::id::{ChannelId, MessageId};
use std::{
    sync::Mutex as StdMutex,
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::sync::Mutex;

/// Everything needed to send (or re-send) one reply. Held verbatim in the
/// queue: release sends exactly this, and rephrase replaces only `content` —
/// addressing (channel, reply threading, ping suppression) carries over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyRequest {
    /// Target channel.
    pub channel_id: ChannelId,
    /// Message text.
    pub content: String,
    /// Message being replied to, if any.
    pub reply_to_message_id: Option<MessageId>,
    /// Whether to suppress the reply ping.
    pub suppress_ping: bool,
    /// Full-message contradictionary records pending completion after a
    /// partial chunked delivery. This is internal retry context, not wire data.
    pub(crate) pending_diary_records: Vec<DiaryRecord>,
    /// Fence state active before `content`, retained across a partial retry.
    pub(crate) fence_context: ReplyFenceContext,
}

#[derive(Debug, Default)]
pub(crate) struct ReplyFenceContext(StdMutex<Option<FenceContext>>);

impl ReplyFenceContext {
    fn get(&self) -> Option<FenceContext> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn set(&self, context: Option<FenceContext>) {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = context;
    }
}

impl Clone for ReplyFenceContext {
    fn clone(&self) -> Self {
        Self(StdMutex::new(self.get()))
    }
}

impl PartialEq for ReplyFenceContext {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other) || self.get() == other.get()
    }
}

impl Eq for ReplyFenceContext {}

impl ReplyRequest {
    pub(crate) fn fence_context(&self) -> Option<FenceContext> {
        self.fence_context.get()
    }

    pub(crate) fn set_fence_context(&self, context: Option<FenceContext>) {
        self.fence_context.set(context);
    }
}

/// A failed delivery, carrying enough to make a retry an informed, idempotent
/// choice. A multi-chunk send that gets some chunks out before failing reports
/// what already landed (`sent_ids`, at-least-once on the wire) and the
/// `undelivered` remainder, so the gate can resume from the remainder instead
/// of re-posting the delivered chunks.
#[derive(Debug, Clone)]
pub struct DeliverError {
    /// Human-readable failure the construct sees verbatim.
    pub message: String,
    /// Chunk message IDs already posted to Discord before the failure.
    pub sent_ids: Vec<u64>,
    /// The content not yet delivered — what a retry should send. `None` when
    /// nothing went out (retry re-sends the whole payload).
    pub undelivered: Option<String>,
    /// Send-side diary records for a partial delivery. Retained with the
    /// remainder and emitted once the logical message finishes.
    pub diary_records: Vec<DiaryRecord>,
}

impl DeliverError {
    /// A total failure — nothing reached the wire.
    pub fn total(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            sent_ids: Vec::new(),
            undelivered: None,
            diary_records: Vec::new(),
        }
    }
}

/// The delivery seam: sends a [`ReplyRequest`] to Discord and returns the
/// sent message IDs. Implemented by the real messaging context and by test
/// doubles.
pub trait DeliverReply: Send + Sync {
    /// Attempt the send. `Err` carries the failure plus any partial progress
    /// (see [`DeliverError`]).
    fn deliver(
        &self,
        request: &ReplyRequest,
    ) -> impl Future<Output = Result<Vec<u64>, DeliverError>> + Send;
}

/// What a bounce hands back to the construct: the single-use handle, the
/// named reason, and how long the decision window is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BounceTicket {
    /// The freshly minted single-use handle.
    pub handle: HoldHandle,
    /// Why the message bounced.
    pub reason: RejectReason,
    /// Time until the handle expires.
    pub expires_in: Duration,
    /// The handle this bounce chains from (set on rephrase re-bounces).
    pub parent: Option<HoldHandle>,
}

/// A successful release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Released {
    /// Discord message IDs of the sent chunks.
    pub message_ids: Vec<u64>,
    /// Bounce-to-release latency in milliseconds.
    pub latency_ms: u64,
}

/// A successful rephrase — either the replacement went out, or it bounced
/// again and the chain continues under a new handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rephrased {
    /// The replacement passed judgment and was sent.
    Sent {
        /// Discord message IDs of the sent chunks.
        message_ids: Vec<u64>,
    },
    /// The replacement bounced too. The old handle is dead; the new ticket
    /// chains to it.
    ReBounced(BounceTicket),
}

/// Why an action on a handle did not go through.
#[derive(Debug, Error)]
pub enum RejectedHandle {
    /// The handle never existed, was already used, or expired and was swept.
    #[error("unknown or already-used handle: {0}")]
    Unknown(HoldHandle),
    /// The handle expired before the action; the expiry has been journaled.
    #[error("handle {handle} expired before action; the held message was journaled as expired")]
    Expired {
        /// The dead handle.
        handle: HoldHandle,
        /// Why the message had bounced.
        reason: RejectReason,
    },
    /// Delivery failed. The handle is still live until its deadline.
    #[error("send failed: {error} (handle {handle} still live for {}s)", expires_in.as_secs())]
    SendFailed {
        /// The still-live handle.
        handle: HoldHandle,
        /// The delivery error.
        error: String,
        /// Time remaining before the handle expires.
        expires_in: Duration,
    },
}

/// The consent gate: hold queue plus the audit journal's single-writer handle.
///
/// The queue lock is held only to reserve/settle a handle — never across the
/// Discord send. A delivery reserves its entry under the lock (marking it
/// in-flight so no sweep, drain, eviction, or concurrent claim can touch it),
/// drops the lock for the await, then re-acquires it to settle or release the
/// reservation. So one slow send no longer serializes bounce/expire/stats/drain
/// or eats the shutdown drain window, while single-use is still enforced by the
/// in-flight reservation.
#[derive(Debug)]
pub struct ConsentGate {
    queue: Mutex<HoldQueue<ReplyRequest>>,
    journal: JournalHandle,
}

impl ConsentGate {
    /// A gate whose journal lives in `state_dir`. Spawns the journal's
    /// single-writer task, so it must be constructed within a Tokio runtime.
    pub fn new(state_dir: &Utf8Path) -> Self {
        Self {
            queue: Mutex::new(HoldQueue::new()),
            journal: JournalHandle::spawn(state_dir),
        }
    }

    /// The gate's audit journal.
    pub fn journal(&self) -> &JournalHandle {
        &self.journal
    }

    /// Number of messages currently held.
    pub async fn pending(&self) -> usize {
        self.queue.lock().await.len()
    }

    /// Park a bounced request and mint its ticket.
    ///
    /// `max_pending` caps the queue (values below 1 are treated as 1): a
    /// bounce arriving at capacity first evicts the held entry closest to
    /// expiry and journals it as expired, so a runaway loop cannot grow the
    /// queue without bound between sweeps.
    pub async fn bounce(
        &self,
        request: ReplyRequest,
        reason: RejectReason,
        ttl: Duration,
        max_pending: usize,
        now: Instant,
    ) -> BounceTicket {
        let mut queue = self.queue.lock().await;
        while queue.len() >= max_pending.max(1) {
            let Some((evicted_handle, entry)) = queue.evict_next_expiring() else {
                break;
            };
            tracing::warn!(
                handle = %evicted_handle,
                "no_rly hold queue at capacity; evicting entry closest to expiry"
            );
            self.journal_expired(&evicted_handle, &entry, now);
        }
        let handle = queue.hold(request, reason.clone(), None, ttl, now);
        BounceTicket {
            handle,
            reason,
            expires_in: ttl,
            parent: None,
        }
    }

    /// Release: send the byte-identical held message. On success the handle
    /// is consumed and the bounce is journaled as [`Outcome::Released`].
    ///
    /// Ordering semantic: the message lands **when released**, not at its
    /// original position in the conversation. That is inherent to the design
    /// — the pause for judgment is the feature — and is documented rather
    /// than fought.
    pub async fn release<D: DeliverReply>(
        &self,
        deliver: &D,
        handle: &HoldHandle,
        now: Instant,
    ) -> Result<Released, RejectedHandle> {
        // Reserve under the lock (marks the entry in-flight), then drop the
        // lock for the send.
        let entry = {
            let mut queue = self.queue.lock().await;
            self.reserve_live(&mut queue, handle, now)?
        };

        match deliver.deliver(&entry.payload).await {
            Ok(ids) => {
                {
                    let mut queue = self.queue.lock().await;
                    queue.settle(handle);
                }
                let latency_ms = entry.latency(now).as_millis() as u64;
                let mut message_ids = entry.sent_ids.clone();
                message_ids.extend(ids);
                self.journal_release(handle, &entry, latency_ms);
                Ok(Released {
                    message_ids,
                    latency_ms,
                })
            }
            Err(DeliverError {
                message,
                sent_ids,
                undelivered,
                diary_records,
            }) => {
                let mut queue = self.queue.lock().await;
                match undelivered {
                    // Partial send: resume from the undelivered remainder so a
                    // retry never re-posts an already-sent chunk. The full text
                    // is preserved for the eventual journal record.
                    Some(remainder) if !sent_ids.is_empty() => {
                        let full = entry
                            .original_message
                            .clone()
                            .unwrap_or_else(|| entry.payload.content.clone());
                        let remainder_req = ReplyRequest {
                            content: remainder,
                            pending_diary_records: diary_records,
                            ..entry.payload.clone()
                        };
                        queue.record_partial(handle, remainder_req, sent_ids, full);
                    }
                    // Nothing landed: clear the reservation, leave the handle
                    // live for a clean retry.
                    _ => queue.release_reservation(handle),
                }
                drop(queue);
                Err(RejectedHandle::SendFailed {
                    handle: handle.clone(),
                    error: message,
                    expires_in: entry.expires_in(now),
                })
            }
        }
    }

    /// Rephrase: judge `replacement` and either send it (killing the handle,
    /// journaling the (original, reason, replacement) triple) or re-bounce it
    /// under a new handle chained to the old one.
    ///
    /// The old handle dies on a re-bounce too: the original text can never
    /// be sent once a replacement was offered — the chain carries the story
    /// forward instead.
    ///
    /// A judged-clear replacement whose *delivery* fails leaves the handle
    /// live for retry — holding the replacement, not the original. Offering
    /// a replacement withdraws consent for the original text on every path,
    /// so a later release or rephrase of the still-live handle operates on
    /// the rephrased content and can never silently revert.
    pub async fn rephrase<D: DeliverReply, J: OutboundJudge + ?Sized>(
        &self,
        deliver: &D,
        judge: &J,
        handle: &HoldHandle,
        replacement: &str,
        ttl: Duration,
        now: Instant,
    ) -> Result<Rephrased, RejectedHandle> {
        let entry = {
            let mut queue = self.queue.lock().await;
            self.reserve_live(&mut queue, handle, now)?
        };

        // Build field-by-field: only the content changes, so there is no
        // reason to clone the original content string just to overwrite it.
        let request = ReplyRequest {
            channel_id: entry.payload.channel_id,
            content: replacement.to_string(),
            reply_to_message_id: entry.payload.reply_to_message_id,
            suppress_ping: entry.payload.suppress_ping,
            pending_diary_records: entry.payload.pending_diary_records.clone(),
            fence_context: ReplyFenceContext::default(),
        };

        match judge.judge(replacement) {
            Verdict::Clear => match deliver.deliver(&request).await {
                Ok(ids) => {
                    {
                        let mut queue = self.queue.lock().await;
                        queue.settle(handle);
                    }
                    let latency_ms = entry.latency(now).as_millis() as u64;
                    let mut message_ids = entry.sent_ids.clone();
                    message_ids.extend(ids);
                    self.journal_rephrase(handle, &entry, replacement, latency_ms);
                    Ok(Rephrased::Sent { message_ids })
                }
                Err(DeliverError {
                    message,
                    sent_ids,
                    undelivered,
                    diary_records,
                }) => {
                    // The construct consented to the replacement, so the held
                    // entry carries it from here on — a retry never reverts to
                    // the original — while the original text is retained for
                    // the journal so the (original, reason, replacement) triple
                    // survives. A judged-clear send that only failed to deliver
                    // also earns a fresh decision window, symmetric with the
                    // fresh TTL a re-bounce mints.
                    let original = entry
                        .withdrawn_original
                        .clone()
                        .unwrap_or_else(|| entry.payload.content.clone());
                    let mut queue = self.queue.lock().await;
                    queue.set_withdrawn_original(handle, original);
                    match undelivered {
                        Some(remainder) if !sent_ids.is_empty() => {
                            let remainder_req = ReplyRequest {
                                content: remainder,
                                pending_diary_records: diary_records,
                                ..request.clone()
                            };
                            queue.record_partial(handle, remainder_req, sent_ids, request.content);
                        }
                        _ => {
                            queue.update_payload(handle, request);
                            queue.release_reservation(handle);
                        }
                    }
                    queue.refresh_deadline(handle, now.checked_add(ttl).unwrap_or(now));
                    drop(queue);
                    Err(RejectedHandle::SendFailed {
                        handle: handle.clone(),
                        error: message,
                        expires_in: ttl,
                    })
                }
            },
            Verdict::Bounce(new_reason) => {
                let latency_ms = entry.latency(now).as_millis() as u64;
                let new_handle = {
                    let mut queue = self.queue.lock().await;
                    queue.settle(handle);
                    queue.hold(request, new_reason.clone(), Some(handle.clone()), ttl, now)
                };
                self.journal_rephrase(handle, &entry, replacement, latency_ms);
                Ok(Rephrased::ReBounced(BounceTicket {
                    handle: new_handle,
                    reason: new_reason,
                    expires_in: ttl,
                    parent: Some(handle.clone()),
                }))
            }
        }
    }

    /// Sweep entries past their deadline, journaling each as expired.
    /// Returns how many expired. Run periodically by the server.
    pub async fn expire_due(&self, now: Instant) -> usize {
        let expired = self.queue.lock().await.sweep_expired(now);
        for (handle, entry) in &expired {
            self.journal_expired(handle, entry, now);
        }
        expired.len()
    }

    /// Drain every pending entry as expired. Called at shutdown: the queue
    /// is in-memory, so anything still held would otherwise vanish without
    /// an audit trail. Blocks until those records have actually reached disk
    /// (in-flight sends own their own entries and are skipped by the drain).
    pub async fn drain_shutdown(&self) -> usize {
        let now = Instant::now();
        let drained = self.queue.lock().await.drain();
        for (handle, entry) in &drained {
            self.journal_expired(handle, entry, now);
        }
        // The journal writer is asynchronous; make sure the drained records
        // are durable before the process exits.
        self.journal.flush().await;
        drained.len()
    }

    /// Reserve a live entry for delivery (marking it in-flight) or map the
    /// failure, journaling lazy expiry.
    fn reserve_live(
        &self,
        queue: &mut HoldQueue<ReplyRequest>,
        handle: &HoldHandle,
        now: Instant,
    ) -> Result<Held<ReplyRequest>, RejectedHandle> {
        match queue.reserve(handle, now) {
            Ok(entry) => Ok(entry),
            Err(ClaimError::Unknown) => Err(RejectedHandle::Unknown(handle.clone())),
            Err(ClaimError::Expired(entry)) => {
                self.journal_expired(handle, &entry, now);
                Err(RejectedHandle::Expired {
                    handle: handle.clone(),
                    reason: entry.reason.clone(),
                })
            }
        }
    }

    /// Journal a plain release. If the entry carries a withdrawn original (a
    /// prior rephrase moved consent to the replacement), record it as the
    /// (original, reason, replacement) triple instead of losing the original.
    fn journal_release(&self, handle: &HoldHandle, entry: &Held<ReplyRequest>, latency_ms: u64) {
        let sent = entry
            .original_message
            .as_deref()
            .unwrap_or(entry.payload.content.as_str());
        match entry.withdrawn_original.as_deref() {
            Some(original) => self.append_resolution(
                handle,
                entry,
                Outcome::Rephrased,
                original,
                Some(sent),
                latency_ms,
            ),
            None => {
                self.append_resolution(handle, entry, Outcome::Released, sent, None, latency_ms)
            }
        }
    }

    /// Journal a rephrase (clean send or re-bounce): the `message` is the true
    /// original (preserved across a prior failed rephrase), the `replacement`
    /// is the text that went out (or re-bounced).
    fn journal_rephrase(
        &self,
        handle: &HoldHandle,
        entry: &Held<ReplyRequest>,
        replacement: &str,
        latency_ms: u64,
    ) {
        let original = entry
            .withdrawn_original
            .as_deref()
            .or(entry.original_message.as_deref())
            .unwrap_or(entry.payload.content.as_str());
        self.append_resolution(
            handle,
            entry,
            Outcome::Rephrased,
            original,
            Some(replacement),
            latency_ms,
        );
    }

    fn journal_expired(&self, handle: &HoldHandle, entry: &Held<ReplyRequest>, now: Instant) {
        let latency_ms = entry.latency(now).as_millis() as u64;
        // Record the full held text that was abandoned — the replacement if a
        // rephrase moved consent, the remainder-preserving full text if a
        // partial send shrank the payload.
        let message = entry
            .original_message
            .as_deref()
            .unwrap_or(entry.payload.content.as_str());
        self.append_resolution(handle, entry, Outcome::Expired, message, None, latency_ms);
    }

    /// Build and enqueue one journal record. The enqueue is fire-and-forget
    /// (the single-writer task owns the file), so a journal problem can never
    /// block or undo the consented action.
    fn append_resolution(
        &self,
        handle: &HoldHandle,
        entry: &Held<ReplyRequest>,
        outcome: Outcome,
        message: &str,
        replacement: Option<&str>,
        latency_ms: u64,
    ) {
        let record = journal::ResolvedBounce {
            handle: handle.as_str(),
            parent: entry.parent.as_ref().map(HoldHandle::as_str),
            message,
            reason: entry.reason.clone(),
            outcome,
            replacement,
            bounced_at: entry.bounced_at.clone(),
            latency_ms,
        }
        .into_record();
        self.journal.append(&record);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::ChunkMode,
        contradictionary::{Action, Contradictionary, Entry, MatchMode},
        discord::{chunk_preserving_fences, chunk_preserving_fences_with_context},
        no_rly::{
            journal::{BounceRecord, JournalRecord, StatsFilter},
            judge::{AlwaysClear, ReasonEntry},
        },
    };
    use camino::Utf8PathBuf;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex as StdMutex},
    };
    use tempfile::TempDir;

    const TTL: Duration = Duration::from_secs(180);
    const MAX_PENDING: usize = 32;

    /// Scripted deliverer: pops one result per call, records every request.
    struct MockDeliver {
        results: StdMutex<VecDeque<Result<Vec<u64>, String>>>,
        requests: StdMutex<Vec<ReplyRequest>>,
    }

    impl MockDeliver {
        fn scripted(results: Vec<Result<Vec<u64>, String>>) -> Self {
            Self {
                results: StdMutex::new(results.into()),
                requests: StdMutex::new(Vec::new()),
            }
        }

        fn ok() -> Self {
            Self::scripted(vec![Ok(vec![1001])])
        }

        fn requests(&self) -> Vec<ReplyRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl DeliverReply for MockDeliver {
        async fn deliver(&self, request: &ReplyRequest) -> Result<Vec<u64>, DeliverError> {
            self.requests.lock().unwrap().push(request.clone());
            self.results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(vec![9999]))
                .map_err(DeliverError::total)
        }
    }

    fn gate() -> (TempDir, ConsentGate) {
        let dir = TempDir::new().unwrap();
        let path = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
        let gate = ConsentGate::new(&path);
        (dir, gate)
    }

    fn reason() -> RejectReason {
        RejectReason {
            matches: vec![ReasonEntry {
                pattern: "straightforward".into(),
                reason: Some("nothing ever is".into()),
            }],
        }
    }

    fn request(content: &str) -> ReplyRequest {
        ReplyRequest {
            channel_id: ChannelId::new(42),
            content: content.into(),
            pending_diary_records: Vec::new(),
            fence_context: ReplyFenceContext::default(),
            reply_to_message_id: Some(MessageId::new(7)),
            suppress_ping: true,
        }
    }

    fn judge() -> Contradictionary {
        Contradictionary::new(vec![Entry {
            pattern: "straightforward".into(),
            action: Action::Block,
            match_mode: MatchMode::Word,
            reason: Some("nothing ever is".into()),
        }])
    }

    async fn journal_bounces(gate: &ConsentGate) -> Vec<BounceRecord> {
        // The journal writer is asynchronous; flush so queued records are on
        // disk before we read them back.
        gate.journal().flush().await;
        gate.journal()
            .load()
            .unwrap()
            .records
            .into_iter()
            .filter_map(|r| match r {
                JournalRecord::Bounce(b) => Some(b),
                JournalRecord::Summary(_) => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn release_sends_byte_identical_and_journals() {
        let (_dir, gate) = gate();
        let deliver = MockDeliver::ok();
        let now = Instant::now();

        let original = request("a straightforward plan");
        let ticket = gate
            .bounce(original.clone(), reason(), TTL, MAX_PENDING, now)
            .await;
        assert_eq!(ticket.expires_in, TTL);
        assert_eq!(ticket.parent, None);

        let released = gate
            .release(&deliver, &ticket.handle, now + Duration::from_secs(2))
            .await
            .expect("release must succeed");
        assert_eq!(released.message_ids, vec![1001]);
        assert_eq!(released.latency_ms, 2000);
        assert_eq!(
            deliver.requests(),
            vec![original],
            "release must send the exact held request — content, addressing, and all"
        );

        let bounces = journal_bounces(&gate).await;
        assert_eq!(bounces.len(), 1);
        assert_eq!(bounces[0].outcome, Outcome::Released);
        assert_eq!(bounces[0].handle, ticket.handle);
        assert_eq!(bounces[0].message, "a straightforward plan");
        assert_eq!(bounces[0].latency_ms, 2000);
        assert_eq!(bounces[0].replacement, None);
    }

    #[tokio::test]
    async fn released_handle_is_dead() {
        let (_dir, gate) = gate();
        let deliver = MockDeliver::scripted(vec![Ok(vec![1]), Ok(vec![2])]);
        let now = Instant::now();
        let ticket = gate
            .bounce(request("msg"), reason(), TTL, MAX_PENDING, now)
            .await;

        gate.release(&deliver, &ticket.handle, now).await.unwrap();
        match gate.release(&deliver, &ticket.handle, now).await {
            Err(RejectedHandle::Unknown(_)) => {}
            other => panic!("second release must see a dead handle, got {other:?}"),
        }
        assert_eq!(deliver.requests().len(), 1, "no replay: one send, ever");
    }

    #[tokio::test]
    async fn expired_handle_cannot_release_and_is_journaled() {
        let (_dir, gate) = gate();
        let deliver = MockDeliver::ok();
        let now = Instant::now();
        let ticket = gate
            .bounce(request("msg"), reason(), TTL, MAX_PENDING, now)
            .await;

        let late = now + TTL + Duration::from_secs(1);
        match gate.release(&deliver, &ticket.handle, late).await {
            Err(RejectedHandle::Expired { handle, .. }) => {
                assert_eq!(handle, ticket.handle);
            }
            other => panic!("expected Expired, got {other:?}"),
        }
        assert!(
            deliver.requests().is_empty(),
            "nothing may send past expiry"
        );

        let bounces = journal_bounces(&gate).await;
        assert_eq!(bounces.len(), 1);
        assert_eq!(bounces[0].outcome, Outcome::Expired);
    }

    #[tokio::test]
    async fn failed_send_leaves_handle_live_for_retry() {
        let (_dir, gate) = gate();
        let deliver = MockDeliver::scripted(vec![Err("discord hiccup".into()), Ok(vec![1002])]);
        let now = Instant::now();
        let ticket = gate
            .bounce(request("msg"), reason(), TTL, MAX_PENDING, now)
            .await;

        match gate.release(&deliver, &ticket.handle, now).await {
            Err(RejectedHandle::SendFailed { error, .. }) => {
                assert_eq!(error, "discord hiccup");
            }
            other => panic!("expected SendFailed, got {other:?}"),
        }
        assert!(
            journal_bounces(&gate).await.is_empty(),
            "a failed send is not an outcome — the bounce is still open"
        );

        let released = gate
            .release(&deliver, &ticket.handle, now + Duration::from_secs(5))
            .await
            .expect("retry within the TTL must work");
        assert_eq!(released.message_ids, vec![1002]);

        let bounces = journal_bounces(&gate).await;
        assert_eq!(bounces.len(), 1, "exactly one outcome per bounce");
        assert_eq!(bounces[0].outcome, Outcome::Released);
    }

    #[tokio::test]
    async fn failed_rephrase_delivery_retry_sends_replacement_not_original() {
        let (_dir, gate) = gate();
        let deliver = MockDeliver::scripted(vec![Err("discord hiccup".into()), Ok(vec![1003])]);
        let now = Instant::now();
        let ticket = gate
            .bounce(
                request("a straightforward plan"),
                reason(),
                TTL,
                MAX_PENDING,
                now,
            )
            .await;

        match gate
            .rephrase(&deliver, &judge(), &ticket.handle, "a solid plan", TTL, now)
            .await
        {
            Err(RejectedHandle::SendFailed { handle, .. }) => assert_eq!(handle, ticket.handle),
            other => panic!("expected SendFailed, got {other:?}"),
        }

        let released = gate
            .release(&deliver, &ticket.handle, now + Duration::from_secs(5))
            .await
            .expect("retry within the TTL must work");
        assert_eq!(released.message_ids, vec![1003]);

        let sent = deliver.requests();
        assert_eq!(sent.len(), 2, "the failed attempt and the retry");
        assert_eq!(
            sent[1].content, "a solid plan",
            "consent went to the replacement — the retry must never revert to the original"
        );

        // Journal integrity: a rephrase that only failed to *deliver* the first
        // time is still a rephrase, not a verbatim release. The record keeps
        // the original text (which the reason names) and the replacement that
        // actually went out — the (original, reason, replacement) triple.
        let bounces = journal_bounces(&gate).await;
        assert_eq!(bounces.len(), 1);
        assert_eq!(bounces[0].outcome, Outcome::Rephrased);
        assert_eq!(
            bounces[0].message, "a straightforward plan",
            "the original bounced text must survive into the journal"
        );
        assert_eq!(
            bounces[0].replacement.as_deref(),
            Some("a solid plan"),
            "the journal records the replacement that actually went out"
        );
    }

    #[tokio::test]
    async fn failed_rephrase_delivery_second_rephrase_operates_on_replacement() {
        let (_dir, gate) = gate();
        let deliver = MockDeliver::scripted(vec![Err("discord hiccup".into()), Ok(vec![1004])]);
        let now = Instant::now();
        let ticket = gate
            .bounce(
                request("a straightforward plan"),
                reason(),
                TTL,
                MAX_PENDING,
                now,
            )
            .await;

        gate.rephrase(&deliver, &judge(), &ticket.handle, "a solid plan", TTL, now)
            .await
            .expect_err("first delivery is scripted to fail");

        let result = gate
            .rephrase(
                &deliver,
                &judge(),
                &ticket.handle,
                "a revised plan",
                TTL,
                now,
            )
            .await
            .expect("second rephrase must succeed");
        assert!(matches!(result, Rephrased::Sent { .. }));
        assert_eq!(deliver.requests().last().unwrap().content, "a revised plan");

        let bounces = journal_bounces(&gate).await;
        assert_eq!(bounces.len(), 1);
        assert_eq!(bounces[0].outcome, Outcome::Rephrased);
        assert_eq!(
            bounces[0].message, "a straightforward plan",
            "the true original survives across chained rephrase attempts, not the intermediate"
        );
        assert_eq!(bounces[0].replacement.as_deref(), Some("a revised plan"));
    }

    #[tokio::test]
    async fn rephrase_clean_sends_replacement_and_journals_triple() {
        let (_dir, gate) = gate();
        let deliver = MockDeliver::ok();
        let now = Instant::now();
        let original = request("a straightforward plan");
        let ticket = gate
            .bounce(original.clone(), reason(), TTL, MAX_PENDING, now)
            .await;

        let result = gate
            .rephrase(
                &deliver,
                &judge(),
                &ticket.handle,
                "a solid plan",
                TTL,
                now + Duration::from_secs(3),
            )
            .await
            .expect("clean rephrase must succeed");
        assert!(matches!(result, Rephrased::Sent { .. }));

        let sent = deliver.requests();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].content, "a solid plan");
        assert_eq!(
            sent[0].channel_id, original.channel_id,
            "rephrase replaces content, not addressing"
        );
        assert_eq!(sent[0].reply_to_message_id, original.reply_to_message_id);
        assert_eq!(sent[0].suppress_ping, original.suppress_ping);

        let bounces = journal_bounces(&gate).await;
        assert_eq!(bounces.len(), 1);
        assert_eq!(bounces[0].outcome, Outcome::Rephrased);
        assert_eq!(bounces[0].message, "a straightforward plan");
        assert_eq!(bounces[0].replacement.as_deref(), Some("a solid plan"));
        assert_eq!(bounces[0].reason, reason());
        assert_eq!(bounces[0].latency_ms, 3000);
    }

    #[tokio::test]
    async fn rephrase_rebounce_chains_new_handle_and_kills_old() {
        let (_dir, gate) = gate();
        let deliver = MockDeliver::ok();
        let now = Instant::now();
        let ticket = gate
            .bounce(
                request("a straightforward plan"),
                reason(),
                TTL,
                MAX_PENDING,
                now,
            )
            .await;

        let result = gate
            .rephrase(
                &deliver,
                &judge(),
                &ticket.handle,
                "still a straightforward plan",
                TTL,
                now,
            )
            .await
            .expect("re-bounce is a success shape, not an error");
        let new_ticket = match result {
            Rephrased::ReBounced(t) => t,
            other => panic!("expected ReBounced, got {other:?}"),
        };
        assert_ne!(new_ticket.handle, ticket.handle);
        assert_eq!(new_ticket.parent, Some(ticket.handle.clone()));
        assert!(deliver.requests().is_empty(), "nothing sent on re-bounce");

        match gate.release(&deliver, &ticket.handle, now).await {
            Err(RejectedHandle::Unknown(_)) => {}
            other => panic!("old handle must be dead after rephrase, got {other:?}"),
        }

        let released = gate
            .release(&deliver, &new_ticket.handle, now + Duration::from_secs(1))
            .await
            .expect("chained handle releases the replacement text");
        assert_eq!(released.message_ids, vec![1001]);
        assert_eq!(
            deliver.requests().last().unwrap().content,
            "still a straightforward plan"
        );

        let bounces = journal_bounces(&gate).await;
        assert_eq!(bounces.len(), 2);
        assert_eq!(bounces[0].outcome, Outcome::Rephrased);
        assert_eq!(bounces[1].outcome, Outcome::Released);
        assert_eq!(
            bounces[1].parent.as_ref(),
            Some(&ticket.handle),
            "chain link must survive into the journal"
        );
        let stats = gate.journal().stats(&StatsFilter::default()).unwrap();
        assert_eq!(stats.chained, 1);
        assert_eq!(stats.dangling_parents, 0);
    }

    #[tokio::test]
    async fn rephrase_with_always_clear_judge_sends() {
        let (_dir, gate) = gate();
        let deliver = MockDeliver::ok();
        let now = Instant::now();
        let ticket = gate
            .bounce(
                request("a straightforward plan"),
                reason(),
                TTL,
                MAX_PENDING,
                now,
            )
            .await;
        let result = gate
            .rephrase(
                &deliver,
                &AlwaysClear,
                &ticket.handle,
                "still a straightforward plan",
                TTL,
                now,
            )
            .await
            .unwrap();
        assert!(
            matches!(result, Rephrased::Sent { .. }),
            "with no judge configured the replacement goes out"
        );
    }

    #[tokio::test]
    async fn bounce_at_capacity_evicts_entry_closest_to_expiry() {
        let (_dir, gate) = gate();
        let deliver = MockDeliver::scripted(vec![Ok(vec![1]), Ok(vec![2])]);
        let now = Instant::now();

        let soonest = gate
            .bounce(request("one"), reason(), Duration::from_secs(10), 2, now)
            .await;
        let second = gate.bounce(request("two"), reason(), TTL, 2, now).await;
        let third = gate.bounce(request("three"), reason(), TTL, 2, now).await;
        assert_eq!(gate.pending().await, 2, "capacity holds at max_pending");

        match gate.release(&deliver, &soonest.handle, now).await {
            Err(RejectedHandle::Unknown(_)) => {}
            other => panic!("evicted handle must be dead, got {other:?}"),
        }
        gate.release(&deliver, &second.handle, now)
            .await
            .expect("survivor releases");
        gate.release(&deliver, &third.handle, now)
            .await
            .expect("the bounce that triggered eviction is held normally");

        let bounces = journal_bounces(&gate).await;
        assert_eq!(bounces.len(), 3, "the eviction is journaled, not dropped");
        assert_eq!(bounces[0].outcome, Outcome::Expired);
        assert_eq!(bounces[0].message, "one");
        assert_eq!(bounces[0].handle, soonest.handle);
    }

    #[tokio::test]
    async fn bounce_treats_zero_max_pending_as_one() {
        let (_dir, gate) = gate();
        let now = Instant::now();
        gate.bounce(request("one"), reason(), TTL, 0, now).await;
        gate.bounce(request("two"), reason(), TTL, 0, now).await;
        assert_eq!(
            gate.pending().await,
            1,
            "a zero cap must not strand or loop"
        );
    }

    #[tokio::test]
    async fn expire_due_journals_and_empties() {
        let (_dir, gate) = gate();
        let now = Instant::now();
        gate.bounce(request("one"), reason(), TTL, MAX_PENDING, now)
            .await;
        gate.bounce(request("two"), reason(), TTL, MAX_PENDING, now)
            .await;

        assert_eq!(gate.expire_due(now + Duration::from_secs(60)).await, 0);
        assert_eq!(gate.pending().await, 2);

        assert_eq!(gate.expire_due(now + TTL + Duration::from_secs(1)).await, 2);
        assert_eq!(gate.pending().await, 0);

        let bounces = journal_bounces(&gate).await;
        assert_eq!(bounces.len(), 2);
        assert!(bounces.iter().all(|b| b.outcome == Outcome::Expired));
    }

    #[tokio::test]
    async fn drain_shutdown_journals_pending_as_expired() {
        let (_dir, gate) = gate();
        let now = Instant::now();
        gate.bounce(request("in flight"), reason(), TTL, MAX_PENDING, now)
            .await;

        assert_eq!(gate.drain_shutdown().await, 1);
        assert_eq!(gate.pending().await, 0);

        let bounces = journal_bounces(&gate).await;
        assert_eq!(bounces.len(), 1);
        assert_eq!(bounces[0].outcome, Outcome::Expired);
        assert_eq!(bounces[0].message, "in flight");
    }

    /// Deliverer that yields mid-send, widening the window in which a second
    /// concurrent action could slip in if the gate did not hold its lock
    /// across delivery.
    struct YieldingDeliver {
        sends: StdMutex<u32>,
    }

    impl DeliverReply for YieldingDeliver {
        async fn deliver(&self, _request: &ReplyRequest) -> Result<Vec<u64>, DeliverError> {
            tokio::task::yield_now().await;
            let mut sends = self.sends.lock().unwrap();
            *sends += 1;
            Ok(vec![u64::from(*sends)])
        }
    }

    /// Witnesses the documented serialization guarantee: two concurrent
    /// releases of the same handle race on real threads, exactly one wins,
    /// the loser sees a dead handle, and exactly one send reaches the wire.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_releases_on_one_handle_yield_exactly_one_send() {
        let dir = TempDir::new().unwrap();
        let path = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
        let gate = Arc::new(ConsentGate::new(&path));
        let deliver = Arc::new(YieldingDeliver {
            sends: StdMutex::new(0),
        });
        let now = Instant::now();
        let ticket = gate
            .bounce(request("msg"), reason(), TTL, MAX_PENDING, now)
            .await;

        let spawn_release = |gate: Arc<ConsentGate>, deliver: Arc<YieldingDeliver>| {
            let handle = ticket.handle.clone();
            tokio::spawn(async move { gate.release(&*deliver, &handle, now).await })
        };
        let a = spawn_release(gate.clone(), deliver.clone());
        let b = spawn_release(gate.clone(), deliver.clone());
        let (a, b) = (a.await.unwrap(), b.await.unwrap());

        let winners = [&a, &b].into_iter().filter(|r| r.is_ok()).count();
        assert_eq!(winners, 1, "exactly one concurrent release may succeed");
        let loser = if a.is_ok() { b } else { a };
        match loser {
            Err(RejectedHandle::Unknown(h)) => assert_eq!(h, ticket.handle),
            other => panic!("the loser must see a dead handle, got {other:?}"),
        }
        assert_eq!(*deliver.sends.lock().unwrap(), 1, "one send, ever");

        let bounces = journal_bounces(&gate).await;
        assert_eq!(bounces.len(), 1, "exactly one outcome per bounce");
        assert_eq!(bounces[0].outcome, Outcome::Released);
    }

    #[tokio::test]
    async fn unknown_handle_is_rejected_without_journaling() {
        let (_dir, gate) = gate();
        let deliver = MockDeliver::ok();
        let bogus = HoldHandle::new("nr-0000-1");
        match gate.release(&deliver, &bogus, Instant::now()).await {
            Err(RejectedHandle::Unknown(h)) => assert_eq!(h, bogus),
            other => panic!("expected Unknown, got {other:?}"),
        }
        assert!(journal_bounces(&gate).await.is_empty());
    }

    // ── Partial multi-chunk delivery ─────────────────────────────────────

    /// Deliverer that fails its first send after posting one chunk, reporting
    /// partial progress; the second send succeeds.
    struct PartialThenOk {
        calls: StdMutex<u32>,
        requests: StdMutex<Vec<ReplyRequest>>,
        diary_records: Option<Vec<DiaryRecord>>,
    }

    /// Real chunker seam: fail after one fenced chunk, then render the typed
    /// continuation on retry.
    struct FencedPartialThenOk {
        calls: StdMutex<u32>,
        requests: StdMutex<Vec<ReplyRequest>>,
        retry_rendered: StdMutex<Vec<String>>,
    }

    impl DeliverReply for FencedPartialThenOk {
        async fn deliver(&self, request: &ReplyRequest) -> Result<Vec<u64>, DeliverError> {
            self.requests.lock().unwrap().push(request.clone());
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            let chunks = chunk_preserving_fences_with_context(
                &request.content,
                28,
                ChunkMode::Paragraph,
                request.fence_context(),
            )
            .expect("fenced chunks");
            if *calls == 1 {
                let failed = &chunks[1];
                request.set_fence_context(failed.incoming.clone());
                Err(DeliverError {
                    message: "chunk 1 failed".into(),
                    sent_ids: vec![100],
                    undelivered: Some(request.content[failed.source.start..].to_string()),
                    diary_records: Vec::new(),
                })
            } else {
                *self.retry_rendered.lock().unwrap() =
                    chunks.iter().map(|chunk| chunk.rendered.clone()).collect();
                Ok(vec![101])
            }
        }
    }

    impl DeliverReply for PartialThenOk {
        async fn deliver(&self, request: &ReplyRequest) -> Result<Vec<u64>, DeliverError> {
            self.requests.lock().unwrap().push(request.clone());
            let mut n = self.calls.lock().unwrap();
            *n += 1;
            if *n == 1 {
                Err(DeliverError {
                    message: "chunk 1 of 2 failed".into(),
                    sent_ids: vec![100],
                    undelivered: Some("the second half".into()),
                    diary_records: self
                        .diary_records
                        .clone()
                        .unwrap_or_else(|| request.pending_diary_records.clone()),
                })
            } else {
                Ok(vec![101])
            }
        }
    }

    #[tokio::test]
    async fn partial_delivery_retry_resumes_from_the_remainder() {
        let (_dir, gate) = gate();
        let deliver = PartialThenOk {
            calls: StdMutex::new(0),
            requests: StdMutex::new(Vec::new()),
            diary_records: None,
        };
        let now = Instant::now();
        let mut payload = request("the first half the second half");
        payload.pending_diary_records = vec![
            DiaryRecord::log_now("log", &payload.content),
            DiaryRecord::celebrate_now("celebrate", &payload.content),
            DiaryRecord::override_now("block", &payload.content),
        ];
        let expected_diary = payload.pending_diary_records.clone();
        let ticket = gate.bounce(payload, reason(), TTL, MAX_PENDING, now).await;

        // First attempt: chunk 0 lands, chunk 1 fails — the handle stays live.
        match gate.release(&deliver, &ticket.handle, now).await {
            Err(RejectedHandle::SendFailed { .. }) => {}
            other => panic!("expected SendFailed on partial delivery, got {other:?}"),
        }

        // Retry: resumes with only the undelivered remainder — chunk 0 is never
        // posted twice.
        let released = gate
            .release(&deliver, &ticket.handle, now + Duration::from_secs(1))
            .await
            .expect("retry resumes and completes");
        let sent = deliver.requests.lock().unwrap().clone();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].content, "the first half the second half");
        assert_eq!(sent[0].pending_diary_records, expected_diary);
        assert_eq!(
            sent[1].content, "the second half",
            "the retry must resume from the remainder, not re-post the delivered chunk"
        );
        assert_eq!(sent[1].pending_diary_records, expected_diary);
        assert_eq!(
            released.message_ids,
            vec![100, 101],
            "the released IDs fold the earlier partial chunk with the resumed one"
        );

        let bounces = journal_bounces(&gate).await;
        assert_eq!(bounces.len(), 1);
        assert_eq!(bounces[0].outcome, Outcome::Released);
        assert_eq!(
            bounces[0].message, "the first half the second half",
            "the journal records the full original message, not the trailing chunk"
        );
    }

    #[tokio::test]
    async fn partial_fenced_retry_carries_rendering_context_through_gate() {
        let (_dir, gate) = gate();
        let deliver = FencedPartialThenOk {
            calls: StdMutex::new(0),
            requests: StdMutex::new(Vec::new()),
            retry_rendered: StdMutex::new(Vec::new()),
        };
        let content = "before\n```rust\none two three four five six seven\n```\nafter";
        assert!(
            chunk_preserving_fences(content, 28, ChunkMode::Paragraph)
                .expect("initial chunks")
                .len()
                > 1
        );
        let now = Instant::now();
        let ticket = gate
            .bounce(request(content), reason(), TTL, MAX_PENDING, now)
            .await;

        assert!(matches!(
            gate.release(&deliver, &ticket.handle, now).await,
            Err(RejectedHandle::SendFailed { .. })
        ));
        gate.release(&deliver, &ticket.handle, now + Duration::from_secs(1))
            .await
            .expect("typed continuation completes");

        let requests = deliver.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].fence_context().is_some());
        let rendered = deliver.retry_rendered.lock().unwrap();
        assert!(
            rendered
                .first()
                .is_some_and(|chunk| chunk.starts_with("```rust\n"))
        );
        for chunk in rendered.iter() {
            let mut state = None;
            for line in chunk.split_inclusive('\n') {
                if line.contains("after") {
                    assert_eq!(state, None, "post-fence text must render outside code");
                }
                state = crate::discord::chunker::advance_fence(&state, line);
            }
            assert_eq!(state, None, "every retry chunk must be fence-balanced");
        }
    }

    #[tokio::test]
    async fn partial_rephrase_retry_preserves_send_diary_context() {
        let (_dir, gate) = gate();
        let mut replacement = request("the first half the second half");
        let expected_diary = vec![
            DiaryRecord::log_now("log", &replacement.content),
            DiaryRecord::celebrate_now("celebrate", &replacement.content),
        ];
        let deliver = PartialThenOk {
            calls: StdMutex::new(0),
            requests: StdMutex::new(Vec::new()),
            diary_records: Some(expected_diary.clone()),
        };
        let now = Instant::now();
        let ticket = gate
            .bounce(replacement.clone(), reason(), TTL, MAX_PENDING, now)
            .await;

        replacement.content = "the first half the second half".into();
        match gate
            .rephrase(
                &deliver,
                &judge(),
                &ticket.handle,
                &replacement.content,
                TTL,
                now,
            )
            .await
        {
            Err(RejectedHandle::SendFailed { .. }) => {}
            other => panic!("expected partial rephrase failure, got {other:?}"),
        }
        gate.rephrase(
            &deliver,
            &judge(),
            &ticket.handle,
            &replacement.content,
            TTL,
            now + Duration::from_secs(1),
        )
        .await
        .expect("retry completes");

        let requests = deliver.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].pending_diary_records, Vec::new());
        assert_eq!(requests[1].pending_diary_records, expected_diary);
    }

    // ── Fail-then-expire lifecycle ───────────────────────────────────────

    #[tokio::test]
    async fn failed_rephrase_then_expire_journals_the_replacement_as_expired() {
        let (_dir, gate) = gate();
        let deliver = MockDeliver::scripted(vec![Err("discord hiccup".into())]);
        let now = Instant::now();
        let ttl = Duration::from_secs(100);
        let ticket = gate
            .bounce(
                request("a straightforward plan"),
                reason(),
                ttl,
                MAX_PENDING,
                now,
            )
            .await;

        gate.rephrase(&deliver, &judge(), &ticket.handle, "a solid plan", ttl, now)
            .await
            .expect_err("scripted to fail delivery");

        // The refreshed deadline is now + ttl; let it lapse.
        let expired = gate.expire_due(now + ttl + Duration::from_secs(1)).await;
        assert_eq!(expired, 1);

        let bounces = journal_bounces(&gate).await;
        assert_eq!(bounces.len(), 1);
        assert_eq!(bounces[0].outcome, Outcome::Expired);
        assert_eq!(
            bounces[0].message, "a solid plan",
            "the abandoned held text is the consented replacement, not the original"
        );
    }

    // ── TTL refresh on judged-clear-but-failed rephrase ──────────────────

    #[tokio::test]
    async fn failed_rephrase_earns_a_fresh_decision_window() {
        let (_dir, gate) = gate();
        let deliver = MockDeliver::scripted(vec![Err("discord hiccup".into()), Ok(vec![1])]);
        let now = Instant::now();
        let ttl = Duration::from_secs(100);
        let ticket = gate
            .bounce(
                request("a straightforward plan"),
                reason(),
                ttl,
                MAX_PENDING,
                now,
            )
            .await;

        // Rephrase near the very end of the original window; delivery fails.
        gate.rephrase(
            &deliver,
            &judge(),
            &ticket.handle,
            "a solid plan",
            ttl,
            now + Duration::from_secs(90),
        )
        .await
        .expect_err("scripted to fail delivery");

        // The original deadline was now + 100. Without a refresh a release at
        // now + 150 would find the handle expired; the fresh TTL keeps it live
        // — symmetric with the fresh window a re-bounce mints.
        let released = gate
            .release(&deliver, &ticket.handle, now + Duration::from_secs(150))
            .await;
        assert!(
            released.is_ok(),
            "a judged-clear replacement that only failed to send must earn a fresh TTL, got {released:?}"
        );
    }

    // ── Shutdown drain vs an in-flight send ──────────────────────────────

    /// Deliverer that signals when it has entered the send (the handle is now
    /// in-flight, the queue lock released) and then blocks until told to
    /// proceed — so a test can race the shutdown drain against a live send.
    struct BlockingDeliver {
        entered: Arc<tokio::sync::Notify>,
        proceed: StdMutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    }

    impl DeliverReply for BlockingDeliver {
        async fn deliver(&self, _request: &ReplyRequest) -> Result<Vec<u64>, DeliverError> {
            self.entered.notify_one();
            let rx = self.proceed.lock().unwrap().take();
            if let Some(rx) = rx {
                let _ = rx.await;
            }
            Ok(vec![777])
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_shutdown_skips_an_in_flight_send_and_never_double_journals() {
        let dir = TempDir::new().unwrap();
        let path = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
        let gate = Arc::new(ConsentGate::new(&path));
        let now = Instant::now();

        let in_flight = gate
            .bounce(request("in flight"), reason(), TTL, MAX_PENDING, now)
            .await;
        let _abandoned = gate
            .bounce(request("abandoned"), reason(), TTL, MAX_PENDING, now)
            .await;

        let entered = Arc::new(tokio::sync::Notify::new());
        let (proceed_tx, proceed_rx) = tokio::sync::oneshot::channel();
        let deliver = Arc::new(BlockingDeliver {
            entered: entered.clone(),
            proceed: StdMutex::new(Some(proceed_rx)),
        });

        let release = {
            let gate = gate.clone();
            let deliver = deliver.clone();
            let handle = in_flight.handle.clone();
            tokio::spawn(async move { gate.release(&*deliver, &handle, now).await })
        };

        // Wait until the release is mid-send (its entry is in-flight).
        entered.notified().await;

        // Drain now: it must skip the in-flight entry and journal only the
        // abandoned one, so the in-flight send is never double-resolved.
        let drained = gate.drain_shutdown().await;
        assert_eq!(drained, 1, "drain skips the in-flight entry");

        // Let the send finish; it settles and journals its own release.
        proceed_tx.send(()).unwrap();
        let released = release.await.unwrap();
        assert!(released.is_ok(), "the in-flight send completes and settles");

        let bounces = journal_bounces(&gate).await;
        assert_eq!(
            bounces.len(),
            2,
            "exactly one outcome per bounce, no double-journal"
        );
        assert_eq!(
            bounces
                .iter()
                .filter(|b| b.outcome == Outcome::Released)
                .count(),
            1
        );
        assert_eq!(
            bounces
                .iter()
                .filter(|b| b.outcome == Outcome::Expired)
                .count(),
            1
        );
    }

    // ── Concurrent-delivery witness (looped, with an in-flight counter) ──

    /// Deliverer that counts how many of its sends run at once and records the
    /// peak, so a test can prove no two deliveries for one handle overlap.
    struct ConcurrencyWitness {
        in_flight: StdMutex<u32>,
        peak: StdMutex<u32>,
    }

    impl DeliverReply for ConcurrencyWitness {
        async fn deliver(&self, _request: &ReplyRequest) -> Result<Vec<u64>, DeliverError> {
            {
                let mut n = self.in_flight.lock().unwrap();
                *n += 1;
                let mut peak = self.peak.lock().unwrap();
                *peak = (*peak).max(*n);
            }
            tokio::task::yield_now().await;
            *self.in_flight.lock().unwrap() -= 1;
            Ok(vec![1])
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_releases_never_deliver_a_handle_twice_over_many_rounds() {
        let (_dir, gate) = gate();
        let gate = Arc::new(gate);
        let witness = Arc::new(ConcurrencyWitness {
            in_flight: StdMutex::new(0),
            peak: StdMutex::new(0),
        });
        let now = Instant::now();

        for _ in 0..64 {
            let ticket = gate
                .bounce(request("msg"), reason(), TTL, MAX_PENDING, now)
                .await;
            let spawn = || {
                let gate = gate.clone();
                let witness = witness.clone();
                let handle = ticket.handle.clone();
                tokio::spawn(async move { gate.release(&*witness, &handle, now).await })
            };
            let a = spawn();
            let b = spawn();
            let (a, b) = (a.await.unwrap(), b.await.unwrap());
            assert_eq!(
                [&a, &b].into_iter().filter(|r| r.is_ok()).count(),
                1,
                "exactly one concurrent release per handle may win"
            );
        }
        assert_eq!(
            *witness.peak.lock().unwrap(),
            1,
            "no two deliveries for a single handle ever overlapped"
        );
    }
}
