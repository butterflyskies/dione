use camino::Utf8PathBuf;
use dione::gaie::{
    Archive, ArchivePaths, CorpusId, MessageContext, build_latest_state, message_batch,
};
use sha2::{Digest as _, Sha256};

const ORACLE_COMMIT: &str = "4a6f8e4";

#[test]
fn gaie_archive_corrected_python_v11_fixture_has_semantic_replay_parity() {
    assert_eq!(ORACLE_COMMIT, "4a6f8e4");
    let temporary = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temporary.path().to_path_buf()).unwrap();
    let corpus = CorpusId::parse("synthetic-test-corpus").unwrap();
    let paths = ArchivePaths::new(root.clone(), &corpus).unwrap();
    std::fs::copy(
        "tests/fixtures/gaie-v11/golden-archive.ndjson",
        root.join("synthetic-test-corpus.ndjson").as_std_path(),
    )
    .unwrap();
    let archive = Archive::open(paths, corpus, "2026-07-21T00:00:00Z").unwrap();
    let actual = serde_json::to_value(build_latest_state(
        &archive.read_committed().unwrap().events,
    ))
    .unwrap();
    let expected: serde_json::Value = serde_json::from_slice(
        &std::fs::read("tests/fixtures/gaie-v11/golden-latest-state.json").unwrap(),
    )
    .unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn gaie_archive_corrected_python_v11_cas_blob_matches_its_filename() {
    let digest = "9ba8b09774b2212931628b425971d17ba04e7e8f7f6892bb880f602e4aba9085";
    let bytes = std::fs::read(format!("tests/fixtures/gaie-v11/cas-blobs/{digest}")).unwrap();
    assert_eq!(bytes.len(), 35);
    assert_eq!(format!("{:x}", Sha256::digest(&bytes)), digest);
}

#[test]
fn gaie_archive_rust_producer_and_writer_match_normalized_oracle_events() {
    let raw = serde_json::json!({
        "id":"3","content":"hello","timestamp":"2026-01-01T00:00:00Z","edited_timestamp":null,
        "author":{"id":"4"},"attachments":[],
        "reactions":[{"emoji":{"id":null,"name":"💜"},"count":2,"count_details":{"normal":2,"burst":0}}]
    });
    let events = message_batch(
        &raw,
        MessageContext {
            corpus_id: "fixture",
            guild_id: "1",
            channel_id: "2",
            thread_id: None,
            thread_parent_channel_id: None,
            observed_at: "2026-01-02T00:00:00Z",
        },
        1,
        &std::collections::HashMap::new(),
    )
    .unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temporary.path().to_path_buf()).unwrap();
    let corpus = CorpusId::parse("fixture").unwrap();
    let paths = ArchivePaths::new(root, &corpus).unwrap();
    let mut archive = Archive::open(paths, corpus, "2026-01-02T00:00:00Z").unwrap();
    archive
        .append_batch(&events, "3", "2026-01-02T00:00:00Z")
        .unwrap();
    let mut actual = serde_json::to_value(archive.read_committed().unwrap().events).unwrap();
    for event in actual.as_array_mut().unwrap() {
        event["event_id"] = serde_json::json!("<uuid>");
        event["observed_at"] = serde_json::json!("<time>");
    }
    let expected: serde_json::Value = serde_json::from_slice(
        &std::fs::read("tests/fixtures/gaie-v11/producer-expected.normalized.json").unwrap(),
    )
    .unwrap();
    assert_eq!(actual, expected);
}
