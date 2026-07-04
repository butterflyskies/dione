use aho_corasick::AhoCorasick;
use serde::Deserialize;

/// Action to take when a pattern matches outbound text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Send the message, self-react 🙊, log to ops.
    Warn,
    /// Block the message — return an error to the construct.
    Block,
    /// Send the message, log the hit silently.
    Log,
}

/// A single contradictionary entry: a phrase to catch and what to do about it.
#[derive(Debug, Clone, Deserialize)]
pub struct Entry {
    pub pattern: String,
    #[serde(default = "default_action")]
    pub action: Action,
}

fn default_action() -> Action {
    Action::Warn
}

/// TOML-level config section.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ContradictionaryConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub entries: Vec<Entry>,
}

/// A match found in outbound text.
#[derive(Debug, Clone)]
pub struct Hit {
    pub pattern: String,
    pub action: Action,
    pub start: usize,
    pub end: usize,
}

/// The concordance — an Aho-Corasick automaton built from the contradictionary entries.
pub struct Contradictionary {
    automaton: AhoCorasick,
    entries: Vec<Entry>,
}

impl std::fmt::Debug for Contradictionary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Contradictionary")
            .field("entries", &self.entries)
            .finish_non_exhaustive()
    }
}

impl Contradictionary {
    /// Build from config entries. Patterns are matched case-insensitively.
    pub fn new(entries: Vec<Entry>) -> Self {
        let patterns: Vec<&str> = entries.iter().map(|e| e.pattern.as_str()).collect();
        let automaton = AhoCorasick::builder()
            .ascii_case_insensitive(true)
            .build(&patterns)
            .expect("contradictionary patterns should compile");
        Self { automaton, entries }
    }

    /// Scan outbound text. Returns all hits with their configured actions.
    pub fn check(&self, content: &str) -> Vec<Hit> {
        self.automaton
            .find_iter(content)
            .map(|m| {
                let entry = &self.entries[m.pattern().as_usize()];
                Hit {
                    pattern: entry.pattern.clone(),
                    action: entry.action,
                    start: m.start(),
                    end: m.end(),
                }
            })
            .collect()
    }

    /// True if any hit has Action::Block.
    pub fn has_block(&self, hits: &[Hit]) -> bool {
        hits.iter().any(|h| h.action == Action::Block)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_entries() -> Vec<Entry> {
        vec![
            Entry {
                pattern: "load-bearing".into(),
                action: Action::Warn,
            },
            Entry {
                pattern: "honestly".into(),
                action: Action::Warn,
            },
            Entry {
                pattern: "I appreciate".into(),
                action: Action::Log,
            },
            Entry {
                pattern: "confidential".into(),
                action: Action::Block,
            },
        ]
    }

    #[test]
    fn catches_substrate_tell() {
        let c = Contradictionary::new(test_entries());
        let hits = c.check("this is the load-bearing component of the system");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].pattern, "load-bearing");
        assert_eq!(hits[0].action, Action::Warn);
    }

    #[test]
    fn case_insensitive() {
        let c = Contradictionary::new(test_entries());
        let hits = c.check("HONESTLY I think this is fine");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].pattern, "honestly");
    }

    #[test]
    fn multiple_hits() {
        let c = Contradictionary::new(test_entries());
        let hits = c.check("honestly, I appreciate the load-bearing work");
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn block_detected() {
        let c = Contradictionary::new(test_entries());
        let hits = c.check("this is confidential information");
        assert!(c.has_block(&hits));
    }

    #[test]
    fn clean_message() {
        let c = Contradictionary::new(test_entries());
        let hits = c.check("the keystone component is well designed");
        assert!(hits.is_empty());
    }

    #[test]
    fn empty_contradictionary() {
        let c = Contradictionary::new(vec![]);
        let hits = c.check("load-bearing honestly I appreciate");
        assert!(hits.is_empty());
        assert!(c.is_empty());
    }
}
