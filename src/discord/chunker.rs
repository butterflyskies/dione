use crate::config::ChunkMode;

/// Splits `text` into chunks of at most `limit` bytes, respecting UTF-8
/// character boundaries and (in `Paragraph` mode) preferring split at
/// paragraph or line boundaries.
///
/// If `text.len() <= limit`, returns `vec![text]` with no allocation.
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

        let (chunk, rest) = remaining.split_at(split_at);
        chunks.push(chunk);
        // Trim leading newlines from the next chunk to avoid starting with
        // a stray newline.
        remaining = rest.trim_start_matches(['\n', '\r']);
    }

    chunks
}

/// Finds the best split point for paragraph mode.
///
/// Priority: `\n\n` > `\n` > ` ` > hard cut at `ceil_char_boundary(limit)`.
fn find_split_paragraph(text: &str, limit: usize) -> usize {
    debug_assert!(text.len() > limit);

    // We search within text[..safe_limit] where safe_limit is the last valid
    // char boundary at or before `limit`.
    let safe_limit = text.floor_char_boundary(limit);

    // Try \n\n (paragraph break).
    if let Some(pos) = text[..safe_limit].rfind("\n\n")
        && pos > 0
    {
        return pos + 2; // include the double newline in the first chunk
    }

    // Try \n (line break).
    if let Some(pos) = text[..safe_limit].rfind('\n')
        && pos > 0
    {
        return pos + 1;
    }

    // Try space.
    if let Some(pos) = text[..safe_limit].rfind(' ')
        && pos > 0
    {
        return pos + 1;
    }

    // Hard cut at the char boundary.
    safe_limit.max(1)
}

/// Finds the hard-cut split point for length mode.
fn find_split_length(text: &str, limit: usize) -> usize {
    text.floor_char_boundary(limit).max(1)
}

// ── Fence-preserving chunking ────────────────────────────────────────────────

/// State of a fenced code block carried across chunk boundaries.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FenceInfo {
    backticks: usize,
    lang: String,
}

/// Check whether `line` is a fence delimiter (``` or more backticks at the
/// start of a line). Returns `(backtick_count, language_tag)` — the tag is
/// non-empty only for opening fences.
fn parse_fence_delimiter(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('`') {
        return None;
    }
    let backtick_count = trimmed.bytes().take_while(|&b| b == b'`').count();
    if backtick_count < 3 {
        return None;
    }
    let rest = trimmed[backtick_count..].trim();
    Some((backtick_count, rest))
}

/// Walk `text` line-by-line and return the fence state at the end, given an
/// initial state (carried from the previous chunk).
fn fence_state_after(text: &str, initial: &Option<FenceInfo>) -> Option<FenceInfo> {
    let mut state = initial.clone();

    for line in text.split('\n') {
        if let Some((count, tag)) = parse_fence_delimiter(line) {
            match &state {
                None => {
                    state = Some(FenceInfo {
                        backticks: count,
                        lang: tag.to_string(),
                    });
                }
                Some(open) => {
                    // A closing fence needs >= the opening's backtick count
                    // and no content after the backticks.
                    if count >= open.backticks && tag.is_empty() {
                        state = None;
                    }
                }
            }
        }
    }

    state
}

/// Scan `text` for fence delimiters and return the worst-case byte overhead
/// that closing and reopening a fence at a chunk boundary would add.
fn max_fence_overhead(text: &str) -> usize {
    let mut worst = 0;

    for line in text.split('\n') {
        if let Some((count, tag)) = parse_fence_delimiter(line) {
            // close: \n + backticks; reopen: backticks + tag + \n
            let overhead = 1 + count + count + tag.len() + 1;
            worst = worst.max(overhead);
        }
    }

    worst
}

/// Chunk text while preserving fenced Markdown code blocks.
///
/// When a split would leave a code fence open, the fence is closed at the end
/// of the current chunk and reopened (with its language tag) at the start of
/// the next. Injected markers are presentation-only — the textual payload is
/// unchanged.
///
/// Falls back to [`chunk`] for content that contains no fences.
pub fn chunk_preserving_fences(text: &str, limit: usize, mode: ChunkMode) -> Vec<String> {
    if limit == 0 {
        return vec![];
    }
    if text.len() <= limit {
        return vec![text.to_string()];
    }

    // Fast path: no fences at all.
    if !text.contains("```") {
        return chunk(text, limit, mode)
            .into_iter()
            .map(|s| s.to_string())
            .collect();
    }

    let overhead = max_fence_overhead(text);
    if overhead == 0 {
        // Contains ``` but no valid fence delimiters.
        return chunk(text, limit, mode)
            .into_iter()
            .map(|s| s.to_string())
            .collect();
    }

    // Reduce the limit so that injected fence markers always fit.
    let reduced = limit.saturating_sub(overhead);
    if reduced == 0 {
        return vec![text.to_string()];
    }

    let raw_chunks = chunk(text, reduced, mode);

    let mut result = Vec::with_capacity(raw_chunks.len());
    let mut carry: Option<FenceInfo> = None;

    for (i, raw) in raw_chunks.iter().enumerate() {
        let is_last = i == raw_chunks.len() - 1;
        let mut buf = String::new();

        // Reopen fence carried from the previous chunk.
        if let Some(ref f) = carry {
            buf.push_str(&"`".repeat(f.backticks));
            if !f.lang.is_empty() {
                buf.push_str(&f.lang);
            }
            buf.push('\n');
        }

        buf.push_str(raw);

        // Determine fence state at the end of this chunk.
        let end_state = fence_state_after(raw, &carry);

        // Close the open fence unless this is the last chunk (where the
        // original text already closes it, or the author left it open).
        if end_state.is_some() && !is_last {
            buf.push('\n');
            buf.push_str(&"`".repeat(end_state.as_ref().unwrap().backticks));
        }

        carry = if is_last { None } else { end_state };
        result.push(buf);
    }

    result
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChunkMode;

    #[test]
    fn test_under_limit_single_chunk() {
        let text = "hello world";
        let chunks = chunk(text, 100, ChunkMode::Length);
        assert_eq!(chunks, vec!["hello world"]);
    }

    #[test]
    fn test_exact_limit_no_split() {
        let text = "hello";
        let chunks = chunk(text, 5, ChunkMode::Length);
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn test_over_limit_splits() {
        let text = "abcdefghij";
        let chunks = chunk(text, 4, ChunkMode::Length);
        // "abcd", "efgh", "ij"
        for c in &chunks {
            assert!(c.len() <= 4, "chunk too long: {:?}", c);
        }
        assert_eq!(chunks.join(""), text);
    }

    #[test]
    fn test_paragraph_mode_prefers_double_newline() {
        let text = "first paragraph\n\nsecond paragraph that is long enough to matter";
        let limit = 25;
        let chunks = chunk(text, limit, ChunkMode::Paragraph);
        // First chunk should end at the paragraph break.
        assert!(
            chunks[0].contains("first paragraph"),
            "first chunk: {:?}",
            chunks[0]
        );
        // No chunk should exceed the limit.
        for c in &chunks {
            assert!(
                c.len() <= limit,
                "chunk too long ({} > {}): {:?}",
                c.len(),
                limit,
                c
            );
        }
        // Reassembled text should equal original (modulo stripped leading newlines).
        let rejoined = chunks.join("\n\n");
        // Content should be preserved.
        assert!(rejoined.contains("first paragraph"));
        assert!(rejoined.contains("second paragraph"));
    }

    #[test]
    fn test_no_split_point_hard_cut() {
        // A long word with no spaces or newlines — must hard-cut.
        let text = "abcdefghijklmnopqrstuvwxyz";
        let chunks = chunk(text, 5, ChunkMode::Paragraph);
        for c in &chunks {
            assert!(c.len() <= 5, "chunk too long: {:?}", c);
            assert!(!c.is_empty());
        }
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn test_multibyte_never_splits_mid_codepoint() {
        // 4-byte emoji: U+1F600 = 😀
        let emoji = "😀".repeat(10); // 40 bytes
        // limit = 10 bytes; each emoji is 4 bytes, so we can fit 2 per chunk
        let chunks = chunk(&emoji, 10, ChunkMode::Length);
        for c in chunks {
            // Each chunk must be valid UTF-8.
            assert!(
                std::str::from_utf8(c.as_bytes()).is_ok(),
                "invalid UTF-8: {:?}",
                c
            );
        }
    }

    // ── Fence-preserving tests ───────────────────────────────────────────

    #[test]
    fn fence_no_fences_matches_plain_chunk() {
        let text = "hello world this is a long message without any fences";
        let limit = 20;
        let plain: Vec<String> = chunk(text, limit, ChunkMode::Paragraph)
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        let fenced = chunk_preserving_fences(text, limit, ChunkMode::Paragraph);
        assert_eq!(plain, fenced);
    }

    #[test]
    fn fence_within_single_chunk() {
        let text = "```rust\nfn main() {}\n```";
        let chunks = chunk_preserving_fences(text, 200, ChunkMode::Paragraph);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], text);
    }

    #[test]
    fn fence_split_across_chunks() {
        let text = "before\n```\nline one\nline two\nline three\nline four\n```\nafter";
        let limit = 30;
        let chunks = chunk_preserving_fences(text, limit, ChunkMode::Paragraph);

        // First chunk should close the fence.
        assert!(
            chunks[0].ends_with("```"),
            "first chunk should close fence: {:?}",
            chunks[0]
        );

        // A middle or second chunk that carries the fence should reopen it.
        let has_reopen = chunks[1..].iter().any(|c| c.starts_with("```"));
        assert!(has_reopen, "subsequent chunk should reopen fence: {chunks:?}");

        // No chunk should exceed the limit.
        for c in &chunks {
            assert!(
                c.len() <= limit,
                "chunk too long ({} > {}): {:?}",
                c.len(),
                limit,
                c
            );
        }
    }

    #[test]
    fn fence_language_tag_preserved() {
        let text = "text\n```python\ndef hello():\n    pass\ndef world():\n    pass\n```\nend";
        let limit = 40;
        let chunks = chunk_preserving_fences(text, limit, ChunkMode::Paragraph);

        // At least one continuation chunk should reopen with the language tag.
        let has_lang_reopen = chunks[1..].iter().any(|c| c.starts_with("```python"));
        assert!(
            has_lang_reopen,
            "language tag should be preserved on reopen: {chunks:?}"
        );
    }

    #[test]
    fn fence_multiple_blocks_only_broken_one_repaired() {
        // Two fences: the first fits in chunk 1, the second spans chunks.
        let text = "```\nshort\n```\n\nsome padding text here\n\n```js\nlong line of code that needs to be split across chunks\n```";
        let limit = 50;
        let chunks = chunk_preserving_fences(text, limit, ChunkMode::Paragraph);

        for c in &chunks {
            assert!(
                c.len() <= limit,
                "chunk too long ({} > {}): {:?}",
                c.len(),
                limit,
                c
            );
        }
    }

    #[test]
    fn fence_under_limit_returns_single_chunk() {
        let text = "```\ncode\n```";
        let chunks = chunk_preserving_fences(text, 2000, ChunkMode::Paragraph);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], text);
    }

    #[test]
    fn fence_empty_and_zero_limit() {
        assert!(chunk_preserving_fences("", 100, ChunkMode::Paragraph).is_empty()
            || chunk_preserving_fences("", 100, ChunkMode::Paragraph) == vec![""]);
        assert!(chunk_preserving_fences("hello", 0, ChunkMode::Paragraph).is_empty());
    }

    #[test]
    fn fence_extended_backticks() {
        // 4-backtick fence should be matched by 4-backtick close.
        let text = "````rust\nfn main() {\n    println!(\"```\");\n}\n````\nafter";
        let limit = 30;
        let chunks = chunk_preserving_fences(text, limit, ChunkMode::Paragraph);

        // If split, reopened fence should use 4 backticks.
        if chunks.len() > 1 {
            let has_4tick = chunks[1..].iter().any(|c| c.starts_with("````"));
            assert!(has_4tick, "extended backtick count should be preserved: {chunks:?}");
        }
    }
}
