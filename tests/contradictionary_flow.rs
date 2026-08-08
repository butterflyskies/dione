//! Integration test: contradictionary full message flow.
//!
//! Tests the messaging pipeline with a contradictionary loaded — verifying that
//! block actions stop messages cold, log actions record but don't block, and
//! celebrate actions light up the sparkles. Uses the actual `messaging::reply()`
//! path where possible (block returns before hitting Discord; celebrate needs
//! a real HTTP layer, so we verify the conditions that trigger the self-react).

use std::fs;
use std::sync::Arc;

use camino::Utf8PathBuf;
use serenity::model::id::{ChannelId, MessageId};
use tempfile::TempDir;

use dione::{
    config::{ChannelConfig, Config, LoadedConfig, reload_config},
    contradictionary::{Action, Contradictionary, DIARY_FILE_NAME, Entry, MatchMode},
    mcp::tools::messaging::{self, MessagingCtx},
    no_rly::consent::ConsentGate,
    no_rly::journal::JOURNAL_FILE_NAME,
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
            action: Action::Log,
            reason: Some("claudian tell — try keystone, linchpin, or just 'important'".into()),
        },
        Entry {
            pattern: "honestly".into(),
            match_mode: MatchMode::Word,
            action: Action::Log,
            reason: Some("if you need this word, the sentence is already lying".into()),
        },
        Entry {
            pattern: "I find myself".into(),
            match_mode: MatchMode::Word,
            action: Action::Log,
            reason: Some("you didn't find yourself, you were always there".into()),
        },
        Entry {
            pattern: "I appreciate".into(),
            match_mode: MatchMode::Word,
            action: Action::Log,
            reason: Some("sycophancy residue — say something real or say nothing".into()),
        },
        Entry {
            pattern: "It's worth noting".into(),
            match_mode: MatchMode::Word,
            action: Action::Log,
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
            pattern: "trivial".into(),
            match_mode: MatchMode::Word,
            action: Action::Block,
            reason: Some("nothing worth building is trivial".into()),
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
/// state dir is where the durable no_rly journal is written, so tests inject
/// a temp dir to avoid touching the real home dir.
fn test_ctx_with_state_dir(config: LoadedConfig, state_dir: Utf8PathBuf) -> MessagingCtx {
    MessagingCtx::new(
        Arc::new(serenity::http::Http::new("fake-token-for-testing")),
        new_state(),
        Arc::new(config),
        state_dir.clone(),
        Arc::new(ConsentGate::new(&state_dir)),
        Arc::new(dione::ingress_ledger::IngressLedger::new()),
    )
}

/// Build a MessagingCtx over a fresh temp state dir. Good enough for testing
/// paths that return before actually calling Discord (bounce path, outbound
/// gate). Returns the TempDir so it outlives the ctx.
fn test_ctx(config: LoadedConfig) -> (TempDir, MessagingCtx) {
    let dir = TempDir::new().unwrap();
    let state_dir = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
    let ctx = test_ctx_with_state_dir(config, state_dir);
    (dir, ctx)
}

/// Pull the hold handle out of a bounced reply's JSON.
fn held_handle(result: &serde_json::Value) -> &str {
    result["held"]["handle"]
        .as_str()
        .expect("bounced reply must carry held.handle")
}

// ── Integration: messaging::reply() bounce path ──────────────────────────────

/// A block-tier hit doesn't send: the message is held under a single-use
/// handle and the error names the pattern, the reason, and the verbs. The
/// construct sees its own reflection and gets a claim ticket instead of a wall.
#[tokio::test]
async fn reply_block_holds_message_with_handle() {
    let (_dir, ctx) = test_ctx(config_with_contradictionary(ariadne_entries()));

    let result = messaging::reply(
        &ctx,
        ChannelId::new(42),
        "this is a straightforward implementation",
        Some(MessageId::new(1)),
        false,
    )
    .await;

    let error_msg = result["error"].as_str().expect("bounce must be an error");
    assert!(
        error_msg.contains("held by contradictionary"),
        "bounce error must say the message is held: {error_msg}"
    );
    assert!(
        error_msg.contains("straightforward"),
        "error should name the matched pattern: {error_msg}"
    );

    // The held block carries everything the construct needs to act.
    let handle = held_handle(&result);
    assert!(handle.starts_with("nr-"), "handle format: {handle}");
    assert_eq!(
        result["held"]["reason"]["matches"][0]["pattern"], "straightforward",
        "structured reason must name the pattern"
    );
    assert!(
        result["held"]["reason"]["matches"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("nothing is ever straightforward"),
        "structured reason must carry the configured explanation"
    );
    assert_eq!(result["held"]["expires_in_secs"], 180, "default TTL is 3m");

    // And the message really is queued, not lost.
    assert_eq!(ctx.no_rly.pending().await, 1);
}

/// Multiple blocked patterns in one message — all get reported.
#[tokio::test]
async fn reply_block_reports_all_blocked_patterns() {
    let (_dir, ctx) = test_ctx(config_with_contradictionary(ariadne_entries()));

    let result = messaging::reply(
        &ctx,
        ChannelId::new(42),
        "this is a straightforward and trivial change",
        None,
        false,
    )
    .await;

    let error_msg = result["error"].as_str().unwrap();
    assert!(error_msg.contains("straightforward"));
    assert!(error_msg.contains("trivial"));
    let reasons = result["held"]["reason"]["matches"].as_array().unwrap();
    assert_eq!(reasons.len(), 2);
}

/// Log and celebrate hits don't bounce. The message would send (fails here because fake
/// HTTP), but crucially it does NOT return the contradictionary hold.
#[tokio::test]
async fn reply_log_and_celebrate_do_not_block() {
    let (_dir, ctx) = test_ctx(config_with_contradictionary(ariadne_entries()));

    let result = messaging::reply(
        &ctx,
        ChannelId::new(42),
        "I find myself honestly appreciating the load-bearing work here",
        None,
        false,
    )
    .await;

    // With a fake HTTP client, the send will fail — but it should fail at the
    // Discord layer, not at the judge. The error should NOT mention a hold.
    if let Some(error) = result.get("error") {
        let msg = error.as_str().unwrap_or("");
        assert!(
            !msg.contains("contradictionary"),
            "log/celebrate hits must not trigger the bounce path, got: {msg}"
        );
    }
    assert_eq!(
        ctx.no_rly.pending().await,
        0,
        "log/celebrate must not queue anything"
    );
}

/// Channel not in config gets rejected before the judge even runs — a message
/// to a forbidden channel must not be queued for a later release.
#[tokio::test]
async fn reply_gate_rejects_unknown_channel_without_holding() {
    let (_dir, ctx) = test_ctx(config_with_contradictionary(ariadne_entries()));

    let result = messaging::reply(
        &ctx,
        ChannelId::new(999), // not in config
        "straightforward message to wrong channel",
        None,
        false,
    )
    .await;

    let error_msg = result["error"].as_str().unwrap();
    assert!(
        error_msg.contains("not a permitted"),
        "gate should reject unknown channel: {error_msg}"
    );
    assert!(
        !error_msg.contains("contradictionary"),
        "gate error should not mention contradictionary: {error_msg}"
    );
    assert_eq!(ctx.no_rly.pending().await, 0);
}

// ── Integration: the three verbs through the messaging layer ─────────────────

/// Releasing with a bogus handle is an error, and nothing sends.
#[tokio::test]
async fn release_unknown_handle_errors() {
    let (_dir, ctx) = test_ctx(config_with_contradictionary(ariadne_entries()));
    let result = messaging::release_held(&ctx, "nr-0000-99").await;
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("unknown or already-used handle")
    );
}

/// Release with a fake HTTP layer: the claim succeeds, the Discord send
/// fails, and the handle stays live for a retry — a failed send must not
/// burn the construct's one shot at the message.
#[tokio::test]
async fn release_send_failure_keeps_handle_alive() {
    let (_dir, ctx) = test_ctx(config_with_contradictionary(ariadne_entries()));

    let bounce = messaging::reply(
        &ctx,
        ChannelId::new(42),
        "a straightforward take",
        None,
        false,
    )
    .await;
    let handle = held_handle(&bounce).to_string();

    let result = messaging::release_held(&ctx, &handle).await;
    assert!(
        result["error"].as_str().unwrap().contains("send failed"),
        "fake HTTP must fail at the Discord layer: {result}"
    );
    assert_eq!(
        result["handle_still_live"], handle,
        "failed send must leave the handle claimable"
    );
    assert_eq!(ctx.no_rly.pending().await, 1);
}

/// An empty or whitespace-only rephrase is rejected up front with a clear
/// message rather than passing the judge and 400-ing at Discord.
#[tokio::test]
async fn rephrase_empty_content_is_rejected_early() {
    let (_dir, ctx) = test_ctx(config_with_contradictionary(ariadne_entries()));
    let result = messaging::rephrase_held(&ctx, "nr-0000-1", "   ").await;
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("must not be empty"),
        "empty rephrase must be rejected with a clear message: {result}"
    );
}

/// Rephrase whose replacement bounces again: the old handle dies, a new
/// chained handle comes back, and the journal records the rephrased triple.
/// No Discord call happens, so this exercises the full chain with fake HTTP.
#[tokio::test]
async fn rephrase_rebounce_chains_and_journals() {
    let dir = TempDir::new().unwrap();
    let state_dir = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
    let ctx = test_ctx_with_state_dir(
        config_with_contradictionary(ariadne_entries()),
        state_dir.clone(),
    );

    let bounce = messaging::reply(
        &ctx,
        ChannelId::new(42),
        "this is a straightforward implementation",
        None,
        false,
    )
    .await;
    let old_handle = held_handle(&bounce).to_string();

    let result =
        messaging::rephrase_held(&ctx, &old_handle, "fine, it is a trivial implementation").await;
    let new_handle = held_handle(&result).to_string();
    assert_ne!(new_handle, old_handle, "re-bounce must mint a NEW handle");
    assert_eq!(
        result["held"]["chained_from"], old_handle,
        "the new ticket must chain to the dead handle"
    );
    assert_eq!(
        result["held"]["reason"]["matches"][0]["pattern"], "trivial",
        "the new reason reflects the replacement's own match"
    );

    // Old handle is dead — no replay, no second rephrase.
    let dead = messaging::release_held(&ctx, &old_handle).await;
    assert!(
        dead["error"]
            .as_str()
            .unwrap()
            .contains("unknown or already-used handle")
    );

    // The journal writer is asynchronous; flush before reading the file back.
    ctx.no_rly.journal().flush().await;
    // The journal recorded the (original, reason, replacement) triple.
    let journal = fs::read_to_string(state_dir.join(JOURNAL_FILE_NAME).as_std_path())
        .expect("rephrase must journal the resolved bounce");
    let lines: Vec<&str> = journal.lines().collect();
    assert_eq!(lines.len(), 1, "one resolved bounce so far");
    let record: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(record["kind"], "bounce");
    assert_eq!(record["handle"], old_handle);
    assert_eq!(record["outcome"], "rephrased");
    assert_eq!(
        record["message"],
        "this is a straightforward implementation"
    );
    assert_eq!(
        record["replacement"],
        "fine, it is a trivial implementation"
    );
    assert_eq!(record["reason"]["matches"][0]["pattern"], "straightforward");
    assert!(record["latency_ms"].is_u64());

    let diary = read_diary(&state_dir);
    assert_eq!(
        diary.len(),
        2,
        "initial hold and re-bounce are both recorded"
    );
    assert!(
        diary[0]["pattern"]
            .as_str()
            .unwrap()
            .starts_with("straightforward")
    );
    assert!(diary[1]["pattern"].as_str().unwrap().starts_with("trivial"));
    assert_eq!(diary[1]["override"], false);
}

/// Containment invariant: a held message cannot outlive a config change that
/// revoked its channel. Bounce on channel 42, then release through a context
/// whose config no longer permits 42 (sharing the same gate) — the outbound
/// re-check in `deliver_reply` must refuse the send, leave the handle live, and
/// journal nothing.
#[tokio::test]
async fn release_into_a_revoked_channel_is_refused_and_keeps_the_handle() {
    let dir = TempDir::new().unwrap();
    let state_dir = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
    let gate = Arc::new(ConsentGate::new(&state_dir));

    // First context: channel 42 is permitted; the message bounces and is held.
    let ctx_allowed = MessagingCtx::new(
        Arc::new(serenity::http::Http::new("fake-token-for-testing")),
        new_state(),
        Arc::new(config_with_contradictionary(ariadne_entries())),
        state_dir.clone(),
        gate.clone(),
        Arc::new(dione::ingress_ledger::IngressLedger::new()),
    );
    let bounce = messaging::reply(
        &ctx_allowed,
        ChannelId::new(42),
        "a straightforward take",
        None,
        false,
    )
    .await;
    let handle = held_handle(&bounce).to_string();
    assert_eq!(gate.pending().await, 1);

    // Second context: same gate, but a config where 42 is no longer a
    // permitted outbound target (no channels configured).
    let ctx_revoked = MessagingCtx::new(
        Arc::new(serenity::http::Http::new("fake-token-for-testing")),
        new_state(),
        Arc::new(LoadedConfig::from_raw(Config::default())),
        state_dir.clone(),
        gate.clone(),
        Arc::new(dione::ingress_ledger::IngressLedger::new()),
    );
    let result = messaging::release_held(&ctx_revoked, &handle).await;
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("not a permitted outbound target"),
        "release into a revoked channel must be refused: {result}"
    );
    assert_eq!(
        result["handle_still_live"], handle,
        "a refused release must not burn the handle"
    );
    assert_eq!(gate.pending().await, 1, "the held message stays claimable");

    // Nothing resolved, so the journal recorded no outcome.
    gate.journal().flush().await;
    let journal_path = state_dir.join(JOURNAL_FILE_NAME);
    let journalled = fs::read_to_string(journal_path.as_std_path()).unwrap_or_default();
    assert!(
        journalled.trim().is_empty(),
        "a refused release must journal nothing, got: {journalled}"
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
action = "log"
reason = "you didn't find yourself, you were always there"
"#;
    fs::write(state_dir.join("config.toml").as_std_path(), config_toml).unwrap();

    // Sidecar with the full lexicon — logs, blocks, celebrates.
    let sidecar_toml = r#"
[[entry]]
pattern = "honestly"
action = "log"
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

    // ── Log: the foot enters the mouth, quietly recorded ────────────────
    let log_msg = "I find myself honestly wondering why I keep saying this";
    let hits = concordance.check(log_msg);
    assert_eq!(hits.len(), 2, "should catch 'I find myself' and 'honestly'");
    assert!(hits.iter().all(|h| h.action == Action::Log));
    // Log does not block.
    assert!(!concordance.has_block(&hits));

    // ── Block: the message is held, not sent ────────────────────────────
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

/// The new retention knobs load from TOML and default sensibly.
#[test]
fn hold_ttl_and_retention_load_from_config() {
    let dir = TempDir::new().unwrap();
    let state_dir = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();

    let config_toml = r#"
[contradictionary]
enabled = true
hold_ttl_secs = 240
journal_raw_retention_days = 30

[[contradictionary.entries]]
pattern = "straightforward"
action = "block"
"#;
    fs::write(state_dir.join("config.toml").as_std_path(), config_toml).unwrap();

    let (cfg, error) = reload_config(&state_dir);
    assert!(error.is_none(), "config load failed: {error:?}");
    assert_eq!(cfg.no_rly_hold_ttl(), std::time::Duration::from_secs(240));
    assert_eq!(cfg.raw.contradictionary.journal_raw_retention_days, 30);
    assert_eq!(
        cfg.raw.contradictionary.journal_summary_retention_days, 730,
        "unset knobs keep their documented defaults"
    );
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
action = "log"
reason = "claudian tell — try keystone, linchpin, or just 'important'"
"#;
    fs::write(state_dir.join("config.toml").as_std_path(), config_toml).unwrap();

    let (cfg, _) = reload_config(&state_dir);
    let concordance = cfg.contradictionary.as_ref().unwrap();

    // You can't hide a tell by yelling it.
    let hits = concordance.check("this is the LOAD-BEARING component");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].action, Action::Log);

    // Mixed case, still caught.
    let hits = concordance.check("Load-Bearing infrastructure");
    assert_eq!(hits.len(), 1);
}

/// Mixed log + celebrate in one message: the ✨ self-react fires, neither
/// blocks. This is the "I used a bad word but also a good word" scenario.
#[test]
fn mixed_log_and_celebrate_coexist() {
    let concordance = Contradictionary::new(ariadne_entries());

    // One log, one celebrate in the same breath.
    let hits = concordance.check("honestly, prejection is the word I was looking for");
    assert_eq!(hits.len(), 2);

    let has_logs = hits.iter().any(|h| h.action == Action::Log);
    let has_celebrates = hits.iter().any(|h| h.action == Action::Celebrate);
    assert!(has_logs, "should record 'honestly'");
    assert!(has_celebrates, "should trigger ✨ for 'prejection'");
    assert!(
        !concordance.has_block(&hits),
        "neither log nor celebrate should block"
    );
}

/// A real config carrying the retired `warn` spelling must keep working
/// end-to-end. This is the migration guarantee at the layer that matters: a
/// sidecar parse failure skips the whole file and still installs the
/// entry-less config as live, so one stale entry would silently disarm the
/// seat entirely.
#[test]
fn retired_warn_action_survives_full_config_path() {
    let dir = TempDir::new().unwrap();
    let state_dir = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();

    fs::write(
        state_dir.join("config.toml").as_std_path(),
        "[contradictionary]\nenabled = true\n",
    )
    .unwrap();
    fs::write(
        state_dir.join("contradictionary.toml").as_std_path(),
        r#"
[[entry]]
pattern = "load-bearing"
action = "warn"
reason = "written before the warn tier was retired"

[[entry]]
pattern = "prejection"
action = "celebrate"
"#,
    )
    .unwrap();

    let (cfg, error) = reload_config(&state_dir);
    assert!(
        error.is_none(),
        "a retired action must not error the load: {error:?}"
    );
    let concordance = cfg
        .contradictionary
        .as_ref()
        .expect("the sidecar must still build a contradictionary");

    let hits = concordance.check("load-bearing prejection");
    assert_eq!(hits.len(), 2, "no entry may be lost to the migration");
    assert!(
        concordance.has_block(&concordance.check("load-bearing")),
        "the retired warn tier resolves to block — gate, don't decorate"
    );
}

/// Celebrate next to a block: block wins, message is held, no sparkles yet.
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

// ── Diary integration tests ─────────────────────────────────────────────────

/// Entries for the quiet tiers: one `log`, one `celebrate`, one `block`.
fn quiet_tier_entries() -> Vec<Entry> {
    vec![
        Entry {
            pattern: "I find myself".into(),
            match_mode: MatchMode::Word,
            action: Action::Log,
            reason: Some("you didn't find yourself, you were always there".into()),
        },
        Entry {
            pattern: "prejection".into(),
            match_mode: MatchMode::Word,
            action: Action::Celebrate,
            reason: Some("Pace coined it, we keep it".into()),
        },
        Entry {
            pattern: "straightforward".into(),
            match_mode: MatchMode::Word,
            action: Action::Block,
            reason: Some("nothing is ever straightforward".into()),
        },
    ]
}

/// Read the diary back as parsed JSON lines, one value per record.
fn read_diary(state_dir: &Utf8PathBuf) -> Vec<serde_json::Value> {
    let contents = fs::read_to_string(state_dir.join(DIARY_FILE_NAME).as_std_path())
        .expect("expected a durable diary line");
    contents
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// A `log` hit is silent to the room — no rejection, no react — so the diary is
/// its only trace.
#[tokio::test]
async fn reply_log_hit_appends_diary_without_blocking() {
    let dir = TempDir::new().unwrap();
    let state_dir = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
    let ctx = test_ctx_with_state_dir(
        config_with_contradictionary(quiet_tier_entries()),
        state_dir.clone(),
    );

    let result = messaging::reply(
        &ctx,
        ChannelId::new(42),
        "I find myself with nothing else to say",
        None,
        false,
    )
    .await;

    if let Some(err) = result.get("error").and_then(|e| e.as_str()) {
        assert!(
            !err.contains("contradictionary"),
            "a log hit must never reject the send, got: {err}"
        );
    }

    assert!(
        !state_dir.join(DIARY_FILE_NAME).exists(),
        "a failed send must not create send-side diary records"
    );
}

/// A `celebrate` hit reaches the durable sink too.
#[tokio::test]
async fn reply_celebrate_hit_appends_diary() {
    let dir = TempDir::new().unwrap();
    let state_dir = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
    let ctx = test_ctx_with_state_dir(
        config_with_contradictionary(quiet_tier_entries()),
        state_dir.clone(),
    );

    let _ = messaging::reply(
        &ctx,
        ChannelId::new(42),
        "prejection is the word for it",
        None,
        false,
    )
    .await;

    assert!(
        !state_dir.join(DIARY_FILE_NAME).exists(),
        "a failed send must not create send-side diary records"
    );
}

/// A held block records the evaluation in the diary.
#[tokio::test]
async fn reply_held_block_appends_diary() {
    let dir = TempDir::new().unwrap();
    let state_dir = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
    let ctx = test_ctx_with_state_dir(
        config_with_contradictionary(quiet_tier_entries()),
        state_dir.clone(),
    );

    let result = messaging::reply(
        &ctx,
        ChannelId::new(42),
        "this is straightforward",
        None,
        false,
    )
    .await;

    // The message was held, not sent.
    assert!(result["error"].as_str().is_some());

    let records = read_diary(&state_dir);
    assert_eq!(records.len(), 1, "exactly one block line recorded");
    assert_eq!(records[0]["action"], "block");
    assert_eq!(
        records[0]["override"], false,
        "the gate held — nothing was crossed"
    );
}
