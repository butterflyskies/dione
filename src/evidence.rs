//! Visible author-offered evidence locators carried in Discord message content.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use serenity::model::id::UserId;
use std::{
    fmt::{self, Write},
    num::NonZeroU64,
};

const MARKER_PREFIX: &str = "[🔍=v1:";
const MARKER_SUFFIX: &str = "]";
const ENCODED_KEY_LEN: usize = 11;

/// Maximum number of evidence references accepted on one message.
pub(crate) const MAX_EVIDENCE_REFS: usize = 4;
/// Maximum UTF-8 bytes occupied by the complete appended marker suffix.
pub(crate) const MAX_EVIDENCE_MARKER_BYTES: usize = 88;

/// An opaque, nonzero key into the external Vaelii evidence bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VaeliiEvidenceKey(NonZeroU64);

impl VaeliiEvidenceKey {
    fn parse_decimal(value: &str) -> Option<Self> {
        if !matches!(value.as_bytes().first(), Some(b'1'..=b'9'))
            || !value.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        value
            .parse::<u64>()
            .ok()
            .and_then(NonZeroU64::new)
            .map(Self)
    }

    fn from_locator_token(token: &str) -> Option<Self> {
        if token.len() != ENCODED_KEY_LEN {
            return None;
        }
        let decoded = URL_SAFE_NO_PAD.decode(token).ok()?;
        let bytes: [u8; 8] = decoded.try_into().ok()?;
        let key = NonZeroU64::new(u64::from_be_bytes(bytes))?;
        if URL_SAFE_NO_PAD.encode(bytes) != token {
            return None;
        }
        Some(Self(key))
    }

    fn locator_token(self) -> String {
        URL_SAFE_NO_PAD.encode(self.0.get().to_be_bytes())
    }
}

/// A validated, bounded sequence of opaque Vaelii keys.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct EvidenceKeys(Vec<VaeliiEvidenceKey>);

impl EvidenceKeys {
    pub(crate) const fn empty() -> Self {
        Self(Vec::new())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn iter(&self) -> impl Iterator<Item = &VaeliiEvidenceKey> {
        self.0.iter()
    }

    fn len(&self) -> usize {
        self.0.len()
    }
}

/// The only supported visible evidence transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EvidenceTransport {
    TerminalVisibleSuffixV1AfterHooks,
}

impl EvidenceTransport {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::TerminalVisibleSuffixV1AfterHooks => "terminal-visible-suffix-v1-after-hooks",
        }
    }
}

/// A canonical versioned locator projected to Dione consumers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EvidenceLocator(String);

impl EvidenceLocator {
    fn from_key(key: VaeliiEvidenceKey) -> Self {
        Self(format!("v1:{}", key.locator_token()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EvidenceLocator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Evidence offered by the Discord author of the containing message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OfferedEvidence {
    pub(crate) locator: EvidenceLocator,
    pub(crate) author_id: UserId,
}

/// Parses the bounded `evidence_keys` tool argument.
pub(crate) fn parse_tool_evidence_keys(args: &Value) -> Result<EvidenceKeys, String> {
    let Some(value) = args.get("evidence_keys") else {
        return Ok(EvidenceKeys::empty());
    };
    let values = value
        .as_array()
        .ok_or_else(|| "evidence_keys must be an array".to_string())?;
    if values.len() > MAX_EVIDENCE_REFS {
        return Err(format!(
            "evidence_keys supports at most {MAX_EVIDENCE_REFS} references"
        ));
    }

    let mut keys = Vec::with_capacity(values.len());
    for value in values {
        let key = value.as_str().and_then(VaeliiEvidenceKey::parse_decimal);
        let key = key.ok_or_else(|| {
            "evidence keys must be canonical positive decimal u64 strings".to_string()
        })?;
        keys.push(key);
    }

    validate_marker_bytes(&keys)?;
    Ok(EvidenceKeys(keys))
}

/// Canonical comma-separated locators used in pre-send and audit metadata.
pub(crate) fn locator_metadata(keys: &EvidenceKeys) -> String {
    let mut metadata = String::new();
    for (index, key) in keys.iter().enumerate() {
        if index > 0 {
            metadata.push(',');
        }
        write!(metadata, "{}", EvidenceLocator::from_key(key.clone()))
            .expect("writing to a String cannot fail");
    }
    metadata
}

/// Appends exact terminal markers after all rewrite hooks have run.
pub(crate) fn append_markers(content: &str, keys: &EvidenceKeys) -> String {
    if keys.is_empty() {
        return content.to_owned();
    }
    let mut output = String::with_capacity(content.len() + marker_bytes(keys.len()));
    output.push_str(content);
    for (index, key) in keys.iter().enumerate() {
        if index > 0 || !content.is_empty() {
            output.push(' ');
        }
        write!(
            output,
            "{MARKER_PREFIX}{}{MARKER_SUFFIX}",
            key.clone().locator_token()
        )
        .expect("writing to a String cannot fail");
    }
    output
}

/// Parses only a complete, bounded sequence of exact terminal locators.
pub(crate) fn parse_evidence_locators(content: &str) -> Vec<EvidenceLocator> {
    let mut end = content.len();
    let mut reversed = Vec::new();
    let mut suffix_start = end;

    loop {
        let candidate = &content[..end];
        if !candidate.ends_with(MARKER_SUFFIX) {
            break;
        }
        let Some(start) = candidate.rfind(MARKER_PREFIX) else {
            return Vec::new();
        };
        if start > 0 && candidate.as_bytes()[start - 1] != b' ' {
            return Vec::new();
        }
        let token_start = start + MARKER_PREFIX.len();
        let token_end = end - MARKER_SUFFIX.len();
        let Some(key) = VaeliiEvidenceKey::from_locator_token(&content[token_start..token_end])
        else {
            return Vec::new();
        };
        reversed.push(key);
        if reversed.len() > MAX_EVIDENCE_REFS {
            return Vec::new();
        }
        suffix_start = start;
        if start == 0 {
            break;
        }
        end = start - 1;
    }

    if reversed.is_empty()
        || marker_bytes(reversed.len()) > MAX_EVIDENCE_MARKER_BYTES
        || marker_is_in_quote_or_code(content, suffix_start)
    {
        return Vec::new();
    }

    reversed
        .into_iter()
        .rev()
        .map(EvidenceLocator::from_key)
        .collect()
}

/// Binds author-free parsed locators to Discord's system-derived author.
pub(crate) fn parse_offered_evidence(content: &str, author_id: UserId) -> Vec<OfferedEvidence> {
    parse_evidence_locators(content)
        .into_iter()
        .map(|locator| OfferedEvidence { locator, author_id })
        .collect()
}

/// Projects parsed evidence into the common MCP JSON shape.
pub(crate) fn offered_evidence_json(content: &str, author_id: UserId) -> Vec<Value> {
    parse_offered_evidence(content, author_id)
        .into_iter()
        .map(|evidence| {
            json!({
                "locator": evidence.locator.as_str(),
                "author_id": evidence.author_id.get().to_string(),
            })
        })
        .collect()
}

/// Adds the optional evidence projection without changing legacy empty shapes.
pub(crate) fn project_evidence(target: &mut Value, content: &str, author_id: UserId) {
    let evidence = offered_evidence_json(content, author_id);
    if !evidence.is_empty() {
        target["evidence"] = json!(evidence);
    }
}

fn marker_is_in_quote_or_code(content: &str, suffix_start: usize) -> bool {
    let preceding = &content[..suffix_start];
    let mut fenced_delimiter = None;
    let mut inline_delimiter = None;
    let mut after_multiline_quote_start = false;
    let mut current_line_is_quote = false;
    // Track block-level state for indented code detection.
    // CommonMark: an indented code block cannot interrupt a paragraph,
    // but CAN follow a heading, fence close, quote, or blank line.
    let mut in_indented_code = false;
    let mut in_paragraph = false;

    for line in preceding.split('\n') {
        let structural = line.trim_start();
        let fence_structural = markdown_fence_structural(line);
        let indentation = line
            .bytes()
            .take_while(|b| *b == b' ' || *b == b'\t')
            .count();
        let line_is_indented = indentation >= 4 || line.starts_with('\t');

        current_line_is_quote =
            fenced_delimiter.is_none() && inline_delimiter.is_none() && structural.starts_with('>');
        if fenced_delimiter.is_none() && inline_delimiter.is_none() && structural.starts_with(">>>")
        {
            after_multiline_quote_start = true;
        }

        // Inside a fenced code block — everything is code content.
        if let Some(opening_length) = fenced_delimiter {
            if fence_structural.is_some_and(|candidate| {
                fence_delimiter(candidate).is_some_and(|length| {
                    length >= opening_length && fence_tail_is_empty(candidate)
                })
            }) {
                fenced_delimiter = None;
                in_paragraph = false;
            }
            in_indented_code = false;
            continue;
        }

        // Blank line — ends indented code blocks and paragraphs.
        if structural.is_empty() {
            in_indented_code = false;
            in_paragraph = false;
            continue;
        }

        // Opening a fenced code block (only at ≤3 spaces indent).
        if inline_delimiter.is_none()
            && let Some(length) = fence_structural.and_then(fence_delimiter)
        {
            fenced_delimiter = Some(length);
            in_indented_code = false;
            in_paragraph = false;
            continue;
        }

        // Indented code block detection:
        // - 4+ spaces (or tab) AND not currently in a paragraph AND no
        //   open inline span → genuine indented code.
        // - Contiguous: once in indented code, further indented lines
        //   remain in the block until a blank or non-indented line.
        // - CommonMark: indented code cannot interrupt a paragraph, but
        //   CAN follow headings, fence closes, block quotes, blank lines,
        //   and document start.
        if line_is_indented && (in_indented_code || !in_paragraph) && inline_delimiter.is_none() {
            in_indented_code = true;
            // Backtick runs are literal code content — do not scan.
            continue;
        }

        // Exiting indented code or starting/continuing a paragraph.
        in_indented_code = false;

        // ATX headings end any paragraph — next line starts a fresh block.
        if structural.starts_with('#') {
            let hashes = structural.bytes().take_while(|b| *b == b'#').count();
            if hashes <= 6 && (structural.len() == hashes || structural.as_bytes()[hashes] == b' ')
            {
                in_paragraph = false;
                scan_inline_delimiters(line, &mut inline_delimiter);
                continue;
            }
        }

        // Block quotes are not paragraph content.
        if current_line_is_quote {
            in_paragraph = false;
            scan_inline_delimiters(line, &mut inline_delimiter);
            continue;
        }

        // Regular content — we are in a paragraph.
        in_paragraph = true;
        scan_inline_delimiters(line, &mut inline_delimiter);
    }

    current_line_is_quote
        || after_multiline_quote_start
        || fenced_delimiter.is_some()
        || inline_delimiter.is_some()
        || in_indented_code
}

/// CommonMark permits at most three leading spaces before a fenced-code
/// delimiter. Four spaces make the backticks indented code content instead,
/// so they must not open or close the surrounding fence.
fn markdown_fence_structural(line: &str) -> Option<&str> {
    let indentation = line.bytes().take_while(|byte| *byte == b' ').count();
    (indentation <= 3).then(|| &line[indentation..])
}

fn fence_delimiter(line: &str) -> Option<usize> {
    let length = line.bytes().take_while(|byte| *byte == b'`').count();
    (length >= 3).then_some(length)
}

fn fence_tail_is_empty(line: &str) -> bool {
    let delimiter_length = line.bytes().take_while(|byte| *byte == b'`').count();
    line[delimiter_length..].trim().is_empty()
}

fn scan_inline_delimiters(line: &str, delimiter: &mut Option<usize>) {
    let bytes = line.as_bytes();
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] != b'`' {
            index += 1;
            continue;
        }

        let start = index;
        while index < bytes.len() && bytes[index] == b'`' {
            index += 1;
        }
        let run_length = index - start;
        let escaped = bytes[..start]
            .iter()
            .rev()
            .take_while(|byte| **byte == b'\\')
            .count()
            % 2
            == 1;
        if escaped && delimiter.is_none() {
            continue;
        }

        match *delimiter {
            Some(opening_length) if opening_length == run_length => *delimiter = None,
            None => *delimiter = Some(run_length),
            Some(_) => {}
        }
    }
}

fn validate_marker_bytes(keys: &[VaeliiEvidenceKey]) -> Result<(), String> {
    let bytes = marker_bytes(keys.len());
    if bytes > MAX_EVIDENCE_MARKER_BYTES {
        return Err(format!(
            "evidence marker suffix exceeds {MAX_EVIDENCE_MARKER_BYTES} bytes"
        ));
    }
    Ok(())
}

const fn marker_bytes(count: usize) -> usize {
    count * (MARKER_PREFIX.len() + ENCODED_KEY_LEN + MARKER_SUFFIX.len())
        + count.saturating_sub(1)
        + if count > 0 { 1 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn terminal_markers_parse_in_original_order_with_discord_author() {
        let content = "claim [🔍=v1:AAAAAAAAAAw] [🔍=v1:AAAAAAAAACI]";
        let evidence = parse_offered_evidence(content, UserId::new(99));
        assert_eq!(evidence.len(), 2);
        assert_eq!(evidence[0].locator.as_str(), "v1:AAAAAAAAAAw");
        assert_eq!(evidence[1].locator.as_str(), "v1:AAAAAAAAACI");
        assert!(
            evidence
                .iter()
                .all(|item| item.author_id == UserId::new(99))
        );
    }

    #[test]
    fn malformed_quoted_and_mid_message_markers_are_prose() {
        for content in [
            "claim [🔍=v1:AAAAAAAAAAA]",
            "claim [🔍=v1:short]",
            "claim [🔍=v1:AAAAAAAAAAw=]",
            "claim [🔍=v2:AAAAAAAAAAw]",
            "claim [🔍=v1:AAAAAAAAAAw",
            "claim[🔍=v1:AAAAAAAAAAw]",
            "claim [🔍=v1:AAAAAAAAAAw] afterward",
            "example `[🔍=v1:AAAAAAAAAAw]`",
            "> quoted claim [🔍=v1:AAAAAAAAAAw]",
            ">>> quoted claim\nstill quoted [🔍=v1:AAAAAAAAAAw]",
            "```\ncode example [🔍=v1:AAAAAAAAAAw]",
            "```rust\nlet marker = \"example\"; [🔍=v1:AAAAAAAAAAw]",
        ] {
            assert_eq!(parse_offered_evidence(content, UserId::new(99)), vec![]);
        }
    }

    #[test]
    fn unmatched_inline_code_marker_is_prose() {
        for content in [
            "example `claim [🔍=v1:AAAAAAAAAAw]",
            "example ``claim [🔍=v1:AAAAAAAAAAw]",
            "example `code\nclaim [🔍=v1:AAAAAAAAAAw]",
            "example ``code\nclaim [🔍=v1:AAAAAAAAAAw]",
            "example ```claim [🔍=v1:AAAAAAAAAAw]",
            "example ````claim [🔍=v1:AAAAAAAAAAw]",
        ] {
            assert!(parse_evidence_locators(content).is_empty());
        }
    }

    #[test]
    fn quote_syntax_inside_a_closed_fence_does_not_poison_later_claims() {
        for content in [
            "```\n>>> code example\n```\nclaim [🔍=v1:AAAAAAAAAAw]",
            "```\n    ```\nstill code\n```\nclaim [🔍=v1:AAAAAAAAAAw]",
            "`inline\n>>> code example\nclosed` claim [🔍=v1:AAAAAAAAAAw]",
        ] {
            assert_eq!(
                parse_evidence_locators(content)[0].as_str(),
                "v1:AAAAAAAAAAw"
            );
        }
    }

    #[test]
    fn four_space_indented_backticks_do_not_close_an_open_fence() {
        let content = "```\n    ```\nstill code [🔍=v1:AAAAAAAAAAw]";
        assert!(parse_evidence_locators(content).is_empty());
    }

    #[test]
    fn four_space_indented_backticks_do_not_poison_a_later_claim() {
        // Indented code block after a blank line — backticks are literal.
        let content = "    ```\nclaim [🔍=v1:AAAAAAAAAAw]";
        let locators = parse_evidence_locators(content);
        assert_eq!(locators.len(), 1);
        assert_eq!(locators[0].as_str(), "v1:AAAAAAAAAAw");
    }

    #[test]
    fn paragraph_continuation_indented_backtick_poisons_later_claim() {
        // No blank line between paragraph and indented line — the indented
        // line is a paragraph continuation under CommonMark, NOT indented code.
        // The backtick opens an inline code span that crosses into the next
        // line, poisoning the evidence marker.
        for content in [
            // Single backtick on indented paragraph continuation
            "paragraph text\n    `unclosed span\nclaim [🔍=v1:AAAAAAAAAAw]",
            // Double backtick on indented paragraph continuation
            "paragraph text\n    ``unclosed span\nclaim [🔍=v1:AAAAAAAAAAw]",
        ] {
            assert!(
                parse_evidence_locators(content).is_empty(),
                "paragraph-continuation indented backtick must poison: {content:?}"
            );
        }
    }

    #[test]
    fn genuine_indented_code_backtick_does_not_poison_claim() {
        // Blank line before indented line — genuine indented code block.
        // Backtick is literal code content, should NOT open an inline span.
        let content = "paragraph text\n\n    `indented code\nclaim [🔍=v1:AAAAAAAAAAw]";
        let locators = parse_evidence_locators(content);
        assert_eq!(locators.len(), 1);
        assert_eq!(locators[0].as_str(), "v1:AAAAAAAAAAw");
    }

    #[test]
    fn marker_on_indented_code_line_is_rejected() {
        // The marker itself is on a 4+-space indented line at document start
        // (indented code block). Must be rejected.
        for content in [
            "    claim [🔍=v1:AAAAAAAAAAw]",
            "\t claim [🔍=v1:AAAAAAAAAAw]",
            "paragraph\n\n    claim [🔍=v1:AAAAAAAAAAw]",
        ] {
            assert!(
                parse_evidence_locators(content).is_empty(),
                "marker on indented code line must be rejected: {content:?}"
            );
        }
    }

    #[test]
    fn consecutive_indented_lines_remain_in_code_block() {
        // Multiple contiguous 4+-space lines after a blank are all indented
        // code — backticks on the second line must not open a span.
        for content in [
            "paragraph\n\n    line 1\n    `line 2\nclaim [🔍=v1:AAAAAAAAAAw]",
            "\n    line 1\n    line 2\n    line 3\nclaim [🔍=v1:AAAAAAAAAAw]",
        ] {
            let locators = parse_evidence_locators(content);
            assert_eq!(
                locators.len(),
                1,
                "claim after exiting contiguous indented code must parse: {content:?}"
            );
        }
    }

    #[test]
    fn indented_code_after_heading_is_rejected() {
        // After a heading (not a paragraph), indented code CAN start
        // without a blank line. A marker there is in code.
        let content = "# Heading\n    claim [🔍=v1:AAAAAAAAAAw]";
        assert!(
            parse_evidence_locators(content).is_empty(),
            "marker in indented code after heading must be rejected"
        );
    }

    #[test]
    fn indented_code_after_fence_close_is_rejected() {
        // After a fence closes, a 4+-space line is indented code (not a
        // paragraph continuation).
        let content = "```\ncode\n```\n    claim [🔍=v1:AAAAAAAAAAw]";
        assert!(
            parse_evidence_locators(content).is_empty(),
            "marker in indented code after fence close must be rejected"
        );
    }

    #[test]
    fn blank_line_inside_indented_block_exits_code() {
        // A blank line inside indented content ends the code block.
        // The next indented line after a paragraph is a continuation.
        let content =
            "paragraph\n\n    code line\n\nparagraph 2\n    `backtick\nclaim [🔍=v1:AAAAAAAAAAw]";
        assert!(
            parse_evidence_locators(content).is_empty(),
            "backtick on paragraph continuation after code block must poison"
        );
    }

    #[test]
    fn unindented_line_exits_code_block() {
        // After an unindented line, we're in a paragraph — indented lines
        // are continuations.
        let content = "\n    code\nparagraph\n    `backtick\nclaim [🔍=v1:AAAAAAAAAAw]";
        assert!(
            parse_evidence_locators(content).is_empty(),
            "backtick on paragraph continuation after exiting code must poison"
        );
    }

    #[test]
    fn closed_quote_and_inline_code_do_not_poison_later_claims() {
        for content in [
            "> quoted example\nclaim [🔍=v1:AAAAAAAAAAw]",
            "`inline example` claim [🔍=v1:AAAAAAAAAAw]",
            "``inline example`` claim [🔍=v1:AAAAAAAAAAw]",
            "`inline\nexample` claim [🔍=v1:AAAAAAAAAAw]",
            "example ```inline``` claim [🔍=v1:AAAAAAAAAAw]",
            "example ````inline\nexample```` claim [🔍=v1:AAAAAAAAAAw]",
        ] {
            assert_eq!(
                parse_evidence_locators(content)[0].as_str(),
                "v1:AAAAAAAAAAw"
            );
        }
    }

    #[test]
    fn locator_requires_exact_nonzero_eight_byte_canonical_payload() {
        assert!(VaeliiEvidenceKey::from_locator_token("AAAAAAAAAAA").is_none());
        assert!(VaeliiEvidenceKey::from_locator_token("AAAAAAAAAAw=").is_none());
        assert!(VaeliiEvidenceKey::from_locator_token("AAAAAAAAAA").is_none());
        assert!(VaeliiEvidenceKey::from_locator_token("AAAAAAAAAAAA").is_none());
        assert_eq!(
            VaeliiEvidenceKey::from_locator_token("__________8"),
            Some(VaeliiEvidenceKey(NonZeroU64::new(u64::MAX).unwrap()))
        );
    }

    #[test]
    fn tool_input_is_optional_and_bounded() {
        assert!(parse_tool_evidence_keys(&json!({})).unwrap().is_empty());
        assert!(parse_tool_evidence_keys(&json!({ "evidence_keys": [1] })).is_err());
        assert!(
            parse_tool_evidence_keys(&json!({ "evidence_keys": [9_007_199_254_740_993u64] }))
                .is_err()
        );
        assert!(parse_tool_evidence_keys(&json!({ "evidence_keys": ["0"] })).is_err());
        assert!(parse_tool_evidence_keys(&json!({ "evidence_keys": ["abc"] })).is_err());
        assert!(
            parse_tool_evidence_keys(&json!({ "evidence_keys": ["1", "2", "3", "4", "5"] }))
                .is_err()
        );
    }

    #[test]
    fn appending_markers_uses_canonical_big_endian_base64url() {
        let keys = parse_tool_evidence_keys(&json!({ "evidence_keys": ["12", "34"] })).unwrap();
        assert_eq!(
            append_markers("claim", &keys),
            "claim [🔍=v1:AAAAAAAAAAw] [🔍=v1:AAAAAAAAACI]"
        );
    }

    #[test]
    fn u64_max_string_round_trips_through_the_canonical_locator() {
        let keys =
            parse_tool_evidence_keys(&json!({ "evidence_keys": [u64::MAX.to_string()] })).unwrap();
        let content = append_markers("claim", &keys);
        assert_eq!(content, "claim [🔍=v1:__________8]");
        assert_eq!(
            parse_evidence_locators(&content)[0].as_str(),
            "v1:__________8"
        );
    }

    #[test]
    fn locator_metadata_is_canonical_and_ordered() {
        let keys = parse_tool_evidence_keys(&json!({ "evidence_keys": ["12", "34"] })).unwrap();
        assert_eq!(locator_metadata(&keys), "v1:AAAAAAAAAAw,v1:AAAAAAAAACI");
    }

    #[test]
    fn marker_byte_bound_covers_the_largest_allowed_suffix() {
        let keys = std::iter::repeat_with(|| VaeliiEvidenceKey(NonZeroU64::new(u64::MAX).unwrap()))
            .take(MAX_EVIDENCE_REFS)
            .collect::<Vec<_>>();
        assert_eq!(marker_bytes(keys.len()), MAX_EVIDENCE_MARKER_BYTES);
        assert!(validate_marker_bytes(&keys).is_ok());
        let over_limit = std::iter::repeat_with(|| VaeliiEvidenceKey(NonZeroU64::new(1).unwrap()))
            .take(MAX_EVIDENCE_REFS + 1)
            .collect::<Vec<_>>();
        assert!(validate_marker_bytes(&over_limit).is_err());
    }
}
