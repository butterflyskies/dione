use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A validated identifier suitable for use in an archive filename.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CorpusId(String);

#[derive(Debug, Error)]
#[error("corpus_id must contain only ASCII letters, digits, `_`, or `-`")]
pub struct CorpusIdError;

impl CorpusId {
    /// Parses a non-empty `[A-Za-z0-9_-]+` corpus identifier.
    pub fn parse(value: impl Into<String>) -> Result<Self, CorpusIdError> {
        let value = value.into();
        if !value.is_empty()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            Ok(Self(value))
        } else {
            Err(CorpusIdError)
        }
    }

    /// Returns the validated identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A GAIE event kind supported by the collector or latest-state replayer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    MessageCreate,
    MessageEdit,
    MessageDelete,
    ReactionSnapshot,
    ReactionAdd,
    ReactionRemove,
}

/// The Discord origin of an event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub platform: String,
    pub guild_id: String,
    pub channel_id: String,
    pub thread_id: Option<String>,
    pub message_id: String,
    pub actor_id: Option<String>,
    pub created_at: Option<String>,
    pub edited_at: Option<String>,
}

/// A downloaded Discord attachment descriptor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub id: String,
    pub filename: String,
    pub media_type: Option<String>,
    pub size: u64,
    pub sha256: Option<String>,
    pub url: String,
}

/// Event content and optional reaction fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Payload {
    pub content: Option<String>,
    pub content_sha256: Option<String>,
    pub attachments: Vec<Attachment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emoji_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub normal_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub burst_count: Option<u64>,
}

/// Message relationships carried by an event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relations {
    pub reply_to_message_id: Option<String>,
    pub thread_parent_channel_id: Option<String>,
}

/// Version-chain information carried by an event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lineage {
    pub observed_version_ordinal: Option<u64>,
    pub predecessor_event_id: Option<String>,
    pub history_status: String,
}

/// Collector provenance for an event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ingest {
    pub collector_version: String,
    pub raw_payload_sha256: String,
}

/// A typed GAIE schema-v1 event inside a format-v2 archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub schema_version: String,
    pub corpus_id: String,
    pub archive_seq: u64,
    pub event_id: String,
    pub event_kind: EventKind,
    pub observed_at: String,
    pub source: Source,
    pub payload: Payload,
    pub relations: Relations,
    pub lineage: Lineage,
    pub ingest: Ingest,
}
