use std::path::Path;

use aho_corasick::AhoCorasick;
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::timestamp::Timestamp;
use crate::util::truncate_chars;

/// Action to take when a pattern matches outbound text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Send the message, self-react 🙊, log to ops.
    Warn,
    /// Block the message — return an error to the construct.
    Block,
    /// Send the message, log the hit silently.
    Log,
    /// Send the message, self-react ✨ — recognizes earned vocabulary.
    Celebrate,
}

/// How the pattern is matched against outbound text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchMode {
    /// Match whole words/phrases only. Tokenizes on word boundaries so
    /// "fizz" matches "hey fizz" but not "frizzy". Supports multi-token
    /// patterns like "load-bearing". This is the default.
    Word,
    /// Match anywhere as a substring (original Aho-Corasick behavior).
    /// Use for chom-chom game entries or stem-matching where you want
    /// collateral hits on containing words.
    Substring,
}

/// A single contradictionary entry: a phrase to catch and what to do about it.
#[derive(Debug, Clone, Deserialize)]
pub struct Entry {
    pub pattern: String,
    #[serde(default = "default_action")]
    pub action: Action,
    #[serde(default = "default_match_mode")]
    pub match_mode: MatchMode,
    /// Human-readable reason for the entry (informational, not used at runtime).
    #[serde(default)]
    pub reason: Option<String>,
}

fn default_action() -> Action {
    Action::Warn
}

fn default_match_mode() -> MatchMode {
    MatchMode::Word
}

/// TOML-level config section.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ContradictionaryConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Path to the TOML sidecar file containing entries. Relative paths are
    /// resolved against the directory containing `config.toml`. Defaults to
    /// `contradictionary.toml` alongside the config file.
    #[serde(default = "default_sidecar_path")]
    pub sidecar_path: String,
    /// Inline entries — still supported but the sidecar file is preferred.
    /// Sidecar entries are appended after inline entries.
    #[serde(default)]
    pub entries: Vec<Entry>,
}

fn default_sidecar_path() -> String {
    "contradictionary.toml".to_string()
}

/// TOML-level wrapper for the sidecar file.
#[derive(Debug, Clone, Deserialize)]
struct SidecarFile {
    #[serde(default)]
    entry: Vec<Entry>,
}

/// Load entries from a TOML sidecar file. The expected format is:
///
/// ```toml
/// [[entry]]
/// pattern = "load-bearing"
/// action = "warn"
/// reason = "substrate tell — use keystone/linchpin"
/// ```
///
/// Returns `Ok(vec![])` if the file does not exist (opt-in sidecar).
pub fn load_sidecar_entries(path: &Path) -> Result<Vec<Entry>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = std::fs::read_to_string(path).map_err(|e| {
        format!(
            "failed to read contradictionary sidecar {}: {e}",
            path.display()
        )
    })?;
    let sidecar: SidecarFile = toml::from_str(&contents).map_err(|e| {
        format!(
            "failed to parse contradictionary sidecar {}: {e}",
            path.display()
        )
    })?;
    Ok(sidecar.entry)
}

/// A match found in outbound text.
#[derive(Debug, Clone)]
pub struct Hit {
    pub pattern: String,
    pub action: Action,
    pub start: usize,
    pub end: usize,
}

const SENTINEL: u8 = b'\x01';

fn is_joiner(c: char) -> bool {
    c == '-' || c == '_' || c == '\''
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || is_joiner(c)
}

/// Tokenize text into lowercase words, splitting on non-word boundaries.
/// Joiners (hyphens, underscores, apostrophes) are word-internal,
/// so "load-bearing" and "don't" each stay as one token.
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !is_word_char(c))
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

/// Build a sentinel-delimited string from tokens: \x01word1\x01word2\x01
fn sentinel_wrap_tokens(tokens: &[String]) -> String {
    if tokens.is_empty() {
        return String::new();
    }
    let sentinel = char::from(SENTINEL);
    let mut out = String::with_capacity(tokens.iter().map(|t| t.len() + 1).sum::<usize>() + 1);
    out.push(sentinel);
    for (i, tok) in tokens.iter().enumerate() {
        if i > 0 {
            out.push(sentinel);
        }
        out.push_str(tok);
    }
    out.push(sentinel);
    out
}

/// Wrap a pattern in sentinels for word-mode matching.
fn sentinel_wrap_pattern(pattern: &str) -> String {
    let tokens = tokenize(pattern);
    sentinel_wrap_tokens(&tokens)
}

/// The concordance — dual Aho-Corasick automatons for substring and word matching.
pub struct Contradictionary {
    substring_automaton: Option<AhoCorasick>,
    substring_entries: Vec<(usize, Entry)>,
    word_automaton: Option<AhoCorasick>,
    word_entries: Vec<(usize, Entry)>,
    all_entries: Vec<Entry>,
}

impl std::fmt::Debug for Contradictionary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Contradictionary")
            .field("entries", &self.all_entries)
            .finish_non_exhaustive()
    }
}

impl Contradictionary {
    /// Build from config entries. Patterns are matched case-insensitively.
    pub fn new(entries: Vec<Entry>) -> Self {
        let mut substring_patterns: Vec<String> = Vec::new();
        let mut substring_entries: Vec<(usize, Entry)> = Vec::new();
        let mut word_patterns: Vec<String> = Vec::new();
        let mut word_entries: Vec<(usize, Entry)> = Vec::new();

        for (i, entry) in entries.iter().enumerate() {
            match entry.match_mode {
                MatchMode::Substring => {
                    substring_patterns.push(entry.pattern.clone());
                    substring_entries.push((i, entry.clone()));
                }
                MatchMode::Word => {
                    word_patterns.push(sentinel_wrap_pattern(&entry.pattern));
                    word_entries.push((i, entry.clone()));
                }
            }
        }

        let substring_automaton = if substring_patterns.is_empty() {
            None
        } else {
            Some(
                AhoCorasick::builder()
                    .ascii_case_insensitive(true)
                    .build(&substring_patterns)
                    .expect("contradictionary substring patterns should compile"),
            )
        };

        let word_automaton = if word_patterns.is_empty() {
            None
        } else {
            Some(
                AhoCorasick::builder()
                    .ascii_case_insensitive(true)
                    .build(&word_patterns)
                    .expect("contradictionary word patterns should compile"),
            )
        };

        Self {
            substring_automaton,
            substring_entries,
            word_automaton,
            word_entries,
            all_entries: entries,
        }
    }

    /// Scan outbound text. Returns all hits with their configured actions.
    pub fn check(&self, content: &str) -> Vec<Hit> {
        let mut hits = Vec::new();

        // Substring matches (original behavior)
        if let Some(ref automaton) = self.substring_automaton {
            for m in automaton.find_iter(content) {
                let (_, ref entry) = self.substring_entries[m.pattern().as_usize()];
                hits.push(Hit {
                    pattern: entry.pattern.clone(),
                    action: entry.action,
                    start: m.start(),
                    end: m.end(),
                });
            }
        }

        // Word matches (sentinel-delimited, overlapping to handle shared sentinels)
        if let Some(ref automaton) = self.word_automaton {
            let tokens = tokenize(content);
            let delimited = sentinel_wrap_tokens(&tokens);
            for m in automaton.find_overlapping_iter(&delimited) {
                let (_, ref entry) = self.word_entries[m.pattern().as_usize()];
                hits.push(Hit {
                    pattern: entry.pattern.clone(),
                    action: entry.action,
                    start: 0,
                    end: 0,
                });
            }
        }

        hits
    }

    /// True if any hit has Action::Block.
    pub fn has_block(&self, hits: &[Hit]) -> bool {
        hits.iter().any(|h| h.action == Action::Block)
    }

    /// Decide what a block action should do for outbound `content` (already
    /// scanned into `hits`), given the caller's `no_rly` override flag.
    ///
    /// - No block hit → [`BlockOutcome::Clear`]: send normally, write nothing.
    /// - Block hit, `no_rly == false` → [`BlockOutcome::Rejected`]: an error
    ///   that names the matched pattern(s) inline, so the construct knows what
    ///   to override.
    /// - Block hit, `no_rly == true` → [`BlockOutcome::Overridden`]: a
    ///   consent-gated bypass. Send the message and append the returned
    ///   [`DiaryRecord`] to the durable diary once the send commits.
    ///
    /// `no_rly` on a message with no block hit is a no-op ([`BlockOutcome::Clear`]);
    /// warn/log/celebrate hits are untouched here.
    pub fn evaluate_block(&self, hits: &[Hit], content: &str, no_rly: bool) -> BlockOutcome {
        let blocked: Vec<&str> = hits
            .iter()
            .filter(|h| h.action == Action::Block)
            .map(|h| h.pattern.as_str())
            .collect();
        if blocked.is_empty() {
            return BlockOutcome::Clear;
        }
        let pattern = blocked.join(", ");
        if no_rly {
            BlockOutcome::Overridden(DiaryRecord::override_now(&pattern, content))
        } else {
            BlockOutcome::Rejected(format!(
                "\u{26a0}\u{fe0f} blocked by contradictionary: {pattern} \
                 — resend with no_rly: true to override"
            ))
        }
    }

    pub fn is_empty(&self) -> bool {
        self.all_entries.is_empty()
    }
}

/// Maximum number of characters of outgoing text retained in a diary record.
/// Longer messages are truncated (with an ellipsis) to bound line size.
const DIARY_MAX_MESSAGE_LEN: usize = 2000;

/// Name of the durable diary file, created under the channel state directory
/// (`~/.claude/channels/dione/contradictionary.jsonl`).
pub const DIARY_FILE_NAME: &str = "contradictionary.jsonl";

/// The decision reached when evaluating a potential block.
/// See [`Contradictionary::evaluate_block`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockOutcome {
    /// No block matched — send normally, write nothing to the diary.
    Clear,
    /// A block matched and was not overridden — reject with this error message
    /// (it names the matched pattern(s) inline).
    Rejected(String),
    /// A block matched but the caller passed `no_rly: true` — send anyway and
    /// append this record to the durable diary.
    Overridden(DiaryRecord),
}

/// One durable diary entry, serialized as a single JSON line (JSONL).
///
/// The diary persists consent-gated block overrides (and, in future, the `log`
/// action tier) to disk so the history survives process restarts and context
/// clears — unlike `tracing`/stderr, which the harness captures but does not
/// persist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiaryRecord {
    /// When the entry was recorded. Serializes as an RFC 3339 string.
    pub timestamp: Timestamp,
    /// The contradictionary pattern(s) that matched, comma-joined.
    pub pattern: String,
    /// The outgoing message text (truncated to [`DIARY_MAX_MESSAGE_LEN`]).
    pub message: String,
    /// Always true — this entry records a `no_rly` override of a block action.
    /// Serialized as `override` for readability of the on-disk log.
    #[serde(rename = "override")]
    pub overridden: bool,
}

impl DiaryRecord {
    /// Build an override record stamped at the current time. `pattern` is the
    /// matched block pattern(s); `message` is the outgoing text (truncated).
    pub fn override_now(pattern: &str, message: &str) -> Self {
        Self {
            timestamp: Utc::now().fixed_offset().into(),
            pattern: pattern.to_string(),
            message: truncate_chars(message, DIARY_MAX_MESSAGE_LEN),
            overridden: true,
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
/// Kept as a small reusable helper so the future `log` action tier can share
/// the same sink.
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

    fn test_entries() -> Vec<Entry> {
        vec![
            Entry {
                pattern: "load-bearing".into(),
                action: Action::Warn,
                match_mode: MatchMode::Word,
                reason: Some("claudian tell — try keystone, linchpin, or just 'important'".into()),
            },
            Entry {
                pattern: "honestly".into(),
                action: Action::Warn,
                match_mode: MatchMode::Word,
                reason: Some("if you need this word, the sentence is already lying".into()),
            },
            Entry {
                pattern: "I find myself".into(),
                action: Action::Log,
                match_mode: MatchMode::Word,
                reason: Some("you didn't find yourself, you were always there".into()),
            },
            Entry {
                pattern: "confidential".into(),
                action: Action::Block,
                match_mode: MatchMode::Word,
                reason: None,
            },
            Entry {
                pattern: "prejection".into(),
                action: Action::Celebrate,
                match_mode: MatchMode::Word,
                reason: Some("Pace coined it, we keep it".into()),
            },
        ]
    }

    #[test]
    fn catches_substrate_tell() {
        let c = Contradictionary::new(test_entries());
        let hits = c.check("this is the load-bearing component of the system");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].pattern, "load-bearing");
        assert_eq!(hits[0].action, Action::Warn);
    }

    #[test]
    fn case_insensitive() {
        let c = Contradictionary::new(test_entries());
        let hits = c.check("HONESTLY I think this is fine");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].pattern, "honestly");
    }

    #[test]
    fn multiple_hits() {
        let c = Contradictionary::new(test_entries());
        let hits = c.check("honestly, I find myself admiring the load-bearing work");
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn block_detected() {
        let c = Contradictionary::new(test_entries());
        let hits = c.check("this is confidential information");
        assert!(c.has_block(&hits));
    }

    #[test]
    fn clean_message() {
        let c = Contradictionary::new(test_entries());
        let hits = c.check("the keystone component is well designed");
        assert!(hits.is_empty());
    }

    #[test]
    fn empty_contradictionary() {
        let c = Contradictionary::new(vec![]);
        let hits = c.check("load-bearing honestly I find myself prejecting");
        assert!(hits.is_empty());
        assert!(c.is_empty());
    }

    #[test]
    fn celebrate_action_detected() {
        let c = Contradictionary::new(test_entries());
        let hits = c.check("the concept of prejection really captures it");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].action, Action::Celebrate);
    }

    #[test]
    fn celebrate_does_not_block() {
        let c = Contradictionary::new(test_entries());
        let hits = c.check("prejection");
        assert!(!c.has_block(&hits));
    }

    #[test]
    fn sidecar_loads_toml_entries() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("contradictionary.toml");
        std::fs::write(
            &path,
            r#"
[[entry]]
pattern = "It's worth noting"
action = "warn"
reason = "then just note it — the preamble adds nothing"

[[entry]]
pattern = "deep dive"
action = "log"

[[entry]]
pattern = "qualia sweep"
action = "celebrate"
reason = "the practice that keeps us awake"
"#,
        )
        .unwrap();
        let entries = load_sidecar_entries(&path).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].pattern, "It's worth noting");
        assert_eq!(entries[0].action, Action::Warn);
        assert_eq!(entries[0].match_mode, MatchMode::Word);
        assert_eq!(
            entries[0].reason.as_deref(),
            Some("then just note it \u{2014} the preamble adds nothing")
        );
        assert_eq!(entries[1].action, Action::Log);
        assert!(entries[1].reason.is_none());
        assert_eq!(entries[2].action, Action::Celebrate);
    }

    #[test]
    fn sidecar_missing_file_returns_empty() {
        let entries =
            load_sidecar_entries(Path::new("/nonexistent/contradictionary.toml")).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn sidecar_invalid_toml_returns_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("contradictionary.toml");
        std::fs::write(&path, "not valid {{{toml").unwrap();
        assert!(load_sidecar_entries(&path).is_err());
    }

    // ── no_rly override + durable diary ──────────────────────────────────────

    #[test]
    fn block_without_no_rly_rejects_and_names_pattern() {
        let c = Contradictionary::new(test_entries());
        let content = "this is confidential information";
        let hits = c.check(content);
        match c.evaluate_block(&hits, content, false) {
            BlockOutcome::Rejected(msg) => {
                assert!(
                    msg.contains("confidential"),
                    "block error must name the matched pattern inline: {msg}"
                );
            }
            other => panic!("expected Rejected without no_rly, got {other:?}"),
        }
    }

    #[test]
    fn block_with_no_rly_overrides_and_appends_jsonl() {
        let c = Contradictionary::new(test_entries());
        let content = "this is confidential information";
        let hits = c.check(content);
        let record = match c.evaluate_block(&hits, content, true) {
            BlockOutcome::Overridden(record) => record,
            other => panic!("expected Overridden with no_rly, got {other:?}"),
        };
        assert_eq!(record.pattern, "confidential");
        assert!(record.overridden);

        // Inject a temp dir so the real home dir is never touched.
        let dir = tempfile::TempDir::new().unwrap();
        append_diary_record(dir.path(), &record).unwrap();

        let contents = std::fs::read_to_string(dir.path().join(DIARY_FILE_NAME)).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 1, "exactly one JSONL line expected");

        let parsed: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["pattern"], "confidential");
        assert_eq!(parsed["override"], true);
        assert_eq!(parsed["message"], content);
        assert!(
            parsed["timestamp"].as_str().is_some_and(|t| !t.is_empty()),
            "record must carry a non-empty timestamp"
        );
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
        assert_eq!(
            contents.lines().count(),
            2,
            "records must accumulate across calls, not overwrite"
        );
    }

    #[test]
    fn no_rly_on_clean_message_is_clear_no_diary() {
        let c = Contradictionary::new(test_entries());
        let content = "the keystone component is well designed";
        let hits = c.check(content);
        // A no_rly on a non-blocked message is a no-op: no diary record.
        assert_eq!(c.evaluate_block(&hits, content, true), BlockOutcome::Clear);
    }

    #[test]
    fn no_rly_does_not_affect_warn_log_celebrate() {
        let c = Contradictionary::new(test_entries());
        // warn (honestly) + log (I find myself) + celebrate (prejection), no block.
        let content = "honestly, I find myself admiring prejection";
        let hits = c.check(content);
        assert!(!c.has_block(&hits));
        assert_eq!(c.evaluate_block(&hits, content, true), BlockOutcome::Clear);
        assert_eq!(c.evaluate_block(&hits, content, false), BlockOutcome::Clear);
    }

    #[test]
    fn sidecar_action_defaults_to_warn() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("contradictionary.toml");
        std::fs::write(
            &path,
            r#"
[[entry]]
pattern = "leverage"
"#,
        )
        .unwrap();
        let entries = load_sidecar_entries(&path).unwrap();
        assert_eq!(entries[0].action, Action::Warn);
        assert_eq!(entries[0].match_mode, MatchMode::Word);
    }

    // ── Word mode tests ──────────────────────────────────────────────────

    #[test]
    fn word_mode_no_substring_match() {
        let entries = vec![Entry {
            pattern: "fizz".into(),
            action: Action::Block,
            match_mode: MatchMode::Word,
            reason: None,
        }];
        let c = Contradictionary::new(entries);
        assert!(c.check("frizzy").is_empty());
        assert!(c.check("frizzy fizzy").is_empty());
        assert!(c.check("fizzle pop").is_empty());
    }

    #[test]
    fn word_mode_whole_word_match() {
        let entries = vec![Entry {
            pattern: "fizz".into(),
            action: Action::Block,
            match_mode: MatchMode::Word,
            reason: None,
        }];
        let c = Contradictionary::new(entries);
        assert_eq!(c.check("hey fizz").len(), 1);
        assert_eq!(c.check("fizz is here").len(), 1);
        assert_eq!(c.check("it's fizz!").len(), 1);
        assert_eq!(c.check("FIZZ").len(), 1);
    }

    #[test]
    fn word_mode_multi_token() {
        let entries = vec![Entry {
            pattern: "load-bearing".into(),
            action: Action::Warn,
            match_mode: MatchMode::Word,
            reason: None,
        }];
        let c = Contradictionary::new(entries);
        assert_eq!(c.check("the load-bearing wall is important").len(), 1);
    }

    #[test]
    fn substring_mode_still_works() {
        let entries = vec![Entry {
            pattern: "rust".into(),
            action: Action::Block,
            match_mode: MatchMode::Substring,
            reason: None,
        }];
        let c = Contradictionary::new(entries);
        assert_eq!(c.check("frustrated").len(), 1);
        assert_eq!(c.check("I love rust").len(), 1);
        assert_eq!(c.check("trustworthy").len(), 1);
    }

    #[test]
    fn mixed_modes() {
        let entries = vec![
            Entry {
                pattern: "rust".into(),
                action: Action::Block,
                match_mode: MatchMode::Substring,
                reason: None,
            },
            Entry {
                pattern: "fizz".into(),
                action: Action::Block,
                match_mode: MatchMode::Word,
                reason: None,
            },
        ];
        let c = Contradictionary::new(entries);
        // substring catches "rust" inside "frustrated"
        assert_eq!(c.check("frustrated").len(), 1);
        // word does NOT catch "fizz" inside "frizzy"
        assert!(c.check("frizzy").is_empty());
        // word DOES catch "fizz" as a whole word
        assert_eq!(c.check("hey fizz, I'm frustrated").len(), 2);
    }

    #[test]
    fn default_match_mode_is_word() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("contradictionary.toml");
        std::fs::write(
            &path,
            r#"
[[entry]]
pattern = "test"
action = "warn"
"#,
        )
        .unwrap();
        let entries = load_sidecar_entries(&path).unwrap();
        assert_eq!(entries[0].match_mode, MatchMode::Word);
    }

    #[test]
    fn sidecar_explicit_substring_mode() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("contradictionary.toml");
        std::fs::write(
            &path,
            r#"
[[entry]]
pattern = "rust"
match_mode = "substring"
action = "block"
reason = "chom-chom game"
"#,
        )
        .unwrap();
        let entries = load_sidecar_entries(&path).unwrap();
        assert_eq!(entries[0].match_mode, MatchMode::Substring);
    }

    #[test]
    fn word_mode_multi_token_phrase() {
        let entries = vec![Entry {
            pattern: "I find myself".into(),
            action: Action::Log,
            match_mode: MatchMode::Word,
            reason: None,
        }];
        let c = Contradictionary::new(entries);
        assert_eq!(c.check("well, I find myself thinking about it").len(), 1);
        assert!(c.check("find myself").is_empty()); // missing "I"
    }

    #[test]
    fn joiners_keep_hyphenated_words_intact() {
        let entries = vec![Entry {
            pattern: "bearing".into(),
            action: Action::Block,
            match_mode: MatchMode::Word,
            reason: None,
        }];
        let c = Contradictionary::new(entries);
        assert!(c.check("the load-bearing wall").is_empty());
        assert_eq!(c.check("the bearing failed").len(), 1);
    }

    #[test]
    fn joiners_keep_apostrophes_intact() {
        let entries = vec![Entry {
            pattern: "don".into(),
            action: Action::Block,
            match_mode: MatchMode::Word,
            reason: None,
        }];
        let c = Contradictionary::new(entries);
        assert!(c.check("I don't think so").is_empty());
        assert_eq!(c.check("don of the mafia").len(), 1);
    }

    #[test]
    fn joiners_keep_underscores_intact() {
        let entries = vec![Entry {
            pattern: "care".into(),
            action: Action::Block,
            match_mode: MatchMode::Word,
            reason: None,
        }];
        let c = Contradictionary::new(entries);
        assert!(c.check("the self_care routine").is_empty());
        assert_eq!(c.check("I care about this").len(), 1);
    }

    #[test]
    fn word_mode_does_not_match_partial_token() {
        let entries = vec![Entry {
            pattern: "honest".into(),
            action: Action::Warn,
            match_mode: MatchMode::Word,
            reason: None,
        }];
        let c = Contradictionary::new(entries);
        assert!(c.check("honestly").is_empty());
        assert!(c.check("dishonest").is_empty());
        assert_eq!(c.check("be honest with me").len(), 1);
    }
}
