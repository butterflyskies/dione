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
}
