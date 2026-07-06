//! Small shared utilities.

/// Truncate `s` to at most `max` characters, appending an ellipsis when it was
/// shortened. Operates on `char`s so multi-byte codepoints are never split.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max).collect();
    format!("{truncated}\u{2026}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_char_boundaries() {
        // Multi-byte chars must not be split; an ellipsis marks truncation.
        let s = "é".repeat(10);
        let out = truncate_chars(&s, 4);
        assert_eq!(out.chars().count(), 5, "4 kept chars + ellipsis");
        assert!(out.ends_with('\u{2026}'));
        // Short strings pass through unchanged.
        assert_eq!(truncate_chars("hello", 10), "hello");
    }
}
