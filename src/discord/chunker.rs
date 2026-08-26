use crate::config::ChunkMode;
use std::ops::Range;
use thiserror::Error;

/// Splits `text` into chunks of at most `limit` bytes, respecting UTF-8
/// character boundaries and (in `Paragraph` mode) preferring paragraph or
/// line boundaries.
///
/// If `text.len() <= limit`, returns `vec![text]` without allocation.
pub fn chunk(text: &str, limit: usize, mode: ChunkMode) -> Vec<&str> {
    if limit == 0 {
        return vec![];
    }
    if text.len() <= limit {
        return vec![text];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;
    while !remaining.is_empty() {
        if remaining.len() <= limit {
            chunks.push(remaining);
            break;
        }
        let split_at = match mode {
            ChunkMode::Paragraph => find_split_paragraph(remaining, limit),
            ChunkMode::Length => find_split_length(remaining, limit),
        };
        let (next, rest) = remaining.split_at(split_at);
        chunks.push(next);
        remaining = rest.trim_start_matches(['\n', '\r']);
    }
    chunks
}

/// Find the best split point: paragraph, line, space, then a hard UTF-8 cut.
fn find_split_paragraph(text: &str, limit: usize) -> usize {
    debug_assert!(text.len() > limit);
    let safe_limit = text.floor_char_boundary(limit);
    if let Some(pos) = text[..safe_limit].rfind("\n\n")
        && pos > 0
    {
        return pos + 2;
    }
    if let Some(pos) = text[..safe_limit].rfind('\n')
        && pos > 0
    {
        return pos + 1;
    }
    if let Some(pos) = text[..safe_limit].rfind(' ')
        && pos > 0
    {
        return pos + 1;
    }
    safe_limit.max(1)
}

fn find_split_length(text: &str, limit: usize) -> usize {
    text.floor_char_boundary(limit).max(1)
}

/// One Discord-ready chunk and the exact original bytes it represents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FenceChunk {
    /// Text to publish, including presentation-only close/reopen markers.
    pub rendered: String,
    /// Byte range in the caller's original text represented by this chunk.
    pub source: Range<usize>,
    /// Fence state active immediately before this chunk's source bytes.
    pub(crate) incoming: Option<FenceContext>,
}

/// Why fenced Markdown cannot be represented safely as inline Discord text.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum FenceChunkError {
    /// A delimiter or one UTF-8 scalar plus required presentation markers
    /// cannot fit within the configured inline limit.
    #[error(
        "message requires attachment fallback: fenced Markdown cannot fit within {limit}-byte chunks"
    )]
    InlineImpossible { limit: usize },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FenceContext {
    backticks: usize,
    lang: String,
}

/// Parse a structural CommonMark fence delimiter. At most three leading
/// spaces are allowed; tabs and four-space indentation are content.
fn parse_fence_delimiter(line: &str) -> Option<(usize, &str)> {
    let indentation = line.bytes().take_while(|byte| *byte == b' ').count();
    if indentation > 3 {
        return None;
    }
    let structural = &line[indentation..];
    let backticks = structural.bytes().take_while(|byte| *byte == b'`').count();
    if backticks < 3 {
        return None;
    }
    Some((backticks, structural[backticks..].trim()))
}

pub(crate) fn advance_fence(state: &Option<FenceContext>, line: &str) -> Option<FenceContext> {
    let Some((backticks, tag)) = parse_fence_delimiter(line) else {
        return state.clone();
    };
    match state {
        None => Some(FenceContext {
            backticks,
            lang: tag.to_string(),
        }),
        Some(open) if backticks >= open.backticks && tag.is_empty() => None,
        Some(_) => state.clone(),
    }
}

fn reopen_marker(state: &FenceContext) -> String {
    let mut marker = "`".repeat(state.backticks);
    marker.push_str(&state.lang);
    marker.push('\n');
    marker
}

fn close_marker(state: &FenceContext, raw: &str) -> String {
    let mut marker = String::new();
    if !raw.ends_with('\n') {
        marker.push('\n');
    }
    marker.push_str(&"`".repeat(state.backticks));
    marker
}

fn source_line_end(text: &str, start: usize) -> usize {
    text[start..]
        .find('\n')
        .map_or(text.len(), |offset| start + offset + 1)
}

fn split_piece(text: &str, limit: usize, mode: ChunkMode) -> Option<usize> {
    let safe_limit = text.floor_char_boundary(limit.min(text.len()));
    if safe_limit == 0 {
        return None;
    }
    if text.len() <= safe_limit {
        return Some(text.len());
    }
    let split = match mode {
        ChunkMode::Length => safe_limit,
        ChunkMode::Paragraph => {
            if let Some(pos) = text[..safe_limit].rfind("\n\n").filter(|pos| *pos > 0) {
                pos + 2
            } else if let Some(pos) = text[..safe_limit].rfind('\n').filter(|pos| *pos > 0) {
                pos + 1
            } else if let Some(pos) = text[..safe_limit].rfind(' ').filter(|pos| *pos > 0) {
                pos + 1
            } else {
                safe_limit
            }
        }
    };
    Some(split)
}

fn render_chunk(
    text: &str,
    source: Range<usize>,
    incoming: &Option<FenceContext>,
    outgoing: &Option<FenceContext>,
) -> String {
    let raw = &text[source.clone()];
    let mut rendered = incoming.as_ref().map_or_else(String::new, reopen_marker);
    rendered.push_str(raw);
    if source.end < text.len()
        && let Some(state) = outgoing
    {
        rendered.push_str(&close_marker(state, raw));
    }
    rendered
}

/// Chunk text while keeping structural fence delimiters atomic.
///
/// Presentation-only close/reopen markers keep every published chunk valid,
/// while [`FenceChunk::source`] retains the untouched logical remainder for a
/// partial-delivery retry. If the configured limit cannot represent the
/// fenced text safely, returns a deterministic attachment-fallback signal.
pub fn chunk_preserving_fences(
    text: &str,
    limit: usize,
    mode: ChunkMode,
) -> Result<Vec<FenceChunk>, FenceChunkError> {
    chunk_preserving_fences_with_context(text, limit, mode, None)
}

/// Resume fence-preserving chunking from a prior chunk's incoming state.
pub(crate) fn chunk_preserving_fences_with_context(
    text: &str,
    limit: usize,
    mode: ChunkMode,
    initial_context: Option<FenceContext>,
) -> Result<Vec<FenceChunk>, FenceChunkError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let has_structural_fence = text
        .split_inclusive('\n')
        .any(|line| parse_fence_delimiter(line).is_some());
    if initial_context.is_none() && !has_structural_fence {
        let base = text.as_ptr() as usize;
        return Ok(chunk(text, limit, mode)
            .into_iter()
            .map(|part| {
                let start = part.as_ptr() as usize - base;
                FenceChunk {
                    rendered: part.to_string(),
                    source: start..start + part.len(),
                    incoming: None,
                }
            })
            .collect());
    }

    let mut chunks = Vec::new();
    let mut source_start = 0;
    let mut position = 0;
    let mut incoming = initial_context;
    let mut outgoing = incoming.clone();

    while position < text.len() {
        let line_end = source_line_end(text, position);
        let line = &text[position..line_end];
        let delimiter = parse_fence_delimiter(line).is_some();
        let candidate_state = advance_fence(&outgoing, line);
        let candidate = render_chunk(text, source_start..line_end, &incoming, &candidate_state);
        if candidate.len() <= limit {
            outgoing = candidate_state;
            position = line_end;
            continue;
        }

        if position > source_start {
            let rendered = render_chunk(text, source_start..position, &incoming, &outgoing);
            debug_assert!(rendered.len() <= limit);
            chunks.push(FenceChunk {
                rendered,
                source: source_start..position,
                incoming: incoming.clone(),
            });
            source_start = position;
            incoming = outgoing.clone();
            continue;
        }

        if delimiter {
            return Err(FenceChunkError::InlineImpossible { limit });
        }

        let prefix_len = incoming
            .as_ref()
            .map_or(0, |state| reopen_marker(state).len());
        let suffix_len = incoming.as_ref().map_or(0, |state| 1 + state.backticks);
        let available = limit.saturating_sub(prefix_len + suffix_len);
        let split = split_piece(line, available, mode)
            .ok_or(FenceChunkError::InlineImpossible { limit })?;
        let end = position + split;
        let rendered = render_chunk(text, source_start..end, &incoming, &outgoing);
        if rendered.len() > limit {
            return Err(FenceChunkError::InlineImpossible { limit });
        }
        chunks.push(FenceChunk {
            rendered,
            source: source_start..end,
            incoming: incoming.clone(),
        });
        source_start = end;
        position = end;
        incoming = outgoing.clone();
    }

    if source_start < text.len() || (text.is_empty() && chunks.is_empty()) {
        let rendered = render_chunk(text, source_start..text.len(), &incoming, &outgoing);
        if rendered.len() > limit {
            return Err(FenceChunkError::InlineImpossible { limit });
        }
        chunks.push(FenceChunk {
            rendered,
            source: source_start..text.len(),
            incoming,
        });
    }

    debug_assert!(chunks.iter().all(|chunk| chunk.rendered.len() <= limit));
    Ok(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fenced(text: &str, limit: usize, mode: ChunkMode) -> Vec<FenceChunk> {
        chunk_preserving_fences(text, limit, mode).expect("inline chunks")
    }

    #[test]
    fn plain_chunking_stays_unchanged() {
        let text = "hello world this is a long message without any fences";
        let plain = chunk(text, 20, ChunkMode::Paragraph);
        let preserved = fenced(text, 20, ChunkMode::Paragraph);
        assert_eq!(
            plain,
            preserved
                .iter()
                .map(|chunk| chunk.rendered.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn inline_backticks_still_use_plain_chunking() {
        let text = "before ``` inline code ``` after enough text to split";
        let plain = chunk(text, 18, ChunkMode::Paragraph);
        let preserved = fenced(text, 18, ChunkMode::Paragraph);
        assert_eq!(
            plain,
            preserved
                .iter()
                .map(|chunk| chunk.rendered.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn plain_chunk_boundaries_and_utf8_are_preserved() {
        assert_eq!(
            chunk("hello world", 100, ChunkMode::Length),
            vec!["hello world"]
        );
        assert_eq!(chunk("hello", 5, ChunkMode::Length), vec!["hello"]);

        let chunks = chunk("abcdefghij", 4, ChunkMode::Length);
        assert!(chunks.iter().all(|chunk| chunk.len() <= 4));
        assert_eq!(chunks.concat(), "abcdefghij");

        let word = "abcdefghijklmnopqrstuvwxyz";
        let chunks = chunk(word, 5, ChunkMode::Paragraph);
        assert!(
            chunks
                .iter()
                .all(|chunk| !chunk.is_empty() && chunk.len() <= 5)
        );
        assert_eq!(chunks.concat(), word);

        let emoji = "😀".repeat(10);
        assert!(
            chunk(&emoji, 10, ChunkMode::Length)
                .iter()
                .all(|chunk| std::str::from_utf8(chunk.as_bytes()).is_ok())
        );
    }

    #[test]
    fn paragraph_mode_prefers_paragraph_boundary() {
        let text = "first paragraph\n\nsecond paragraph that is long enough to matter";
        let chunks = chunk(text, 25, ChunkMode::Paragraph);
        assert!(chunks[0].contains("first paragraph"));
        assert!(chunks.iter().all(|chunk| chunk.len() <= 25));
        let rejoined = chunks.join("\n\n");
        assert!(rejoined.contains("first paragraph"));
        assert!(rejoined.contains("second paragraph"));
    }

    #[test]
    fn fenced_chunks_are_bounded_and_preserve_source() {
        let text = "before\n```rust\nline one\nline two\nline three\nline four\n```\nafter";
        let chunks = fenced(text, 30, ChunkMode::Paragraph);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|chunk| chunk.rendered.len() <= 30));
        assert!(
            chunks[1..]
                .iter()
                .any(|chunk| chunk.rendered.starts_with("```rust\n"))
        );
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| &text[chunk.source.clone()])
                .collect::<String>(),
            text
        );
    }

    #[test]
    fn splits_before_inside_and_after_fence_near_discord_limit() {
        let cases = [
            format!("{}\n```rust\ncode\n```", "a".repeat(1_995)),
            format!("```rust\n{}\n```", "b".repeat(2_010)),
            format!("```rust\ncode\n```\n{}", "c".repeat(1_995)),
        ];

        for text in cases {
            let chunks = fenced(&text, 2_000, ChunkMode::Paragraph);
            assert!(chunks.len() > 1);
            assert!(chunks.iter().all(|chunk| chunk.rendered.len() <= 2_000));
            assert_eq!(
                chunks
                    .iter()
                    .map(|chunk| &text[chunk.source.clone()])
                    .collect::<String>(),
                text
            );
        }
    }

    #[test]
    fn length_mode_keeps_delimiter_atomic() {
        let text = "1234567890\n```rust\nabcdefghijklmnop\n```\nafter";
        let chunks = fenced(text, 20, ChunkMode::Length);
        assert!(chunks.iter().all(|chunk| chunk.rendered.len() <= 20));
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.rendered.contains("```rust"))
        );
        assert!(
            !chunks
                .iter()
                .any(|chunk| chunk.rendered.contains("```ru\n"))
        );
    }

    #[test]
    fn mixed_fence_widths_never_exceed_limit() {
        let text = "```xx\nabcdefghijklm\nq\n```\n````\nabcdefgh\n````\nafter";
        let chunks = fenced(text, 30, ChunkMode::Paragraph);
        assert!(chunks.iter().all(|chunk| chunk.rendered.len() <= 30));
    }

    #[test]
    fn four_space_backticks_are_not_structural() {
        let text = "```rust\nline\n    ```\nstill code\n```\nafter";
        let chunks = fenced(text, 25, ChunkMode::Paragraph);
        assert!(chunks.iter().all(|chunk| chunk.rendered.len() <= 25));
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.rendered.contains("    ```"))
        );
    }

    #[test]
    fn impossible_fence_returns_fallback_signal_without_panicking() {
        let text = format!("```{}\n😀xx", "a".repeat(1_991));
        assert_eq!(
            chunk_preserving_fences(&text, 2_000, ChunkMode::Length),
            Err(FenceChunkError::InlineImpossible { limit: 2_000 })
        );
    }

    #[test]
    fn partial_retry_preserves_original_source_and_incoming_fence() {
        let text = "before\n```rust\none two three four five six seven\n```\nafter";
        let chunks = fenced(text, 28, ChunkMode::Paragraph);
        let failed_index = 1;
        let remainder = &text[chunks[failed_index].source.start..];
        assert_eq!(
            remainder,
            chunks[failed_index..]
                .iter()
                .map(|chunk| &text[chunk.source.clone()])
                .collect::<String>()
        );

        let retry = chunk_preserving_fences_with_context(
            remainder,
            28,
            ChunkMode::Paragraph,
            chunks[failed_index].incoming.clone(),
        )
        .expect("retry chunks");
        assert_eq!(
            retry
                .iter()
                .map(|chunk| &remainder[chunk.source.clone()])
                .collect::<String>(),
            remainder
        );
        for chunk in &retry {
            let mut state = None;
            for line in chunk.rendered.split_inclusive('\n') {
                if line.contains("after") {
                    assert_eq!(state, None, "post-fence text must render outside code");
                }
                state = advance_fence(&state, line);
            }
            assert_eq!(state, None, "every wire chunk must be fence-balanced");
        }
    }

    #[test]
    fn multibyte_content_always_splits_on_scalar_boundaries() {
        let text = "```txt\n😀😀😀😀😀\n```";
        let chunks = fenced(text, 18, ChunkMode::Length);
        assert!(chunks.iter().all(|chunk| chunk.rendered.len() <= 18));
    }

    #[test]
    fn zero_limit_has_no_inline_chunks() {
        assert!(fenced("hello", 0, ChunkMode::Paragraph).is_empty());
    }
}
