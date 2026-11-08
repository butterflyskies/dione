use camino::Utf8PathBuf;
use dione_attention::{
    config::{AttentionConfig, NoticeMode},
    health::{AttentionHealth, AttentionHealthController, HealthPersistenceErrorKind},
};
use std::{error::Error, sync::Arc, time::Duration};

fn controller(dir: &tempfile::TempDir, state: AttentionHealth) -> Arc<AttentionHealthController> {
    Arc::new(AttentionHealthController::from_restored(
        state,
        Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap(),
        1_000,
    ))
}

#[tokio::test(start_paused = true)]
async fn lifecycle_backoff_and_literal_status_shape() {
    let dir = tempfile::tempdir().unwrap();
    let health = controller(&dir, AttentionHealth::default());
    health.unknown("fixture unknown");
    health.failed("fixture outage".into(), 200);
    assert!(!health.probe_due());
    tokio::time::advance(Duration::from_millis(200)).await;
    assert!(health.probe_due());
    health.recovered();
    assert_eq!(
        serde_json::to_value(health.snapshot()).unwrap(),
        serde_json::json!({
            "degraded":null,"last_unknown":"fixture unknown","failures":1,"recoveries":1,
            "retry_after_ms":0,"last_notice_ms":null,"noticed_failures":0,"noticed_recoveries":0,
            "status_write_error":null,"notice_failures":0,"last_notice_error":null
        })
    );
    health.policy_unavailable("fixture calibration".into());
    health.policy_unavailable("fixture calibration".into());
    assert_eq!(health.snapshot().failures, 2);
    assert!(health.probe_due());
}

#[tokio::test(start_paused = true)]
async fn disabled_notices_consume_events_and_cooldown_coalesces_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let health = controller(&dir, AttentionHealth::default());
    let mut config = AttentionConfig::default();
    config.notices = NoticeMode::Off;
    config.notice_cooldown_ms = 100;
    health.failed("fixture outage".into(), 20);
    assert_eq!(health.propose_notice(&config).unwrap(), None);
    config.notices = NoticeMode::FailuresAndRecovery;
    assert_eq!(health.propose_notice(&config).unwrap(), None);
    health.failed("fixture outage".into(), 20);
    assert_eq!(
        health.propose_notice(&config).unwrap().as_deref(),
        Some(
            "Attention is degraded: fixture outage. Eligible messages are using ordinary delivery."
        )
    );
    health.recovered();
    assert_eq!(health.propose_notice(&config).unwrap(), None);
    tokio::time::advance(Duration::from_millis(100)).await;
    assert_eq!(
        health.propose_notice(&config).unwrap().as_deref(),
        Some("Attention evaluation recovered. Existing modes and room settings are unchanged.")
    );
    health.record_notice_outcome(Err("fixture sink failure"));
    assert_eq!(health.snapshot().notice_failures, 1);
    assert_eq!(
        health.snapshot().last_notice_error.as_deref(),
        Some("fixture sink failure")
    );
    health.record_notice_outcome(Ok(()));
    assert_eq!(health.snapshot().last_notice_error, None);
}

#[tokio::test]
async fn synchronous_restore_distinguishes_absent_corrupt_and_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    let path = Utf8PathBuf::from_path_buf(dir.path().join("attention-status.json")).unwrap();
    assert_eq!(AttentionHealthController::load(&path).degraded, None);
    std::fs::write(&path, b"{not json").unwrap();
    assert_eq!(
        AttentionHealthController::load(&path).degraded.as_deref(),
        Some("attention status is corrupt")
    );
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert_eq!(
        AttentionHealthController::load(&path).degraded.as_deref(),
        Some("attention status is unreadable")
    );
}

#[tokio::test]
async fn persistence_roundtrip_and_failed_write_remains_dirty_for_retry() {
    let dir = tempfile::tempdir().unwrap();
    let health = controller(&dir, AttentionHealth::default());
    health.failed("fixture outage".into(), 50);
    health.persist().await.unwrap();
    let path = Utf8PathBuf::from_path_buf(dir.path().join("attention-status.json")).unwrap();
    assert_eq!(AttentionHealthController::load(&path).failures, 1);
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    health.unknown("fixture changed");
    let error = health.persist().await.unwrap_err();
    assert_eq!(error.to_string(), "attention status persistence failed");
    assert!(matches!(error.kind, HealthPersistenceErrorKind::Io(_)));
    assert!(error.source().unwrap().source().is_some());
    assert_eq!(
        health.snapshot().status_write_error.as_deref(),
        Some("attention status persistence failed")
    );
    std::fs::remove_dir(&path).unwrap();
    health.persist().await.unwrap();
    let restored = AttentionHealthController::load(&path);
    assert_eq!(restored.last_unknown.as_deref(), Some("fixture changed"));
    assert_eq!(restored.status_write_error, None);
}
