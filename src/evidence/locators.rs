//! Provider-independent, role-preserving terminal sentex locators.

use super::error::{HandleError, HandleErrorKind, SentexError, SentexErrorKind};
use crate::markdown::BlockScanner;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use std::{fmt, num::NonZeroU64};

const LEGACY_CITATION_MARKER_PREFIX: &str = "[🔍=v1:";
const V2_CLAIM_MARKER_PREFIX: &str = "[🔍=v2:claim:";
const V2_CITATION_MARKER_PREFIX: &str = "[🔍=v2:citation:";
const CLAIM_MARKER_PREFIX: &str = "[🔍=v3:claim:";
const CITATION_MARKER_PREFIX: &str = "[🔍=v3:citation:";
const MARKER_SUFFIX: &str = "]";
/// Unpadded base64url length of all eight big-endian bytes; v1 and v2 tokens
/// always have this length, and it bounds a v3 token.
const FIXED_LOCATOR_TOKEN_LEN: usize = 11;

/// Maximum total number of claim and citation handles accepted on one message.
pub(crate) const MAX_SENTEX_REFS: usize = 4;
/// Maximum UTF-8 bytes occupied by the complete appended v3 marker suffix.
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

    /// Decodes a fixed-width v1/v2 token: all eight big-endian bytes.
    pub(super) fn from_locator_token(token: &str) -> Option<Self> {
        if token.len() != FIXED_LOCATOR_TOKEN_LEN {
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

    /// Decodes a v3 token: one to eight big-endian bytes with no leading zero
    /// byte, in canonical unpadded base64url.
    pub(super) fn from_minimal_locator_token(token: &str) -> Option<Self> {
        if token.len() > FIXED_LOCATOR_TOKEN_LEN {
            return None;
        }
        // The strict decoder rejects padding and nonzero trailing bits, so a
        // decoded token is already in canonical form.
        let decoded = URL_SAFE_NO_PAD.decode(token).ok()?;
        if decoded.first().is_none_or(|byte| *byte == 0) {
            return None;
        }
        let mut bytes = [0u8; 8];
        // At most 11 characters decode to at most 8 bytes.
        let start = bytes.len() - decoded.len();
        bytes[start..].copy_from_slice(&decoded);
        // A nonzero first byte makes the value nonzero, so this `?` never returns early.
        let handle = NonZeroU64::new(u64::from_be_bytes(bytes))?;
        Some(Self(handle))
    }

    fn minimal_bytes(&self) -> ([u8; 8], usize) {
        let leading_zero_bytes = (self.0.leading_zeros() / 8) as usize;
        (self.0.get().to_be_bytes(), leading_zero_bytes)
    }

    /// The v3 token: minimal-width big-endian bytes, unpadded base64url.
    fn locator_token(&self) -> String {
        let (bytes, start) = self.minimal_bytes();
        URL_SAFE_NO_PAD.encode(&bytes[start..])
    }

    /// The length of [`Self::locator_token`] without allocating it.
    fn locator_token_len(&self) -> usize {
        let (bytes, start) = self.minimal_bytes();
        (4 * (bytes.len() - start)).div_ceil(3)
    }

    fn fixed_locator_token(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0.get().to_be_bytes())
    }
}

/// The marker grammar version a locator was written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocatorVersion {
    /// Role-free, fixed-width citations from before the role split.
    V1,
    /// Role-bearing, fixed-width eight-byte tokens.
    V2,
    /// Role-bearing, minimal-width tokens; the only version emitted.
    V3,
}

impl LocatorVersion {
    fn decode(self, token: &str) -> Option<VaeliiSentexHandle> {
        match self {
            Self::V1 | Self::V2 => VaeliiSentexHandle::from_locator_token(token),
            Self::V3 => VaeliiSentexHandle::from_minimal_locator_token(token),
        }
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
    TerminalVisibleRoleSuffixV3AfterHooks,
}

impl SentexTransport {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::TerminalVisibleRoleSuffixV3AfterHooks => {
                "terminal-visible-role-suffix-v3-after-hooks"
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
    fn current(role: SentexRole, handle: &VaeliiSentexHandle) -> Self {
        Self::written(LocatorVersion::V3, role, handle)
    }

    /// Re-renders a decoded locator in its own version's canonical form, which
    /// the canonical decoders guarantee equals the text that was parsed.
    fn written(version: LocatorVersion, role: SentexRole, handle: &VaeliiSentexHandle) -> Self {
        let role_name = match role {
            SentexRole::Claim => "claim",
            SentexRole::Citation => "citation",
        };
        let encoded = match version {
            LocatorVersion::V1 => format!("v1:{}", handle.fixed_locator_token()),
            LocatorVersion::V2 => format!("v2:{role_name}:{}", handle.fixed_locator_token()),
            LocatorVersion::V3 => format!("v3:{role_name}:{}", handle.locator_token()),
        };
        Self { role, encoded }
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
        metadata.push_str(SentexLocator::current(role, handle).as_str());
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
        output.push_str(&handle.locator_token());
        output.push_str(MARKER_SUFFIX);
    }
    output
}

/// Parses only a complete, bounded sequence of exact terminal sentex locators.
/// Legacy v1 evidence markers remain readable and project as citations, and
/// fixed-width v2 markers remain readable alongside current v3 markers.
pub(crate) fn parse_sentex_locators(content: &str) -> Vec<SentexLocator> {
    let mut end = content.len();
    let mut reversed = Vec::new();
    let mut suffix_start = end;

    loop {
        let candidate = &content[..end];
        if !candidate.ends_with(MARKER_SUFFIX) {
            break;
        }
        let Some((start, prefix, role, version)) = marker_start(candidate) else {
            return Vec::new();
        };
        if start > 0 && candidate.as_bytes()[start - 1] != b' ' {
            return Vec::new();
        }
        let token_start = start + prefix.len();
        let token_end = end - MARKER_SUFFIX.len();
        let Some(handle) = version.decode(&content[token_start..token_end]) else {
            return Vec::new();
        };
        reversed.push(SentexLocator::written(version, role, &handle));
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

fn marker_start(candidate: &str) -> Option<(usize, &'static str, SentexRole, LocatorVersion)> {
    [
        (CLAIM_MARKER_PREFIX, SentexRole::Claim, LocatorVersion::V3),
        (
            CITATION_MARKER_PREFIX,
            SentexRole::Citation,
            LocatorVersion::V3,
        ),
        (
            V2_CLAIM_MARKER_PREFIX,
            SentexRole::Claim,
            LocatorVersion::V2,
        ),
        (
            V2_CITATION_MARKER_PREFIX,
            SentexRole::Citation,
            LocatorVersion::V2,
        ),
        (
            LEGACY_CITATION_MARKER_PREFIX,
            SentexRole::Citation,
            LocatorVersion::V1,
        ),
    ]
    .into_iter()
    .filter_map(|(prefix, role, version)| {
        candidate
            .rfind(prefix)
            .map(|start| (start, prefix, role, version))
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

/// Exact UTF-8 bytes [`append_markers`] adds to non-empty content.
pub(super) fn marker_bytes(handles: &SentexHandles) -> usize {
    let marker_content: usize = handles
        .claims()
        .iter()
        .map(|handle| (SentexRole::Claim, handle))
        .chain(
            handles
                .citations()
                .iter()
                .map(|handle| (SentexRole::Citation, handle)),
        )
        .map(|(role, handle)| {
            role.marker_prefix().len() + handle.locator_token_len() + MARKER_SUFFIX.len()
        })
        .sum();
    marker_content + handles.len().saturating_sub(1) + usize::from(!handles.is_empty())
}
