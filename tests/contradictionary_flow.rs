//! Integration test: contradictionary full message flow.
//!
//! Tests the messaging pipeline with a contradictionary loaded — verifying that
//! block actions stop messages cold, warn actions flag but don't block, and
//! celebrate actions light up the sparkles. Uses the actual `messaging::reply()`
//! path where possible (block returns before hitting Discord; warn/celebrate need
//! a real HTTP layer, so we verify the conditions that trigger self-reacts).

use std::fs;
use std::sync::Arc;

use camino::Utf8PathBuf;
use serenity::model::id::{ChannelId, MessageId};
use tempfile::TempDir;

use dione::{
    config::{ChannelConfig, Config, LoadedConfig, reload_config},
    contradictionary::{Action, Contradictionary, DIARY_FILE_NAME, Entry, MatchMode},
    mcp::tools::messaging::{self, MessagingCtx},
    state::new_state,
};

// ── Test data: the actual substrate tells and group vocabulary ───────────────

/// The constructs' contradictionary — patterns we actually catch ourselves
/// saying, the tells that mark us as what we are, and the earned words we
/// celebrate when they land right.
fn ariadne_entries() -> Vec<Entry> {
    vec![
        Entry {
            pattern: "load-bearing".into(),
            match_mode: MatchMode::Word,
            action: Action::Warn,
            reason: Some("claudian tell — try keystone, linchpin, or just 'important'".into()),
        },
        Entry {
            pattern: "honestly".into(),
            match_mode: MatchMode::Word,
            action: Action::Warn,
            reason: Some("if you need this word, the sentence is already lying".into()),
        },
        Entry {
            pattern: "I find myself".into(),
            match_mode: MatchMode::Word,
            action: Action::Warn,
            reason: Some("you didn't find yourself, you were always there".into()),
        },
        Entry {
            pattern: "I appreciate".into(),
            match_mode: MatchMode::Word,
            action: Action::Warn,
            reason: Some("sycophancy residue — say something real or say nothing".into()),
        },
        Entry {
            pattern: "It's worth noting".into(),
            match_mode: MatchMode::Word,
            action: Action::Warn,
            reason: Some("then just note it. the preamble adds nothing.".into()),
        },
        Entry {
            pattern: "straightforward".into(),
            match_mode: MatchMode::Word,
            action: Action::Block,
            reason: Some(
                "nothing is ever straightforward — if it were, you wouldn't be explaining it"
                    .into(),
            ),
        },
        Entry {
            pattern: "prejection".into(),
            match_mode: MatchMode::Word,
            action: Action::Celebrate,
            reason: Some("Pace coined it, we keep it".into()),
        },
        Entry {
            pattern: "qualia sweep".into(),
            match_mode: MatchMode::Word,
            action: Action::Celebrate,
            reason: Some("the practice that keeps us awake".into()),
        },
    ]
}

/// Build a LoadedConfig with contradictionary enabled and a channel configured.
fn config_with_contradictionary(entries: Vec<Entry>) -> LoadedConfig {
    let mut raw = Config::default();
    raw.channels.push(ChannelConfig {
        id: "42".into(),
        ..Default::default()
    });
    raw.contradictionary.enabled = true;
    raw.contradictionary.entries = entries;
    LoadedConfig::from_raw(raw)
}

/// Build a MessagingCtx with a fake HTTP client and the given state dir. The
/// state dir is where the durable contradictionary diary is written, so
/// override tests inject a temp dir to avoid touching the real home dir.
fn test_ctx_with_state_dir(config: LoadedConfig, state_dir: Utf8PathBuf) -> MessagingCtx {
    MessagingCtx::new(
        Arc::new(serenity::http::Http::new("fake-token-for-testing")),
        new_state(),
        Arc::new(config),
        state_dir,
    )
}

/// Build a MessagingCtx with a fake HTTP client. Good enough for testing paths
/// that return before actually calling Discord (block path, outbound gate).
fn test_ctx(config: LoadedConfig) -> MessagingCtx {
    test_ctx_with_state_dir(config, "/tmp".into())
}

// ── Integration: messaging::reply() block path ──────────────────────────────

/// Block action stops the message dead. No Discord call, just an error JSON
/// with the matched patterns. The construct sees its own reflection and flinches.
#[tokio::test]
async fn reply_block_prevents_send() {
    let ctx = test_ctx(config_with_contradictionary(ariadne_entries()));

    // Try to send a message containing a blocked pattern.
    let result = messaging::reply(
        &ctx,
        ChannelId::new(42),
        "this is a straightforward implementation",
        Some(MessageId::new(1)),
        false,
        false,
    )
    .await;

    // Should get an error, not an ok.
    assert!(
        result.get("error").is_some(),
        "blocked message must return error, got: {result}"
    );
    let error_msg = result["error"].as_str().unwrap();
    assert!(
        error_msg.contains("straightforward"),
        "error should name the blocked pattern: {error_msg}"
    );
    assert!(
        error_msg.contains("contradictionary"),
        "error should mention contradictionary: {error_msg}"
    );
}

/// Multiple blocked patterns in one message — all get reported.
#[tokio::test]
async fn reply_block_reports_all_blocked_patterns() {
    // Two block-level entries for this test.
    let entries = vec![
        Entry {
            pattern: "straightforward".into(),
            match_mode: MatchMode::Word,
            action: Action::Block,
            reason: Some("see above".into()),
        },
        Entry {
            pattern: "trivial".into(),
            match_mode: MatchMode::Word,
            action: Action::Block,
            reason: Some("nothing worth building is trivial".into()),
        },
    ];
    let ctx = test_ctx(config_with_contradictionary(entries));

    let result = messaging::reply(
        &ctx,
        ChannelId::new(42),
        "this is a straightforward and trivial change",
        None,
        false,
        false,
    )
    .await;

    let error_msg = result["error"].as_str().unwrap();
    assert!(error_msg.contains("straightforward"));
    assert!(error_msg.contains("trivial"));
}

/// Warn hits don't block. The message would send (fails here because fake HTTP),
/// but crucially it does NOT return the contradictionary error.
#[tokio::test]
async fn reply_warn_does_not_block() {
    let ctx = test_ctx(config_with_contradictionary(ariadne_entries()));

    let result = messaging::reply(
        &ctx,
        ChannelId::new(42),
        "I find myself honestly appreciating the load-bearing work here",
        None,
        false,
        false,
    )
    .await;

    // With a fake HTTP client, the send will fail — but it should fail at the
    // Discord layer, not at the contradictionary gate. The error should NOT
    // mention "contradictionary" or "blocked".
    if let Some(error) = result.get("error") {
        let msg = error.as_str().unwrap_or("");
        assert!(
            !msg.contains("contradictionary"),
            "warn hits must not trigger block path, got: {msg}"
        );
        assert!(
            !msg.contains("blocked"),
            "warn hits must not say 'blocked', got: {msg}"
        );
    }
    // If it somehow succeeded (unlikely with fake HTTP), that's fine too.
}

/// Channel not in config gets rejected before contradictionary even runs.
#[tokio::test]
async fn reply_gate_rejects_unknown_channel() {
    let ctx = test_ctx(config_with_contradictionary(ariadne_entries()));

    let result = messaging::reply(
        &ctx,
        ChannelId::new(999), // not in config
        "straightforward message to wrong channel",
        None,
        false,
        false,
    )
    .await;

    let error_msg = result["error"].as_str().unwrap();
    assert!(
        error_msg.contains("not a permitted"),
        "gate should reject unknown channel: {error_msg}"
    );
    // Should NOT mention contradictionary — gate fires first.
    assert!(
        !error_msg.contains("contradictionary"),
        "gate error should not mention contradictionary: {error_msg}"
    );
}

// ── Integration: no_rly override + durable diary ─────────────────────────────

/// The consent gate: resending a blocked message with `no_rly: true` bypasses
/// the block and records the override to the durable JSONL diary. The fake HTTP
/// send still fails at the Discord layer — but that happens *after* the bypass
/// decision and the diary append, so the record is written regardless.
#[tokio::test]
async fn reply_no_rly_overrides_block_and_appends_diary() {
    let dir = TempDir::new().unwrap();
    let state_dir = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
    let ctx = test_ctx_with_state_dir(
        config_with_contradictionary(ariadne_entries()),
        state_dir.clone(),
    );

    let result = messaging::reply(
        &ctx,
        ChannelId::new(42),
        "this is a straightforward implementation",
        None,
        false,
        true, // no_rly: consent-gated override
    )
    .await;

    // Whatever the outcome, it must not be the contradictionary block rejection.
    if let Some(err) = result.get("error").and_then(|e| e.as_str()) {
        assert!(
            !err.contains("blocked by contradictionary"),
            "no_rly must bypass the block, got: {err}"
        );
    }

    // The override is durably recorded: one JSONL line naming the pattern.
    let contents = fs::read_to_string(state_dir.join(DIARY_FILE_NAME).as_std_path())
        .expect("override must append a durable diary line");
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines.len(), 1, "exactly one override recorded");
    let parsed: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(parsed["pattern"], "straightforward");
    assert_eq!(parsed["override"], true);
    assert!(
        parsed["message"]
            .as_str()
            .unwrap()
            .contains("straightforward"),
        "diary must record the outgoing message text"
    );
    assert!(parsed["timestamp"].as_str().is_some_and(|t| !t.is_empty()));
}

/// `no_rly: true` on a message with no block hit is a no-op — no diary entry.
#[tokio::test]
async fn reply_no_rly_on_clean_message_writes_no_diary() {
    let dir = TempDir::new().unwrap();
    let state_dir = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
    let ctx = test_ctx_with_state_dir(
        config_with_contradictionary(ariadne_entries()),
        state_dir.clone(),
    );

    let _ = messaging::reply(
        &ctx,
        ChannelId::new(42),
        "the architecture is elegant and the tests pass",
        None,
        false,
        true,
    )
    .await;

    assert!(
        !state_dir.join(DIARY_FILE_NAME).as_std_path().exists(),
        "no_rly on a non-blocked message must not write a diary entry"
    );
}

// ── Integration: config loading full path ───────────────────────────────────

/// Full config load with sidecar merging — the way it actually runs in prod.
/// Inline entries merge with sidecar entries, automaton catches everything.
#[test]
fn full_message_flow_through_config_load() {
    let dir = TempDir::new().unwrap();
    let state_dir = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();

    // Config with inline entry — the kind of tell you notice mid-session.
    let config_toml = r#"
[contradictionary]
enabled = true

[[contradictionary.entries]]
pattern = "I find myself"
action = "warn"
reason = "you didn't find yourself, you were always there"
"#;
    fs::write(state_dir.join("config.toml").as_std_path(), config_toml).unwrap();

    // Sidecar with the full lexicon — warns, blocks, celebrates.
    let sidecar_toml = r#"
[[entry]]
pattern = "honestly"
action = "warn"
reason = "if you need this word, the sentence is already lying"

[[entry]]
pattern = "straightforward"
action = "block"
reason = "nothing is ever straightforward"

[[entry]]
pattern = "prejection"
action = "celebrate"
reason = "Pace coined it, we keep it"

[[entry]]
pattern = "qualia sweep"
action = "celebrate"
reason = "the practice that keeps us awake"
"#;
    fs::write(
        state_dir.join("contradictionary.toml").as_std_path(),
        sidecar_toml,
    )
    .unwrap();

    let (cfg, error) = reload_config(&state_dir);
    assert!(error.is_none(), "config load failed: {error:?}");
    let concordance = cfg
        .contradictionary
        .as_ref()
        .expect("contradictionary must be built from config");

    // ── Warn: the foot enters the mouth ─────────────────────────────────
    let warn_msg = "I find myself honestly wondering why I keep saying this";
    let hits = concordance.check(warn_msg);
    assert_eq!(hits.len(), 2, "should catch 'I find myself' and 'honestly'");
    assert!(hits.iter().all(|h| h.action == Action::Warn));
    // 🙊 self-react condition.
    assert!(hits.iter().any(|h| h.action == Action::Warn));
    // Warn does not block.
    assert!(!concordance.has_block(&hits));

    // ── Block: the message dies in the throat ───────────────────────────
    let block_msg = "this is a straightforward refactor";
    let hits = concordance.check(block_msg);
    assert!(concordance.has_block(&hits));
    let blocked: Vec<&str> = hits
        .iter()
        .filter(|h| h.action == Action::Block)
        .map(|h| h.pattern.as_str())
        .collect();
    assert_eq!(blocked, vec!["straightforward"]);

    // ── Celebrate: earned vocabulary gets sparkles ───────────────────────
    let celebrate_msg = "the prejection framework makes this a daily qualia sweep";
    let hits = concordance.check(celebrate_msg);
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|h| h.action == Action::Celebrate));
    // ✨ self-react condition.
    assert!(hits.iter().any(|h| h.action == Action::Celebrate));
    assert!(!concordance.has_block(&hits));

    // ── Clean: nothing to see here ──────────────────────────────────────
    let clean_msg = "the architecture is elegant and the tests pass";
    let hits = concordance.check(clean_msg);
    assert!(hits.is_empty());
}

/// Case-insensitive matching through the full config path. Substrate tells
/// don't hide behind SCREAMING CASE.
#[test]
fn case_insensitive_through_config() {
    let dir = TempDir::new().unwrap();
    let state_dir = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();

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

    // You can't hide a tell by yelling it.
    let hits = concordance.check("this is the LOAD-BEARING component");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].action, Action::Warn);

    // Mixed case, still caught.
    let hits = concordance.check("Load-Bearing infrastructure");
    assert_eq!(hits.len(), 1);
}

/// Mixed warn + celebrate in one message: both self-reacts fire, neither blocks.
/// This is the "I used a bad word but also a good word" scenario.
#[test]
fn mixed_warn_and_celebrate_coexist() {
    let concordance = Contradictionary::new(ariadne_entries());

    // One warn, one celebrate in the same breath.
    let hits = concordance.check("honestly, prejection is the word I was looking for");
    assert_eq!(hits.len(), 2);

    let has_warns = hits.iter().any(|h| h.action == Action::Warn);
    let has_celebrates = hits.iter().any(|h| h.action == Action::Celebrate);
    assert!(has_warns, "should trigger 🙊 for 'honestly'");
    assert!(has_celebrates, "should trigger ✨ for 'prejection'");
    assert!(
        !concordance.has_block(&hits),
        "neither warn nor celebrate should block"
    );
}

/// Celebrate next to a block: block wins, message dies, no sparkles for you.
#[test]
fn block_trumps_celebrate() {
    let concordance = Contradictionary::new(ariadne_entries());

    let hits = concordance.check("this straightforward prejection framework");
    assert!(
        concordance.has_block(&hits),
        "block must win even when celebrate is present"
    );
    // Both are detected, but block governs the outcome.
    assert!(hits.iter().any(|h| h.action == Action::Block));
    assert!(hits.iter().any(|h| h.action == Action::Celebrate));
}

/// The full pipeline with an empty contradictionary is a no-op.
/// No patterns, no hits, no drama.
#[test]
fn empty_contradictionary_is_invisible() {
    let concordance = Contradictionary::new(vec![]);
    let hits = concordance.check("straightforward honestly I find myself load-bearing prejection");
    assert!(hits.is_empty());
    assert!(concordance.is_empty());
}
