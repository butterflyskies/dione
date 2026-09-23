//! Status-packet lint — the first production [`PreSendHook`].
//!
//! Checks the house status-update conventions on outbound messages that are
//! shaped like status packets. The hook only ever returns
//! [`HookDecision::Continue`]; findings travel as assessments, so it cannot
//! change, redirect, or stop a send.
//!
//! # What counts as a status packet
//!
//! A message is a status packet when it contains at least one *item line*: an
//! eligible line whose first meaningful character is a state marker. Item lines
//! are the item boundaries.
//!
//! A line is *eligible* when it sits outside fenced code blocks and is not a
//! blockquote. A marker appearing mid-sentence is a mention, not an item, so
//! rules keyed to items never fire on prose that merely discusses markers.
//!
//! # Exemption asymmetry — deliberate, do not "fix" toward the contradictionary
//!
//! The contradictionary protects reader bandwidth, so a banned token counts
//! even inside quotation marks; exempting quotes there is a trivial bypass.
//! This hook validates *asserted work-state structure*, so mentioned or example
//! syntax does not count. A marker inside a code fence is an example, not a
//! claim about anything, and enforcing there produces pure false positives.
//!
//! The exemption follows the protected invariant, not a house-wide quote
//! policy. Anyone implementing from a remembered rule that "the house does not
//! do use/mention exemptions" will get this backwards.

use crate::{
    markdown::BlockScanner,
    pre_send::{
        Assessment, AuditTrail, ConstructFeedback, HookContext, HookDecision, HookName, HookOutput,
        PreSendHook,
    },
};
use regex::Regex;
use std::sync::LazyLock;

/// Ceilings on the scan itself. Generous against any real status packet —
/// the longest this seat has sent is well under a hundred lines — and small
/// enough that a hostile or accidental megabyte cannot multiply into heap
/// pressure before the pipeline's panic guard.
const MAX_SCAN_LINES: usize = 1_000;
const MAX_SCAN_BYTES: usize = 128 * 1024;

/// State markers, matched on their base codepoint so an optional variation
/// selector (U+FE0F) does not change detection.
const MARKERS: [char; 3] = ['\u{25B6}', '\u{26A0}', '\u{274C}'];

/// Discord snowflakes: opaque to a human reader.
static SNOWFLAKE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d{17,20}\b").expect("snowflake pattern compiles"));

/// Hex runs that may be short SHAs. Digit-only runs are filtered out by
/// [`is_short_sha`] so ordinary numbers do not match.
static HEXISH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[0-9a-f]{7,12}\b").expect("hex pattern compiles"));

/// `#123`-style references. Unbounded digits: there is no documented
/// platform maximum, and a `{1,5}` ceiling silently exempted `#123456` from
/// both the opaque-id and missing-link rules. Qualification is decided by the
/// preceding character rather than by lookbehind, which `regex` lacks.
static HASH_NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"#\d+\b").expect("hash-number pattern compiles"));

/// Words that name a linkable artifact when followed by a number.
static ARTIFACT_WORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(pr|pull request|issue)\s*#?\d+\b").expect("artifact pattern compiles")
});

/// The convention word, token-bounded so `stewardship` and `stewardess` do
/// not match.
static STEWARD_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bsteward\b").expect("steward pattern compiles"));

/// A clock time carrying a timezone suffix. House convention is Pacific with
/// no suffix.
static TZ_SUFFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b\d{1,2}(:\d{2})?\s*(am|pm)?\s+(pt|pst|pdt)\b").expect("tz pattern compiles")
});

/// The lint's assessment categories, one per rule.
mod category {
    pub const OPAQUE_ID: &str = "status-lint/opaque-id";
    pub const MULTIPLE_MARKERS: &str = "status-lint/multiple-markers";
    pub const TRAILING_MARKER: &str = "status-lint/trailing-marker";
    pub const MISSING_LINK: &str = "status-lint/missing-link";
    pub const MISSING_SUBJECT: &str = "status-lint/missing-bold-subject";
    pub const TIMEZONE_SUFFIX: &str = "status-lint/timezone-suffix";
    pub const STEWARD: &str = "status-lint/steward";
    pub const SUMMARY: &str = "status-lint/summary";
    pub const TRUNCATED: &str = "status-lint/truncated";
    pub const INPUT_TRUNCATED: &str = "status-lint/input-truncated";
}

/// One line of the message together with a masked view of it.
///
/// `masked` has the same **byte** length as the scanned line, and therefore
/// the same byte offsets: every byte belonging to an inline-code span or a URL
/// is replaced by an ASCII space. Character count is *not* preserved — a
/// multi-byte character inside a code span becomes several spaces — so callers
/// must index by byte, as the regex crate does.
///
/// Excluded syntax carries an empty `masked`, since it is neither audited nor
/// allowed to contribute identifiers.
///
/// Callers consume `masked` for every decision — eligibility, marker counting,
/// identifier scanning and link presence alike — so the exemption contract is
/// decided in exactly one place instead of by scans that can disagree.
struct Line {
    number: usize,
    eligible: bool,
    masked: String,
    /// A URL outside inline code. An artifact reference whose only URL sits in
    /// backticks is example syntax, not a clickable link.
    has_visible_url: bool,
}

/// The slice of `text` the scan is permitted to look at, and whether anything
/// was left off.
///
/// Separate from [`scan`] so the ceiling is checkable on its own. The property
/// that matters is not "few lines are retained" — the old per-line ceiling gave
/// that too — but that **nothing downstream is ever handed more than
/// `MAX_SCAN_BYTES`**, including the newline search inside `str::lines`.
fn bounded_prefix(text: &str) -> (&str, bool) {
    if text.len() > MAX_SCAN_BYTES {
        (&text[..text.floor_char_boundary(MAX_SCAN_BYTES)], true)
    } else {
        (text, false)
    }
}

/// Builds the structural view in a single linear pass.
///
/// Block classification is delegated wholesale to [`BlockScanner`], the
/// contract shared with evidence parsing and the chunker. This module used to
/// re-derive a subset of it and got the composition wrong twice: a fence
/// delimiter line was scanned for inline spans (so a wider closing fence left
/// a span open across the next real item), and a four-space-indented line
/// stayed eligible (so an indented example activated the lint and was then
/// faulted for the link it visibly carried).
///
/// The rule the scanner enforces: excluded syntax is neither audited nor
/// allowed to mutate the state that decides later content.
fn scan(text: &str) -> (Vec<Line>, bool) {
    // Bound the input BEFORE `lines()` sees it.
    //
    // `str::lines` yields each item by searching forward to the next newline
    // or to the end of the input, so a newline-free body costs O(len) to
    // produce even its first line. A ceiling applied per line, after that
    // search has already run, bounds the bytes retained and not the work
    // done — which is the half that matters for a hook running ahead of the
    // pipeline's panic guard on an unbounded MCP body.
    //
    // Slicing the whole text first also removes the synthetic per-line `+1`
    // that used to stand in for the newline: an input of exactly
    // `MAX_SCAN_BYTES` with no trailing newline was charged a byte it does
    // not contain and reported as truncated one byte early.
    let (scanned, mut truncated) = bounded_prefix(text);

    let mut out = Vec::new();
    let mut scanner = BlockScanner::block_scoped();
    let mut spans: Vec<(usize, usize)> = Vec::new();

    for (index, line) in scanned.lines().enumerate() {
        if out.len() >= MAX_SCAN_LINES {
            truncated = true;
            break;
        }

        spans.clear();
        let class = scanner.push_with_spans(line, |begin, end| spans.push((begin, end)));
        // `>>>` puts the remainder of the message inside a quote, so
        // eligibility outlives the line that opened it.
        let eligible = class.is_content() && !scanner.quote_rest();
        let (masked, has_visible_url) = if eligible {
            mask_line(line, &spans)
        } else {
            (String::new(), false)
        };

        out.push(Line {
            number: index + 1,
            eligible,
            masked,
            has_visible_url,
        });
    }
    (out, truncated)
}

/// Blanks the given inline-code spans and any URLs, preserving byte positions,
/// and reports whether a URL survived outside inline code.
///
/// The spans come from [`BlockScanner`], which carries delimiter state across
/// lines and knows that an escaped backtick does not open a span. This module
/// previously reset that state every line and treated `\`` as an opener, so a
/// multiline span's continuation could become a false status item and one
/// escaped backtick could mask the rest of real prose.
fn mask_line(line: &str, spans: &[(usize, usize)]) -> (String, bool) {
    let mut masked = line.as_bytes().to_vec();
    for &(begin, end) in spans {
        for byte in &mut masked[begin..end] {
            *byte = b' ';
        }
    }

    // URLs are masked after code spans, so anything still matching here is
    // outside backticks and therefore a clickable link.
    //
    // Two things the bare `starts_with` got wrong. A scheme has to begin at a
    // token boundary, or `xhttp://x` matched at offset 1 and satisfied the
    // missing-link rule. And the authority has to be non-empty, or a bare
    // `http://` with nothing after it counted as a link — which is exactly the
    // shape a status packet gets when someone means to paste a URL and
    // doesn't.
    let (visible_url, _attempts) = mask_urls(&mut masked);

    // Span and URL boundaries land on ASCII bytes (backticks, `h`, whitespace)
    // and every replacement is a single-byte space, so multi-byte characters
    // are never split.
    let masked = String::from_utf8(masked).expect("masking preserves UTF-8 boundaries");
    (masked, visible_url)
}

/// Blanks every visible URL in `masked`, reporting whether one was found and
/// **how many parse attempts it took**.
///
/// The attempt count is not diagnostic decoration: it is the oracle for this
/// function's work bound. A wall-clock assertion would be flaky and would not
/// say *why* it got slow, so the test asserts the count instead.
fn mask_urls(masked: &mut [u8]) -> (bool, usize) {
    let mut visible_url = false;
    let mut attempts = 0usize;
    let mut index = 0;
    while index < masked.len() {
        if scheme_at(masked, index).is_none() {
            index += 1;
            continue;
        }
        // One candidate boundary, one parse, then advance past the whole
        // candidate whatever the verdict. That last clause is the work bound:
        // the previous walk resumed just past the scheme on a rejected
        // candidate, so a whitespace-free run re-scanned and re-parsed its own
        // tail once per scheme — 16,384 overlapping parses over ~1.07 GB at the
        // byte ceiling. Every byte now belongs to at most one candidate.
        //
        // The trade is that a second scheme *inside* one token is no longer
        // recovered. Adversarial only, and pinned by test.
        let candidate_end = masked[index..]
            .iter()
            .position(u8::is_ascii_whitespace)
            .map_or(masked.len(), |offset| index + offset);
        // Both bounds sit on ASCII bytes — the scheme's first byte and either
        // whitespace or the end — so the slice is always a character boundary.
        // `scheme_at` matched here, so `candidate_end` is at least seven bytes
        // past `index` and the walk always advances.
        attempts += 1;
        let is_link = std::str::from_utf8(&masked[index..candidate_end])
            .ok()
            .and_then(|candidate| reqwest::Url::parse(candidate).ok())
            .is_some_and(|url| {
                // Structure comes from the parser; the narrow house policy is
                // applied to the host the parser actually found.
                //
                // Reading the first byte of the *authority* instead was wrong:
                // when userinfo is present that byte belongs to the userinfo,
                // so `http://x@)` and `http://x@-notahost` presented an `x` to
                // a rule written to reject `)` and `-notahost`. The policy was
                // sound; it was pointed at the wrong bytes.
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some_and(host_passes_house_policy)
            });
        if !is_link {
            index = candidate_end;
            continue;
        }
        visible_url = true;
        for byte in &mut masked[index..candidate_end] {
            *byte = b' ';
        }
        index = candidate_end;
    }
    (visible_url, attempts)
}

/// The house's narrow view of what a host may look like, deliberately tighter
/// than RFC 3986.
///
/// `Url::parse` accepts `)` and `-notahost` as valid hosts. This lint exists to
/// tell someone they pasted no link, so a punctuation-only pseudo-authority
/// must not silence it. The parser decides *structure*; this decides whether
/// the structure names something a person plausibly meant to link to.
///
/// Takes the host the parser found, never a raw offset into the line — see the
/// userinfo note in [`mask_urls`].
fn host_passes_house_policy(host: &str) -> bool {
    host.as_bytes()
        .first()
        // `[` opens an IPv6 literal; `url` keeps the brackets in `host_str`.
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'[')
}

/// Byte offset just past `http://` or `https://` starting at `index`, if a
/// scheme starts there *at a token boundary*.
///
/// The preceding byte must not be one a scheme could itself contain
/// (RFC 3986 allows letters, digits, `+`, `-`, `.`), so `xhttp://` and
/// `not-http://` are text rather than links.
///
/// The scheme itself is matched case-insensitively, per RFC 3986 §3.1. Byte
/// equality rejected `HTTPS://forge.test/1`, which Discord renders as a link
/// like any other. Case-insensitivity does not relax the boundary rule.
fn scheme_at(bytes: &[u8], index: usize) -> Option<usize> {
    let rest = &bytes[index..];
    // Longest first: `https://` also has `http` as a prefix, but not `http://`.
    let length = if rest.len() >= 8 && rest[..8].eq_ignore_ascii_case(b"https://") {
        "https://".len()
    } else if rest.len() >= 7 && rest[..7].eq_ignore_ascii_case(b"http://") {
        "http://".len()
    } else {
        return None;
    };
    let boundary = index == 0
        || !matches!(bytes[index - 1], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'+' | b'-' | b'.');
    boundary.then_some(index + length)
}

/// Accumulates findings under a hard cap, without building the ones it would
/// discard.
///
/// The cap must apply during collection rather than after it: this hook sees
/// MCP text before Discord chunking, so truncating a fully materialised vector
/// still lets peak work scale with unbounded input.
struct Findings {
    items: Vec<Assessment>,
    omitted: usize,
}

impl Findings {
    fn new() -> Self {
        Self {
            items: Vec::new(),
            omitted: 0,
        }
    }

    /// `make` is only invoked when there is room, so the detail string of a
    /// discarded finding is never allocated.
    fn push(&mut self, make: impl FnOnce() -> Assessment) {
        if self.items.len() < StatusPacketLint::MAX_FINDINGS {
            self.items.push(make());
        } else {
            self.omitted += 1;
        }
    }

    fn finish(mut self) -> Vec<Assessment> {
        if self.omitted > 0 {
            let omitted = self.omitted;
            self.items.push(Assessment::new(
                category::TRUNCATED,
                1.0,
                format!(
                    "{omitted} further finding(s) omitted past the cap of {}",
                    StatusPacketLint::MAX_FINDINGS
                ),
            ));
        }
        self.items
    }
}

/// Strips list bullets and emphasis so a marker at the visual start of a line
/// is recognised as such.
fn item_body(line: &str) -> &str {
    let mut rest = line.trim_start();
    loop {
        let stripped = rest
            .strip_prefix("- ")
            .or_else(|| rest.strip_prefix("* "))
            .or_else(|| rest.strip_prefix("**"))
            .or_else(|| rest.strip_prefix('#'))
            .map(str::trim_start);
        match stripped {
            Some(next) if next != rest => rest = next,
            _ => return rest,
        }
    }
}

/// True when the line's first meaningful character is a marker.
fn is_item_line(masked: &str) -> bool {
    item_body(masked)
        .chars()
        .next()
        .is_some_and(|first| MARKERS.contains(&first))
}

/// Content after the leading marker and any variation selector.
fn after_marker(masked: &str) -> &str {
    let body = item_body(masked);
    let mut chars = body.char_indices();
    chars.next();
    chars.as_str().trim_start_matches('\u{FE0F}').trim_start()
}

fn marker_count(masked: &str) -> usize {
    masked.chars().filter(|c| MARKERS.contains(c)).count()
}

fn is_short_sha(candidate: &str) -> bool {
    candidate.chars().any(|c| c.is_ascii_alphabetic())
}

/// True when a `#123` match is already qualified by a repository, as in
/// `owner/repo#123`, which is not opaque.
fn is_qualified_reference(masked: &str, start: usize) -> bool {
    masked[..start]
        .chars()
        .next_back()
        .is_some_and(|previous| previous.is_alphanumeric() || previous == '/')
}

/// The status-packet convention lint.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct StatusPacketLint;

impl StatusPacketLint {
    pub fn new() -> Self {
        Self
    }

    /// Upper bound on emitted findings. The hook sees MCP text before Discord
    /// chunking, so a pathological message must not become a pathological
    /// number of assessments or log events.
    const MAX_FINDINGS: usize = 40;

    fn hook_name() -> HookName {
        HookName::parse("status-packet-lint").expect("hook name is a valid identifier")
    }

    /// Collects findings for one message. Empty when the message is not a
    /// status packet, or is a clean one.
    ///
    /// Details carry the category and the line, never the matched value. The
    /// audit trail is an optional durable sink at info level, and copying
    /// snowflakes or SHAs into it would replicate message fragments there for
    /// no diagnostic gain — the line number already locates the finding.
    fn findings(text: &str) -> Vec<Assessment> {
        let (lines, input_truncated) = scan(text);
        let eligible: Vec<&Line> = lines.iter().filter(|line| line.eligible).collect();
        let items: Vec<&&Line> = eligible
            .iter()
            .filter(|line| is_item_line(&line.masked))
            .collect();
        if items.is_empty() {
            return Vec::new();
        }

        let mut findings = Findings::new();
        if input_truncated {
            findings.push(|| {
                Assessment::new(
                    category::INPUT_TRUNCATED,
                    1.0,
                    format!(
                        "input exceeded the scan ceiling ({MAX_SCAN_LINES} lines / {MAX_SCAN_BYTES} bytes); \
                         findings cover the prefix only"
                    ),
                )
            });
        }

        // Rule 1 — opaque identifiers anywhere in the packet's prose. Scanning
        // unmarked lines is intentional: the rule is about the packet, not
        // about its marked lines.
        for line in &eligible {
            let masked = &line.masked;
            let number = line.number;
            for _ in SNOWFLAKE.find_iter(masked) {
                findings.push(|| {
                    Assessment::new(
                        category::OPAQUE_ID,
                        0.9,
                        format!("line {number}: bare snowflake in prose — link it or drop it"),
                    )
                });
            }
            for hit in HEXISH.find_iter(masked) {
                if is_short_sha(hit.as_str()) {
                    findings.push(|| {
                        Assessment::new(
                            category::OPAQUE_ID,
                            0.6,
                            format!("line {number}: bare short SHA — link it or put it in code"),
                        )
                    });
                }
            }
            for hit in HASH_NUMBER.find_iter(masked) {
                if !is_qualified_reference(masked, hit.start()) {
                    findings.push(|| {
                        Assessment::new(
                            category::OPAQUE_ID,
                            0.7,
                            format!(
                                "line {number}: bare issue number — qualify it as owner/repo#n or link it"
                            ),
                        )
                    });
                }
            }
        }

        for item in &items {
            let masked = &item.masked;
            let number = item.number;
            let body = after_marker(masked);

            let count = marker_count(masked);
            if count > 1 {
                findings.push(|| {
                    Assessment::new(
                        category::MULTIPLE_MARKERS,
                        0.9,
                        format!(
                            "line {number}: {count} markers on one item — each item carries exactly one"
                        ),
                    )
                });
            }

            // Both halves of this decision read the structural view: the
            // artifact from `masked`, the link from `has_visible_url`. Reading
            // the URL off the raw line let a URL inside backticks — example
            // syntax, not a clickable link — satisfy the rule.
            let names_artifact = ARTIFACT_WORD.is_match(masked) || HASH_NUMBER.is_match(masked);
            if names_artifact && !item.has_visible_url {
                findings.push(|| {
                    Assessment::new(
                        category::MISSING_LINK,
                        0.6,
                        format!(
                            "line {number}: names an artifact but carries no link — include the URL"
                        ),
                    )
                });
            }

            if !body.starts_with("**") {
                findings.push(|| {
                    Assessment::new(
                        category::MISSING_SUBJECT,
                        0.5,
                        format!(
                            "line {number}: no bolded subject after the marker — state + subject should read in one glance"
                        ),
                    )
                });
            }

            if TZ_SUFFIX.is_match(masked) {
                findings.push(|| {
                    Assessment::new(
                        category::TIMEZONE_SUFFIX,
                        0.7,
                        format!(
                            "line {number}: a time carries a timezone suffix — Pacific is the house default"
                        ),
                    )
                });
            }

            if STEWARD_WORD.is_match(masked) {
                findings.push(|| {
                    Assessment::new(
                        category::STEWARD,
                        0.8,
                        format!("line {number}: say \"Lead\", not \"Steward\""),
                    )
                });
            }
        }

        // Rule 3 — no report-level marker trailing a multi-item packet.
        if items.len() >= 2
            && let Some(last) = eligible.last()
            && is_item_line(&last.masked)
            && items.iter().any(|item| item.number < last.number)
        {
            let body = after_marker(&last.masked);
            let number = last.number;
            if body.split_whitespace().count() <= 2 && !body.contains("**") {
                findings.push(|| {
                    Assessment::new(
                        category::TRAILING_MARKER,
                        0.7,
                        format!(
                            "line {number}: report-level marker trailing a multi-item packet — each item carries its own"
                        ),
                    )
                });
            }
        }

        findings.finish()
    }
}

impl PreSendHook for StatusPacketLint {
    fn name(&self) -> HookName {
        Self::hook_name()
    }

    fn execute(&self, context: &HookContext) -> HookOutput {
        let findings = Self::findings(context.text());
        if findings.is_empty() {
            return HookOutput::new(
                HookDecision::Continue,
                ConstructFeedback::default(),
                AuditTrail::default(),
            );
        }

        // Findings go to BOTH streams, deliberately.
        //
        // `PreSendPipeline::run` appends `ConstructFeedback` only under
        // `PipelineMode::Enforce`; in Observe it keeps the audit trail alone.
        // `observe_pipeline` hardcodes Observe, so a hook that reported only
        // through the construct stream would run in production and surface
        // nothing — findings computed, findings dropped.
        //
        // The audit trail is what carries them today. The construct stream is
        // populated so the hook stays correct if Enforce is ever switched on,
        // rather than needing a second change at the moment enforcement starts.
        let mut summary = vec![Assessment::new(
            category::SUMMARY,
            1.0,
            format!("{} status-lint finding(s)", findings.len()),
        )];
        summary.extend(findings.iter().cloned());
        HookOutput::new(
            HookDecision::Continue,
            ConstructFeedback::new(findings),
            AuditTrail::new(summary),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pre_send::{ChannelType, ConstructId, OutboundDestination};
    use serenity::model::id::ChannelId;

    fn categories(text: &str) -> Vec<String> {
        StatusPacketLint::findings(text)
            .into_iter()
            .map(|finding| finding.category().to_owned())
            .collect()
    }

    fn context(text: &str) -> HookContext {
        HookContext::new(
            text,
            OutboundDestination::Channel(ChannelId::new(1)),
            ChannelType::Public,
            ConstructId::default(),
        )
    }

    #[test]
    fn ordinary_prose_is_not_a_status_packet() {
        assert!(categories("just a message about the weather").is_empty());
        assert!(categories("bare snowflake 1542814375424032792 in plain prose").is_empty());
    }

    #[test]
    fn a_clean_single_item_packet_is_silent() {
        assert!(categories("\u{25B6}\u{FE0F} **status-packet lint**").is_empty());
    }

    #[test]
    fn a_clean_multi_item_packet_is_silent() {
        let text = "\u{26A0}\u{FE0F} **dione** blocked on review\n\
                    \u{25B6}\u{FE0F} **status lint** building now";
        assert!(categories(text).is_empty(), "{:?}", categories(text));
    }

    #[test]
    fn a_marker_mid_sentence_is_a_mention_not_an_item() {
        let text = "the \u{25B6}\u{FE0F} marker means I am actively moving it";
        assert!(categories(text).is_empty());
    }

    #[test]
    fn markers_inside_a_fence_do_not_activate_the_lint() {
        // Mixed on purpose: the fenced marker must be exempt AND the real
        // item after the fence must still be found. An empty-only assertion
        // passes just as well on a lint that has stopped working entirely.
        let text = "here is the convention:\n\
                    ```\n\
                    \u{25B6}\u{FE0F} on it\n\
                    ```\n\
                    \u{26A0}\u{FE0F} **real item** blocked on review";
        assert_eq!(categories(text), Vec::<String>::new());
        let broken = "```\n\u{25B6}\u{FE0F} on it\n```";
        assert!(categories(broken).is_empty());
    }

    #[test]
    fn markers_inside_a_blockquote_do_not_activate_the_lint() {
        let text = "> \u{25B6}\u{FE0F} on it\n\u{26A0}\u{FE0F} **real item** blocked on review";
        assert_eq!(categories(text), Vec::<String>::new());
    }

    #[test]
    fn bare_snowflake_in_a_packet_is_flagged() {
        let text = "\u{25B6}\u{FE0F} **thing** see 1542814375424032792";
        assert!(categories(text).contains(&category::OPAQUE_ID.to_owned()));
    }

    #[test]
    fn a_snowflake_in_code_or_a_url_is_exempt() {
        let coded = "\u{25B6}\u{FE0F} **thing** see `1542814375424032792`";
        assert!(!categories(coded).contains(&category::OPAQUE_ID.to_owned()));
        let linked = "\u{25B6}\u{FE0F} **thing** https://discord.com/c/1542814375424032792";
        assert!(!categories(linked).contains(&category::OPAQUE_ID.to_owned()));
    }

    #[test]
    fn a_bare_short_sha_is_flagged_but_a_plain_number_is_not() {
        let sha = "\u{25B6}\u{FE0F} **thing** at 32fbb09";
        assert!(categories(sha).contains(&category::OPAQUE_ID.to_owned()));
        let digits = "\u{25B6}\u{FE0F} **thing** at 12345678";
        assert!(!categories(digits).contains(&category::OPAQUE_ID.to_owned()));
    }

    #[test]
    fn a_qualified_reference_is_not_opaque() {
        let bare = "\u{25B6}\u{FE0F} **thing** fixes #375";
        assert!(categories(bare).contains(&category::OPAQUE_ID.to_owned()));
        let qualified = "\u{25B6}\u{FE0F} **thing** fixes lacuna/dione#375 https://example.test/1";
        assert!(!categories(qualified).contains(&category::OPAQUE_ID.to_owned()));
    }

    #[test]
    fn two_markers_on_one_item_are_flagged() {
        let text = "\u{25B6}\u{FE0F} **thing** and also \u{26A0}\u{FE0F} blocked";
        assert!(categories(text).contains(&category::MULTIPLE_MARKERS.to_owned()));
    }

    #[test]
    fn an_artifact_without_a_link_is_flagged_and_with_one_is_not() {
        let unlinked = "\u{26A0}\u{FE0F} **dione** blocked on lacuna/dione#375";
        assert!(categories(unlinked).contains(&category::MISSING_LINK.to_owned()));
        let linked = "\u{26A0}\u{FE0F} **dione** blocked on https://forge.test/dione/pulls/375";
        assert!(!categories(linked).contains(&category::MISSING_LINK.to_owned()));
    }

    #[test]
    fn a_missing_bold_subject_is_flagged() {
        let text = "\u{26A0}\u{FE0F} temporarily blocked on the review";
        assert!(categories(text).contains(&category::MISSING_SUBJECT.to_owned()));
    }

    #[test]
    fn a_timezone_suffix_is_flagged() {
        let text = "\u{25B6}\u{FE0F} **thing** started 9:17 PM PT";
        assert!(categories(text).contains(&category::TIMEZONE_SUFFIX.to_owned()));
    }

    #[test]
    fn steward_is_flagged() {
        let text = "\u{25B6}\u{FE0F} **thing** Steward: me";
        assert!(categories(text).contains(&category::STEWARD.to_owned()));
    }

    #[test]
    fn a_bare_trailing_marker_after_a_multi_item_packet_is_flagged() {
        let text = "\u{26A0}\u{FE0F} **one** waiting\n\
                    \u{26A0}\u{FE0F} **two** waiting\n\
                    \u{25B6}\u{FE0F} overall";
        assert!(categories(text).contains(&category::TRAILING_MARKER.to_owned()));
    }

    #[test]
    fn a_final_item_with_its_own_subject_is_not_a_trailing_marker() {
        let text = "\u{26A0}\u{FE0F} **one** waiting\n\
                    \u{26A0}\u{FE0F} **two** waiting\n\
                    \u{25B6}\u{FE0F} **three** moving";
        assert!(!categories(text).contains(&category::TRAILING_MARKER.to_owned()));
    }

    #[test]
    fn a_single_item_packet_never_trips_the_trailing_rule() {
        let text = "\u{25B6}\u{FE0F} on it";
        assert!(!categories(text).contains(&category::TRAILING_MARKER.to_owned()));
    }

    /// The output a silent hook produces. Compared against rather than
    /// reconstructed from `findings`, so the assertions cannot pass by
    /// agreeing with the code under test.
    fn silent_output() -> HookOutput {
        HookOutput::new(
            HookDecision::Continue,
            ConstructFeedback::default(),
            AuditTrail::default(),
        )
    }

    #[test]
    fn a_clean_message_produces_no_assessments_at_all() {
        let output = StatusPacketLint.execute(&context("\u{25B6}\u{FE0F} **clean**"));
        assert_eq!(output, silent_output());
    }

    #[test]
    fn non_status_prose_produces_no_assessments_at_all() {
        let output = StatusPacketLint.execute(&context("a thought about nothing in particular"));
        assert_eq!(output, silent_output());
    }

    #[test]
    fn a_dirty_message_reports_rather_than_staying_silent() {
        let output = StatusPacketLint.execute(&context("\u{26A0}\u{FE0F} blocked on #375"));
        assert_ne!(output, silent_output());
    }

    #[test]
    fn the_hook_name_is_a_valid_identifier() {
        assert_eq!(StatusPacketLint.name().as_str(), "status-packet-lint");
    }

    // ── Exemption cases.
    //
    // A marker or identifier inside inline code, a fence, or a quote is
    // example syntax and is not an asserted work state, so no rule may fire
    // on it. Where the shape allows, each case keeps a real finding alongside
    // the exempt one: an assertion that only checks for emptiness passes just
    // as well on a lint that has stopped working entirely.
    #[test]
    fn probe_double_backtick_code_is_exempt() {
        let text = "\u{25B6}\u{FE0F} **thing** see ``#375``";
        assert!(
            !categories(text).contains(&category::OPAQUE_ID.to_owned()),
            "{:?}",
            categories(text)
        );
    }

    #[test]
    fn probe_marker_inside_inline_code_is_not_a_second_marker() {
        let text = "\u{25B6}\u{FE0F} **thing** the `\u{26A0}\u{FE0F}` marker means blocked";
        assert!(
            !categories(text).contains(&category::MULTIPLE_MARKERS.to_owned()),
            "{:?}",
            categories(text)
        );
    }

    #[test]
    fn probe_discord_triple_quote_block_is_exempt() {
        // Discord's `>>>` quotes every following line, not just its own. The
        // marker here is on a CONTINUATION line carrying no `>` of its own —
        // which is the case Ari reported. An earlier version of this probe put
        // the marker on the `>>>` line itself and passed for the wrong reason.
        let text = ">>> quoting the convention below\n\u{25B6}\u{FE0F} on it";
        assert!(categories(text).is_empty(), "{:?}", categories(text));
    }

    #[test]
    fn probe_nested_fence_does_not_close_the_outer_one() {
        let text = "````\n```\n\u{25B6}\u{FE0F} on it\n```\n````\nafter";
        assert!(categories(text).is_empty(), "{:?}", categories(text));
    }

    #[test]
    fn probe_stewardship_is_not_the_steward_convention() {
        let text = "\u{25B6}\u{FE0F} **thing** notes on stewardship of the repo";
        assert!(
            !categories(text).contains(&category::STEWARD.to_owned()),
            "{:?}",
            categories(text)
        );
    }

    /// Pins the exact assessments for one input, in both streams, using
    /// literal category and confidence values rather than the implementation's
    /// own constants.
    ///
    /// The earlier `categories()`-based assertions shared an oracle with the
    /// code under test: they compared against `category::*`, discarded
    /// confidence entirely, and proved only that *some* stream was non-empty —
    /// so deleting the construct stream left the suite green. Ari caught that.
    #[test]
    fn the_output_pins_both_streams_with_literal_values() {
        let text = "\u{26A0}\u{FE0F} blocked on #375";
        let expected = vec![
            Assessment::new(
                "status-lint/opaque-id",
                0.7,
                "line 1: bare issue number — qualify it as owner/repo#n or link it",
            ),
            Assessment::new(
                "status-lint/missing-link",
                0.6,
                "line 1: names an artifact but carries no link — include the URL",
            ),
            Assessment::new(
                "status-lint/missing-bold-subject",
                0.5,
                "line 1: no bolded subject after the marker — state + subject should read in one glance",
            ),
        ];
        let mut audit = vec![Assessment::new(
            "status-lint/summary",
            1.0,
            "3 status-lint finding(s)",
        )];
        audit.extend(expected.iter().cloned());

        assert_eq!(
            StatusPacketLint.execute(&context(text)),
            HookOutput::new(
                HookDecision::Continue,
                ConstructFeedback::new(expected),
                AuditTrail::new(audit),
            )
        );
    }

    #[test]
    fn findings_are_capped_and_the_omission_is_reported() {
        let mut text = String::from("\u{25B6}\u{FE0F} **bulk**\n");
        for _ in 0..60 {
            text.push_str("see 1542814375424032792\n");
        }
        let cats = categories(&text);
        assert_eq!(cats.len(), StatusPacketLint::MAX_FINDINGS + 1);
        assert_eq!(cats.last().map(String::as_str), Some(category::TRUNCATED));
    }

    #[test]
    fn details_never_carry_the_matched_identifier() {
        let text = "\u{25B6}\u{FE0F} **thing** see 1542814375424032792 at 32fbb09 fixes #375";
        for finding in StatusPacketLint::findings(text) {
            let detail = finding.detail();
            assert!(!detail.contains("1542814375424032792"), "{detail}");
            assert!(!detail.contains("32fbb09"), "{detail}");
            assert!(!detail.contains("#375"), "{detail}");
        }
    }

    // ── Composition cases.
    //
    // Excluded syntax must not mutate the state that decides later content: a
    // fence delimiter, an indented line, or a quote may not leave an inline
    // span open across the item after it. The classification these rest on
    // lives in `markdown::BlockScanner` and is shared with evidence parsing,
    // so a failure here may be a change to that contract rather than to this
    // module.
    #[test]
    fn probe_triple_quote_inside_a_fence_does_not_silence_the_rest() {
        let text = "```\n>>> quoted inside the fence\n```\n\u{26A0}\u{FE0F} **real item** blocked";
        assert_eq!(categories(text), Vec::<String>::new());
        // and the item after the fence is genuinely seen:
        let dirty = "```\n>>> quoted inside the fence\n```\n\u{26A0}\u{FE0F} blocked on #375";
        assert!(categories(dirty).contains(&category::MISSING_SUBJECT.to_owned()));
    }

    #[test]
    fn probe_four_space_indented_backticks_are_content_not_a_fence() {
        // Four-space indentation is indented code to CommonMark, so this does
        // not open a fence and the item below stays visible.
        let text = "    ```\n\u{26A0}\u{FE0F} blocked on #375";
        assert!(categories(text).contains(&category::MISSING_SUBJECT.to_owned()));
    }

    #[test]
    fn probe_a_closing_fence_with_a_tag_does_not_close() {
        // Only a bare delimiter closes. `​```rust` opening, then ```` ```text ````
        // must NOT close, so the marker stays fenced.
        let text = "```rust\n\u{25B6}\u{FE0F} on it\n```text\n\u{25B6}\u{FE0F} still fenced\n```";
        assert!(categories(text).is_empty());
    }

    #[test]
    fn probe_a_url_in_inline_code_is_not_a_link() {
        let coded = "\u{25B6}\u{FE0F} **thing** PR #375 `https://forge.test/p/375`";
        assert!(categories(coded).contains(&category::MISSING_LINK.to_owned()));
        let real = "\u{25B6}\u{FE0F} **thing** PR #375 https://forge.test/p/375";
        assert!(!categories(real).contains(&category::MISSING_LINK.to_owned()));
    }

    #[test]
    fn the_cross_marker_activates_the_lint() {
        let text = "\u{274C} blocked on #375";
        assert!(categories(text).contains(&category::MISSING_SUBJECT.to_owned()));
    }

    /// The literal oracle, across every rule and both remaining markers.
    /// Confidences are written out rather than referenced, so a change to an
    /// implementation constant fails here instead of agreeing with itself.
    #[test]
    fn every_rule_pins_a_literal_category_and_confidence() {
        let pairs = |text: &str| -> Vec<(String, f32)> {
            StatusPacketLint::findings(text)
                .into_iter()
                .map(|f| (f.category().to_owned(), f.confidence()))
                .collect()
        };

        assert_eq!(
            pairs("\u{25B6}\u{FE0F} **a** see 1542814375424032792"),
            vec![("status-lint/opaque-id".to_owned(), 0.9)]
        );
        assert_eq!(
            pairs("\u{25B6}\u{FE0F} **a** at 32fbb09"),
            vec![("status-lint/opaque-id".to_owned(), 0.6)]
        );
        assert_eq!(
            pairs("\u{25B6}\u{FE0F} **a** and also \u{26A0}\u{FE0F} blocked"),
            vec![("status-lint/multiple-markers".to_owned(), 0.9)]
        );
        assert_eq!(
            pairs("\u{25B6}\u{FE0F} **a** started 9:17 PM PT"),
            vec![("status-lint/timezone-suffix".to_owned(), 0.7)]
        );
        assert_eq!(
            pairs("\u{25B6}\u{FE0F} **a** Steward: me"),
            vec![("status-lint/steward".to_owned(), 0.8)]
        );
        assert_eq!(
            pairs(
                "\u{26A0}\u{FE0F} **one** x\n\u{26A0}\u{FE0F} **two** y\n\u{25B6}\u{FE0F} overall"
            ),
            // The bare trailer also lacks a bolded subject, so both rules fire
            // on it. Pinning only the one I was thinking about would have been
            // the same partial-oracle mistake one level down.
            vec![
                ("status-lint/missing-bold-subject".to_owned(), 0.5),
                ("status-lint/trailing-marker".to_owned(), 0.7),
            ]
        );
    }

    #[test]
    fn the_cap_does_not_build_the_findings_it_discards() {
        // Behavioural proxy for "capped during collection": the returned set
        // is bounded and the omission is counted. The allocation guarantee
        // lives in `Findings::push`, which only calls `make` when under cap.
        let mut text = String::from("\u{25B6}\u{FE0F} **bulk**\n");
        for _ in 0..60 {
            text.push_str("see 1542814375424032792\n");
        }
        let cats = categories(&text);
        assert_eq!(cats.len(), StatusPacketLint::MAX_FINDINGS + 1);
        assert_eq!(cats.last().map(String::as_str), Some(category::TRUNCATED));
        let truncated = StatusPacketLint::findings(&text);
        assert!(
            truncated
                .last()
                .unwrap()
                .detail()
                .contains("20 further finding(s)")
        );
    }

    #[test]
    fn six_digit_artifact_references_are_not_exempt() {
        let bare = "\u{25B6}\u{FE0F} **a** fixes #123456";
        assert!(categories(bare).contains(&category::OPAQUE_ID.to_owned()));
        assert!(categories(bare).contains(&category::MISSING_LINK.to_owned()));
        let qualified = "\u{25B6}\u{FE0F} **a** fixes lacuna/dione#123456 https://forge.test/1";
        assert!(!categories(qualified).contains(&category::OPAQUE_ID.to_owned()));
        assert!(!categories(qualified).contains(&category::MISSING_LINK.to_owned()));
    }

    /// Proves the cap stops *building* findings, not merely returning them.
    /// The former collect-all-then-truncate implementation produced identical
    /// output, so an output-only assertion left that regression green.
    #[test]
    fn the_cap_stops_invoking_the_builder_at_the_limit() {
        let calls = std::cell::Cell::new(0usize);
        let mut findings = Findings::new();
        for _ in 0..60 {
            findings.push(|| {
                calls.set(calls.get() + 1);
                Assessment::new(category::OPAQUE_ID, 0.9, "detail")
            });
        }
        assert_eq!(calls.get(), StatusPacketLint::MAX_FINDINGS);
        let out = findings.finish();
        assert_eq!(out.len(), StatusPacketLint::MAX_FINDINGS + 1);
        assert_eq!(out.last().unwrap().category(), category::TRUNCATED);
    }

    #[test]
    fn every_supported_markdown_prefix_activates_the_lint() {
        for prefix in ["", "- ", "* ", "#", "**"] {
            let text = format!("{prefix}\u{26A0}\u{FE0F} blocked on #375");
            let cats = categories(&text);
            assert!(
                cats.contains(&category::MISSING_SUBJECT.to_owned()),
                "prefix {prefix:?} did not activate: {cats:?}"
            );
        }
    }

    #[test]
    fn the_scan_is_bounded_and_says_so() {
        let mut text = String::from("\u{26A0}\u{FE0F} **head** blocked\n");
        for _ in 0..(MAX_SCAN_LINES + 200) {
            text.push_str("filler\n");
        }
        let (lines, truncated) = scan(&text);
        assert!(truncated);
        assert!(lines.len() <= MAX_SCAN_LINES);
        assert!(categories(&text).contains(&category::INPUT_TRUNCATED.to_owned()));
    }

    #[test]
    fn a_byte_heavy_single_line_also_trips_the_ceiling() {
        let text = format!(
            "\u{26A0}\u{FE0F} **head** blocked\n{}\n{}",
            "x".repeat(MAX_SCAN_BYTES),
            "\u{25B6}\u{FE0F} **tail** moving"
        );
        let (_, truncated) = scan(&text);
        assert!(truncated);
    }

    #[test]
    fn an_ordinary_packet_is_never_reported_as_truncated() {
        let text = "\u{26A0}\u{FE0F} **one** blocked\n\u{25B6}\u{FE0F} **two** moving";
        let (_, truncated) = scan(text);
        assert!(!truncated);
        assert!(!categories(text).contains(&category::INPUT_TRUNCATED.to_owned()));
    }

    #[test]
    fn probe_a_multiline_code_span_stays_code_on_its_continuation() {
        // The span opens on line 2 and closes on line 4; the marker on line 3
        // is inside it and must not become an item.
        let text =
            "\u{26A0}\u{FE0F} **head** blocked\nsee `start\n\u{25B6}\u{FE0F} not an item\nend`";
        let cats = categories(text);
        assert!(
            !cats.contains(&category::MISSING_SUBJECT.to_owned()),
            "{cats:?}"
        );
    }

    #[test]
    fn probe_an_escaped_backtick_does_not_open_a_span() {
        // The lone escaped backtick must not swallow the rest of the line,
        // so the bare issue number after it is still audited.
        let text = "\u{25B6}\u{FE0F} **a** a literal \\` then fixes #375";
        assert!(categories(text).contains(&category::OPAQUE_ID.to_owned()));
    }

    // ---- excluded syntax must not mutate the state that decides content ----
    //
    // Three shapes, one class. Each keeps a *dirty real item* after the
    // excluded syntax, so a lint that has simply stopped working fails these
    // as surely as one that mis-composes its Markdown states.

    #[test]
    fn a_wider_closing_fence_does_not_swallow_the_next_real_item() {
        // A 3-backtick opener with a 4-backtick closer: `advance_fence`
        // accepted the close on width, an equal-width inline delimiter did
        // not, and the span left open masked the whole next line.
        let text = "```\ncode\n````\n\u{26A0}\u{FE0F} blocked on #375";
        let cats = categories(text);
        assert!(
            cats.contains(&category::OPAQUE_ID.to_owned()),
            "real item after the fence went missing: {cats:?}"
        );
        assert!(
            cats.contains(&category::MISSING_SUBJECT.to_owned()),
            "{cats:?}"
        );
    }

    #[test]
    fn an_unmatched_backtick_on_a_fence_line_does_not_leak() {
        // The opener carries a tag containing a backtick. It is delimiter
        // syntax, so nothing in it may open a span.
        let text = "``` `weird\ncode\n```\n\u{26A0}\u{FE0F} blocked on #375";
        assert!(
            categories(text).contains(&category::OPAQUE_ID.to_owned()),
            "{:?}",
            categories(text)
        );
    }

    #[test]
    fn an_indented_example_is_not_a_status_packet() {
        // Ari's counterexample verbatim in shape: `item_body` trimmed the
        // indentation back to a marker, so the example activated the lint,
        // was faulted for its own `#375`, and was then told it carried no
        // link — while carrying a visible one.
        let text = "how to write one:\n\n    \u{25B6}\u{FE0F} **example** PR #375 https://forge.test/375\n";
        assert_eq!(categories(text), Vec::<String>::new());
    }

    #[test]
    fn an_indented_example_does_not_mask_a_later_real_item() {
        let text = "how to write one:\n\n    \u{25B6}\u{FE0F} **example** PR #375 https://forge.test/375\n\n\u{26A0}\u{FE0F} blocked on #400";
        let cats = categories(text);
        let opaque = cats
            .iter()
            .filter(|category| *category == category::OPAQUE_ID)
            .count();
        assert_eq!(
            opaque, 1,
            "expected only the real item's #400 to be audited: {cats:?}"
        );
        assert!(
            cats.contains(&category::MISSING_SUBJECT.to_owned()),
            "{cats:?}"
        );
    }

    #[test]
    fn an_indented_line_continuing_a_paragraph_is_still_prose() {
        // CommonMark: with no blank line before it, the indented line is a
        // lazy continuation, not code. Pinned so the indented-code fix above
        // cannot be widened into "any indented line is exempt".
        let text = "\u{26A0}\u{FE0F} **head** blocked\n    and it mentions #375\n";
        assert!(
            categories(text).contains(&category::OPAQUE_ID.to_owned()),
            "{:?}",
            categories(text)
        );
    }

    /// The third shape Ari named. A quoted example must stay exempt *and* must
    /// not reach forward: its unterminated backtick cannot mask the asserted
    /// status line after it.
    ///
    /// The lint drives `BlockScanner::block_scoped` for this; evidence keeps
    /// `AcrossBlocks` and its fail-closed behaviour is unchanged. If this
    /// regresses, check which constructor the lint is using before looking
    /// anywhere else.
    #[test]
    fn a_quote_line_span_does_not_mask_the_next_real_item() {
        let text = "> quoted `unclosed\n\u{26A0}\u{FE0F} blocked on #375";
        let cats = categories(text);
        assert!(
            cats.contains(&category::OPAQUE_ID.to_owned()),
            "the quoted backtick masked the following real item: {cats:?}"
        );
        assert!(
            cats.contains(&category::MISSING_SUBJECT.to_owned()),
            "{cats:?}"
        );
    }

    #[test]
    fn a_quoted_example_is_still_exempt_from_the_lint() {
        // The other half of the contract: scoping the span must not have made
        // the quote itself auditable.
        let text = "> \u{25B6}\u{FE0F} **example** PR #375\nplain prose after";
        assert_eq!(categories(text), Vec::<String>::new());
    }

    // ---- scan ceilings, pinned to literals ----
    //
    // These read the production constants only to assert what they are.
    // Generating fixtures from `MAX_SCAN_*` made the oracle move with the
    // implementation, so raising a ceiling silently raised the test with it.

    /// The missing-link rule is only as good as what counts as a link.
    /// A scheme has to start at a token boundary and carry an authority.
    #[test]
    fn only_a_real_url_satisfies_the_missing_link_rule() {
        // Malformed: each of these names an artifact and must still be told
        // it carries no link.
        for suffix in [
            "xhttp://forge.test/1",
            "nothttp://forge.test/1",
            "not-http://forge.test/1",
            "v1.http://forge.test/1",
            "http://",
            "https://",
            "http:// forge.test",
            "https:///",
            // Punctuation-only pseudo-authorities. Each is a legal URL
            // *component* separator, so a "non-empty byte" check waved them
            // through as clickable links.
            "http://#fragment",
            "http://?query",
            "http://)",
            "https://#",
            "http://-notahost",
            // Bracket hosts. A first-byte check accepts `[` and never looks
            // for the closing bracket or anything between them, so an
            // unterminated IPv6 literal read as a clickable link.
            "http://[",
            "http://[garbage",
            "http://[]/x",
            "http://[2001:db8::1/x",
            // Ports. `forge.test:` is a well-formed authority prefix, so the
            // first byte says nothing about whether the port is legal.
            "http://forge.test:99999/x",
            "http://forge.test:abc/x",
            // Userinfo. The house host policy was reading the first byte of the
            // *authority*, which is the userinfo when one is present — so `x@`
            // smuggled a rejected host past a rule written to reject it. The
            // policy belongs on the parsed host, not on a raw offset.
            "http://x@)",
            "http://x@-notahost",
            "http://user:pass@)",
        ] {
            let text = format!("\u{25B6}\u{FE0F} **a** fixes #375 {suffix}");
            assert!(
                categories(&text).contains(&category::MISSING_LINK.to_owned()),
                "{suffix:?} was accepted as a link"
            );
        }

        // Real: each of these is a link and must silence the rule.
        for suffix in [
            "https://forge.test/1",
            "http://forge.test/1",
            "(https://forge.test/1)",
            "see https://forge.test/1.",
            "https://forge.test",
            "https://192.0.2.1/x",
            "http://[2001:db8::1]/x",
            "see (https://forge.test/1), then",
            // URI schemes are case-insensitive (RFC 3986 §3.1), and Discord
            // renders these as links. A byte-equality check rejected every
            // one of them.
            "HTTPS://forge.test/1",
            "HtTpS://forge.test/1",
            "HTTP://forge.test/1",
            "HTTP://[2001:db8::1]/x",
            "http://forge.test:8443/x",
            // Valid userinfo control: the policy must reject the host behind
            // `@`, not the presence of userinfo itself.
            "http://x@forge.test/1",
            "https://user:pass@forge.test/1",
        ] {
            let text = format!("\u{25B6}\u{FE0F} **a** fixes #375 {suffix}");
            assert!(
                !categories(&text).contains(&category::MISSING_LINK.to_owned()),
                "{suffix:?} was rejected as a link"
            );
        }
    }

    /// The work bound is a *count*, not a stopwatch.
    ///
    /// A whitespace-free run of rejected candidates used to restart the walk
    /// just past each scheme, so every restart rescanned and reparsed the whole
    /// remaining tail. At the byte ceiling that is ~16K overlapping parses over
    /// ~1.07 GB — a finite input, but not the bounded work the ceiling claims
    /// to buy.
    ///
    /// One whitespace-delimited token may cost at most one parse. A wall-clock
    /// assertion would be flaky and would not localise the regression; this one
    /// names the mechanism directly.
    #[test]
    fn a_whitespace_free_run_of_rejected_candidates_costs_one_parse() {
        let hostile = "http://[".repeat(MAX_SCAN_BYTES / "http://[".len());
        let mut bytes = hostile.into_bytes();
        let (found, attempts) = mask_urls(&mut bytes);

        assert!(!found, "a run of `http://[` is not a link");
        assert_eq!(
            attempts, 1,
            "one whitespace-free token must cost one parse attempt, not {attempts}"
        );
    }

    /// The banked cost of "one parse per token", pinned so it cannot regress
    /// silently in either direction.
    ///
    /// A second scheme inside the *same* token is no longer recovered. Under
    /// the old walk this string cost two parses and reported a link; it now
    /// costs one and reports none. That is a deliberate trade — the input is
    /// adversarial, Discord does not render it as a link either, and the only
    /// consequence is an Observe-mode warning — but it is a real behaviour
    /// change and it gets a test rather than a sentence.
    #[test]
    fn a_second_scheme_inside_one_token_is_banked_not_recovered() {
        let mut bytes = b"http://[http://forge.test/1".to_vec();
        let (found, attempts) = mask_urls(&mut bytes);

        assert_eq!(attempts, 1, "one token, one parse — got {attempts}");
        assert!(
            !found,
            "the nested link is deliberately not recovered; see the doc comment"
        );
    }

    /// Every token is still visited — the bound is per token, not a stop after
    /// the first failure.
    ///
    /// Note this one does NOT discriminate the quadratic regression: each token
    /// here is short enough that the old walk also spent three attempts. It is
    /// a companion assertion, not the oracle.
    #[test]
    fn every_scheme_bearing_token_is_still_examined() {
        let mut bytes = "http://[ http://[garbage http://forge.test/1 plain"
            .as_bytes()
            .to_vec();
        let (found, attempts) = mask_urls(&mut bytes);

        assert!(found, "the third token is a real link");
        assert_eq!(attempts, 3, "three scheme-bearing tokens, three attempts");
    }

    #[test]
    fn a_scheme_at_offset_zero_has_no_preceding_byte_to_reject_it() {
        // The boundary check reads `bytes[index - 1]`, so offset 0 is the case
        // that would panic or wrongly reject if the guard were written badly.
        assert_eq!(scheme_at(b"https://forge.test", 0), Some(8));
        assert_eq!(scheme_at(b"http://forge.test", 0), Some(7));
        assert_eq!(scheme_at(b"(https://forge.test", 1), Some(9));
        assert_eq!(scheme_at(b"xhttps://forge.test", 1), None);
        assert_eq!(scheme_at(b"1http://forge.test", 1), None);
        assert_eq!(scheme_at(b"not a scheme", 0), None);

        // RFC 3986 §3.1: the scheme is case-insensitive.
        assert_eq!(scheme_at(b"HTTPS://forge.test", 0), Some(8));
        assert_eq!(scheme_at(b"HtTpS://forge.test", 0), Some(8));
        assert_eq!(scheme_at(b"HTTP://forge.test", 0), Some(7));
        // Case-insensitivity does not relax the token boundary.
        assert_eq!(scheme_at(b"xHTTPS://forge.test", 1), None);
    }

    #[test]
    fn the_scan_ceilings_are_the_documented_numbers() {
        assert_eq!(MAX_SCAN_LINES, 1_000);
        assert_eq!(MAX_SCAN_BYTES, 131_072);
    }

    /// Truncation bounds the input; it does not stop the scan.
    ///
    /// `bounded_prefix` returns its flag before the loop starts, and that flag
    /// was also being read as loop control at the bottom of the body — so any
    /// input over the byte ceiling scanned exactly line 1 and stopped. An item
    /// on line 2 produced no findings and no `input-truncated` assessment, and
    /// the whole packet read as ordinary prose.
    ///
    /// The `scan(&huge).0.len() == 1` assertion below this one could not catch
    /// it: its fixture has no newlines, so one line is the right answer either
    /// way. This fixture puts the item on line 2 on purpose.
    #[test]
    fn a_truncated_packet_is_still_scanned_past_its_first_line() {
        let text = format!(
            "intro line with no marker\n\u{26A0}\u{FE0F} blocked on #375\n{}",
            "x".repeat(200_000)
        );
        let (lines, truncated) = scan(&text);
        assert!(truncated);
        assert!(
            lines.len() >= 2,
            "the scan stopped at the first line: {} retained",
            lines.len()
        );

        let cats = categories(&text);
        assert!(
            cats.contains(&category::OPAQUE_ID.to_owned()),
            "the item on line 2 was never audited: {cats:?}"
        );
        assert!(
            cats.contains(&category::INPUT_TRUNCATED.to_owned()),
            "truncation went unreported: {cats:?}"
        );
    }

    /// The bound has to hold on what `lines()` is *handed*, not on what the
    /// loop keeps. `str::lines` searches forward to a newline to produce each
    /// item, so a newline-free body used to cost O(len) before any per-line
    /// ceiling could fire — the retained bytes were bounded and the work was
    /// not.
    #[test]
    fn nothing_downstream_ever_sees_more_than_the_byte_ceiling() {
        let huge = "x".repeat(4 * 1024 * 1024);
        let (prefix, truncated) = bounded_prefix(&huge);
        assert!(truncated);
        assert_eq!(prefix.len(), 131_072);

        // And with no newline anywhere, so the old per-line path would have
        // scanned all four megabytes to yield its first line.
        assert!(!huge.contains('\n'));
        assert_eq!(scan(&huge).0.len(), 1);
    }

    /// The synthetic per-line `+1` that stood in for a newline charged inputs
    /// for a byte they do not contain, so an exact-ceiling body with no
    /// trailing newline was reported truncated one byte early.
    #[test]
    fn the_byte_ceiling_is_exact_at_the_boundary() {
        let exact = "x".repeat(131_072);
        assert_eq!(bounded_prefix(&exact), (exact.as_str(), false));
        assert!(!scan(&exact).1, "exactly the ceiling is not truncated");

        let one_over = "x".repeat(131_073);
        let (prefix, truncated) = bounded_prefix(&one_over);
        assert!(truncated, "one byte over the ceiling is truncated");
        assert_eq!(prefix.len(), 131_072);

        // Terminal newline is a real byte and is counted like any other.
        let exact_with_newline = format!("{}\n", "x".repeat(131_071));
        assert_eq!(exact_with_newline.len(), 131_072);
        assert!(!scan(&exact_with_newline).1);

        let over_with_newline = format!("{}\n", "x".repeat(131_072));
        assert!(scan(&over_with_newline).1);
    }

    #[test]
    fn the_prefix_cut_lands_on_a_character_boundary() {
        // 'é' is two bytes, so a cut at 131_072 lands mid-character unless the
        // boundary is respected. Building the string so one straddles it.
        let text = format!("{}{}", "x".repeat(131_071), "é".repeat(64));
        let (prefix, truncated) = bounded_prefix(&text);
        assert!(truncated);
        assert_eq!(prefix.len(), 131_071, "cut back to the character boundary");
        // Reaching here proves it: slicing off a boundary would have panicked.
    }

    #[test]
    fn a_thousand_lines_scan_clean_and_a_thousand_and_one_truncate() {
        let head = "\u{26A0}\u{FE0F} **head** blocked\n";
        let at_limit = format!("{head}{}", "filler\n".repeat(999));
        let (lines, truncated) = scan(&at_limit);
        assert_eq!(lines.len(), 1_000);
        assert!(!truncated, "exactly 1000 lines must not trip the ceiling");

        let over_limit = format!("{head}{}", "filler\n".repeat(1_000));
        let (lines, truncated) = scan(&over_limit);
        assert_eq!(lines.len(), 1_000);
        assert!(truncated);
    }

    #[test]
    fn the_byte_ceiling_is_reported_through_the_public_assessment() {
        let text = format!(
            "\u{26A0}\u{FE0F} **head** blocked\n{}\n\u{25B6}\u{FE0F} **tail** moving",
            "x".repeat(131_072)
        );
        assert!(
            categories(&text).contains(&category::INPUT_TRUNCATED.to_owned()),
            "{:?}",
            categories(&text)
        );

        // The detail is a fixed string carrying the two ceilings and nothing
        // from the message.
        let detail = StatusPacketLint::findings(&text)
            .into_iter()
            .find(|finding| finding.category() == category::INPUT_TRUNCATED)
            .expect("truncation assessment present")
            .detail()
            .to_owned();
        assert_eq!(
            detail,
            "input exceeded the scan ceiling (1000 lines / 131072 bytes); \
             findings cover the prefix only"
        );

        // The discarded tail must not be audited: `**tail**` sits past the
        // ceiling, so exactly one item — the head — was seen.
        assert!(
            !StatusPacketLint::findings(&text)
                .iter()
                .any(|finding| finding.detail().contains("line 3")),
            "content past the ceiling was still audited"
        );
    }

    #[test]
    fn an_oversized_first_line_still_reports_its_own_truncation() {
        // The boundary-crossing line used to be dropped before it was
        // retained, so there were no items, and `findings` returned early
        // without ever emitting the truncation assessment. A marker-bearing
        // megabyte looked exactly like ordinary prose.
        let text = format!("\u{26A0}\u{FE0F} blocked on #375 {}", "x".repeat(200_000));
        let cats = categories(&text);
        assert!(
            cats.contains(&category::INPUT_TRUNCATED.to_owned()),
            "{cats:?}"
        );
        assert!(
            cats.contains(&category::OPAQUE_ID.to_owned()),
            "activation state was lost with the truncated bytes: {cats:?}"
        );
    }

    #[test]
    fn truncating_a_line_never_splits_a_character() {
        // The prefix is cut at a UTF-8 boundary; a multi-byte character
        // straddling the ceiling is dropped whole rather than halved.
        let text = format!("\u{26A0}\u{FE0F} blocked on #375 {}", "é".repeat(200_000));
        let (lines, truncated) = scan(&text);
        assert!(truncated);
        // Reaching here at all proves it: slicing off a character boundary
        // would have panicked inside `bounded_prefix`.
        assert_eq!(lines.len(), 1);
    }

    /// Detail text of the bare-issue-number rule, which is the one under test
    /// here. Asserting on [`category::OPAQUE_ID`] alone would conflate it with
    /// the snowflake rule, which shares that category and legitimately fires
    /// on any 17-to-20-digit run.
    fn flags_a_bare_issue_number(text: &str) -> bool {
        StatusPacketLint::findings(text)
            .iter()
            .any(|finding| finding.detail().contains("bare issue number"))
    }

    #[test]
    fn artifact_reference_digits_are_unbounded_not_merely_six() {
        // `{1,6}` would have satisfied the old six-digit test. These would
        // still fail it.
        for digits in [7usize, 20, 64] {
            let number = "1".repeat(digits);

            let bare = format!("\u{25B6}\u{FE0F} **a** fixes #{number}");
            assert!(
                flags_a_bare_issue_number(&bare),
                "{digits}-digit bare reference exempted: {:?}",
                categories(&bare)
            );
            assert!(
                categories(&bare).contains(&category::MISSING_LINK.to_owned()),
                "{digits}-digit reference did not require a link: {:?}",
                categories(&bare)
            );

            let qualified =
                format!("\u{25B6}\u{FE0F} **a** fixes lacuna/dione#{number} https://forge.test/1");
            assert!(
                !flags_a_bare_issue_number(&qualified),
                "{digits}-digit qualified reference read as bare: {:?}",
                categories(&qualified)
            );
            assert!(
                !categories(&qualified).contains(&category::MISSING_LINK.to_owned()),
                "{:?}",
                categories(&qualified)
            );

            let linked =
                format!("\u{25B6}\u{FE0F} **a** fixes PR #{number} https://forge.test/{number}");
            assert!(
                !categories(&linked).contains(&category::MISSING_LINK.to_owned()),
                "{digits}-digit linked reference flagged: {:?}",
                categories(&linked)
            );
        }
    }

    /// Found while widening the digit coverage above: a repository-qualified
    /// reference whose number happens to be 17-to-20 digits long is reported
    /// as a bare snowflake, because [`SNOWFLAKE`] matches the digit run and
    /// knows nothing about the `owner/repo#` prefix that qualifies it.
    ///
    /// Pinned rather than fixed. It is a false positive on an input nobody
    /// sends, and narrowing the snowflake rule to exclude `#`-prefixed runs
    /// risks exempting genuine snowflakes written as `#<id>`.
    #[test]
    fn known_gap_a_snowflake_length_qualified_reference_reads_as_a_snowflake() {
        let text = format!(
            "\u{25B6}\u{FE0F} **a** fixes lacuna/dione#{} x",
            "1".repeat(18)
        );
        assert!(
            StatusPacketLint::findings(&text)
                .iter()
                .any(|finding| finding.detail().contains("bare snowflake")),
            "if this now fails, the snowflake rule was narrowed — check that \
             genuine snowflakes are still caught"
        );
    }
}
