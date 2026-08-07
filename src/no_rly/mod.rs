//! no_rly v2 — the handle-queue consent gate for outbound messages.
//!
//! When an [`OutboundJudge`] bounces an outbound message, the message is not
//! rejected outright: it is queued under a **single-use handle** and the
//! construct is told why. Three verbs act on the handle:
//!
//! - **release** (`no_rly` tool) — send the byte-identical queued message.
//! - **rephrase** — send a replacement, re-judged; a re-bounce mints a new
//!   handle chained to the old one.
//! - **ignore** — the handle expires and the abandonment is journaled.
//!
//! Handles die on release, rephrase, and expiry. A handle cannot exist before
//! its bounce, so pre-emptive overrides are structurally impossible, and it
//! cannot be replayed after use.
//!
//! Every bounce ends up in a durable JSONL audit journal (see [`journal`])
//! with its outcome, timing, and chain links. The judge is a trait seam
//! ([`judge::OutboundJudge`]) so the contradictionary word-matcher can later
//! be swapped for a classifier without touching the queue machinery.

pub mod consent;
pub mod journal;
pub mod judge;
pub mod queue;

pub use consent::{
    BounceTicket, ConsentGate, DeliverReply, RejectedHandle, Released, Rephrased, ReplyRequest,
};
pub use journal::{BounceRecord, Journal, JournalRecord, Outcome, SummaryRecord};
pub use judge::{OutboundJudge, ReasonEntry, RejectReason, Verdict};
pub use queue::{Held, HoldHandle, HoldQueue};
