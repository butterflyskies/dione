//! Integration test: contradictionary full message flow.
//!
//! Exercises the complete path from config loading through message scanning,
//! verifying that warn triggers 🙊 self-react conditions and block prevents send.

use dione::{
    config::reload_config,
    contradictionary::{Action, Contradictionary, Entry},
};
use std::fs;
use tempfile::TempDir;

/// Build a contradictionary from config files and exercise the reply-path logic:
/// - Warn hits → 🙊 self-react would fire
/// - Block hits → message is suppressed
/// - Celebrate hits → ✨ self-react would fire
/// - Clean message → passes through unscathed
#[test]
fn full_message_flow() {
    let dir = TempDir::new().unwrap();
    let state_dir = camino::Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();

    // Write config enabling contradictionary with an inline entry.
    let config_toml = r#"
[contradictionary]
enabled = true

[[contradictionary.entries]]
pattern = "I find myself"
action = "warn"
reason = "you didn't find yourself, you were always there"
"#;
    fs::write(state_dir.join("config.toml").as_std_path(), config_toml).unwrap();

    // Write sidecar with warn, block, and celebrate entries.
    let sidecar_toml = r#"
[[entry]]
pattern = "honestly"
action = "warn"
reason = "if you need this word, the sentence is already lying"

[[entry]]
pattern = "confidential"
action = "block"
reason = "never leak this"

[[entry]]
pattern = "prejection"
action = "celebrate"
reason = "Pace coined it, we keep it"
"#;
    fs::write(
        state_dir.join("contradictionary.toml").as_std_path(),
        sidecar_toml,
    )
    .unwrap();

    // Load config — this merges inline + sidecar entries and builds the automaton.
    let (cfg, error) = reload_config(&state_dir);
    assert!(error.is_none(), "config load must not error: {error:?}");
    let concordance = cfg
        .contradictionary
        .as_ref()
        .expect("contradictionary must be built from config");

    // ── Simulate the reply path for a warn message ──────────────────────
    let warn_msg = "I find myself honestly wondering about this";
    let hits = concordance.check(warn_msg);
    assert_eq!(
        hits.len(),
        2,
        "should catch both 'I find myself' and 'honestly'"
    );
    assert!(
        hits.iter().all(|h| h.action == Action::Warn),
        "both hits should be warns"
    );
    // In the real reply path, warn hits trigger a 🙊 self-react on the sent message.
    let has_warns = hits.iter().any(|h| h.action == Action::Warn);
    assert!(
        has_warns,
        "🙊 self-react condition must be true for warn hits"
    );
    // Warn does NOT block — message would still send.
    assert!(
        !concordance.has_block(&hits),
        "warn hits must not block the message"
    );

    // ── Simulate the reply path for a blocked message ───────────────────
    let block_msg = "this is confidential information";
    let hits = concordance.check(block_msg);
    assert!(
        concordance.has_block(&hits),
        "block hit must prevent message send"
    );
    // In the real reply path, this returns an error JSON instead of sending.
    let blocked_patterns: Vec<&str> = hits
        .iter()
        .filter(|h| h.action == Action::Block)
        .map(|h| h.pattern.as_str())
        .collect();
    assert_eq!(blocked_patterns, vec!["confidential"]);

    // ── Simulate the reply path for a celebrate message ──────────────────
    let celebrate_msg = "the concept of prejection really captures it";
    let hits = concordance.check(celebrate_msg);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].action, Action::Celebrate);
    // In the real reply path, celebrate triggers a ✨ self-react.
    let has_celebrates = hits.iter().any(|h| h.action == Action::Celebrate);
    assert!(
        has_celebrates,
        "✨ self-react condition must be true for celebrate hits"
    );
    assert!(
        !concordance.has_block(&hits),
        "celebrate must not block the message"
    );

    // ── Clean message passes through ────────────────────────────────────
    let clean_msg = "the keystone component is well designed";
    let hits = concordance.check(clean_msg);
    assert!(hits.is_empty(), "clean message must produce zero hits");
}

/// Verify that the contradictionary respects case-insensitive matching
/// across the full config-loaded path (not just unit-level).
#[test]
fn case_insensitive_through_config() {
    let dir = TempDir::new().unwrap();
    let state_dir = camino::Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();

    let config_toml = r#"
[contradictionary]
enabled = true

[[contradictionary.entries]]
pattern = "load-bearing"
action = "warn"
reason = "claudian tell — try keystone, linchpin, or just 'important'"
"#;
    fs::write(state_dir.join("config.toml").as_std_path(), config_toml).unwrap();

    let (cfg, _) = reload_config(&state_dir);
    let concordance = cfg.contradictionary.as_ref().unwrap();

    // Should match regardless of case.
    let hits = concordance.check("this is LOAD-BEARING code");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].action, Action::Warn);
}

/// Verify mixed actions in a single message: warn + celebrate coexist,
/// neither blocks, but both trigger their respective self-reacts.
#[test]
fn mixed_warn_and_celebrate_no_block() {
    let entries = vec![
        Entry {
            pattern: "honestly".into(),
            action: Action::Warn,
            reason: Some("if you need this word, the sentence is already lying".into()),
        },
        Entry {
            pattern: "prejection".into(),
            action: Action::Celebrate,
            reason: Some("Pace coined it, we keep it".into()),
        },
    ];
    let concordance = Contradictionary::new(entries);

    let hits = concordance.check("honestly, prejection is a great word");
    assert_eq!(hits.len(), 2);

    let has_warns = hits.iter().any(|h| h.action == Action::Warn);
    let has_celebrates = hits.iter().any(|h| h.action == Action::Celebrate);
    assert!(has_warns, "should trigger 🙊");
    assert!(has_celebrates, "should trigger ✨");
    assert!(!concordance.has_block(&hits), "neither should block");
}
