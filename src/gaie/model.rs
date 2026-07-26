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

/// The adapter which transformed retained origin bytes into a GAIE event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OriginAdapter {
    pub name: String,
    pub version: String,
}

/// Optional agent-harness identity supplied by adapters which have one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OriginHarness {
    pub name: String,
    pub version: String,
}

/// A typed reference from a normalized event to retained byte-exact evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OriginEvidenceRef {
    pub adapter: OriginAdapter,
    pub sha256: String,
    pub location: String,
    pub media_type: String,
    pub selector: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<OriginHarness>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
