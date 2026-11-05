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

//! External requests and progress use checked public construction.
//!
//! ```compile_fail
//! use dione_outbound_consent::ReplyRequest;
//! use serenity::model::id::ChannelId;
//! let mut request = ReplyRequest::new(ChannelId::new(42), "text".into(), None, true, false);
//! request.pending_diary_records = Vec::new();
//! ```
//!
//! ```compile_fail
//! use dione_outbound_consent::ReplyRequest;
//! use serenity::model::id::ChannelId;
//! let request = ReplyRequest::new(ChannelId::new(42), "text".into(), None, true, false);
//! let context = request.fence_context;
//! ```
//!
//! ```compile_fail
//! use dione_outbound_consent::FenceContext;
//! let context = FenceContext { backticks: 3, lang: "rs".to_owned() };
//! ```
//!
//! ```compile_fail
//! use dione_outbound_consent::chunker::FenceChunk;
//! let chunk = FenceChunk { rendered: "text".into(), source: 0..4, incoming: None };
//! ```
//!
//! ```compile_fail
//! use dione_outbound_consent::delivery::PostedMessages;
//! use serenity::model::id::MessageId;
//! let posted = PostedMessages { first: MessageId::new(1), rest: Vec::new() };
//! ```
//!
//! ```compile_fail
//! use dione_outbound_consent::delivery::{DeliverError, DeliveryCause};
//! use serenity::model::id::{ChannelId, MessageId};
//! let error = DeliverError::partial(DeliveryCause::OutboundDenied(ChannelId::new(42)), Vec::<MessageId>::new(), "remaining".into(), Vec::new());
//! ```
//!
//! ```compile_fail
//! use dione_outbound_consent::delivery::{PartialFailure, DeliveryCause, PostedMessages};
//! use serenity::model::id::{ChannelId, MessageId};
//! let failure = PartialFailure {
//!     cause: DeliveryCause::OutboundDenied(ChannelId::new(42)),
//!     sent: PostedMessages::new(MessageId::new(1), Vec::new()),
//!     remainder: "remaining".into(), diary: Vec::new(),
//! };
//! ```

#![warn(missing_docs)]

pub mod chunker;
pub mod diary;
mod util;

pub mod consent;
pub mod delivery;
pub mod journal;
pub mod judge;
pub mod queue;

pub use consent::{
    BounceTicket, ConsentGate, DeliverReply, RejectedHandle, Released, Rephrased, Replacement,
    ReplyRequest,
};
pub use journal::{BounceRecord, Journal, JournalRecord, Outcome, SummaryRecord};
pub use judge::{OutboundJudge, ReasonEntry, RejectReason, Verdict};
pub use queue::{Held, HoldHandle, HoldQueue};

pub use chunker::{
    FenceContext, chunk, chunk_preserving_fences, chunk_preserving_fences_with_context,
};
