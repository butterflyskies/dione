use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A validated identifier suitable for use in an archive filename.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CorpusId(String);

#[derive(Debug, Error)]
#[error("corpus_id must contain only ASCII letters, digits, `_`, or `-`")]
/// The supplied corpus identifier does not match the accepted grammar.
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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use test_case::test_case;

    #[test_case("a"; "letter")]
    #[test_case("A0_-"; "all admitted classes")]
    #[test_case(""; "empty")]
    #[test_case("../escape"; "traversal punctuation")]
    #[test_case("space here"; "space")]
    fn test_gaie_archive_corpus_examples(value: &str) {
        let expected = !value.is_empty()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
        assert_eq!(CorpusId::parse(value).is_ok(), expected);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        #[test]
        fn prop_gaie_archive_corpus_matches_ascii_grammar(value in ".{0,48}") {
            let expected = !value.is_empty() && value.bytes().all(|byte| {
                matches!(byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-')
            });
            prop_assert_eq!(CorpusId::parse(&value).is_ok(), expected);
        }
    }
}

#[cfg(kani)]
mod proofs {
    use super::CorpusId;

    #[kani::proof]
    #[kani::unwind(9)]
    fn corpus_acceptance_matches_bounded_ascii_grammar() {
        let bytes: [u8; 8] = kani::any();
        let length: usize = kani::any();
        kani::assume(length <= bytes.len());
        kani::assume(bytes[..length].iter().all(u8::is_ascii));
        let value = std::str::from_utf8(&bytes[..length]).unwrap_or("");
        let expected = length > 0
            && bytes[..length]
                .iter()
                .all(|byte| matches!(*byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-'));
        assert_eq!(CorpusId::parse(value).is_ok(), expected);
    }
}

/// A GAIE event kind supported by the collector or latest-state replayer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// Observed creation or first captured state of a message.
    MessageCreate,
    /// Observed change to message content.
    MessageEdit,
    /// Observed message deletion.
    MessageDelete,
    /// Observed aggregate reaction count.
    ReactionSnapshot,
    /// Observed reaction count increment.
    ReactionAdd,
    /// Observed reaction count decrement.
    ReactionRemove,
}

/// The Discord origin of an event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    /// Source platform name.
    pub platform: String,
    /// Source guild identifier.
    pub guild_id: String,
    /// Source channel identifier.
    pub channel_id: String,
    /// Source thread identifier when present.
    pub thread_id: Option<String>,
    /// Source message identifier.
    pub message_id: String,
    /// Supplied source actor identifier when present.
    pub actor_id: Option<String>,
    /// Source creation timestamp when available.
    pub created_at: Option<String>,
    /// Source edit timestamp when available.
    pub edited_at: Option<String>,
}

/// A downloaded Discord attachment descriptor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    /// Source attachment identifier.
    pub id: String,
    /// Supplied attachment filename.
    pub filename: String,
    /// Supplied media type when available.
    pub media_type: Option<String>,
    /// Declared attachment size in bytes.
    pub size: u64,
    /// Content digest when the attachment was retained.
    pub sha256: Option<String>,
    /// Source attachment URL.
    pub url: String,
}

/// Event content and optional reaction fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Payload {
    /// Observed message content when present.
    pub content: Option<String>,
    /// Content digest when present.
    pub content_sha256: Option<String>,
    /// Observed attachment descriptors.
    pub attachments: Vec<Attachment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Reaction identifier when present.
    pub emoji_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Aggregate reaction count when present.
    pub count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Normal reaction count when present.
    pub normal_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Burst reaction count when present.
    pub burst_count: Option<u64>,
}

/// Message relationships carried by an event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relations {
    /// Referenced message identifier when present.
    pub reply_to_message_id: Option<String>,
    /// Parent channel identifier for a thread when present.
    pub thread_parent_channel_id: Option<String>,
}

/// Version-chain information carried by an event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lineage {
    /// Observed version number when known.
    pub observed_version_ordinal: Option<u64>,
    /// Previous event identifier when known.
    pub predecessor_event_id: Option<String>,
    /// Collector's description of history completeness.
    pub history_status: String,
}

/// Collector provenance for an event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ingest {
    /// Collector implementation version.
    pub collector_version: String,
    /// Digest of the collector's raw payload representation.
    pub raw_payload_sha256: String,
}

/// The adapter which transformed retained origin bytes into a GAIE event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OriginAdapter {
    /// Origin adapter implementation name.
    pub name: String,
    /// Origin adapter implementation version.
    pub version: String,
}

/// Optional agent-harness identity supplied by adapters which have one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OriginHarness {
    /// Supplied harness implementation name.
    pub name: String,
    /// Supplied harness implementation version.
    pub version: String,
}

/// A typed reference from a normalized event to retained byte-exact evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OriginEvidenceRef {
    /// Adapter used to project retained source bytes.
    pub adapter: OriginAdapter,
    /// Digest of the retained source bytes.
    pub sha256: String,
    /// Archive-relative location of retained source bytes.
    pub location: String,
    /// Retained source media type.
    pub media_type: String,
    /// Selector identifying the source object within retained bytes.
    pub selector: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Optional supplied harness identity.
    pub harness: Option<OriginHarness>,
}

/// A typed GAIE schema-v1 event inside a format-v2 archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Event schema version.
    pub schema_version: String,
    /// Owning archive corpus identifier.
    pub corpus_id: String,
    /// Sequence number within the archive.
    pub archive_seq: u64,
    /// Event identifier.
    pub event_id: String,
    /// Observed event category.
    pub event_kind: EventKind,
    /// Collector observation timestamp.
    pub observed_at: String,
    /// Supplied source coordinates and timestamps.
    pub source: Source,
    /// Observed content, attachments, or reaction values.
    pub payload: Payload,
    /// Observed message relationships.
    pub relations: Relations,
    /// Observed version-chain metadata.
    pub lineage: Lineage,
    /// Collector provenance metadata.
    pub ingest: Ingest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Optional reference to retained exact source bytes.
    pub origin_evidence: Option<OriginEvidenceRef>,
}

#[cfg(test)]
mod event_tests {
    use super::*;

    #[test]
    fn historical_event_without_origin_evidence_deserializes_and_serializes_unchanged() {
        let historical = serde_json::json!({
            "schema_version":"1",
            "corpus_id":"fixture",
            "archive_seq":1,
            "event_id":"00000000-0000-4000-8000-000000000001",
            "event_kind":"message_create",
            "observed_at":"2026-01-01T00:00:00Z",
            "source":{
                "platform":"discord","guild_id":"1","channel_id":"2","thread_id":null,
                "message_id":"3","actor_id":"4","created_at":"2026-01-01T00:00:00Z",
                "edited_at":null
            },
            "payload":{
                "content":"hello","content_sha256":null,"attachments":[]
            },
            "relations":{"reply_to_message_id":null,"thread_parent_channel_id":null},
            "lineage":{
                "observed_version_ordinal":null,"predecessor_event_id":null,
                "history_status":"complete"
            },
            "ingest":{"collector_version":"fixture","raw_payload_sha256":"00"}
        });

        let event: Event = serde_json::from_value(historical.clone()).unwrap();
        assert!(event.origin_evidence.is_none());
        assert_eq!(serde_json::to_value(event).unwrap(), historical);
    }
}
