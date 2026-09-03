//! Visible, role-preserving Vaelii sentex locators carried in Discord messages.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use serenity::model::id::UserId;
use std::{
    fmt::{self, Write},
    num::NonZeroU64,
};

const LEGACY_CITATION_MARKER_PREFIX: &str = "[🔍=v1:";
const CLAIM_MARKER_PREFIX: &str = "[🔍=v2:claim:";
const CITATION_MARKER_PREFIX: &str = "[🔍=v2:citation:";
const MARKER_SUFFIX: &str = "]";
const LOCATOR_TOKEN_LEN: usize = 11;

/// Maximum total number of claim and citation handles accepted on one message.
pub(crate) const MAX_SENTEX_REFS: usize = 4;
/// Maximum UTF-8 bytes occupied by the complete appended v2 marker suffix.
pub(crate) const MAX_SENTEX_MARKER_BYTES: usize = 124;

/// An opaque, nonzero handle naming one Vaelii sentex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VaeliiSentexHandle(NonZeroU64);

impl VaeliiSentexHandle {
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
        if token.len() != LOCATOR_TOKEN_LEN {
            return None;
        }
        let decoded = URL_SAFE_NO_PAD.decode(token).ok()?;
        let bytes: [u8; 8] = decoded.try_into().ok()?;
        let handle = NonZeroU64::new(u64::from_be_bytes(bytes))?;
        if URL_SAFE_NO_PAD.encode(bytes) != token {
            return None;
        }
        Some(Self(handle))
    }

    fn locator_token(self) -> String {
        URL_SAFE_NO_PAD.encode(self.0.get().to_be_bytes())
    }
}

/// The role a sentex plays in the containing Discord message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SentexRole {
    Claim,
    Citation,
}

impl SentexRole {
    const fn marker_prefix(self) -> &'static str {
        match self {
            Self::Claim => CLAIM_MARKER_PREFIX,
            Self::Citation => CITATION_MARKER_PREFIX,
        }
    }
}

/// Validated, bounded Vaelii handles partitioned by message role.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SentexHandles {
    claims: Vec<VaeliiSentexHandle>,
    citations: Vec<VaeliiSentexHandle>,
}

impl SentexHandles {
    pub(crate) const fn empty() -> Self {
        Self {
            claims: Vec::new(),
            citations: Vec::new(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.claims.is_empty() && self.citations.is_empty()
    }

    pub(crate) fn claims(&self) -> &[VaeliiSentexHandle] {
        &self.claims
    }

    pub(crate) fn citations(&self) -> &[VaeliiSentexHandle] {
        &self.citations
    }

    fn len(&self) -> usize {
        self.claims.len() + self.citations.len()
    }
}

/// The visible sentex transport emitted by current Dione versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SentexTransport {
    TerminalVisibleRoleSuffixV2AfterHooks,
}

impl SentexTransport {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::TerminalVisibleRoleSuffixV2AfterHooks => {
                "terminal-visible-role-suffix-v2-after-hooks"
            }
        }
    }
}

/// A canonical, versioned, role-bearing locator projected to Dione consumers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SentexLocator {
    role: SentexRole,
    encoded: String,
}

impl SentexLocator {
    fn current(role: SentexRole, handle: VaeliiSentexHandle) -> Self {
        let role_name = match role {
            SentexRole::Claim => "claim",
            SentexRole::Citation => "citation",
        };
        Self {
            role,
            encoded: format!("v2:{role_name}:{}", handle.locator_token()),
        }
    }

    fn legacy_citation(handle: VaeliiSentexHandle) -> Self {
        Self {
            role: SentexRole::Citation,
            encoded: format!("v1:{}", handle.locator_token()),
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.encoded
    }

    pub(crate) const fn role(&self) -> SentexRole {
        self.role
    }
}

impl fmt::Display for SentexLocator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.encoded.fmt(formatter)
    }
}

/// A role-bearing sentex reference offered by the Discord message author.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OfferedSentex {
    pub(crate) locator: SentexLocator,
    pub(crate) author_id: UserId,
}

/// Parses the role-separated `claim_handles` and `citation_handles` arguments.
/// The removed `evidence_keys` name is rejected rather than silently ignored.
pub(crate) fn parse_tool_sentex_handles(args: &Value) -> Result<SentexHandles, String> {
    if args.get("evidence_keys").is_some() {
        return Err(
            "evidence_keys was removed; use claim_handles and/or citation_handles".to_string(),
        );
    }
    let claim_values = handle_array(args, "claim_handles")?;
    let citation_values = handle_array(args, "citation_handles")?;
    let total = claim_values
        .len()
        .checked_add(citation_values.len())
        .ok_or_else(|| "sentex handle count overflowed".to_string())?;
    if total > MAX_SENTEX_REFS {
        return Err(format!(
            "claim_handles and citation_handles support at most {MAX_SENTEX_REFS} total references"
        ));
    }
    let claims = parse_handle_array(claim_values, "claim_handles")?;
    let citations = parse_handle_array(citation_values, "citation_handles")?;
    let handles = SentexHandles { claims, citations };
    validate_marker_bytes(&handles)?;
    Ok(handles)
}

fn handle_array<'a>(args: &'a Value, field: &str) -> Result<&'a [Value], String> {
    let Some(value) = args.get(field) else {
        return Ok(&[]);
    };
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| format!("{field} must be an array"))
}

fn parse_handle_array(values: &[Value], field: &str) -> Result<Vec<VaeliiSentexHandle>, String> {
    let mut handles = Vec::with_capacity(values.len());
    for value in values {
        let handle = value.as_str().and_then(VaeliiSentexHandle::parse_decimal);
        let handle = handle.ok_or_else(|| {
            format!("{field} must contain canonical positive decimal u64 strings")
        })?;
        handles.push(handle);
    }
    Ok(handles)
}

/// Whether content ends in unquoted sentex marker syntax, including malformed
/// or over-limit sequences that the read-side parser intentionally ignores.
pub(crate) fn has_terminal_sentex_syntax(content: &str) -> bool {
    if !content.ends_with(MARKER_SUFFIX) {
        return false;
    }
    let Some((start, _, _, _)) = marker_start(content) else {
        return false;
    };
    if start > 0 && content.as_bytes()[start - 1] != b' ' {
        return false;
    }
    let before_final_suffix = &content[start..content.len() - MARKER_SUFFIX.len()];
    if before_final_suffix.contains(MARKER_SUFFIX) {
        return false;
    }
    !marker_is_in_quote_or_code(content, start)
}

/// Canonical comma-separated role-bearing locators for hook/audit metadata.
pub(crate) fn locator_metadata(handles: &[VaeliiSentexHandle], role: SentexRole) -> String {
    let mut metadata = String::new();
    for (index, handle) in handles.iter().enumerate() {
        if index > 0 {
            metadata.push(',');
        }
        write!(metadata, "{}", SentexLocator::current(role, handle.clone()))
            .expect("writing to a String cannot fail");
    }
    metadata
}

/// Appends exact terminal markers after all rewrite hooks have run.
pub(crate) fn append_markers(content: &str, handles: &SentexHandles) -> String {
    if handles.is_empty() {
        return content.to_owned();
    }
    let mut output = String::with_capacity(content.len() + marker_bytes(handles));
    output.push_str(content);
    for (index, (role, handle)) in handles
        .claims()
        .iter()
        .map(|handle| (SentexRole::Claim, handle))
        .chain(
            handles
                .citations()
                .iter()
                .map(|handle| (SentexRole::Citation, handle)),
        )
        .enumerate()
    {
        if index > 0 || !content.is_empty() {
            output.push(' ');
        }
        write!(
            output,
            "{}{}{MARKER_SUFFIX}",
            role.marker_prefix(),
            handle.clone().locator_token()
        )
        .expect("writing to a String cannot fail");
    }
    output
}

/// Parses only a complete, bounded sequence of exact terminal sentex locators.
/// Legacy v1 evidence markers remain readable and project as citations.
pub(crate) fn parse_sentex_locators(content: &str) -> Vec<SentexLocator> {
    let mut end = content.len();
    let mut reversed = Vec::new();
    let mut suffix_start = end;

    loop {
        let candidate = &content[..end];
        if !candidate.ends_with(MARKER_SUFFIX) {
            break;
        }
        let Some((start, prefix, role, legacy)) = marker_start(candidate) else {
            return Vec::new();
        };
        if start > 0 && candidate.as_bytes()[start - 1] != b' ' {
            return Vec::new();
        }
        let token_start = start + prefix.len();
        let token_end = end - MARKER_SUFFIX.len();
        let Some(handle) = VaeliiSentexHandle::from_locator_token(&content[token_start..token_end])
        else {
            return Vec::new();
        };
        let locator = if legacy {
            SentexLocator::legacy_citation(handle)
        } else {
            SentexLocator::current(role, handle)
        };
        reversed.push(locator);
        if reversed.len() > MAX_SENTEX_REFS {
            return Vec::new();
        }
        suffix_start = start;
        if start == 0 {
            break;
        }
        end = start - 1;
    }

    if reversed.is_empty() || marker_is_in_quote_or_code(content, suffix_start) {
        return Vec::new();
    }

    reversed.into_iter().rev().collect()
}

fn marker_start(candidate: &str) -> Option<(usize, &'static str, SentexRole, bool)> {
    [
        (CLAIM_MARKER_PREFIX, SentexRole::Claim, false),
        (CITATION_MARKER_PREFIX, SentexRole::Citation, false),
        (LEGACY_CITATION_MARKER_PREFIX, SentexRole::Citation, true),
    ]
    .into_iter()
    .filter_map(|(prefix, role, legacy)| {
        candidate
            .rfind(prefix)
            .map(|start| (start, prefix, role, legacy))
    })
    .max_by_key(|(start, _, _, _)| *start)
}

/// Binds author-free parsed locators to Discord's system-derived author.
pub(crate) fn parse_offered_sentexes(content: &str, author_id: UserId) -> Vec<OfferedSentex> {
    parse_sentex_locators(content)
        .into_iter()
        .map(|locator| OfferedSentex { locator, author_id })
        .collect()
}

fn offered_sentex_json(content: &str, author_id: UserId, role: SentexRole) -> Vec<Value> {
    parse_offered_sentexes(content, author_id)
        .into_iter()
        .filter(|sentex| sentex.locator.role() == role)
        .map(|sentex| {
            json!({
                "locator": sentex.locator.as_str(),
                "author_id": sentex.author_id.get().to_string(),
            })
        })
        .collect()
}

/// Adds role-separated projections without changing legacy empty shapes.
pub(crate) fn project_sentexes(target: &mut Value, content: &str, author_id: UserId) {
    let claims = offered_sentex_json(content, author_id, SentexRole::Claim);
    let citations = offered_sentex_json(content, author_id, SentexRole::Citation);
    if !claims.is_empty() {
        target["claim_locators"] = json!(claims);
    }
    if !citations.is_empty() {
        target["citation_locators"] = json!(citations);
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

fn validate_marker_bytes(handles: &SentexHandles) -> Result<(), String> {
    let bytes = marker_bytes(handles);
    if bytes > MAX_SENTEX_MARKER_BYTES {
        return Err(format!(
            "sentex marker suffix exceeds {MAX_SENTEX_MARKER_BYTES} bytes"
        ));
    }
    Ok(())
}

fn marker_bytes(handles: &SentexHandles) -> usize {
    let marker_content = handles.claims().len()
        * (CLAIM_MARKER_PREFIX.len() + LOCATOR_TOKEN_LEN + MARKER_SUFFIX.len())
        + handles.citations().len()
            * (CITATION_MARKER_PREFIX.len() + LOCATOR_TOKEN_LEN + MARKER_SUFFIX.len());
    marker_content + handles.len().saturating_sub(1) + usize::from(!handles.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn legacy_terminal_markers_parse_as_citations_with_discord_author() {
        let content = "claim [🔍=v1:AAAAAAAAAAw] [🔍=v1:AAAAAAAAACI]";
        let sentexes = parse_offered_sentexes(content, UserId::new(99));
        assert_eq!(sentexes.len(), 2);
        assert_eq!(sentexes[0].locator.as_str(), "v1:AAAAAAAAAAw");
        assert_eq!(sentexes[1].locator.as_str(), "v1:AAAAAAAAACI");
        assert!(
            sentexes.iter().all(|item| item.author_id == UserId::new(99)
                && item.locator.role() == SentexRole::Citation)
        );
    }

    #[test]
    fn v2_terminal_markers_preserve_roles_and_original_order() {
        let content = concat!(
            "claim ",
            "[🔍=v2:citation:AAAAAAAAACI] ",
            "[🔍=v2:claim:AAAAAAAAAAw]"
        );
        let sentexes = parse_offered_sentexes(content, UserId::new(99));
        assert_eq!(sentexes.len(), 2);
        assert_eq!(sentexes[0].locator.role(), SentexRole::Citation);
        assert_eq!(sentexes[0].locator.as_str(), "v2:citation:AAAAAAAAACI");
        assert_eq!(sentexes[1].locator.role(), SentexRole::Claim);
        assert_eq!(sentexes[1].locator.as_str(), "v2:claim:AAAAAAAAAAw");
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
            assert_eq!(parse_offered_sentexes(content, UserId::new(99)), vec![]);
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
            assert!(parse_sentex_locators(content).is_empty());
        }
    }

    #[test]
    fn quote_syntax_inside_a_closed_fence_does_not_poison_later_claims() {
        for content in [
            "```\n>>> code example\n```\nclaim [🔍=v1:AAAAAAAAAAw]",
            "```\n    ```\nstill code\n```\nclaim [🔍=v1:AAAAAAAAAAw]",
            "`inline\n>>> code example\nclosed` claim [🔍=v1:AAAAAAAAAAw]",
        ] {
            assert_eq!(parse_sentex_locators(content)[0].as_str(), "v1:AAAAAAAAAAw");
        }
    }

    #[test]
    fn four_space_indented_backticks_do_not_close_an_open_fence() {
        let content = "```\n    ```\nstill code [🔍=v1:AAAAAAAAAAw]";
        assert!(parse_sentex_locators(content).is_empty());
    }

    #[test]
    fn four_space_indented_backticks_do_not_poison_a_later_claim() {
        // Indented code block after a blank line — backticks are literal.
        let content = "    ```\nclaim [🔍=v1:AAAAAAAAAAw]";
        let locators = parse_sentex_locators(content);
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
                parse_sentex_locators(content).is_empty(),
                "paragraph-continuation indented backtick must poison: {content:?}"
            );
        }
    }

    #[test]
    fn genuine_indented_code_backtick_does_not_poison_claim() {
        // Blank line before indented line — genuine indented code block.
        // Backtick is literal code content, should NOT open an inline span.
        let content = "paragraph text\n\n    `indented code\nclaim [🔍=v1:AAAAAAAAAAw]";
        let locators = parse_sentex_locators(content);
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
                parse_sentex_locators(content).is_empty(),
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
            let locators = parse_sentex_locators(content);
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
            parse_sentex_locators(content).is_empty(),
            "marker in indented code after heading must be rejected"
        );
    }

    #[test]
    fn indented_code_after_fence_close_is_rejected() {
        // After a fence closes, a 4+-space line is indented code (not a
        // paragraph continuation).
        let content = "```\ncode\n```\n    claim [🔍=v1:AAAAAAAAAAw]";
        assert!(
            parse_sentex_locators(content).is_empty(),
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
            parse_sentex_locators(content).is_empty(),
            "backtick on paragraph continuation after code block must poison"
        );
    }

    #[test]
    fn unindented_line_exits_code_block() {
        // After an unindented line, we're in a paragraph — indented lines
        // are continuations.
        let content = "\n    code\nparagraph\n    `backtick\nclaim [🔍=v1:AAAAAAAAAAw]";
        assert!(
            parse_sentex_locators(content).is_empty(),
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
            assert_eq!(parse_sentex_locators(content)[0].as_str(), "v1:AAAAAAAAAAw");
        }
    }

    #[test]
    fn locator_requires_exact_nonzero_eight_byte_canonical_payload() {
        assert!(VaeliiSentexHandle::from_locator_token("AAAAAAAAAAA").is_none());
        assert!(VaeliiSentexHandle::from_locator_token("AAAAAAAAAAw=").is_none());
        assert!(VaeliiSentexHandle::from_locator_token("AAAAAAAAAA").is_none());
        assert!(VaeliiSentexHandle::from_locator_token("AAAAAAAAAAAA").is_none());
        assert_eq!(
            VaeliiSentexHandle::from_locator_token("__________8"),
            Some(VaeliiSentexHandle(NonZeroU64::new(u64::MAX).unwrap()))
        );
    }

    #[test]
    fn tool_input_is_role_separated_canonical_and_combined_bounded() {
        assert!(parse_tool_sentex_handles(&json!({})).unwrap().is_empty());
        assert!(parse_tool_sentex_handles(&json!({ "evidence_keys": ["1"] })).is_err());
        for field in ["claim_handles", "citation_handles"] {
            for invalid in [
                json!([1]),
                json!([9_007_199_254_740_993u64]),
                json!(["0"]),
                json!(["01"]),
                json!(["-1"]),
                json!(["abc"]),
                json!(["18446744073709551616"]),
                json!("1"),
            ] {
                assert!(
                    parse_tool_sentex_handles(&json!({ field: invalid })).is_err(),
                    "{field} accepted a noncanonical handle"
                );
            }
        }
        assert!(
            parse_tool_sentex_handles(&json!({
                "claim_handles": ["1", "2"],
                "citation_handles": ["3", "4"]
            }))
            .is_ok()
        );
        assert!(
            parse_tool_sentex_handles(&json!({
                "claim_handles": ["1", "2"],
                "citation_handles": ["3", "4", "5"]
            }))
            .is_err()
        );
        let oversized_invalid = vec![Value::Null; MAX_SENTEX_REFS + 1];
        assert_eq!(
            parse_tool_sentex_handles(&json!({ "claim_handles": oversized_invalid })).unwrap_err(),
            "claim_handles and citation_handles support at most 4 total references"
        );
    }

    #[test]
    fn terminal_sentex_syntax_distinguishes_absent_from_invalid() {
        assert!(!has_terminal_sentex_syntax("ordinary prose"));
        assert!(!has_terminal_sentex_syntax(
            "quoted `[🔍=v2:claim:garbage]`"
        ));
        assert!(!has_terminal_sentex_syntax(
            "discussion [🔍=v2:claim:garbage] (see appendix [A])"
        ));
        assert!(has_terminal_sentex_syntax(
            "malformed [🔍=v2:claim:garbage]"
        ));
        assert!(has_terminal_sentex_syntax(concat!(
            "over limit ",
            "[🔍=v2:claim:AAAAAAAAAAE] ",
            "[🔍=v2:claim:AAAAAAAAAAI] ",
            "[🔍=v2:claim:AAAAAAAAAAM] ",
            "[🔍=v2:claim:AAAAAAAAAAQ] ",
            "[🔍=v2:claim:AAAAAAAAAAU]"
        )));
    }

    #[test]
    fn appending_markers_uses_role_preserving_v2_big_endian_base64url() {
        let handles = parse_tool_sentex_handles(&json!({
            "claim_handles": ["12"],
            "citation_handles": ["34"]
        }))
        .unwrap();
        assert_eq!(
            append_markers("claim", &handles),
            "claim [🔍=v2:claim:AAAAAAAAAAw] [🔍=v2:citation:AAAAAAAAACI]"
        );
    }

    #[test]
    fn u64_max_string_round_trips_through_the_canonical_locator() {
        let handles =
            parse_tool_sentex_handles(&json!({ "claim_handles": [u64::MAX.to_string()] })).unwrap();
        let content = append_markers("claim", &handles);
        assert_eq!(content, "claim [🔍=v2:claim:__________8]");
        assert_eq!(
            parse_sentex_locators(&content)[0].as_str(),
            "v2:claim:__________8"
        );
    }

    #[test]
    fn locator_metadata_is_role_preserving_canonical_and_ordered() {
        let handles = parse_tool_sentex_handles(&json!({
            "claim_handles": ["12", "34"],
            "citation_handles": ["56"]
        }))
        .unwrap();
        assert_eq!(
            locator_metadata(handles.claims(), SentexRole::Claim),
            "v2:claim:AAAAAAAAAAw,v2:claim:AAAAAAAAACI"
        );
        assert_eq!(
            locator_metadata(handles.citations(), SentexRole::Citation),
            "v2:citation:AAAAAAAAADg"
        );
    }

    #[test]
    fn marker_byte_bound_covers_the_largest_allowed_suffix() {
        let largest = SentexHandles {
            claims: Vec::new(),
            citations: std::iter::repeat_with(|| {
                VaeliiSentexHandle(NonZeroU64::new(u64::MAX).unwrap())
            })
            .take(MAX_SENTEX_REFS)
            .collect(),
        };
        assert_eq!(marker_bytes(&largest), MAX_SENTEX_MARKER_BYTES);
        assert!(validate_marker_bytes(&largest).is_ok());
        let over_limit = SentexHandles {
            claims: Vec::new(),
            citations: std::iter::repeat_with(|| VaeliiSentexHandle(NonZeroU64::new(1).unwrap()))
                .take(MAX_SENTEX_REFS + 1)
                .collect(),
        };
        assert!(validate_marker_bytes(&over_limit).is_err());
    }
}
