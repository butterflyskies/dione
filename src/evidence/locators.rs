//! Provider-independent, role-preserving terminal sentex locators.

use super::error::{HandleError, HandleErrorKind, SentexError, SentexErrorKind};
use crate::markdown::BlockScanner;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use std::{fmt, num::NonZeroU64};

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
pub(crate) struct VaeliiSentexHandle(pub(super) NonZeroU64);

impl VaeliiSentexHandle {
    pub(super) fn parse_decimal(value: &str) -> Result<Self, HandleError> {
        if !matches!(value.as_bytes().first(), Some(b'1'..=b'9'))
            || !value.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(HandleError {
                kind: HandleErrorKind::Noncanonical,
            });
        }
        let value = value.parse::<NonZeroU64>().map_err(|source| HandleError {
            kind: HandleErrorKind::Number(source),
        })?;
        Ok(Self(value))
    }

    pub(super) fn from_locator_token(token: &str) -> Option<Self> {
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
    pub(super) claims: Vec<VaeliiSentexHandle>,
    pub(super) citations: Vec<VaeliiSentexHandle>,
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
        metadata.push_str(&SentexLocator::current(role, handle.clone()).to_string());
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
        output.push_str(role.marker_prefix());
        output.push_str(&handle.clone().locator_token());
        output.push_str(MARKER_SUFFIX);
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

/// True when the evidence marker at `suffix_start` sits inside a blockquote,
/// a fenced block, an inline code span, or an indented code block — in which
/// case it is an example rather than an offered locator.
///
/// The block-structure walk is [`crate::markdown::BlockScanner`], shared with
/// the status lint so the two cannot disagree about what counts as code.
fn marker_is_in_quote_or_code(content: &str, suffix_start: usize) -> bool {
    let mut scanner = BlockScanner::new();
    for line in content[..suffix_start].split('\n') {
        scanner.push(line);
    }
    scanner.inside_quote_or_code()
}

pub(super) fn validate_marker_bytes(handles: &SentexHandles) -> Result<(), SentexError> {
    let bytes = marker_bytes(handles);
    if bytes > MAX_SENTEX_MARKER_BYTES {
        return Err(SentexError {
            kind: SentexErrorKind::MarkerBytes,
        });
    }
    Ok(())
}

pub(super) fn marker_bytes(handles: &SentexHandles) -> usize {
    let marker_content = handles.claims().len()
        * (CLAIM_MARKER_PREFIX.len() + LOCATOR_TOKEN_LEN + MARKER_SUFFIX.len())
        + handles.citations().len()
            * (CITATION_MARKER_PREFIX.len() + LOCATOR_TOKEN_LEN + MARKER_SUFFIX.len());
    marker_content + handles.len().saturating_sub(1) + usize::from(!handles.is_empty())
}
