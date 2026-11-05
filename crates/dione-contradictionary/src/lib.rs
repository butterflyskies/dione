//! Pattern matching, conservative automatic rewriting, and sidecar entry compatibility.
#![warn(missing_docs)]

use aho_corasick::AhoCorasick;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Action to take when a pattern matches outbound text.
///
/// The `warn` tier (send the message, self-react 🙊) was retired by
/// `contradictionary-action-tiers-v2` (2026-07-05) on the grounds that it was
/// room-facing and invisible to the construct: "decoration, not instrument."
/// See [`Action::Block`] for the accepted-but-deprecated `"warn"` spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
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
    /// Rewrite the match to the entry's `replace` text and send. Word-match
    /// only. A hit that cannot be rewritten cleanly is gated exactly like
    /// [`Action::Block`]: an `auto` entry with no `replace`, with
    /// `match_mode = "substring"`, or with an identifier-shaped pattern loads
    /// as `block` (see [`Contradictionary::new`]), and every path that only
    /// judges (rather than rewrites) holds an `auto` hit.
    Auto,
}

impl Action {
    /// True for the tiers that hold a message: `block`, and an `auto` hit
    /// that reaches evaluation unrewritten.
    pub fn gates(self) -> bool {
        matches!(self, Action::Block | Action::Auto)
    }
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
    /// The pattern to match.
    pub pattern: String,
    #[serde(default = "default_action")]
    /// The configured action tier.
    pub action: Action,
    #[serde(default = "default_match_mode")]
    /// Whether to match words or substrings.
    pub match_mode: MatchMode,
    /// Human-readable reason for the entry (informational, not used at runtime).
    #[serde(default)]
    pub reason: Option<String>,
    /// Replacement text for an `auto` entry. Required when `action = "auto"`;
    /// ignored by every other action.
    #[serde(default)]
    pub replace: Option<String>,
}

/// Why an `auto` entry cannot rewrite safely, or `None` when it can (or is
/// not an `auto` entry): it has no `replace`; it is in substring mode (a
/// substring rewrite corrupts words that contain the pattern); or its
/// pattern is shaped like an identifier — it contains `_`, starts with `-`,
/// or has an interior camelCase hump — so a match is likely code.
fn auto_entry_problem(entry: &Entry) -> Option<&'static str> {
    let pattern = &entry.pattern;
    let camel = pattern
        .chars()
        .zip(pattern.chars().skip(1))
        .any(|(a, b)| a.is_lowercase() && b.is_uppercase());
    if entry.action != Action::Auto {
        None
    } else if entry.replace.is_none() {
        Some("has no `replace`")
    } else if entry.match_mode == MatchMode::Substring {
        Some("uses match_mode = \"substring\" (auto is word-match only)")
    } else if pattern.contains('_') || pattern.starts_with('-') || camel {
        Some("has an identifier-shaped pattern (snake_case, camelCase or --flag)")
    } else {
        None
    }
}

/// Fail closed on an `auto` entry that cannot rewrite safely (see
/// [`auto_entry_problem`]): it is logged and treated as `block`, for the same
/// reason the retired `warn` spelling maps to `block` — a bad entry must not
/// silently stop gating.
fn fail_closed_auto(mut entry: Entry) -> Entry {
    if let Some(problem) = auto_entry_problem(&entry) {
        // The pattern is operator-authored and deliberately not logged.
        tracing::warn!(
            pattern_len = entry.pattern.len(),
            problem,
            "contradictionary auto entry is invalid; treating it as 'block'"
        );
        entry.action = Action::Block;
    }
    entry
}

fn default_action() -> Action {
    Action::Block
}

fn default_match_mode() -> MatchMode {
    MatchMode::Word
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
/// keep deserializing (see `RETIRED_ACTIONS`) rather than being deleted.
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
    let value: toml::Value = toml::from_str(&contents).map_err(|_error: toml::de::Error| {
        format!(
            "failed to parse contradictionary sidecar {}: invalid TOML syntax",
            path.display()
        )
    })?;
    for (pattern, action) in find_retired_actions(&value) {
        // The pattern is operator-authored and deliberately not logged.
        tracing::warn!(
            path = %path.display(),
            pattern_len = pattern.len(),
            action,
            "contradictionary entry uses retired action; treating it as 'block'. \
             Update the entry — this alias is a migration shim, not a supported value."
        );
    }
    let sidecar = SidecarFile::deserialize(value).map_err(|_error| {
        format!(
            "failed to parse contradictionary sidecar {}: invalid entry schema",
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
    /// The matching entry pattern.
    pub pattern: String,
    /// The matching entry action.
    pub action: Action,
    /// The entry's configured human-readable reason, carried through so the
    /// no_rly judge can name it when a block-tier hit bounces the message.
    pub reason: Option<String>,
    /// The substring start byte offset, or zero in word mode.
    pub start: usize,
    /// The substring end byte offset, or zero in word mode.
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
    ///
    /// An `auto` entry that cannot rewrite safely is downgraded to `block`
    /// here, so both inline and sidecar entries fail closed.
    pub fn new(entries: Vec<Entry>) -> Self {
        let entries: Vec<Entry> = entries.into_iter().map(fail_closed_auto).collect();
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

    /// True if any hit gates the message: `block`, or an `auto` hit that has
    /// not been rewritten away.
    pub fn has_block(&self, hits: &[Hit]) -> bool {
        hits.iter().any(|h| h.action.gates())
    }

    /// Returns whether the matcher contains no entries.
    pub fn is_empty(&self) -> bool {
        self.all_entries.is_empty()
    }

    /// Rewrite every `auto` hit in `content` to its entry's `replace` text.
    ///
    /// Returns `None` — rewrite nothing — unless every gating hit is a clean
    /// `auto` hit and the rewritten text has no gating hit of its own: any
    /// `block` hit, any `auto` hit that cannot be rewritten cleanly, or a
    /// rewrite that creates a new hit leaves the whole message to the judge,
    /// which holds it unrewritten. `None` is also the answer when there is no
    /// `auto` hit.
    pub fn apply_auto(&self, content: &str) -> Option<AutoRewritten> {
        let hits = self.check(content);
        let auto_hits = hits.iter().filter(|h| h.action == Action::Auto).count();
        if auto_hits == 0 || hits.iter().any(|h| h.action == Action::Block) {
            return None;
        }

        let tokens = token_spans(content);
        let mut edits: Vec<(std::ops::Range<usize>, &Entry)> = Vec::new();
        for (_, entry) in &self.word_entries {
            if entry.action != Action::Auto {
                continue;
            }
            for span in find_token_runs(content, &tokens, &entry.pattern) {
                edits.push((span, entry));
            }
        }
        // Every hit the automaton reported must map to exactly one located
        // span; anything else (an empty pattern, say) is not a clean rewrite.
        if edits.len() != auto_hits {
            return None;
        }
        edits.sort_by_key(|(span, _)| span.start);
        if edits.windows(2).any(|w| w[0].0.end > w[1].0.start) {
            return None;
        }
        // Never rewrite inside a marked span: such a hit falls back to `block`.
        let marked = marked_spans(content);
        if edits
            .iter()
            .any(|(span, _)| overlaps_any(span, &marked) || position_holds(content, &tokens, span))
        {
            return None;
        }

        let mut out = String::with_capacity(content.len());
        let mut rewrites = Vec::with_capacity(edits.len());
        let mut cursor = 0;
        for (span, entry) in edits {
            let matched = &content[span.clone()];
            let replace = entry.replace.as_deref()?;
            let replacement = preserve_case(matched, replace);
            out.push_str(&content[cursor..span.start]);
            out.push_str(&replacement);
            cursor = span.end;
            rewrites.push(AutoRewrite {
                pattern: entry.pattern.clone(),
                matched: matched.to_string(),
                replacement,
            });
        }
        out.push_str(&content[cursor..]);
        // An empty `replace` can leave nothing to send, and a `replace` can
        // itself be a gating pattern: either way the rewrite is not clean.
        if out.trim().is_empty() || self.check(&out).iter().any(|h| h.action.gates()) {
            return None;
        }
        Some(AutoRewritten {
            content: out,
            rewrites,
        })
    }
}

/// One match rewritten by an `auto` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoRewrite {
    /// The entry's pattern.
    pub pattern: String,
    /// The text as written in the message.
    pub matched: String,
    /// What it was rewritten to (the entry's `replace`, case-adjusted).
    pub replacement: String,
}

impl AutoRewrite {
    /// The sender-facing line: `auto: <match> → <replace>`.
    pub fn line(&self) -> String {
        format!("auto: {} \u{2192} {}", self.matched, self.replacement)
    }
}

/// The result of [`Contradictionary::apply_auto`]: the text to send, and
/// every rewrite that produced it, in message order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoRewritten {
    /// The rewritten text.
    pub content: String,
    /// The rewrites in message order.
    pub rewrites: Vec<AutoRewrite>,
}

/// Byte ranges of the word tokens in `text`, split exactly as [`tokenize`]
/// splits them, so a word-mode hit maps back to source positions.
fn token_spans(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        match (is_word_char(c), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                spans.push(s..i);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        spans.push(s..text.len());
    }
    spans
}

/// True when tokens `a` and `b` are separated only by spaces or tabs.
fn spaced(text: &str, tokens: &[std::ops::Range<usize>], a: usize, b: usize) -> bool {
    let gap = &text[tokens[a].end..tokens[b].start];
    !gap.is_empty() && gap.chars().all(|c| c == ' ' || c == '\t')
}

/// Source ranges (first token's start to last token's end) of every run of
/// consecutive tokens equal to `pattern`'s tokens, compared the way the word
/// automaton compares them (ASCII case-insensitively). Overlapping runs are
/// all reported, as the automaton reports them.
///
/// A run counts only when its words are separated by spaces or tabs: the
/// automaton matches across punctuation, newlines and emphasis too, but a
/// splice there would delete whatever sat between the words. Such a hit is
/// left unlocated, so it holds.
fn find_token_runs(
    text: &str,
    tokens: &[std::ops::Range<usize>],
    pattern: &str,
) -> Vec<std::ops::Range<usize>> {
    let wanted = tokenize(pattern);
    if wanted.is_empty() || wanted.len() > tokens.len() {
        return Vec::new();
    }
    (0..=tokens.len() - wanted.len())
        .filter(|&i| {
            wanted
                .iter()
                .zip(&tokens[i..])
                .all(|(w, t)| text[t.clone()].eq_ignore_ascii_case(w))
                && (i..i + wanted.len() - 1).all(|j| spaced(text, tokens, j, j + 1))
        })
        .map(|i| tokens[i].start..tokens[i + wanted.len() - 1].end)
        .collect()
}

/// True when `span` overlaps any of the `marked` ranges.
fn overlaps_any(span: &std::ops::Range<usize>, marked: &[std::ops::Range<usize>]) -> bool {
    marked
        .iter()
        .any(|m| m.start < span.end && span.start < m.end)
}

/// True when the match's position or capitalization says it is likely a
/// label, title, or proper noun rather than prose, so it falls back to
/// `block`:
/// - it is the whole content of its line or bullet item, trailing
///   punctuation aside (`- Utilize`, `- Utilize.`);
/// - it is Capitalized (or ALL-CAPS) and sits in a run of two or more
///   consecutive Capitalized words, anywhere (`Utilize Your Data`);
/// - it is Capitalized (or ALL-CAPS) and not sentence-initial
///   (`click the Utilize button`).
///
/// Sentence-initial means the start of the message or of a line, after a
/// list marker (`- `, `* `, `1. `), or after `.`, `!` or `?` and whitespace.
/// A line that continues one ending in a lowercase word or a comma is a soft
/// wrap, and a period that closes an abbreviation (`e.g.`) ends nothing.
fn position_holds(
    text: &str,
    tokens: &[std::ops::Range<usize>],
    span: &std::ops::Range<usize>,
) -> bool {
    let line_start = text[..span.start].rfind('\n').map_or(0, |p| p + 1);
    let line_end = text[span.end..]
        .find('\n')
        .map_or(text.len(), |p| span.end + p);
    let before = &text[line_start..span.start];
    let lead = before.trim_start();
    let listed = strip_list_marker(lead);
    let line_initial = listed.unwrap_or(lead).trim().is_empty();
    let rest_of_line = &text[span.end..line_end];
    if line_initial && !rest_of_line.contains(char::is_alphanumeric) {
        return true;
    }
    // A line that continues one ending mid-sentence (in a lowercase word or
    // a comma) is a soft wrap, not a sentence start.
    let previous_line = text[..line_start]
        .strip_suffix('\n')
        .map(|above| above.rsplit('\n').next().unwrap_or(above));
    let continues = listed.is_none()
        && previous_line.is_some_and(|line| {
            line.trim_end()
                .ends_with(|c: char| c.is_lowercase() || c == ',')
        });
    let line_initial = line_initial && !continues;

    let is_capitalized = |range: &std::ops::Range<usize>| {
        text[range.clone()]
            .chars()
            .find(|c| c.is_alphabetic())
            .is_some_and(char::is_uppercase)
    };
    if !is_capitalized(span) {
        return false;
    }

    // Title Case run: neighbours joined by spaces only, all Capitalized.
    let (Some(first), Some(last)) = (
        tokens.iter().position(|t| t.start == span.start),
        tokens.iter().position(|t| t.end == span.end),
    ) else {
        return true;
    };
    let spaced = |a: usize, b: usize| spaced(text, tokens, a, b);
    let (mut lo, mut hi) = (first, last);
    while lo > 0 && spaced(lo - 1, lo) && is_capitalized(&tokens[lo - 1]) {
        lo -= 1;
    }
    while hi + 1 < tokens.len() && spaced(hi, hi + 1) && is_capitalized(&tokens[hi + 1]) {
        hi += 1;
    }
    if hi > lo && tokens[lo..=hi].iter().all(is_capitalized) {
        return true;
    }

    // An abbreviation (`e.g.`, `i.e.`) does not end the sentence.
    let prior = before.trim_end();
    let abbreviation = prior
        .rsplit(char::is_whitespace)
        .next()
        .and_then(|word| word.strip_suffix('.'))
        .is_some_and(|word| word.contains('.'));
    let sentence_end =
        before.ends_with(char::is_whitespace) && prior.ends_with(['.', '!', '?']) && !abbreviation;
    !(line_initial || sentence_end)
}

/// The rest of `line` after a leading list marker (`- `, `* `, `1. `), or
/// `None` when it does not start with one.
fn strip_list_marker(line: &str) -> Option<&str> {
    let after = line.strip_prefix(['-', '*']).or_else(|| {
        let digits = line.bytes().take_while(u8::is_ascii_digit).count();
        (digits > 0)
            .then(|| line[digits..].strip_prefix('.'))
            .flatten()
    })?;
    after
        .starts_with(char::is_whitespace)
        .then(|| after.trim_start())
}

/// Byte ranges of `text` that are marked syntactically as not-prose: fenced
/// and indented code, `>` blockquote lines (and everything after `>>>`),
/// inline code, URLs, straight and curly quotations, Discord tokens, paths,
/// emails, and markdown link targets. An `auto` hit touching one is not
/// rewritten.
///
/// Block structure and code spans come from [`dione_evidence::markdown::BlockScanner`],
/// the one Markdown contract the chunker, evidence parsing and the status
/// lint also drive. Purely syntactic and deliberately greedy: an unclosed
/// fence, code span or quotation runs to the end of its region, since
/// over-marking only holds a message that `block` would have held anyway.
fn marked_spans(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut marked = Vec::new();
    let mut prose: Vec<std::ops::Range<usize>> = Vec::new();
    let mut scanner = dione_evidence::markdown::BlockScanner::new();
    let mut offset = 0;
    for raw in text.split_inclusive('\n') {
        let start = offset;
        offset += raw.len();
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        let class = scanner.push_with_spans(line, |from, to| marked.push(start + from..start + to));
        // An indented line is code to someone, even where CommonMark reads
        // it as a paragraph continuation.
        let indented = line.starts_with('\t') || line.starts_with("    ");
        if class.is_content() && !scanner.quote_rest() && !indented {
            match prose.last_mut() {
                Some(segment) if segment.end == start => segment.end = offset,
                _ => prose.push(start..offset),
            }
        } else if class != dione_evidence::markdown::LineClass::Blank {
            marked.push(start..offset);
        }
    }
    for segment in prose {
        mark_inline(text, segment, &mut marked);
    }
    marked
}

/// Mark the inline spans (see [`marked_spans`]) inside one prose segment.
/// Code spans are already marked by the block scanner.
fn mark_inline(
    text: &str,
    segment: std::ops::Range<usize>,
    marked: &mut Vec<std::ops::Range<usize>>,
) {
    mark_quotes(text, segment.clone(), marked);
    let base = segment.start;
    let s = &text[segment];
    // Link targets are marked, and break chunks, so the visible link text
    // stays rewritable.
    let mut targets: Vec<std::ops::Range<usize>> = Vec::new();
    let mut from = 0;
    while let Some(p) = s[from..].find("](") {
        let start = from + p + 1;
        let len = s[start..].find(')').map_or(s.len() - start, |q| q + 1);
        targets.push(start..start + len);
        from = start + len;
    }
    let mut chunk_start = None;
    for (i, c) in s.char_indices().chain(std::iter::once((s.len(), ' '))) {
        let breaks = c.is_whitespace() || targets.iter().any(|t| t.contains(&i));
        match chunk_start {
            None if !breaks => chunk_start = Some(i),
            Some(start) if breaks => {
                if is_marked_chunk(&s[start..i]) {
                    marked.push(base + start..base + i);
                }
                chunk_start = None;
            }
            _ => {}
        }
    }
    marked.extend(targets.into_iter().map(|t| base + t.start..base + t.end));
}

/// True when a whitespace-delimited chunk reads as a path, URL, host or file
/// name, email, handle, Discord token or `key=value` rather than prose: it
/// contains `/`, `\`, `@` or `=`; a `:` or `.` directly before a word
/// character (`scheme:`, `<:name:id>`, `utilize.md`, `www.`); a `<…>`; or
/// it is a `#channel`.
fn is_marked_chunk(chunk: &str) -> bool {
    let bare = chunk.trim_start_matches(['(', '[', '{', '"', '\'', '*', '_', '~', '|']);
    let joins_word = |sep: char| {
        chunk.char_indices().any(|(i, c)| {
            c == sep
                && i > 0
                && chunk[i + 1..]
                    .chars()
                    .next()
                    .is_some_and(char::is_alphanumeric)
        })
    };
    chunk.contains(['/', '\\', '@', '='])
        || chunk
            .find('<')
            .is_some_and(|open| chunk[open..].contains('>'))
        || bare
            .strip_prefix('#')
            .and_then(|rest| rest.chars().next())
            .is_some_and(char::is_alphanumeric)
        || joins_word(':')
        || joins_word('.')
}

/// Mark the quotations in one prose segment. Quote characters inside code
/// spans (already marked) do not count.
///
/// - `"` cannot say which end it is, so the first to the last is marked, and
///   an odd count (a stray or an inch mark) marks the whole segment: one
///   nested or unpaired quote must not flip the pairing.
/// - `“…”`, `«…»` and `‹…›` (in either direction) mark from the first to the
///   last of the pair, or the whole segment when the counts differ.
/// - `‘…’` and `'…'` double as apostrophes, so they pair from an opener at
///   the start of a word to the next quote that ends one; an apostrophe
///   between two letters (`don’t`) is neither. An opener with no closer marks
///   to the end of the segment.
fn mark_quotes(
    text: &str,
    segment: std::ops::Range<usize>,
    marked: &mut Vec<std::ops::Range<usize>>,
) {
    let base = segment.start;
    let s = &text[segment.clone()];
    let in_code = |i: usize| marked.iter().any(|m| m.contains(&(base + i)));
    let positions = |set: &[char]| -> Vec<(usize, char)> {
        s.char_indices()
            .filter(|&(i, c)| set.contains(&c) && !in_code(i))
            .collect()
    };
    let mut found: Vec<std::ops::Range<usize>> = Vec::new();
    let mut whole = false;

    let straight = positions(&['"']);
    if let (Some(first), Some(last)) = (straight.first(), straight.last()) {
        whole |= straight.len() % 2 == 1;
        found.push(first.0..last.0 + 1);
    }
    for (open, close) in [
        ('\u{201c}', '\u{201d}'),
        ('\u{ab}', '\u{bb}'),
        ('\u{2039}', '\u{203a}'),
    ] {
        let quotes = positions(&[open, close]);
        let opens = quotes.iter().filter(|&&(_, c)| c == open).count();
        if let (Some(first), Some(last)) = (quotes.first(), quotes.last()) {
            whole |= opens * 2 != quotes.len();
            found.push(first.0..last.0 + last.1.len_utf8());
        }
    }

    let word = |c: Option<char>| c.is_some_and(char::is_alphanumeric);
    let mut open_at: Option<usize> = None;
    for (i, c) in positions(&['\'', '\u{2018}', '\u{2019}']) {
        let before = s[..i].chars().next_back();
        let after = s[i + c.len_utf8()..].chars().next();
        if word(before) && word(after) {
            continue;
        }
        match open_at {
            None if c != '\u{2019}'
                && !word(before)
                && after.is_some_and(|a| !a.is_whitespace()) =>
            {
                open_at = Some(i);
            }
            Some(start)
                if c != '\u{2018}'
                    && before.is_some_and(|b| !b.is_whitespace())
                    && !word(after) =>
            {
                found.push(start..i + c.len_utf8());
                open_at = None;
            }
            _ => {}
        }
    }
    if let Some(start) = open_at {
        found.push(start..s.len());
    }

    if whole {
        marked.push(segment);
    } else {
        marked.extend(found.into_iter().map(|r| base + r.start..base + r.end));
    }
}

/// Carry the match's casing onto `replace` where it is cheap to: an
/// all-lowercase match keeps `replace` as written, a Capitalized match
/// capitalizes its first letter, an ALL-CAPS match uppercases it. Anything
/// else keeps `replace` as written.
fn preserve_case(matched: &str, replace: &str) -> String {
    let cased: Vec<char> = matched
        .chars()
        .filter(|c| c.is_uppercase() || c.is_lowercase())
        .collect();
    let Some((first, rest)) = cased.split_first() else {
        return replace.to_string();
    };
    if !first.is_uppercase() {
        return replace.to_string();
    }
    if rest.iter().all(|c| c.is_lowercase()) {
        // Capitalized (a lone capital letter lands here too).
        let mut chars = replace.chars();
        return match chars.next() {
            Some(c) => c.to_uppercase().chain(chars).collect(),
            None => String::new(),
        };
    }
    if rest.iter().all(|c| c.is_uppercase()) {
        return replace.to_uppercase();
    }
    replace.to_string()
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
            replace: None,
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

    // ── auto: load-time fail-closed ──────────────────────────────────────

    fn auto_entry(pattern: &str, replace: Option<&str>, match_mode: MatchMode) -> Entry {
        Entry {
            pattern: pattern.into(),
            action: Action::Auto,
            match_mode,
            reason: None,
            replace: replace.map(Into::into),
        }
    }

    #[test]
    fn sidecar_parses_auto_with_replace() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("contradictionary.toml");
        std::fs::write(
            &path,
            r#"
[[entry]]
pattern = "utilize"
action = "auto"
replace = "use"
"#,
        )
        .unwrap();
        let entries = load_sidecar_entries(&path).unwrap();
        assert_eq!(entries[0].action, Action::Auto);
        assert_eq!(entries[0].replace.as_deref(), Some("use"));
    }

    /// Each bad `auto` entry names its problem (the text that gets logged)
    /// and loads as `block`; a good one loads as `auto`.
    #[test]
    fn invalid_auto_entries_load_as_block() {
        let word =
            |pattern: &str, replace: &str| auto_entry(pattern, Some(replace), MatchMode::Word);
        for (entry, problem) in [
            (
                auto_entry("utilize", None, MatchMode::Word),
                Some("replace"),
            ),
            (
                auto_entry("utilize", Some("use"), MatchMode::Substring),
                Some("substring"),
            ),
            // Identifier-shaped patterns: a rewrite would edit code.
            (word("--verbose", "-v"), Some("identifier")),
            (word("utilize_cache", "use_cache"), Some("identifier")),
            (word("utilizeCache", "useCache"), Some("identifier")),
            (word("utilize", "use"), None),
            (word("load-bearing", "key"), None),
        ] {
            let found = auto_entry_problem(&entry);
            assert_eq!(
                found.map(|p| p.contains(problem.unwrap_or_default())),
                problem.map(|_| true),
                "{entry:?}: {found:?}"
            );
            let expected = if problem.is_some() {
                Action::Block
            } else {
                Action::Auto
            };
            let text = format!("pass {} now", entry.pattern);
            let hits = Contradictionary::new(vec![entry]).check(&text);
            assert_eq!(hits[0].action, expected, "fail closed: gate, not pass");
        }
    }

    // ── auto: rewrite ────────────────────────────────────────────────────

    fn auto_concordance() -> Contradictionary {
        Contradictionary::new(vec![
            auto_entry("utilize", Some("use"), MatchMode::Word),
            Entry {
                pattern: "confidential".into(),
                action: Action::Block,
                match_mode: MatchMode::Word,
                reason: None,
                replace: None,
            },
        ])
    }

    /// What happens to `content` under `c`: the text sent (rewritten, or
    /// verbatim when nothing gates), or `None` when the message is held.
    fn auto_outcome(c: &Contradictionary, content: &str) -> Option<String> {
        match c.apply_auto(content) {
            Some(rewritten) => Some(rewritten.content),
            None if c.has_block(&c.check(content)) => None,
            None => Some(content.to_string()),
        }
    }

    #[test]
    fn auto_plain_rewrite_reports_each_rewrite() {
        let rewritten = auto_concordance()
            .apply_auto("we utilize the cache and utilize it well")
            .expect("a clean auto hit rewrites");
        assert_eq!(rewritten.content, "we use the cache and use it well");
        let lines: Vec<String> = rewritten.rewrites.iter().map(AutoRewrite::line).collect();
        assert_eq!(lines, vec!["auto: utilize \u{2192} use"; 2]);
    }

    /// Messages an `auto` entry (utilize → use) sends, and what goes out.
    const SENDS: &[(&str, &str)] = &[
        ("nothing to see", "nothing to see"),
        // Case preservation.
        ("we utilize it", "we use it"),
        ("Utilize the cache.", "Use the cache."),
        ("UTILIZE the cache.", "USE the cache."),
        // Mixed case that is not camelCase-shaped keeps `replace` as written.
        ("UTILize the cache.", "use the cache."),
        // Markers elsewhere in the message do not stop a clean hit.
        (
            "we utilize `code` and \"quotes\" fine",
            "we use `code` and \"quotes\" fine",
        ),
        // Link text is prose; only the target is marked.
        (
            "read [we utilize it](https://example.com/x) first",
            "read [we use it](https://example.com/x) first",
        ),
        // Sentence-initial Capitalized matches rewrite.
        ("Done. Utilize the cache.", "Done. Use the cache."),
        ("Done! Utilize it? Utilize it.", "Done! Use it? Use it."),
        ("steps:\nUtilize the cache", "steps:\nUse the cache"),
        (
            "steps:\n1. Utilize the cache\n* Utilize it twice",
            "steps:\n1. Use the cache\n* Use it twice",
        ),
        ("- Utilize the cache", "- Use the cache"),
        ("ok.\nUtilize it", "ok.\nUse it"),
        ("intro\n\nUtilize the cache", "intro\n\nUse the cache"),
        (
            "we utilize it.\u{a0}Utilize more",
            "we use it.\u{a0}Use more",
        ),
        ("we utilize. It works", "we use. It works"),
        // Quotations elsewhere in the message, and apostrophes, leave a
        // clean hit alone.
        ("a \"b\" c \"d\" we utilize it", "a \"b\" c \"d\" we use it"),
        ("don't utilize the dogs' bowls", "don't use the dogs' bowls"),
        (
            "don\u{2019}t utilize the dogs\u{2019} bowls",
            "don\u{2019}t use the dogs\u{2019} bowls",
        ),
        ("run `\"` then utilize it", "run `\"` then use it"),
        // Punctuation that is not a path, address or token leaves prose.
        ("we utilize it: done", "we use it: done"),
        ("(we utilize it) ok", "(we use it) ok"),
        ("see [utilize](https://x/y) ok", "see [use](https://x/y) ok"),
        // A code span that crosses a line ends where it closes.
        ("x `a\nb` utilize", "x `a\nb` use"),
        // Joiners keep identifiers one token, so the word automaton never
        // reports `utilize` inside them: sent verbatim, never rewritten.
        ("call utilize_cache now", "call utilize_cache now"),
        ("call utilizeCache now", "call utilizeCache now"),
        ("pass --utilize now", "pass --utilize now"),
    ];

    /// Messages that hold instead of rewriting: the `auto` hit falls back to
    /// `block`, so the judge holds the message unrewritten.
    const HOLDS: &[&str] = &[
        // Block wins.
        "we utilize confidential data",
        // Inline code.
        "run `utilize` now",
        "run ``we utilize it`` now",
        // Fenced code.
        "see:\n```\nwe utilize it\n```\ndone",
        "see:\n```rust\nlet x = 1; // utilize\n```\ndone",
        // A ``` that opens mid-line opens a span that runs across lines.
        "see ```\nwe utilize it\n```",
        "a ``` b\nutilize\n```",
        // An unmatched backtick run marks to the end.
        "a ``` b utilize",
        // A ``` cannot close a ```` fence; a tagged ``` cannot close any.
        "````\nsome text\n```\nutilize this\n```\n````\n",
        "```\na\n```rust\nwe utilize it\n```\n",
        "x\n  ```\nutilize\n  ```\ny",
        // Indented code, and an indented line after prose.
        "intro\n\n    we utilize it\n\nafter",
        "x\n    indented utilize code\ny",
        // URL.
        "see https://example.com/utilize/docs for more",
        // Quotations.
        "he said \"we utilize it\" yesterday",
        "he said \u{201c}we utilize it\u{201d} yesterday",
        "he said \u{201c}we \u{201c}utilize\u{201d} it\u{201d} today",
        // Nested or stray straight quotes cannot flip the pairing.
        "he said \"we \"utilize\" it\" today",
        "5\" of rain\nwe \"utilize\" it",
        "a \"b\" c\" we utilize it",
        "he said \"we\n> quoted\nutilize it\" ok",
        // Single and guillemet quotations.
        "he said 'we utilize it' ok",
        "he said \u{2018}we utilize it\u{2019} ok",
        "he said \u{ab}we utilize it\u{bb} ok",
        "er sagte \u{bb}we utilize it\u{ab} heute",
        "he said 'we utilize it",
        // Blockquotes.
        "> we utilize it\nagreed",
        "agreed:\n>>> we\nutilize it",
        // Discord tokens.
        "nice <:utilize:123456> emoji",
        "nice <a:utilize:123456> emoji",
        "hi <@utilize> and <#utilize> now",
        // Unclosed markers run to the end of their region.
        "he said \"we utilize it",
        "run `utilize",
        "read [the docs](utilize",
        // Paths and email.
        "open ~/utilize/notes.md now",
        "open /srv/utilize/notes now",
        "mail utilize@example.com today",
        // Paths, URLs, file and host names, handles, tokens, key=value.
        "see (/srv/utilize/notes) now",
        "edit src/utilize/mod.rs now",
        "open utilize.md now",
        "see example.com/utilize now",
        "see www.utilize.com now",
        "path=~/utilize/x now",
        "set mode=utilize now",
        "run /utilize now",
        "run </utilize:123> now",
        "run <utilize> now",
        "<https://x.com/utilize> ok",
        "ping @utilize now",
        "go to #utilize now",
        "see C:\\utilize\\x now",
        "tag [x=v2:utilize:AAAAAAAAAAA]",
        // Link target.
        "read [the docs](utilize.md) first",
        // Capitalized mid-sentence ("click the Block button"), ALL-CAPS.
        "please click the Utilize button",
        "we UTILIZE it",
        // Title Case runs, anywhere.
        "Utilize Your Data today",
        "Utilize Data",
        "read Smart Utilize Guide first",
        // The whole of a line or bullet: a menu item or title.
        "menu:\n- Utilize\n- Quit",
        "menu:\nUtilize\nthat is all",
        "- Utilize.",
        // A line that continues an unfinished one is mid-sentence.
        "please click the\nUtilize button",
        "first,\nUtilize the cache",
        // An abbreviation's period does not end the sentence.
        "e.g. Utilize the cache",
        // A heading is a title.
        "# Utilize the cache",
    ];

    #[test]
    fn auto_sends() {
        let c = auto_concordance();
        let failures: Vec<String> = SENDS
            .iter()
            .filter_map(|&(content, sent)| {
                let got = auto_outcome(&c, content);
                (got.as_deref() != Some(sent))
                    .then(|| format!("{content:?}: want {sent:?}, got {got:?}"))
            })
            .collect();
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[test]
    fn auto_holds() {
        let c = auto_concordance();
        let failures: Vec<String> = HOLDS
            .iter()
            .filter_map(|&content| {
                let got = auto_outcome(&c, content);
                got.is_some()
                    .then(|| format!("{content:?}: want held, got {got:?}"))
            })
            .collect();
        assert!(failures.is_empty(), "{failures:#?}");
    }

    /// A rewrite that creates a new gating hit is not clean: the message
    /// holds as written, never half-rewritten.
    #[test]
    fn auto_rewrite_that_creates_a_new_hit_holds() {
        let c = Contradictionary::new(vec![
            auto_entry("utilize", Some("confidential use"), MatchMode::Word),
            auto_entry("employ", Some("leverage"), MatchMode::Word),
            auto_entry("leverage", Some("use"), MatchMode::Word),
            Entry {
                pattern: "confidential".into(),
                action: Action::Block,
                match_mode: MatchMode::Word,
                reason: None,
                replace: None,
            },
        ]);
        assert_eq!(c.apply_auto("we utilize it"), None);
        assert_eq!(c.apply_auto("we employ it"), None);
        assert_eq!(
            auto_outcome(&c, "we leverage it").as_deref(),
            Some("we use it")
        );
    }

    /// An empty `replace` deletes the match, but a rewrite that leaves
    /// nothing to send holds instead (Discord rejects a blank message).
    #[test]
    fn auto_blank_result_holds() {
        let c = Contradictionary::new(vec![auto_entry("utilize", Some(""), MatchMode::Word)]);
        assert_eq!(auto_outcome(&c, "we utilize it").as_deref(), Some("we  it"));
        assert_eq!(auto_outcome(&c, "utilize utilize"), None);
        assert_eq!(auto_outcome(&c, "utilize, utilize"), Some(", ".into()));
    }

    /// A multi-word pattern rewrites only a run whose words are separated by
    /// spaces or tabs. Across punctuation, a newline or emphasis the
    /// automaton still reports the hit, nothing is located, and it holds:
    /// a splice there would delete what sat between the words.
    #[test]
    fn auto_multi_word_runs_only_across_spaces() {
        let c = Contradictionary::new(vec![auto_entry("in order to", Some("to"), MatchMode::Word)]);
        let failures: Vec<String> = [
            ("we did it in order to win", Some("we did it to win")),
            ("in order\tto win", Some("to win")),
            ("I put them in order. To be fair, it worked.", None),
            ("sorted in order\n\nto ship", None),
            ("in **order** to win", None),
        ]
        .into_iter()
        .filter_map(|(content, want)| {
            let got = auto_outcome(&c, content);
            (got.as_deref() != want).then(|| format!("{content:?}: want {want:?}, got {got:?}"))
        })
        .collect();
        assert!(failures.is_empty(), "{failures:#?}");
    }

    /// Every input the #462 review probed, with the outcome it must have
    /// (`None`: held). Rows rewritten before the fix either hold now or are
    /// rewritten correctly.
    #[test]
    fn review_probe_inputs() {
        const UTILIZE: (&str, &str) = ("utilize", "use");
        let rows: &[((&str, &str), &str, Option<&str>)] = &[
            (UTILIZE, "see ```\nwe utilize it\n```", None),
            (UTILIZE, "see (/srv/utilize/notes) now", None),
            (UTILIZE, "edit src/utilize/mod.rs now", None),
            (UTILIZE, "open utilize.md now", None),
            (UTILIZE, "run </utilize:123> now", None),
            (UTILIZE, "ping @utilize now", None),
            (UTILIZE, "go to #utilize now", None),
            (UTILIZE, "see example.com/utilize now", None),
            (UTILIZE, "path=~/utilize/x now", None),
            (UTILIZE, "he said 'we utilize it' ok", None),
            (UTILIZE, "he said \u{2018}we utilize it\u{2019} ok", None),
            (UTILIZE, "he said \u{ab}we utilize it\u{bb} ok", None),
            (UTILIZE, "- Utilize.", None),
            (UTILIZE, "please click the\nUtilize button", None),
            (UTILIZE, "e.g. Utilize the cache", None),
            (UTILIZE, "**Utilize** the cache", None),
            (UTILIZE, "a ``` b\nutilize\n```", None),
            (UTILIZE, "x `a\nb` utilize", Some("x `a\nb` use")),
            (UTILIZE, "he said \"we\n> quoted\nutilize it\" ok", None),
            (UTILIZE, "> q\nwe utilize it", Some("> q\nwe use it")),
            (UTILIZE, "x ||utilize|| y", Some("x ||use|| y")),
            (
                UTILIZE,
                "see [utilize](https://x/y) ok",
                Some("see [use](https://x/y) ok"),
            ),
            (UTILIZE, "U+00e9 caf\u{e9} Utilize", None),
            (UTILIZE, "\u{c9}t\u{e9}: Utilize it", None),
            (UTILIZE, "1) Utilize it", None),
            (UTILIZE, "Utilize", None),
            (
                UTILIZE,
                "we utilize it.\u{a0}Utilize more",
                Some("we use it.\u{a0}Use more"),
            ),
            (UTILIZE, "ok.\nUtilize it", Some("ok.\nUse it")),
            (UTILIZE, "OK UTILIZE IT", None),
            (UTILIZE, "hi\r\n- Utilize\r\n- Quit", None),
            (UTILIZE, "~~utilize~~ ok", Some("~~use~~ ok")),
            (UTILIZE, "<https://x.com/utilize> ok", None),
            (UTILIZE, "*utilize* ok", Some("*use* ok")),
            // Joiners make these one token: no hit, sent verbatim.
            (UTILIZE, "__utilize__ ok", Some("__utilize__ ok")),
            (UTILIZE, "_utilize_ ok", Some("_utilize_ ok")),
            (UTILIZE, "he said \"we \"utilize\" it\" today", None),
            (
                UTILIZE,
                "he said \u{201c}we \u{201c}utilize\u{201d} it\u{201d} today",
                None,
            ),
            (
                UTILIZE,
                "````\nsome text\n```\nutilize this\n```\n````\n",
                None,
            ),
            (UTILIZE, "```\na\n```rust\nutilize\n```\n", None),
            (UTILIZE, "x\n  ```\nutilize\n  ```\ny", None),
            (UTILIZE, "```\na\n```rust\nwe utilize it\n```\n", None),
            (UTILIZE, "x\n    indented utilize code\ny", None),
            (UTILIZE, "a \"b\" c\" we utilize it", None),
            (UTILIZE, "5\" of rain\nwe \"utilize\" it", None),
            (UTILIZE, "run /utilize now", None),
            (UTILIZE, "we utilize. It works", Some("we use. It works")),
            (
                ("citation", "source"),
                "see this [\u{1f50d}=v2:citation:AAAAAAAAAAA]",
                None,
            ),
            (
                ("in order to", "to"),
                "I put them in order. To be fair, it worked.",
                None,
            ),
            (("in order to", "to"), "sorted in order\n\nto ship", None),
            (
                ("in order to", "to"),
                "we did it in order to win",
                Some("we did it to win"),
            ),
            (("in order to", "to"), "in **order** to win", None),
            (("--verbose", "-v"), "pass --verbose now", None),
        ];
        let failures: Vec<String> = rows
            .iter()
            .filter_map(|&((pattern, replace), content, want)| {
                let c = Contradictionary::new(vec![auto_entry(
                    pattern,
                    Some(replace),
                    MatchMode::Word,
                )]);
                let got = auto_outcome(&c, content);
                (got.as_deref() != want).then(|| format!("{content:?}: want {want:?}, got {got:?}"))
            })
            .collect();
        assert!(failures.is_empty(), "{failures:#?}");
    }

    // ── Word mode tests ──────────────────────────────────────────────────

    #[test]
    fn word_mode_no_substring_match() {
        let entries = vec![Entry {
            pattern: "fizz".into(),
            action: Action::Block,
            match_mode: MatchMode::Word,
            replace: None,
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
            replace: None,
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
            replace: None,
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
            replace: None,
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
            replace: None,
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
                replace: None,
                reason: None,
            },
            Entry {
                pattern: "fizz".into(),
                action: Action::Block,
                match_mode: MatchMode::Word,
                replace: None,
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
            replace: None,
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
            replace: None,
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
            replace: None,
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
            replace: None,
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
            replace: None,
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
            replace: None,
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
            replace: None,
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
            replace: None,
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
            replace: None,
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
            replace: None,
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
            replace: None,
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
            replace: None,
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
            replace: None,
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
            replace: None,
            reason: None,
        }];
        let c = Contradictionary::new(entries);
        assert!(c.check("honestly").is_empty());
        assert!(c.check("dishonest").is_empty());
        assert_eq!(c.check("be honest with me").len(), 1);
    }

    // ── Diary + evaluate_block tests ────────────────────────────────────
}
