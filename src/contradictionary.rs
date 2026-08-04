use std::path::Path;

use aho_corasick::AhoCorasick;
use serde::Deserialize;

/// Action to take when a pattern matches outbound text.
///
/// The `warn` tier (send the message, self-react 🙊) was retired by
/// `contradictionary-action-tiers-v2` (2026-07-05) on the grounds that it was
/// room-facing and invisible to the construct: "decoration, not instrument."
/// See [`Action::Block`] for the accepted-but-deprecated `"warn"` spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Block the message — return an error to the construct. This is the
    /// default: the substrate defaults to send, so the prosthetic defaults to
    /// stop.
    ///
    /// Accepts `"warn"` as a deprecated alias. This is a migration shim, not a
    /// supported value — [`load_sidecar_entries`] returns `Err` for the whole
    /// file on an unknown action, so removing the spelling outright would make
    /// a single stale entry silently erase every rule on that seat.
    #[serde(alias = "warn")]
    Block,
    /// Send the message, log the hit silently.
    Log,
    /// Send the message, self-react ✨ — recognizes earned vocabulary.
    Celebrate,
}

/// Action names that no longer exist but still deserialize, so an existing
/// sidecar cannot be broken by a tier's removal. Logged on load so the entries
/// get cleaned up rather than lingering indefinitely.
const RETIRED_ACTIONS: &[&str] = &["warn"];

/// How the pattern is matched against outbound text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchMode {
    /// Match whole words/phrases only. Tokenizes on word boundaries so
    /// "fizz" matches "hey fizz" but not "fizzy". Supports multi-token
    /// patterns like "load-bearing". This is the default.
    Word,
    /// Match anywhere as a substring (original Aho-Corasick behavior).
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
    Action::Block
}

fn default_match_mode() -> MatchMode {
    MatchMode::Word
}

/// TOML-level config section.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ContradictionaryConfig {
    pub enabled: bool,
    /// Path to the TOML sidecar file containing entries. Relative paths are
    /// resolved against the directory containing `config.toml`. Defaults to
    /// `contradictionary.toml` alongside the config file.
    pub sidecar_path: String,
    /// Inline entries — still supported but the sidecar file is preferred.
    /// Sidecar entries are appended after inline entries.
    pub entries: Vec<Entry>,
    /// How long a bounced message stays claimable, in seconds. Default 180
    /// (3 minutes): long enough to survive a bounce landing mid-tool-chain —
    /// slow tool calls can hold the construct's attention for a minute or
    /// more — but short enough that a release is still a decision about a
    /// live message rather than archaeology.
    pub hold_ttl_secs: u64,
    /// Maximum number of messages held at once. A new bounce arriving at
    /// capacity evicts the held entry closest to expiry (journaling it as
    /// expired), so a runaway tool loop cannot grow the queue without bound
    /// between sweeps. Default 32; values below 1 are treated as 1.
    pub max_pending: usize,
    /// How long raw bounce records stay in the no_rly journal before the
    /// condense tool folds them into daily summaries, in days. Bounces are
    /// low-volume, so the default keeps a full year of raw detail (chain
    /// links and message text) at negligible disk cost.
    pub journal_raw_retention_days: u32,
    /// How long condensed summaries survive before the vacuum tool drops
    /// them, in days. Default two years of aggregate history.
    pub journal_summary_retention_days: u32,
}

impl Default for ContradictionaryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sidecar_path: "contradictionary.toml".to_string(),
            entries: Vec::new(),
            hold_ttl_secs: 180,
            max_pending: 32,
            journal_raw_retention_days: 365,
            journal_summary_retention_days: 730,
        }
    }
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
/// action = "block"
/// reason = "substrate tell — use keystone/linchpin"
/// ```
///
/// Returns `Ok(vec![])` if the file does not exist (opt-in sidecar).
///
/// Note that an unparseable entry fails the *whole file* — callers get `Err`
/// and no entries at all, not a partial load. That is why retired action names
/// keep deserializing (see [`RETIRED_ACTIONS`]) rather than being deleted.
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
    let value: toml::Value = toml::from_str(&contents).map_err(|e| {
        format!(
            "failed to parse contradictionary sidecar {}: {e}",
            path.display()
        )
    })?;
    for (pattern, action) in find_retired_actions(&value) {
        tracing::warn!(
            path = %path.display(),
            pattern,
            action,
            "contradictionary entry uses retired action; treating it as 'block'. \
             Update the entry — this alias is a migration shim, not a supported value."
        );
    }
    let sidecar = SidecarFile::deserialize(value).map_err(|e| {
        format!(
            "failed to parse contradictionary sidecar {}: {e}",
            path.display()
        )
    })?;
    Ok(sidecar.entry)
}

/// Find entries still using a retired action name, as `(pattern, action)`
/// pairs, so the caller can name both the entry and its file when reporting
/// the deprecation. Returns an empty vec for a sidecar with nothing retired.
fn find_retired_actions(value: &toml::Value) -> Vec<(String, String)> {
    let Some(entries) = value.get("entry").and_then(toml::Value::as_array) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let action = entry.get("action").and_then(toml::Value::as_str)?;
            if !RETIRED_ACTIONS.contains(&action) {
                return None;
            }
            let pattern = entry
                .get("pattern")
                .and_then(toml::Value::as_str)
                .unwrap_or("<unnamed>");
            Some((pattern.to_string(), action.to_string()))
        })
        .collect()
}

/// A match found in outbound text.
///
/// `start`/`end` are byte offsets in the original text for substring-mode hits.
/// Word-mode hits set both to 0 — the sentinel-delimited positions don't map
/// back to source text.
#[derive(Debug, Clone)]
pub struct Hit {
    pub pattern: String,
    pub action: Action,
    /// The entry's configured human-readable reason, carried through so the
    /// no_rly judge can name it when a block-tier hit bounces the message.
    pub reason: Option<String>,
    pub start: usize,
    pub end: usize,
}

const SENTINEL: u8 = b'\x01';

fn is_joiner(c: char) -> bool {
    c == '-' || c == '_' || c == '\'' || c == '\u{2019}'
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
                    reason: entry.reason.clone(),
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
                    reason: entry.reason.clone(),
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

    pub fn is_empty(&self) -> bool {
        self.all_entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_entries() -> Vec<Entry> {
        vec![
            // Non-blocking tells. These were `warn` before that tier was
            // retired; `log` is now the only send-and-record action, so it
            // carries the "caught but not gated" case these tests rely on.
            Entry {
                pattern: "load-bearing".into(),
                action: Action::Log,
                match_mode: MatchMode::Word,
                reason: Some("claudian tell — try keystone, linchpin, or just 'important'".into()),
            },
            Entry {
                pattern: "honestly".into(),
                action: Action::Log,
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
    fn config_max_pending_defaults_and_parses() {
        let defaults = ContradictionaryConfig::default();
        assert_eq!(defaults.max_pending, 32);

        let parsed: ContradictionaryConfig =
            toml::from_str("enabled = true\nmax_pending = 4\n").unwrap();
        assert_eq!(parsed.max_pending, 4);
    }

    #[test]
    fn catches_substrate_tell() {
        let c = Contradictionary::new(test_entries());
        let hits = c.check("this is the load-bearing component of the system");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].pattern, "load-bearing");
        assert_eq!(hits[0].action, Action::Log);
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

    fn assert_empty_pattern_matches_all(mode: MatchMode) {
        let c = Contradictionary::new(vec![Entry {
            pattern: "".into(),
            action: Action::Block,
            match_mode: mode,
            reason: Some("empty pattern test".into()),
        }]);
        assert!(!c.check("literally anything").is_empty());
        assert!(!c.check("a").is_empty());
        assert!(!c.check("hello world").is_empty());
    }

    #[test]
    fn empty_pattern_matches_all_text() {
        assert_empty_pattern_matches_all(default_match_mode());
    }

    #[test]
    fn empty_pattern_substring_mode_matches_all_text() {
        assert_empty_pattern_matches_all(MatchMode::Substring);
    }

    #[test]
    fn empty_pattern_word_mode_matches_all_text() {
        assert_empty_pattern_matches_all(MatchMode::Word);
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
        // Written as `action = "warn"` — a retired tier kept as a deserializing
        // alias so real sidecars survive its removal. Resolves to `block`.
        assert_eq!(entries[0].action, Action::Block);
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

    #[test]
    fn hits_carry_the_entry_reason() {
        let c = Contradictionary::new(test_entries());
        let hits = c.check("honestly now");
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].reason.as_deref(),
            Some("if you need this word, the sentence is already lying")
        );
    }

    /// The default action is `block`, per `contradictionary-action-tiers-v2`
    /// (2026-07-05): the substrate defaults to send, so the prosthetic defaults
    /// to stop. An entry that names no action must gate, not decorate.
    #[test]
    fn sidecar_action_defaults_to_block() {
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
        assert_eq!(entries[0].action, Action::Block);
        assert_eq!(entries[0].match_mode, MatchMode::Word);
    }

    /// Migration shim: `action = "warn"` is a retired tier that must still
    /// deserialize. Dropping the variant outright would make the whole sidecar
    /// fail to parse — and `load_sidecar_entries` returns `Err` for the entire
    /// file, so a single stale entry would silently erase every rule on that
    /// seat. The alias maps to `block` (gate, don't decorate).
    #[test]
    fn sidecar_retired_warn_action_deserializes_to_block() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("contradictionary.toml");
        std::fs::write(
            &path,
            r#"
[[entry]]
pattern = "leverage"
action = "warn"

[[entry]]
pattern = "confidential"
action = "block"
"#,
        )
        .unwrap();
        let entries = load_sidecar_entries(&path)
            .expect("a retired warn action must not fail the whole sidecar");
        assert_eq!(entries.len(), 2, "no entry may be dropped by the migration");
        assert_eq!(entries[0].action, Action::Block);
        assert_eq!(entries[1].action, Action::Block);
    }

    /// The deprecation is reported per entry, naming the pattern — a silent
    /// alias would let retired spellings accumulate forever.
    #[test]
    fn retired_actions_are_found_and_named() {
        let value: toml::Value = toml::from_str(
            r#"
[[entry]]
pattern = "stale"
action = "warn"

[[entry]]
pattern = "current"
action = "block"

[[entry]]
pattern = "defaulted"
"#,
        )
        .unwrap();
        assert_eq!(
            find_retired_actions(&value),
            vec![("stale".to_string(), "warn".to_string())],
            "only the retired entry is reported, and it is named"
        );
    }

    #[test]
    fn no_retired_actions_reports_nothing() {
        let value: toml::Value = toml::from_str(
            r#"
[[entry]]
pattern = "current"
action = "block"
"#,
        )
        .unwrap();
        assert!(find_retired_actions(&value).is_empty());
    }

    /// The failure mode the alias exists to prevent: one unparseable entry
    /// takes the entire file down, not just itself.
    #[test]
    fn sidecar_unknown_action_fails_whole_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("contradictionary.toml");
        std::fs::write(
            &path,
            r#"
[[entry]]
pattern = "kept"
action = "block"

[[entry]]
pattern = "bogus"
action = "nonsense"
"#,
        )
        .unwrap();
        assert!(
            load_sidecar_entries(&path).is_err(),
            "an unknown action must fail the load — this is why 'warn' needs an alias"
        );
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
    fn word_mode_fizz_matches_fizz_not_fizzy() {
        let entries = vec![Entry {
            pattern: "fizz".into(),
            action: Action::Block,
            match_mode: MatchMode::Word,
            reason: None,
        }];
        let c = Contradictionary::new(entries);
        assert_eq!(c.check("hey fizz").len(), 1);
        assert!(c.check("fizzy").is_empty());
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
            action: Action::Block,
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

    // ── Unicode tests ────────────────────────────────────────────────

    #[test]
    fn unicode_substring_match() {
        let entries = vec![Entry {
            pattern: "café".into(),
            action: Action::Block,
            match_mode: MatchMode::Substring,
            reason: None,
        }];
        let c = Contradictionary::new(entries);
        assert_eq!(c.check("the café downtown").len(), 1);
        // ascii_case_insensitive folds A-Z only — É (U+00C9) ≠ é (U+00E9)
        assert!(c.check("CAFÉ").is_empty());
        assert_eq!(c.check("Café").len(), 1); // ASCII C folds, é stays
    }

    #[test]
    fn unicode_word_match() {
        let entries = vec![Entry {
            pattern: "naïve".into(),
            action: Action::Block,
            match_mode: MatchMode::Word,
            reason: None,
        }];
        let c = Contradictionary::new(entries);
        assert_eq!(c.check("that's naïve").len(), 1);
        assert_eq!(c.check("a naïve approach").len(), 1);
        assert!(c.check("naive").is_empty()); // different codepoint
    }

    #[test]
    fn curly_apostrophe_is_joiner() {
        let entries = vec![Entry {
            pattern: "don".into(),
            action: Action::Block,
            match_mode: MatchMode::Word,
            reason: None,
        }];
        let c = Contradictionary::new(entries);
        // curly right single quote (U+2019) from rich-text paste
        assert!(c.check("I don\u{2019}t think so").is_empty());
        assert_eq!(c.check("don of the mafia").len(), 1);
    }

    #[test]
    fn em_dash_is_boundary_but_hyphen_is_joiner() {
        let entries = vec![Entry {
            pattern: "load".into(),
            action: Action::Block,
            match_mode: MatchMode::Word,
            reason: None,
        }];
        let c = Contradictionary::new(entries);
        // em-dash (U+2014) is a boundary — "load" is its own token
        assert_eq!(c.check("load\u{2014}squirreling").len(), 1);
        // hyphen-minus is a joiner — "load-squirreling" is one token
        assert!(c.check("load-squirreling").is_empty());
    }

    #[test]
    fn unicode_compound_with_joiner() {
        // prêt-à-porter: Unicode on both sides of hyphens
        let entries = vec![Entry {
            pattern: "porter".into(),
            action: Action::Block,
            match_mode: MatchMode::Word,
            reason: None,
        }];
        let c = Contradictionary::new(entries);
        assert!(c.check("she wore prêt-à-porter fashion").is_empty());
        assert_eq!(c.check("the porter carried bags").len(), 1);
    }

    #[test]
    fn unicode_immediately_flanking_joiner() {
        // Ülkü-Özlem: Unicode on BOTH sides of the hyphen (ü-Ö)
        let entries = vec![Entry {
            pattern: "Özlem".into(),
            action: Action::Block,
            match_mode: MatchMode::Word,
            reason: None,
        }];
        let c = Contradictionary::new(entries);
        assert!(c.check("Ülkü-Özlem arrived").is_empty());
        assert_eq!(c.check("Özlem arrived").len(), 1);
    }

    // ── Hit position tests ───────────────────────────────────────────

    #[test]
    fn substring_hit_has_correct_byte_offsets() {
        let entries = vec![Entry {
            pattern: "fizz".into(),
            action: Action::Block,
            match_mode: MatchMode::Substring,
            reason: None,
        }];
        let c = Contradictionary::new(entries);
        let hits = c.check("the fizzle pop");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].start, 4);
        assert_eq!(hits[0].end, 8);
    }

    #[test]
    fn substring_hit_byte_offsets_with_unicode() {
        let entries = vec![Entry {
            pattern: "café".into(),
            action: Action::Block,
            match_mode: MatchMode::Substring,
            reason: None,
        }];
        let c = Contradictionary::new(entries);
        let hits = c.check("the café is nice");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].start, 4);
        // é is 2 bytes in UTF-8, so "café" is 5 bytes
        assert_eq!(hits[0].end, 9);
    }

    #[test]
    fn word_mode_hit_positions_are_zero() {
        let entries = vec![Entry {
            pattern: "fizz".into(),
            action: Action::Block,
            match_mode: MatchMode::Word,
            reason: None,
        }];
        let c = Contradictionary::new(entries);
        let hits = c.check("the fizz is here");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].start, 0);
        assert_eq!(hits[0].end, 0);
    }

    #[test]
    fn word_mode_does_not_match_partial_token() {
        let entries = vec![Entry {
            pattern: "honest".into(),
            action: Action::Block,
            match_mode: MatchMode::Word,
            reason: None,
        }];
        let c = Contradictionary::new(entries);
        assert!(c.check("honestly").is_empty());
        assert!(c.check("dishonest").is_empty());
        assert_eq!(c.check("be honest with me").len(), 1);
    }
}
