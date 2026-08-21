//! The judge seam — who decides whether outbound text passes.
//!
//! The queue machinery in [`super::consent`] and [`super::queue`] only ever
//! sees a [`Verdict`]. Today the judge is the contradictionary word-matcher;
//! a future classifier (the "cingulate") can implement [`OutboundJudge`] and
//! slot in without the queue, handles, journal, or expiry changing shape.

use crate::contradictionary::{Action, Contradictionary};
use serde::{Deserialize, Serialize};
use std::fmt;

/// A single matched rule inside a [`RejectReason`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasonEntry {
    /// The pattern (or, for future judges, rule identifier) that matched.
    pub pattern: String,
    /// The configured human-readable explanation, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Why outbound text bounced — named and structured, so the construct sees
/// exactly which rules fired and the journal records them queryably.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectReason {
    /// The rules that fired, deduplicated, in match order.
    pub matches: Vec<ReasonEntry>,
}

impl RejectReason {
    /// Comma-joined pattern list, e.g. `"straightforward, trivial"`.
    ///
    /// This is the stable grouping key used by journal summaries.
    pub fn patterns(&self) -> String {
        let mut out = String::new();
        for (i, m) in self.matches.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&m.pattern);
        }
        out
    }
}

impl fmt::Display for RejectReason {
    /// Human-facing summary: `pattern (explanation), pattern, …`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, m) in self.matches.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            f.write_str(&m.pattern)?;
            if let Some(ref reason) = m.reason {
                write!(f, " ({reason})")?;
            }
        }
        Ok(())
    }
}

/// The verdict a judge renders on outbound text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Send it.
    Clear,
    /// Hold it — the reason names every rule that fired.
    Bounce(RejectReason),
}

/// The seam between the hold-queue machinery and whatever decides that a
/// message should not go out as written.
pub trait OutboundJudge: Send + Sync {
    /// Judge outbound text. [`Verdict::Bounce`] holds the message under a
    /// single-use handle instead of sending it.
    fn judge(&self, content: &str) -> Verdict;
}

/// The contradictionary judges by its `block`-tier entries. Log and celebrate
/// hits are not the judge's concern — they ride along after the
/// send as before.
impl OutboundJudge for Contradictionary {
    fn judge(&self, content: &str) -> Verdict {
        let mut matches: Vec<ReasonEntry> = Vec::new();
        for hit in self.check(content) {
            if hit.action != Action::Block {
                continue;
            }
            if matches.iter().any(|m| m.pattern == hit.pattern) {
                continue;
            }
            matches.push(ReasonEntry {
                pattern: hit.pattern,
                reason: hit.reason,
            });
        }
        if matches.is_empty() {
            Verdict::Clear
        } else {
            Verdict::Bounce(RejectReason { matches })
        }
    }
}

/// A judge that clears everything. Stands in when the contradictionary is
/// disabled mid-flight (config reload between bounce and rephrase), and keeps
/// tests honest about which paths involve no judgment at all.
#[derive(Debug, Clone, Copy, Default)]
pub struct AlwaysClear;

impl OutboundJudge for AlwaysClear {
    fn judge(&self, _content: &str) -> Verdict {
        Verdict::Clear
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contradictionary::{Entry, MatchMode};

    fn entries() -> Vec<Entry> {
        vec![
            Entry {
                pattern: "straightforward".into(),
                action: Action::Block,
                match_mode: MatchMode::Word,
                reason: Some("nothing ever is".into()),
            },
            Entry {
                pattern: "trivial".into(),
                action: Action::Block,
                match_mode: MatchMode::Word,
                reason: None,
            },
            Entry {
                pattern: "honestly".into(),
                action: Action::Log,
                match_mode: MatchMode::Word,
                reason: Some("log tier must not bounce".into()),
            },
            Entry {
                pattern: "prejection".into(),
                action: Action::Celebrate,
                match_mode: MatchMode::Word,
                reason: None,
            },
        ]
    }

    #[test]
    fn block_hit_bounces_with_pattern_and_reason() {
        let judge = Contradictionary::new(entries());
        match judge.judge("a straightforward plan") {
            Verdict::Bounce(reason) => {
                assert_eq!(reason.matches.len(), 1);
                assert_eq!(reason.matches[0].pattern, "straightforward");
                assert_eq!(reason.matches[0].reason.as_deref(), Some("nothing ever is"));
            }
            Verdict::Clear => panic!("block-tier hit must bounce"),
        }
    }

    #[test]
    fn multiple_block_hits_all_named() {
        let judge = Contradictionary::new(entries());
        match judge.judge("a straightforward and trivial fix") {
            Verdict::Bounce(reason) => {
                assert_eq!(reason.patterns(), "straightforward, trivial");
            }
            Verdict::Clear => panic!("expected bounce"),
        }
    }

    #[test]
    fn repeated_pattern_reported_once() {
        let judge = Contradictionary::new(vec![Entry {
            pattern: "rust".into(),
            action: Action::Block,
            match_mode: MatchMode::Substring,
            reason: None,
        }]);
        match judge.judge("rust makes me frustrated, trust me") {
            Verdict::Bounce(reason) => {
                assert_eq!(reason.matches.len(), 1, "same rule fires once per bounce");
            }
            Verdict::Clear => panic!("expected bounce"),
        }
    }

    #[test]
    fn log_and_celebrate_do_not_bounce() {
        let judge = Contradictionary::new(entries());
        assert_eq!(
            judge.judge("honestly, prejection is the word"),
            Verdict::Clear
        );
    }

    #[test]
    fn clean_text_is_clear() {
        let judge = Contradictionary::new(entries());
        assert_eq!(judge.judge("the tests pass"), Verdict::Clear);
    }

    #[test]
    fn always_clear_never_bounces() {
        assert_eq!(AlwaysClear.judge("straightforward trivial"), Verdict::Clear);
    }

    #[test]
    fn display_includes_reasons() {
        let reason = RejectReason {
            matches: vec![
                ReasonEntry {
                    pattern: "straightforward".into(),
                    reason: Some("nothing ever is".into()),
                },
                ReasonEntry {
                    pattern: "trivial".into(),
                    reason: None,
                },
            ],
        };
        assert_eq!(
            reason.to_string(),
            "straightforward (nothing ever is), trivial"
        );
    }
}
