//! Markdown block structure, in one place.
//!
//! Three call sites need to know whether a given line is ordinary prose or
//! excluded syntax: the Discord chunker (fences only, so it can reopen a split
//! fence), evidence parsing (so a locator inside code or a quote is not read as
//! an offered claim), and the status lint (so an example is not audited as an
//! asserted work state).
//!
//! Each of the three grew its own answer, and they disagreed. The chunker knew
//! that at most three leading spaces may precede a fence; evidence knew that an
//! inline span carries across lines and that indented code cannot interrupt a
//! paragraph; the status lint knew neither and reimplemented both badly. This
//! module is the single contract, and the other three drive it.
//!
//! # The composition rule
//!
//! Excluded syntax must neither be treated as content **nor mutate the state
//! that decides later content**. Those are separate failures and the second is
//! the quiet one: a fence delimiter line that leaves an inline span open makes
//! the *next* ordinary line unreadable, long after the excluded line is gone.

/// What one line does to fence state, decided without touching the open
/// fence's tag.
///
/// Callers that only need "am I inside a fence" drive this and keep an
/// `Option<usize>`. [`crate::discord::chunker::advance_fence`] is the owning
/// form, for the one caller that must reproduce the delimiter later.
pub(crate) enum FenceTransition<'a> {
    /// Not a fence delimiter: state is unchanged.
    Unchanged,
    /// Opens a fence of this width, carrying this tag.
    Opens { backticks: usize, tag: &'a str },
    /// Closes the fence that was open.
    Closes,
}

/// Parses a structural CommonMark fence delimiter. At most three leading
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

/// True when the line is a structural fence delimiter, open or close.
pub(crate) fn is_fence_delimiter(line: &str) -> bool {
    parse_fence_delimiter(line).is_some()
}

/// Classifies `line` against an open fence of `open` backticks, if any.
///
/// The fence grammar lives here and only here: at most three leading spaces,
/// at least three backticks, and a closing delimiter at least as wide as its
/// opener carrying no trailing tag.
pub(crate) fn fence_transition(open: Option<usize>, line: &str) -> FenceTransition<'_> {
    let Some((backticks, tag)) = parse_fence_delimiter(line) else {
        return FenceTransition::Unchanged;
    };
    match open {
        None => FenceTransition::Opens { backticks, tag },
        Some(width) if backticks >= width && tag.is_empty() => FenceTransition::Closes,
        Some(_) => FenceTransition::Unchanged,
    }
}

/// Walks the inline code-span byte ranges of one line, carrying delimiter
/// state across lines so a multiline span stays code on its continuation
/// lines, and hands each completed range to `on_span`.
///
/// A backtick run preceded by an odd number of backslashes is escaped and does
/// not open a span.
///
/// Allocation-free by construction. Evidence parsing wants only the delimiter
/// state, and it sees MCP content that is unbounded at that point — it reaches
/// the parser before Discord's size rejection and under no status-lint
/// ceiling. Collecting a `Vec` per line there paid, in tuples alone, several
/// times the size of the input for a value that was immediately discarded.
fn walk_inline_code_spans(
    line: &str,
    delimiter: &mut Option<usize>,
    mut on_span: impl FnMut(usize, usize),
) {
    let bytes = line.as_bytes();
    let mut span_start = delimiter.is_some().then_some(0usize);
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
            Some(opening_length) if opening_length == run_length => {
                *delimiter = None;
                if let Some(begin) = span_start.take() {
                    on_span(begin, index);
                }
            }
            None => {
                *delimiter = Some(run_length);
                span_start = Some(start);
            }
            Some(_) => {}
        }
    }

    if delimiter.is_some()
        && let Some(begin) = span_start
    {
        on_span(begin, bytes.len());
    }
}

/// Collecting form of the span walk, for callers that need the ranges
/// themselves rather than only the delimiter state.
#[cfg(test)]
pub(crate) fn inline_code_spans(line: &str, delimiter: &mut Option<usize>) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    walk_inline_code_spans(line, delimiter, |begin, end| spans.push((begin, end)));
    spans
}

/// How one line participates in the document's block structure.
///
/// Only [`LineClass::Content`] is ordinary prose. Every other class is
/// excluded syntax: it is not audited, and it does not advance inline state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LineClass {
    /// Ordinary paragraph or heading content. Inline spans were scanned.
    Content,
    /// A blockquote line. Whether an unterminated span in it reaches the
    /// following block depends on the scanner's [`InlineScope`].
    Quote,
    /// A fenced-code delimiter line, or a line inside a fence.
    Fenced,
    /// A line of an indented code block.
    IndentedCode,
    /// A blank line.
    Blank,
}

impl LineClass {
    /// True when the line is ordinary prose a caller may audit.
    pub(crate) fn is_content(self) -> bool {
        matches!(self, LineClass::Content)
    }
}

/// A single-pass Markdown block-structure walker.
///
/// Feed it lines in order; it reports each line's [`LineClass`] and carries
/// fence, quote, indented-code, paragraph and inline-span state between them.
///
/// # Fence and indented-code lines do not advance inline state
///
/// This is the property the status lint was missing. A ` ``` ` opener is not
/// prose, so its backticks are not span delimiters; a four-space-indented
/// backtick is literal code content. Scanning either one leaves a span open
/// across the whole rest of the message.
///
/// # Indented code cannot interrupt a paragraph
///
/// CommonMark's rule, and the reason `in_paragraph` is tracked: an indented
/// line directly after prose is a lazy continuation of that paragraph, so its
/// backticks *are* span delimiters. After a blank line, a heading, a quote or
/// a fence close, the same line is code.
///
/// # Quote scoping is a consumer choice — see [`InlineScope`]
///
/// The two consumers want opposite things from an unterminated backtick inside
/// a blockquote, and both are defensible, so the scanner takes it as a
/// parameter rather than picking one and being wrong for somebody.
pub(crate) struct BlockScanner {
    fence: Option<usize>,
    inline: Option<usize>,
    quote_rest: bool,
    last_line_was_quote: bool,
    in_indented_code: bool,
    in_paragraph: bool,
    inline_scope: InlineScope,
}

/// Whether an inline code span opened inside a blockquote may escape it.
///
/// CommonMark says it may not — a blockquote is a container and its inline
/// context ends with the block. But the two consumers here are protecting
/// different things, and the difference is which way each one fails.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum InlineScope {
    /// A span opened in a quote continues into the following blocks.
    ///
    /// Evidence parsing's long-standing behaviour. It is not CommonMark, and
    /// it is kept anyway because it fails **closed**: an escaped span causes a
    /// locator to be *rejected*, never accepted. Narrowing it would loosen a
    /// security check, which is not a side effect a lint fix gets to have.
    AcrossBlocks,
    /// A span is confined to the block that opened it.
    ///
    /// What the status lint needs. Its contract is that excluded syntax stays
    /// exempt *and* does not reach forward: a quoted example must not be
    /// audited, and must not mask the asserted status line after it. Failing
    /// open here costs a missed finding, not an accepted claim.
    WithinBlock,
}

impl BlockScanner {
    /// A scanner with evidence parsing's semantics.
    pub(crate) fn new() -> Self {
        Self::with_inline_scope(InlineScope::AcrossBlocks)
    }

    /// A scanner whose inline state does not cross a blockquote boundary.
    pub(crate) fn block_scoped() -> Self {
        Self::with_inline_scope(InlineScope::WithinBlock)
    }

    fn with_inline_scope(inline_scope: InlineScope) -> Self {
        Self {
            fence: None,
            inline: None,
            quote_rest: false,
            last_line_was_quote: false,
            in_indented_code: false,
            in_paragraph: false,
            inline_scope,
        }
    }

    /// Advances by one line, discarding span ranges.
    pub(crate) fn push(&mut self, line: &str) -> LineClass {
        self.push_with_spans(line, |_, _| {})
    }

    /// Advances by one line, reporting the inline code-span byte ranges found
    /// within it. Ranges are only ever reported for scanned classes; excluded
    /// syntax reports none, because it was never scanned.
    pub(crate) fn push_with_spans(
        &mut self,
        line: &str,
        on_span: impl FnMut(usize, usize),
    ) -> LineClass {
        let structural = line.trim_start();
        let indentation = line
            .bytes()
            .take_while(|byte| *byte == b' ' || *byte == b'\t')
            .count();
        let line_is_indented = indentation >= 4 || line.starts_with('\t');

        // Quote detection is gated on being outside code, so a `>` inside a
        // fenced example cannot silence the rest of the message.
        let outside_code = self.fence.is_none() && self.inline.is_none();
        self.last_line_was_quote = outside_code && structural.starts_with('>');
        if outside_code && structural.starts_with(">>>") {
            self.quote_rest = true;
        }

        // Inside a fence: everything is code content, including the closing
        // delimiter. No inline scan — that is the composition rule.
        if let Some(width) = self.fence {
            if matches!(fence_transition(Some(width), line), FenceTransition::Closes) {
                self.fence = None;
                self.in_paragraph = false;
            }
            self.in_indented_code = false;
            return LineClass::Fenced;
        }

        if structural.is_empty() {
            self.in_indented_code = false;
            self.in_paragraph = false;
            return LineClass::Blank;
        }

        // An opening fence, likewise, is delimiter syntax rather than prose.
        // Gated on no open inline span: inside `` `a ``, a ``` is span content.
        if self.inline.is_none()
            && let FenceTransition::Opens { backticks, .. } = fence_transition(None, line)
        {
            self.fence = Some(backticks);
            self.in_indented_code = false;
            self.in_paragraph = false;
            return LineClass::Fenced;
        }

        if line_is_indented
            && (self.in_indented_code || !self.in_paragraph)
            && self.inline.is_none()
        {
            self.in_indented_code = true;
            return LineClass::IndentedCode;
        }
        self.in_indented_code = false;

        // ATX headings end the paragraph, so the next indented line is code.
        if is_atx_heading(structural) {
            self.in_paragraph = false;
            walk_inline_code_spans(line, &mut self.inline, on_span);
            return LineClass::Content;
        }

        if self.last_line_was_quote {
            self.in_paragraph = false;
            match self.inline_scope {
                InlineScope::AcrossBlocks => {
                    walk_inline_code_spans(line, &mut self.inline, on_span);
                }
                InlineScope::WithinBlock => {
                    // The quote is its own block, so it gets its own inline
                    // context and whatever it leaves open dies with it.
                    // `self.inline` is necessarily `None` here — quote
                    // classification is gated on being outside code — so this
                    // starts fresh rather than discarding live state.
                    let mut block_local = None;
                    walk_inline_code_spans(line, &mut block_local, on_span);
                }
            }
            return LineClass::Quote;
        }

        self.in_paragraph = true;
        walk_inline_code_spans(line, &mut self.inline, on_span);
        LineClass::Content
    }

    /// True while a `>>>` multiline quote is in force.
    pub(crate) fn quote_rest(&self) -> bool {
        self.quote_rest
    }

    /// True when the position after the consumed lines sits inside a quote,
    /// a fence, an inline span, or an indented code block.
    pub(crate) fn inside_quote_or_code(&self) -> bool {
        self.last_line_was_quote
            || self.quote_rest
            || self.fence.is_some()
            || self.inline.is_some()
            || self.in_indented_code
    }
}

fn is_atx_heading(structural: &str) -> bool {
    let hashes = structural.bytes().take_while(|byte| *byte == b'#').count();
    (1..=6).contains(&hashes)
        && (structural.len() == hashes || structural.as_bytes()[hashes] == b' ')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classes(text: &str) -> Vec<LineClass> {
        let mut scanner = BlockScanner::new();
        text.split('\n').map(|line| scanner.push(line)).collect()
    }

    #[test]
    fn a_fence_delimiter_does_not_open_an_inline_span() {
        // The exact shape Ari found: a 3-backtick opener and a wider closer.
        // `advance_fence` accepted the close on width; an equal-width inline
        // delimiter did not, and swallowed the following line.
        let classes = classes("```\ncode\n````\nprose");
        assert_eq!(
            classes,
            vec![
                LineClass::Fenced,
                LineClass::Fenced,
                LineClass::Fenced,
                LineClass::Content
            ]
        );

        let mut scanner = BlockScanner::new();
        for line in ["```", "code", "````"] {
            scanner.push(line);
        }
        let mut spans = Vec::new();
        let class = scanner.push_with_spans("prose", |begin, end| spans.push((begin, end)));
        assert_eq!(class, LineClass::Content);
        assert!(
            spans.is_empty(),
            "fence delimiters leaked a span: {spans:?}"
        );
    }

    #[test]
    fn indented_code_is_not_content_and_its_backticks_are_literal() {
        let classes = classes("intro\n\n    `literal\nprose");
        assert_eq!(
            classes,
            vec![
                LineClass::Content,
                LineClass::Blank,
                LineClass::IndentedCode,
                LineClass::Content
            ]
        );

        let mut scanner = BlockScanner::new();
        for line in ["intro", "", "    `literal"] {
            scanner.push(line);
        }
        let mut spans = Vec::new();
        scanner.push_with_spans("prose", |begin, end| spans.push((begin, end)));
        assert!(spans.is_empty(), "indented backtick leaked a span");
    }

    #[test]
    fn indented_code_cannot_interrupt_a_paragraph() {
        // No blank line, so the indented line is a lazy continuation: its
        // backtick DOES open a span. CommonMark, and evidence pins it.
        let classes = classes("paragraph\n    `unclosed\nnext");
        assert_eq!(
            classes,
            vec![LineClass::Content, LineClass::Content, LineClass::Content]
        );

        let mut scanner = BlockScanner::new();
        for line in ["paragraph", "    `unclosed"] {
            scanner.push(line);
        }
        assert!(
            scanner.inside_quote_or_code(),
            "a paragraph continuation's backtick must open a span"
        );
    }

    #[test]
    fn a_heading_ends_the_paragraph_so_the_next_indent_is_code() {
        assert_eq!(
            classes("paragraph\n# heading\n    code"),
            vec![
                LineClass::Content,
                LineClass::Content,
                LineClass::IndentedCode
            ]
        );
    }

    #[test]
    fn a_quote_is_not_content_and_a_triple_quote_carries_forward() {
        assert_eq!(
            classes("> quoted\nprose"),
            vec![LineClass::Quote, LineClass::Content]
        );

        let mut scanner = BlockScanner::new();
        scanner.push(">>> everything after this");
        scanner.push("still quoted");
        assert!(scanner.quote_rest());
    }

    #[test]
    fn a_quote_inside_a_fence_does_not_silence_the_rest() {
        assert_eq!(
            classes("```\n>>> example\n```\nprose"),
            vec![
                LineClass::Fenced,
                LineClass::Fenced,
                LineClass::Fenced,
                LineClass::Content
            ]
        );
        let mut scanner = BlockScanner::new();
        for line in ["```", ">>> example", "```"] {
            scanner.push(line);
        }
        assert!(!scanner.quote_rest());
    }

    #[test]
    fn a_four_space_indented_fence_is_content_not_a_delimiter() {
        // Both halves of the composition: it does not close an open fence,
        // and outside one it is indented code rather than an opener.
        assert_eq!(
            classes("```\n    ```\nstill code\n```\nprose"),
            vec![
                LineClass::Fenced,
                LineClass::Fenced,
                LineClass::Fenced,
                LineClass::Fenced,
                LineClass::Content
            ]
        );
        assert_eq!(
            classes("intro\n\n    ```\nprose"),
            vec![
                LineClass::Content,
                LineClass::Blank,
                LineClass::IndentedCode,
                LineClass::Content
            ]
        );
    }

    /// Both halves of the consumer policy, side by side, so a change to one
    /// cannot quietly become a change to both.
    #[test]
    fn quote_scoping_is_the_consumer_s_choice() {
        // Evidence's semantics: the span escapes the quote. Not CommonMark,
        // kept because it fails closed — a locator gets rejected, not accepted.
        let mut across = BlockScanner::new();
        across.push("> quoted `unclosed");
        assert!(
            across.inside_quote_or_code(),
            "AcrossBlocks must let a quote-line span escape"
        );

        // The status lint's semantics: the quote is its own block, so the span
        // dies with it and the following line is readable.
        //
        // The assertion is on the line *after* the quote, not on the scanner
        // state right after it — `inside_quote_or_code` is true there under
        // either policy, because the last line genuinely was a quote. What
        // differs is whether the next line arrives already inside a span.
        let mut within = BlockScanner::block_scoped();
        within.push("> quoted `unclosed");
        let mut spans = Vec::new();
        let class = within.push_with_spans("plain prose", |b, e| spans.push((b, e)));
        assert_eq!(class, LineClass::Content);
        assert!(
            spans.is_empty(),
            "WithinBlock let a quote-line span reach the next line: {spans:?}"
        );
        assert!(!within.inside_quote_or_code());

        // Same input under evidence's policy: the next line IS swallowed.
        let mut across_next = BlockScanner::new();
        across_next.push("> quoted `unclosed");
        let mut escaped_spans = Vec::new();
        across_next.push_with_spans("plain prose", |b, e| escaped_spans.push((b, e)));
        assert_eq!(
            escaped_spans,
            vec![(0, "plain prose".len())],
            "AcrossBlocks must still swallow the following line"
        );
    }

    /// Scoping the quote must not have changed anything about ordinary prose,
    /// which is where multiline spans legitimately do carry across lines.
    #[test]
    fn block_scoping_leaves_paragraph_spans_alone() {
        let mut within = BlockScanner::block_scoped();
        assert_eq!(within.push("opens `a span"), LineClass::Content);
        assert!(
            within.inside_quote_or_code(),
            "a paragraph span must still carry across lines"
        );
        within.push("closes` it");
        assert!(!within.inside_quote_or_code());
    }

    #[test]
    fn an_escaped_backtick_does_not_open_a_span() {
        let mut delimiter = None;
        let spans = inline_code_spans(r"a literal \` then prose", &mut delimiter);
        assert!(spans.is_empty());
        assert!(delimiter.is_none());
    }

    #[test]
    fn fence_transition_is_the_only_fence_grammar() {
        assert!(matches!(
            fence_transition(None, "```rust"),
            FenceTransition::Opens {
                backticks: 3,
                tag: "rust"
            }
        ));
        assert!(matches!(
            fence_transition(Some(3), "````"),
            FenceTransition::Closes
        ));
        // A closer must carry no tag, and must be at least as wide.
        assert!(matches!(
            fence_transition(Some(4), "```"),
            FenceTransition::Unchanged
        ));
        assert!(matches!(
            fence_transition(Some(3), "``` rust"),
            FenceTransition::Unchanged
        ));
        // Four spaces make it content.
        assert!(matches!(
            fence_transition(None, "    ```"),
            FenceTransition::Unchanged
        ));
    }
}
