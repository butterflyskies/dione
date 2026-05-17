//! Integration tests for `AccessQueue` persistence.
//!
//! These tests verify that the queue survives a process restart (write → reload)
//! and that the atomic write protocol leaves no `.tmp` file behind.

use camino::Utf8PathBuf;
use chrono::Utc;
use dione::queue::{AccessQueue, AccessRequest};
use tempfile::TempDir;

fn temp_state() -> (TempDir, Utf8PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    (dir, path)
}

fn make_request(user_id: u64, preview: &str) -> AccessRequest {
    AccessRequest {
        user_id,
        username: format!("user_{user_id}"),
        message_preview: preview.to_string(),
        timestamp: Utc::now(),
    }
}

// TC-36: Queue persists via atomic write (tmp + rename).
#[test]
fn test_queue_persists_and_reloads() {
    let (_dir, state_dir) = temp_state();

    // First "process": enqueue some items and persist.
    {
        let mut q = AccessQueue::load(&state_dir);
        q.enqueue(make_request(1, "first request"), 50);
        q.enqueue(make_request(2, "second request"), 50);
        q.enqueue(make_request(3, "third request"), 50);
    }
    // queue.json must exist at this point.
    let queue_path = state_dir.join("queue.json");
    assert!(
        queue_path.as_std_path().exists(),
        "queue.json must be written after enqueue"
    );

    // Second "process": load from the same directory.
    let q2 = AccessQueue::load(&state_dir);
    let entries = q2.list();
    assert_eq!(entries.len(), 3, "all 3 entries must survive a reload");

    let ids: Vec<u64> = entries.iter().map(|r| r.user_id).collect();
    assert!(ids.contains(&1), "user 1 must be present after reload");
    assert!(ids.contains(&2), "user 2 must be present after reload");
    assert!(ids.contains(&3), "user 3 must be present after reload");
}

// TC-36: Verify atomic write — .tmp file does not persist after successful write.
#[test]
fn test_atomic_write_no_tmp_file_after_persist() {
    let (_dir, state_dir) = temp_state();
    let mut q = AccessQueue::load(&state_dir);
    q.enqueue(make_request(99, "atomicity test"), 50);

    let tmp_path = state_dir.join("queue.json.tmp");
    assert!(
        !tmp_path.as_std_path().exists(),
        ".tmp file must not remain after successful persist"
    );
}

// TC-36 variant: persist produces valid JSON that round-trips.
#[test]
fn test_persisted_json_is_valid() {
    let (_dir, state_dir) = temp_state();
    let mut q = AccessQueue::load(&state_dir);
    q.enqueue(make_request(42, "check json"), 50);

    let queue_path = state_dir.join("queue.json");
    let bytes = std::fs::read(queue_path.as_std_path()).expect("queue.json must be readable");
    let parsed: serde_json::Value =
        serde_json::from_slice(&bytes).expect("queue.json must be valid JSON");
    // The file is a JSON object keyed by user_id.
    assert!(parsed.is_object(), "queue.json must be a JSON object");
    // Entry for user 42 must be present.
    assert!(
        parsed.get("42").is_some(),
        "queue.json must contain entry for user 42"
    );
}

// Verify that approve removes the entry from persistence too.
#[test]
fn test_approve_removes_entry_from_disk() {
    let (_dir, state_dir) = temp_state();

    let mut q = AccessQueue::load(&state_dir);
    q.enqueue(make_request(7, "approve me"), 50);
    q.approve(7);

    // Reload and check the entry is gone.
    let q2 = AccessQueue::load(&state_dir);
    assert_eq!(
        q2.list().len(),
        0,
        "approved entry must not appear in reloaded queue"
    );
}

// Verify that deny removes the entry from persistence too.
#[test]
fn test_deny_removes_entry_from_disk() {
    let (_dir, state_dir) = temp_state();

    let mut q = AccessQueue::load(&state_dir);
    q.enqueue(make_request(8, "deny me"), 50);
    q.deny(8);

    let q2 = AccessQueue::load(&state_dir);
    assert_eq!(
        q2.list().len(),
        0,
        "denied entry must not appear in reloaded queue"
    );
}

// TC-34 variant: verify dedup survives a reload (second entry not persisted).
#[test]
fn test_dedup_is_persistent() {
    let (_dir, state_dir) = temp_state();

    let mut q = AccessQueue::load(&state_dir);
    q.enqueue(make_request(5, "original"), 50);
    // Attempt to enqueue the same user again — should be rejected.
    let added = q.enqueue(make_request(5, "duplicate"), 50);
    assert!(!added, "duplicate enqueue must return false");

    // Reload: should still have exactly one entry.
    let q2 = AccessQueue::load(&state_dir);
    assert_eq!(
        q2.list().len(),
        1,
        "only one entry per user must be persisted"
    );
    assert_eq!(
        q2.list()[0].message_preview,
        "original",
        "the original preview must be preserved, not the duplicate"
    );
}
