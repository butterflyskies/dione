//! Durable GAIE format-v2 archive support.

mod archive;
mod backfill;
mod model;
mod origin;
mod replay;
mod service;

pub use archive::{Archive, ArchiveError, ArchivePaths, Checkpoint, ReadResult, StreamCheckpoint};
pub use backfill::{BackfillOptions, BackfillRunError, CaptureRoot, CaptureTarget, run_backfill};
pub use model::{
    Attachment, CorpusId, Event, EventKind, Ingest, Lineage, OriginAdapter, OriginEvidenceRef,
    OriginHarness, Payload, Relations, Source,
};
pub use replay::{LatestMessage, ReplayError, build_latest_state};
pub use service::{
    BackfillReport, DiscordArchiveClient, DiscordArchiveError, MessageContext, message_batch,
};
