//! The consent gate — orchestrates bounce → {release | rephrase | expire}.
//!
//! [`ConsentGate`] owns the hold queue and the audit journal, and is the only
//! way the three verbs touch either. Delivery goes through the
//! [`DeliverReply`] seam so every gate semantic is testable without a Discord
//! connection.
//!
//! # Consent semantics
//!
//! A handle yields **at most one successful send**, ever. The gate holds its
//! internal lock across the delivery attempt and only settles (consumes) the
//! handle when the send succeeds — so:
//!
//! - a successful release or rephrase kills the handle (no replay);
//! - a *failed* send (outbound gate, Discord error) leaves the handle live
//!   until its deadline, because consuming it would strand the message with
//!   nothing sent and nothing retrievable;
//! - two concurrent actions on the same handle serialize, and the loser sees
//!   a dead handle.
//!
//! Expiry is enforced at claim time (see [`super::queue`]), so a handle past
//! its TTL is dead even if the background sweep has not caught it yet.

use std::time::{Duration, Instant};

use camino::Utf8Path;
use serenity::model::id::{ChannelId, MessageId};
use thiserror::Error;
use tokio::sync::Mutex;

use crate::no_rly::{
    journal::{self, Journal, Outcome},
    judge::{OutboundJudge, RejectReason, Verdict},
    queue::{ClaimError, Held, HoldHandle, HoldQueue},
};

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
}

/// The delivery seam: sends a [`ReplyRequest`] to Discord and returns the
/// sent message IDs. Implemented by the real messaging context and by test
/// doubles.
pub trait DeliverReply: Send + Sync {
    /// Attempt the send. `Err` is a human-readable failure the construct
    /// sees verbatim.
    fn deliver(
        &self,
        request: &ReplyRequest,
    ) -> impl Future<Output = Result<Vec<u64>, String>> + Send;
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

/// The consent gate: hold queue + audit journal behind one lock.
#[derive(Debug)]
pub struct ConsentGate {
    queue: Mutex<HoldQueue<ReplyRequest>>,
    journal: Journal,
}

impl ConsentGate {
    /// A gate whose journal lives in `state_dir`.
    pub fn new(state_dir: &Utf8Path) -> Self {
        Self {
            queue: Mutex::new(HoldQueue::new()),
            journal: Journal::new(state_dir),
        }
    }

    /// The gate's audit journal.
    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    /// Number of messages currently held.
    pub async fn pending(&self) -> usize {
        self.queue.lock().await.len()
    }

    /// Park a bounced request and mint its ticket.
    pub async fn bounce(
        &self,
        request: ReplyRequest,
        reason: RejectReason,
        ttl: Duration,
        now: Instant,
    ) -> BounceTicket {
        let mut queue = self.queue.lock().await;
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
        let mut queue = self.queue.lock().await;
        let entry = self.claim_live(&mut queue, handle, now)?;

        match deliver.deliver(&entry.payload).await {
            Ok(message_ids) => {
                queue.settle(handle);
                let latency_ms = entry.latency(now).as_millis() as u64;
                self.journal_resolved(handle, &entry, Outcome::Released, None, latency_ms);
                Ok(Released {
                    message_ids,
                    latency_ms,
                })
            }
            Err(error) => Err(RejectedHandle::SendFailed {
                handle: handle.clone(),
                error,
                expires_in: entry.expires_in(now),
            }),
        }
    }

    /// Rephrase: judge `replacement` and either send it (killing the handle,
    /// journaling the (original, reason, replacement) triple) or re-bounce it
    /// under a new handle chained to the old one.
    ///
    /// The old handle dies on a re-bounce too: the original text can never
    /// be sent once a replacement was offered — the chain carries the story
    /// forward instead.
    pub async fn rephrase<D: DeliverReply, J: OutboundJudge + ?Sized>(
        &self,
        deliver: &D,
        judge: &J,
        handle: &HoldHandle,
        replacement: &str,
        ttl: Duration,
        now: Instant,
    ) -> Result<Rephrased, RejectedHandle> {
        let mut queue = self.queue.lock().await;
        let entry = self.claim_live(&mut queue, handle, now)?;

        let request = ReplyRequest {
            content: replacement.to_string(),
            ..entry.payload.clone()
        };

        match judge.judge(replacement) {
            Verdict::Clear => match deliver.deliver(&request).await {
                Ok(message_ids) => {
                    queue.settle(handle);
                    let latency_ms = entry.latency(now).as_millis() as u64;
                    self.journal_resolved(
                        handle,
                        &entry,
                        Outcome::Rephrased,
                        Some(replacement),
                        latency_ms,
                    );
                    Ok(Rephrased::Sent { message_ids })
                }
                Err(error) => Err(RejectedHandle::SendFailed {
                    handle: handle.clone(),
                    error,
                    expires_in: entry.expires_in(now),
                }),
            },
            Verdict::Bounce(new_reason) => {
                queue.settle(handle);
                let latency_ms = entry.latency(now).as_millis() as u64;
                self.journal_resolved(
                    handle,
                    &entry,
                    Outcome::Rephrased,
                    Some(replacement),
                    latency_ms,
                );
                let new_handle = queue.hold(
                    request,
                    new_reason.clone(),
                    Some(handle.clone()),
                    ttl,
                    now,
                );
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
    /// an audit trail.
    pub async fn drain_shutdown(&self) -> usize {
        let now = Instant::now();
        let drained = self.queue.lock().await.drain();
        for (handle, entry) in &drained {
            self.journal_expired(handle, entry, now);
        }
        drained.len()
    }

    /// Claim a live entry or map the failure, journaling lazy expiry.
    fn claim_live(
        &self,
        queue: &mut HoldQueue<ReplyRequest>,
        handle: &HoldHandle,
        now: Instant,
    ) -> Result<Held<ReplyRequest>, RejectedHandle> {
        match queue.claim(handle, now) {
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

    fn journal_resolved(
        &self,
        handle: &HoldHandle,
        entry: &Held<ReplyRequest>,
        outcome: Outcome,
        replacement: Option<&str>,
        latency_ms: u64,
    ) {
        let record = journal::ResolvedBounce {
            handle: handle.as_str(),
            parent: entry.parent.as_ref().map(HoldHandle::as_str),
            message: &entry.payload.content,
            reason: entry.reason.clone(),
            outcome,
            replacement,
            bounced_at: entry.bounced_at.clone(),
            latency_ms,
        }
        .into_record();
        // A journal write failure must not undo or block the consented
        // action — log it and move on.
        if let Err(e) = self.journal.append(&record) {
            tracing::warn!(error = %e, handle = %handle, "failed to append no_rly journal record");
        }
    }

    fn journal_expired(&self, handle: &HoldHandle, entry: &Held<ReplyRequest>, now: Instant) {
        let latency_ms = entry.latency(now).as_millis() as u64;
        self.journal_resolved(handle, entry, Outcome::Expired, None, latency_ms);
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex as StdMutex};

    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    use super::*;
    use crate::{
        contradictionary::{Action, Contradictionary, Entry, MatchMode},
        no_rly::{
            journal::{BounceRecord, JournalRecord, StatsFilter},
            judge::{AlwaysClear, ReasonEntry},
        },
    };

    const TTL: Duration = Duration::from_secs(180);

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
        async fn deliver(&self, request: &ReplyRequest) -> Result<Vec<u64>, String> {
            self.requests.lock().unwrap().push(request.clone());
            self.results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(vec![9999]))
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

    fn journal_bounces(gate: &ConsentGate) -> Vec<BounceRecord> {
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
        let ticket = gate.bounce(original.clone(), reason(), TTL, now).await;
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

        let bounces = journal_bounces(&gate);
        assert_eq!(bounces.len(), 1);
        assert_eq!(bounces[0].outcome, Outcome::Released);
        assert_eq!(bounces[0].handle, ticket.handle.as_str());
        assert_eq!(bounces[0].message, "a straightforward plan");
        assert_eq!(bounces[0].latency_ms, 2000);
        assert_eq!(bounces[0].replacement, None);
    }

    #[tokio::test]
    async fn released_handle_is_dead() {
        let (_dir, gate) = gate();
        let deliver = MockDeliver::scripted(vec![Ok(vec![1]), Ok(vec![2])]);
        let now = Instant::now();
        let ticket = gate.bounce(request("msg"), reason(), TTL, now).await;

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
        let ticket = gate.bounce(request("msg"), reason(), TTL, now).await;

        let late = now + TTL + Duration::from_secs(1);
        match gate.release(&deliver, &ticket.handle, late).await {
            Err(RejectedHandle::Expired { handle, .. }) => {
                assert_eq!(handle, ticket.handle);
            }
            other => panic!("expected Expired, got {other:?}"),
        }
        assert!(deliver.requests().is_empty(), "nothing may send past expiry");

        let bounces = journal_bounces(&gate);
        assert_eq!(bounces.len(), 1);
        assert_eq!(bounces[0].outcome, Outcome::Expired);
    }

    #[tokio::test]
    async fn failed_send_leaves_handle_live_for_retry() {
        let (_dir, gate) = gate();
        let deliver =
            MockDeliver::scripted(vec![Err("discord hiccup".into()), Ok(vec![1002])]);
        let now = Instant::now();
        let ticket = gate.bounce(request("msg"), reason(), TTL, now).await;

        match gate.release(&deliver, &ticket.handle, now).await {
            Err(RejectedHandle::SendFailed { error, .. }) => {
                assert_eq!(error, "discord hiccup");
            }
            other => panic!("expected SendFailed, got {other:?}"),
        }
        assert!(
            journal_bounces(&gate).is_empty(),
            "a failed send is not an outcome — the bounce is still open"
        );

        let released = gate
            .release(&deliver, &ticket.handle, now + Duration::from_secs(5))
            .await
            .expect("retry within the TTL must work");
        assert_eq!(released.message_ids, vec![1002]);

        let bounces = journal_bounces(&gate);
        assert_eq!(bounces.len(), 1, "exactly one outcome per bounce");
        assert_eq!(bounces[0].outcome, Outcome::Released);
    }

    #[tokio::test]
    async fn rephrase_clean_sends_replacement_and_journals_triple() {
        let (_dir, gate) = gate();
        let deliver = MockDeliver::ok();
        let now = Instant::now();
        let original = request("a straightforward plan");
        let ticket = gate.bounce(original.clone(), reason(), TTL, now).await;

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

        let bounces = journal_bounces(&gate);
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
            .bounce(request("a straightforward plan"), reason(), TTL, now)
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

        let bounces = journal_bounces(&gate);
        assert_eq!(bounces.len(), 2);
        assert_eq!(bounces[0].outcome, Outcome::Rephrased);
        assert_eq!(bounces[1].outcome, Outcome::Released);
        assert_eq!(
            bounces[1].parent.as_deref(),
            Some(ticket.handle.as_str()),
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
            .bounce(request("a straightforward plan"), reason(), TTL, now)
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
    async fn expire_due_journals_and_empties() {
        let (_dir, gate) = gate();
        let now = Instant::now();
        gate.bounce(request("one"), reason(), TTL, now).await;
        gate.bounce(request("two"), reason(), TTL, now).await;

        assert_eq!(gate.expire_due(now + Duration::from_secs(60)).await, 0);
        assert_eq!(gate.pending().await, 2);

        assert_eq!(gate.expire_due(now + TTL + Duration::from_secs(1)).await, 2);
        assert_eq!(gate.pending().await, 0);

        let bounces = journal_bounces(&gate);
        assert_eq!(bounces.len(), 2);
        assert!(bounces.iter().all(|b| b.outcome == Outcome::Expired));
    }

    #[tokio::test]
    async fn drain_shutdown_journals_pending_as_expired() {
        let (_dir, gate) = gate();
        let now = Instant::now();
        gate.bounce(request("in flight"), reason(), TTL, now).await;

        assert_eq!(gate.drain_shutdown().await, 1);
        assert_eq!(gate.pending().await, 0);

        let bounces = journal_bounces(&gate);
        assert_eq!(bounces.len(), 1);
        assert_eq!(bounces[0].outcome, Outcome::Expired);
        assert_eq!(bounces[0].message, "in flight");
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
        assert!(journal_bounces(&gate).is_empty());
    }
}
