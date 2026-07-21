//! Durable GAIE format-v2 archive support.

mod archive;
mod model;
mod replay;
mod service;

pub use archive::{Archive, ArchiveError, ArchivePaths, Checkpoint, ReadResult};
pub use model::{
    Attachment, CorpusId, Event, EventKind, Ingest, Lineage, Payload, Relations, Source,
};
pub use replay::{LatestMessage, ReplayError, build_latest_state};
pub use service::{
    BackfillReport, DiscordArchiveClient, DiscordArchiveError, MessageContext, message_batch,
};
