//! Durable GAIE format-v2 archive support.
#![warn(missing_docs)]

/// Default maximum size of one GAIE attachment (25 MiB).
pub const DEFAULT_ARCHIVE_MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;
/// Default cumulative GAIE attachment download budget for one backfill run (250 MiB).
pub const DEFAULT_ARCHIVE_MAX_RUN_DOWNLOAD_BYTES: u64 = 250 * 1024 * 1024;

mod archive;
mod backfill;
mod model;
mod origin;
mod replay;
mod service;

pub use archive::{
    Archive, ArchiveError, ArchivePaths, Checkpoint, QuarantinedEvent, ReadResult, StreamCheckpoint,
};
pub use backfill::{BackfillOptions, BackfillRunError, CaptureRoot, CaptureTarget, run_backfill};
pub use model::{
    Attachment, CorpusId, CorpusIdError, Event, EventKind, Ingest, Lineage, OriginAdapter,
    OriginEvidenceRef, OriginHarness, Payload, Relations, Source,
};
pub use replay::{LatestMessage, ReplayError, build_latest_state};
pub use service::{
    BackfillReport, DiscordArchiveClient, DiscordArchiveError, MessageContext, message_batch,
};
