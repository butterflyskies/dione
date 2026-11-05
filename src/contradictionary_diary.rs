//! Application diary policy and retained contradictionary records.

use crate::util::truncate_chars;
use chrono::Utc;
use dione_contradictionary::{Action, AutoRewrite, Hit};
use dione_messages::timestamp::Timestamp;
use serde::Serialize;
use std::path::Path;

/// Decide what a block action should do for outbound `content` (already
/// scanned into `hits`), given the caller's `no_rly` override flag.
///
/// - Nothing to record → [`BlockOutcome::Clear`]: send normally, write
///   nothing.
/// - No block hit, but `log`/`celebrate` hits →
///   [`BlockOutcome::Recorded`]: send normally and append the returned
///   [`DiaryRecord`]s. These tiers never reject and never react here; the
///   diary is where they land.
/// - Block hit, `no_rly == false` → [`BlockOutcome::Rejected`]: an error
///   that names the matched pattern(s) inline, so the construct knows what
///   to override, plus the [`DiaryRecord`] for the hold itself.
/// - Block hit, `no_rly == true` → [`BlockOutcome::Overridden`]: a
///   consent-gated bypass. Send the message and append the returned
///   [`DiaryRecord`]s to the durable diary once the send commits.
///
/// Every block-tier evaluation produces a record, held or crossed — the two
/// differ only in [`DiaryRecord::overridden`]. Without both halves the diary
/// is one-sided by construction and the gate working is invisible.
///
/// `no_rly` only gates the block tier: it never changes what the `log` and
/// `celebrate` tiers record.
pub(crate) fn evaluate_block(hits: &[Hit], content: &str, no_rly: bool) -> BlockOutcome {
    // An `auto` hit that reaches evaluation unrewritten gates like `block`.
    let blocked: Vec<&str> = hits
        .iter()
        .filter(|h| h.action.gates())
        .map(|h| h.pattern.as_str())
        .collect();
    if blocked.is_empty() {
        let quiet = quiet_tier_records(hits, content);
        return if quiet.is_empty() {
            BlockOutcome::Clear
        } else {
            BlockOutcome::Recorded(quiet)
        };
    }
    let pattern = blocked.join(", ");
    if no_rly {
        let quiet = quiet_tier_records(hits, content);
        let mut records = Vec::with_capacity(1 + quiet.len());
        records.push(DiaryRecord::override_now(&pattern, content));
        records.extend(quiet);
        BlockOutcome::Overridden(records)
    } else {
        // The quiet tiers describe text that reached the room. This text
        // does not, so only the hold is recorded — anything else would put
        // unsent words in the `log`/`celebrate` corpora and double-count
        // them against the rewrite that follows.
        BlockOutcome::Rejected {
            error: format!(
                "\u{26a0}\u{fe0f} blocked by contradictionary: {pattern} \
                 — resend with no_rly: true to override"
            ),
            records: vec![DiaryRecord::held_now(&pattern, content)],
        }
    }
}

/// One [`DiaryRecord`] per non-block tier present in `hits` — `log` first,
/// then `celebrate` — each naming that tier's comma-joined patterns, the
/// same way the block tier joins its own.
///
/// These are the tiers that reach a construct only through the diary:
/// `celebrate` also self-reacts ✨ at send time, `log` is silent to the
/// room by design. Neither is a consent-gated override, so both carry
/// `override: false`.
fn quiet_tier_records(hits: &[Hit], content: &str) -> Vec<DiaryRecord> {
    let joined = |action: Action| -> Option<String> {
        let patterns: Vec<&str> = hits
            .iter()
            .filter(|h| h.action == action)
            .map(|h| h.pattern.as_str())
            .collect();
        (!patterns.is_empty()).then(|| patterns.join(", "))
    };
    let mut records = Vec::new();
    if let Some(pattern) = joined(Action::Log) {
        records.push(DiaryRecord::log_now(&pattern, content));
    }
    if let Some(pattern) = joined(Action::Celebrate) {
        records.push(DiaryRecord::celebrate_now(&pattern, content));
    }
    records
}

/// Maximum number of characters of outgoing text retained in a diary record.
/// Longer messages are truncated (with an ellipsis) to bound line size.
const DIARY_MAX_MESSAGE_LEN: usize = 2000;

/// Name of the durable diary file, created under the channel state directory
/// (`~/.claude/channels/dione/contradictionary.jsonl`).
pub const DIARY_FILE_NAME: &str = "contradictionary.jsonl";

/// The decision reached when evaluating a potential block.
/// The application records held and overridden blocks separately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockOutcome {
    /// Nothing the diary cares about matched — send normally, write nothing.
    Clear,
    /// A block matched and was not overridden — the gate held. Reject the send
    /// with `error`, and append `records` regardless: a hold is an evaluation
    /// and every evaluation is recorded.
    Rejected {
        /// The in-band error returned to the caller, naming the matched
        /// pattern(s) inline. Unchanged by the diary write.
        error: String,
        /// The hold itself, as one `action: "block"`, `override: false` record.
        /// Never empty.
        records: Vec<DiaryRecord>,
    },
    /// A block matched but the caller passed `no_rly: true` — send anyway and
    /// append these records to the durable diary. The first is the override
    /// itself; any others are `log`/`celebrate` hits on the same text.
    Overridden(Vec<DiaryRecord>),
    /// No block matched, but the `log` and/or `celebrate` tiers did — send
    /// normally and append these records. Never empty.
    Recorded(Vec<DiaryRecord>),
}

/// One durable diary entry, serialized as a single JSON line (JSONL).
///
/// The diary persists every action tier that has something to remember — block
/// evaluations both held and crossed, `log` hits, `celebrate` hits, and `auto`
/// rewrites — to disk so the history survives process restarts and context
/// clears, unlike `tracing`/stderr, which the harness captures but does not
/// persist.
///
/// Each line self-identifies via [`DiaryRecord::action`], so a single sink stays
/// partitionable: `jq 'select(.action == "celebrate")' contradictionary.jsonl`.
/// Within the block tier, [`DiaryRecord::overridden`] splits holds from
/// crossings, which is what makes a compliance rate readable off the file
/// itself: `jq 'select(.action == "block" and .override == false)'`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiaryRecord {
    /// When the entry was recorded. Serializes as an RFC 3339 string.
    pub timestamp: Timestamp,
    /// The contradictionary pattern(s) that matched, comma-joined.
    pub pattern: String,
    /// The outgoing message text (truncated to the diary message-length limit).
    pub message: String,
    /// Which action tier produced this line. Serializes lowercase (`"block"`,
    /// `"log"`, `"celebrate"`, `"auto"`).
    pub action: Action,
    /// True only for `no_rly` overrides of a block action — the one tier that
    /// required consent. Meaningful only when `action` is `block`: false there
    /// means the gate held. Always false on the other tiers, which have no
    /// consent gate to cross. Serialized as `override` for readability of the
    /// on-disk log.
    #[serde(rename = "override")]
    pub overridden: bool,
    /// The text actually sent, present only on `auto` records, where it
    /// differs from [`DiaryRecord::message`] (the text as written).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sent: Option<String>,
    /// On `auto` records only: the match as written in the message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched: Option<String>,
    /// On `auto` records only: what the match was rewritten to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacement: Option<String>,
}

impl DiaryRecord {
    /// Build an override record stamped at the current time. `pattern` is the
    /// matched block pattern(s); `message` is the outgoing text (truncated).
    pub fn override_now(pattern: &str, message: &str) -> Self {
        Self::now(pattern, message, Action::Block, true)
    }

    /// Build a held-block record stamped at the current time: the gate fired
    /// and the construct complied rather than overriding.
    pub fn held_now(pattern: &str, message: &str) -> Self {
        Self::now(pattern, message, Action::Block, false)
    }

    /// Build a `log`-tier record stamped at the current time. The `log` tier
    /// sends and records without reacting — the diary is its only trace.
    pub fn log_now(pattern: &str, message: &str) -> Self {
        Self::now(pattern, message, Action::Log, false)
    }

    /// Build a `celebrate`-tier record stamped at the current time. The ✨
    /// self-react is ephemeral; this is the durable half.
    pub fn celebrate_now(pattern: &str, message: &str) -> Self {
        Self::now(pattern, message, Action::Celebrate, false)
    }

    /// Build an `auto`-tier record stamped at the current time: one rewrite
    /// that went out. `original` is the text as written, `sent` the text as
    /// sent (both truncated).
    pub fn auto_now(rewrite: &AutoRewrite, original: &str, sent: &str) -> Self {
        Self {
            sent: Some(truncate_chars(sent, DIARY_MAX_MESSAGE_LEN)),
            matched: Some(rewrite.matched.clone()),
            replacement: Some(rewrite.replacement.clone()),
            ..Self::now(&rewrite.pattern, original, Action::Auto, false)
        }
    }

    fn now(pattern: &str, message: &str, action: Action, overridden: bool) -> Self {
        Self {
            timestamp: Utc::now().fixed_offset().into(),
            pattern: pattern.to_string(),
            message: truncate_chars(message, DIARY_MAX_MESSAGE_LEN),
            action,
            overridden,
            sent: None,
            matched: None,
            replacement: None,
        }
    }
}

/// Append a single [`DiaryRecord`] as one JSONL line to
/// `<dir>/contradictionary.jsonl`.
///
/// `dir` is the channel state directory (e.g. `~/.claude/channels/dione`). The
/// file and any missing parent directories are created on demand. This is the
/// durable sink for the diary — a real append-to-file write, not a `tracing`
/// event — so records survive process restarts.
///
/// One sink for every tier: block evaluations, `log` hits, and `celebrate` hits
/// all land here, each line tagged with its [`DiaryRecord::action`].
pub fn append_diary_record(dir: &Path, record: &DiaryRecord) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    let path = dir.join(DIARY_FILE_NAME);
    let mut line = serde_json::to_string(record).map_err(std::io::Error::other)?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(line.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dione_contradictionary::{Contradictionary, Entry, MatchMode};

    fn test_entries() -> Vec<Entry> {
        vec![
            // Non-blocking tells. These were `warn` before that tier was
            // retired; `log` is now the only send-and-record action, so it
            // carries the "caught but not gated" case these tests rely on.
            Entry {
                pattern: "load-bearing".into(),
                action: Action::Log,
                match_mode: MatchMode::Word,
                replace: None,
                reason: Some("claudian tell — try keystone, linchpin, or just 'important'".into()),
            },
            Entry {
                pattern: "honestly".into(),
                action: Action::Log,
                match_mode: MatchMode::Word,
                replace: None,
                reason: Some("if you need this word, the sentence is already lying".into()),
            },
            Entry {
                pattern: "I find myself".into(),
                action: Action::Log,
                match_mode: MatchMode::Word,
                replace: None,
                reason: Some("you didn't find yourself, you were always there".into()),
            },
            Entry {
                pattern: "confidential".into(),
                action: Action::Block,
                match_mode: MatchMode::Word,
                replace: None,
                reason: None,
            },
            Entry {
                pattern: "prejection".into(),
                action: Action::Celebrate,
                match_mode: MatchMode::Word,
                replace: None,
                reason: Some("Pace coined it, we keep it".into()),
            },
        ]
    }

    fn auto_entry(pattern: &str, replace: Option<&str>, match_mode: MatchMode) -> Entry {
        Entry {
            pattern: pattern.into(),
            action: Action::Auto,
            match_mode,
            reason: None,
            replace: replace.map(Into::into),
        }
    }

    /// An `auto` hit evaluated without having been rewritten (e.g. released
    /// verbatim) gates like `block`.
    #[test]
    fn unrewritten_auto_hit_evaluates_as_block() {
        let c = Contradictionary::new(vec![auto_entry("utilize", Some("use"), MatchMode::Word)]);
        let content = "we utilize it";
        let hits = c.check(content);
        assert!(c.has_block(&hits));
        assert!(matches!(
            evaluate_block(&hits, content, false),
            BlockOutcome::Rejected { .. }
        ));
    }

    #[test]
    fn block_without_no_rly_rejects_and_names_pattern() {
        let c = Contradictionary::new(test_entries());
        let content = "this is confidential information";
        let hits = c.check(content);
        match evaluate_block(&hits, content, false) {
            BlockOutcome::Rejected { error, .. } => {
                assert!(
                    error.contains("confidential"),
                    "block error must name the matched pattern inline: {error}"
                );
            }
            other => panic!("expected Rejected without no_rly, got {other:?}"),
        }
    }

    #[test]
    fn held_block_records_the_evaluation() {
        let c = Contradictionary::new(test_entries());
        let content = "this is confidential information";
        let hits = c.check(content);
        let (error, records) = match evaluate_block(&hits, content, false) {
            BlockOutcome::Rejected { error, records } => (error, records),
            other => panic!("expected Rejected without no_rly, got {other:?}"),
        };

        assert_eq!(
            error,
            "\u{26a0}\u{fe0f} blocked by contradictionary: confidential \
             \u{2014} resend with no_rly: true to override",
            "the error string is the caller-facing contract and is unchanged"
        );

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].action, Action::Block);
        assert_eq!(records[0].pattern, "confidential");
        assert_eq!(records[0].message, content);
        assert!(!records[0].overridden);
    }

    #[test]
    fn held_block_does_not_record_the_unsent_quiet_tiers() {
        let c = Contradictionary::new(test_entries());
        let content = "this confidential note: I find myself admiring prejection";
        let hits = c.check(content);
        assert!(hits.iter().any(|h| h.action == Action::Log));
        assert!(hits.iter().any(|h| h.action == Action::Celebrate));

        match evaluate_block(&hits, content, false) {
            BlockOutcome::Rejected { records, .. } => {
                let actions: Vec<Action> = records.iter().map(|r| r.action).collect();
                assert_eq!(actions, vec![Action::Block]);
            }
            other => panic!("expected Rejected without no_rly, got {other:?}"),
        }
    }

    #[test]
    fn held_and_crossed_blocks_are_one_filter_apart() {
        let dir = tempfile::TempDir::new().unwrap();
        append_diary_record(
            dir.path(),
            &DiaryRecord::held_now("confidential", "held one"),
        )
        .unwrap();
        append_diary_record(
            dir.path(),
            &DiaryRecord::override_now("confidential", "crossed one"),
        )
        .unwrap();

        let contents = std::fs::read_to_string(dir.path().join(DIARY_FILE_NAME)).unwrap();
        let lines: Vec<serde_json::Value> = contents
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);

        let evaluated: Vec<&serde_json::Value> =
            lines.iter().filter(|v| v["action"] == "block").collect();
        assert_eq!(evaluated.len(), 2);

        let held: Vec<&serde_json::Value> = evaluated
            .iter()
            .copied()
            .filter(|v| v["override"] == false)
            .collect();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0]["message"], "held one");

        let crossed: Vec<&serde_json::Value> = evaluated
            .iter()
            .copied()
            .filter(|v| v["override"] == true)
            .collect();
        assert_eq!(crossed.len(), 1);
        assert_eq!(crossed[0]["message"], "crossed one");
    }

    #[test]
    fn block_with_no_rly_overrides_and_appends_jsonl() {
        let c = Contradictionary::new(test_entries());
        let content = "this is confidential information";
        let hits = c.check(content);
        let records = match evaluate_block(&hits, content, true) {
            BlockOutcome::Overridden(records) => records,
            other => panic!("expected Overridden with no_rly, got {other:?}"),
        };
        assert_eq!(records.len(), 1);
        let record = records.into_iter().next().unwrap();
        assert_eq!(record.pattern, "confidential");
        assert!(record.overridden);

        let dir = tempfile::TempDir::new().unwrap();
        append_diary_record(dir.path(), &record).unwrap();

        let contents = std::fs::read_to_string(dir.path().join(DIARY_FILE_NAME)).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 1);

        let parsed: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["pattern"], "confidential");
        assert_eq!(parsed["override"], true);
        assert_eq!(parsed["message"], content);
        assert!(parsed["timestamp"].as_str().is_some_and(|t| !t.is_empty()));
    }

    #[test]
    fn append_diary_record_is_append_only() {
        let dir = tempfile::TempDir::new().unwrap();
        append_diary_record(
            dir.path(),
            &DiaryRecord::override_now("confidential", "first"),
        )
        .unwrap();
        append_diary_record(dir.path(), &DiaryRecord::override_now("secret", "second")).unwrap();
        let contents = std::fs::read_to_string(dir.path().join(DIARY_FILE_NAME)).unwrap();
        assert_eq!(contents.lines().count(), 2);
    }

    #[test]
    fn celebrate_appends_to_the_same_jsonl_sink() {
        let dir = tempfile::TempDir::new().unwrap();
        let record = DiaryRecord::celebrate_now("aww hell", "aww hell, that worked");
        append_diary_record(dir.path(), &record).unwrap();

        let contents = std::fs::read_to_string(dir.path().join(DIARY_FILE_NAME)).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(contents.lines().next().unwrap()).unwrap();
        assert_eq!(parsed["action"], "celebrate");
        assert_eq!(parsed["override"], false);
    }

    #[test]
    fn override_records_carry_the_block_action() {
        let record = DiaryRecord::override_now("confidential", "leaked");
        let parsed: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap();
        assert_eq!(parsed["action"], "block");
        assert_eq!(parsed["override"], true);
    }

    #[test]
    fn log_appends_to_the_same_jsonl_sink() {
        let dir = tempfile::TempDir::new().unwrap();
        let record = DiaryRecord::log_now("I find myself", "I find myself agreeing");
        append_diary_record(dir.path(), &record).unwrap();

        let contents = std::fs::read_to_string(dir.path().join(DIARY_FILE_NAME)).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(contents.lines().next().unwrap()).unwrap();
        assert_eq!(parsed["action"], "log");
        assert_eq!(parsed["override"], false);
    }

    #[test]
    fn auto_record_carries_original_and_sent_message() {
        let dir = tempfile::TempDir::new().unwrap();
        append_diary_record(
            dir.path(),
            &DiaryRecord::held_now("confidential", "held one"),
        )
        .unwrap();
        append_diary_record(
            dir.path(),
            &DiaryRecord::auto_now(
                &AutoRewrite {
                    pattern: "utilize".into(),
                    matched: "Utilize".into(),
                    replacement: "Use".into(),
                },
                "Utilize it",
                "Use it",
            ),
        )
        .unwrap();

        let contents = std::fs::read_to_string(dir.path().join(DIARY_FILE_NAME)).unwrap();
        let lines: Vec<serde_json::Value> = contents
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        // jq 'select(.action=="auto")'
        let auto: Vec<&serde_json::Value> =
            lines.iter().filter(|v| v["action"] == "auto").collect();
        assert_eq!(auto.len(), 1);
        assert_eq!(auto[0]["pattern"], "utilize");
        assert_eq!(auto[0]["matched"], "Utilize");
        assert_eq!(auto[0]["replacement"], "Use");
        assert_eq!(auto[0]["message"], "Utilize it");
        assert_eq!(auto[0]["sent"], "Use it");
        assert_eq!(auto[0]["override"], false);
        // Other tiers' lines keep their shape.
        for field in ["sent", "matched", "replacement"] {
            assert!(lines[0].get(field).is_none(), "{field}");
        }
    }

    #[test]
    fn corpus_holds_every_tier_and_stays_partitionable() {
        let dir = tempfile::TempDir::new().unwrap();
        append_diary_record(
            dir.path(),
            &DiaryRecord::override_now("confidential", "blocked one"),
        )
        .unwrap();
        append_diary_record(
            dir.path(),
            &DiaryRecord::log_now("I find myself", "logged one"),
        )
        .unwrap();
        append_diary_record(
            dir.path(),
            &DiaryRecord::celebrate_now("shevirah", "celebrated one"),
        )
        .unwrap();

        let contents = std::fs::read_to_string(dir.path().join(DIARY_FILE_NAME)).unwrap();
        let actions: Vec<String> = contents
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .map(|v| v["action"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(actions, vec!["block", "log", "celebrate"]);
    }

    #[test]
    fn log_only_message_still_reaches_the_diary() {
        let c = Contradictionary::new(test_entries());
        let content = "I find myself with nothing else to flag";
        let hits = c.check(content);
        assert!(!c.has_block(&hits));
        match evaluate_block(&hits, content, false) {
            BlockOutcome::Recorded(records) => {
                assert_eq!(records.len(), 1);
                assert_eq!(records[0].action, Action::Log);
                assert_eq!(records[0].pattern, "I find myself");
                assert!(!records[0].overridden);
            }
            other => panic!("expected Recorded for a log-only message, got {other:?}"),
        }
    }

    #[test]
    fn celebrate_only_message_still_reaches_the_diary() {
        let c = Contradictionary::new(test_entries());
        let content = "prejection is the word for it";
        let hits = c.check(content);
        match evaluate_block(&hits, content, false) {
            BlockOutcome::Recorded(records) => {
                assert_eq!(records.len(), 1);
                assert_eq!(records[0].action, Action::Celebrate);
                assert!(!records[0].overridden);
            }
            other => panic!("expected Recorded for a celebrate-only message, got {other:?}"),
        }
    }

    #[test]
    fn an_overridden_block_does_not_swallow_the_other_tiers() {
        let c = Contradictionary::new(test_entries());
        let content = "this confidential note: I find myself admiring prejection";
        let hits = c.check(content);
        match evaluate_block(&hits, content, true) {
            BlockOutcome::Overridden(records) => {
                let actions: Vec<Action> = records.iter().map(|r| r.action).collect();
                assert_eq!(actions, vec![Action::Block, Action::Log, Action::Celebrate]);
                assert_eq!(records.iter().filter(|r| r.overridden).count(), 1);
            }
            other => panic!("expected Overridden with no_rly, got {other:?}"),
        }
    }

    #[test]
    fn no_rly_on_clean_message_is_clear_no_diary() {
        let c = Contradictionary::new(test_entries());
        let content = "the keystone component is well designed";
        let hits = c.check(content);
        assert_eq!(evaluate_block(&hits, content, true), BlockOutcome::Clear);
    }

    #[test]
    fn no_rly_does_not_affect_log_celebrate() {
        let c = Contradictionary::new(test_entries());
        let content = "honestly, I find myself admiring prejection";
        let hits = c.check(content);
        assert!(!c.has_block(&hits));

        let actions = |no_rly: bool| match evaluate_block(&hits, content, no_rly) {
            BlockOutcome::Recorded(records) => {
                records.into_iter().map(|r| r.action).collect::<Vec<_>>()
            }
            other => panic!("expected Recorded for log/celebrate hits, got {other:?}"),
        };
        assert_eq!(actions(true), vec![Action::Log, Action::Celebrate]);
        assert_eq!(actions(false), vec![Action::Log, Action::Celebrate]);
    }
}
