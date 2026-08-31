//! Pure candidate composition for the config pipelines.
//!
//! Both config producers — the disk-reload path ([`crate::config::reload_config`])
//! and the tool-mutation path (`ConfigRuntime::mutate` in [`crate::config`]) —
//! compose a validated candidate [`LoadedConfig`] before publishing it to the
//! process-wide snapshot. The composition itself needs no I/O, no statics, and
//! no publishing, so it lives here as pure functions the pipelines share. The
//! callers keep everything else: reading files, writing files, fail-closed
//! fallbacks, logging, and the actual publish.
//!
//! Both pipelines compose the same shape: parsed raw config plus loaded
//! contradictionary sidecar entries, via [`compose_candidate`]. The historic
//! save-path shape that skipped the sidecar merge is gone — that asymmetry was
//! the sidecar dual-publish defect the canonical pipeline eliminated.

use crate::{
    config::{Config, ConfigGenerationError, LoadedConfig},
    contradictionary::Entry,
};
use camino::{Utf8Path, Utf8PathBuf};
use std::sync::atomic::AtomicU64;

/// Compose a validated candidate from an already-parsed raw config plus
/// already-loaded contradictionary sidecar entries (the reload shape).
///
/// Sidecar entries are appended after the config's inline entries, matching
/// the documented sidecar ordering. Pass an empty `Vec` when the sidecar is
/// disabled, missing, or empty — appending nothing is a no-op.
///
/// Entry identity is `(pattern, match_mode)` — owner ruling, 🦋 2026-08-29:
/// the same text under different matching semantics is two distinct rules and
/// both survive composition. When an inline entry and a sidecar entry share
/// the full identity, the sidecar copy supersedes the inline one (the sidecar
/// is the preferred store); collisions are resolved here, at composition, so
/// neither pipeline can publish the same rule twice with diverging fields.
/// Duplicates *within* one source are left alone — this merge only arbitrates
/// the inline↔sidecar boundary the identity question was about.
///
/// Pure except for `counter`, the caller-supplied generation source: the one
/// piece of pipeline state the candidate must consume to get its
/// process-monotonic identity. No I/O, no publishing.
pub(crate) fn compose_candidate(
    mut raw: Config,
    sidecar_entries: Vec<Entry>,
    counter: &AtomicU64,
) -> Result<LoadedConfig, ConfigGenerationError> {
    raw.contradictionary.entries.retain(|inline| {
        !sidecar_entries
            .iter()
            .any(|s| s.pattern == inline.pattern && s.match_mode == inline.match_mode)
    });
    raw.contradictionary.entries.extend(sidecar_entries);
    LoadedConfig::try_from_raw_with_counter(raw, counter)
}

/// Resolve the contradictionary sidecar location for a given config file.
///
/// An empty `sidecar_path` means the default `contradictionary.toml` next to
/// the config file; a relative path resolves against the config file's
/// directory; an absolute path is used as-is.
pub(crate) fn resolve_sidecar_path(config_path: &Utf8Path, sidecar_path: &str) -> Utf8PathBuf {
    let config_dir = config_path.parent().unwrap_or_else(|| Utf8Path::new("."));
    if sidecar_path.is_empty() {
        return config_dir.join("contradictionary.toml");
    }
    let path = Utf8PathBuf::from(sidecar_path);
    if path.is_absolute() {
        path
    } else {
        config_dir.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contradictionary::{Action, MatchMode};
    use std::sync::atomic::Ordering;
    use test_case::test_case;

    fn entry(pattern: &str) -> Entry {
        Entry {
            pattern: pattern.into(),
            action: Action::Block,
            match_mode: MatchMode::Word,
            reason: None,
        }
    }

    #[test]
    fn valid_config_composes_and_allocates_a_generation() {
        let counter = AtomicU64::new(1);
        let loaded = compose_candidate(Config::default(), Vec::new(), &counter)
            .expect("default config must compose");
        assert_eq!(loaded.generation(), 1);
        assert_eq!(counter.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn sidecar_entries_append_after_inline_entries() {
        let mut raw = Config::default();
        raw.contradictionary.entries.push(entry("inline"));
        let counter = AtomicU64::new(1);
        let loaded = compose_candidate(raw, vec![entry("sidecar")], &counter)
            .expect("config with entries must compose");
        let patterns: Vec<&str> = loaded
            .raw
            .contradictionary
            .entries
            .iter()
            .map(|e| e.pattern.as_str())
            .collect();
        assert_eq!(patterns, vec!["inline", "sidecar"]);
    }

    /// Owner ruling (🦋, 2026-08-29): entry identity is `(pattern, match_mode)`.
    /// The same text under different matching semantics is two distinct rules —
    /// migration must never silently collapse one into the other.
    #[test]
    fn same_pattern_under_different_match_modes_survives_as_two_rules() {
        let mut raw = Config::default();
        raw.contradictionary.entries.push(entry("taken")); // Word
        let substring = Entry {
            pattern: "taken".into(),
            action: Action::Block,
            match_mode: MatchMode::Substring,
            reason: None,
        };
        let counter = AtomicU64::new(1);
        let loaded = compose_candidate(raw, vec![substring], &counter)
            .expect("differing match modes must compose");
        let entries = &loaded.raw.contradictionary.entries;
        assert_eq!(entries.len(), 2, "neither rule may be dropped");
        assert!(
            entries
                .iter()
                .any(|e| e.pattern == "taken" && e.match_mode == MatchMode::Word),
            "the word rule survives"
        );
        assert!(
            entries
                .iter()
                .any(|e| e.pattern == "taken" && e.match_mode == MatchMode::Substring),
            "the substring rule survives"
        );
    }

    /// When an inline entry and a sidecar entry share the full identity
    /// `(pattern, match_mode)`, they are the same rule — the sidecar copy
    /// supersedes the inline one (the sidecar is the preferred store), so the
    /// composed config carries exactly one entry with the sidecar's fields.
    #[test]
    fn identical_identity_collision_resolves_to_the_sidecar_entry() {
        let mut raw = Config::default();
        raw.contradictionary.entries.push(entry("taken")); // Word, reason: None
        let sidecar = Entry {
            pattern: "taken".into(),
            action: Action::Block,
            match_mode: MatchMode::Word,
            reason: Some("sidecar wins".into()),
        };
        let counter = AtomicU64::new(1);
        let loaded = compose_candidate(raw, vec![sidecar], &counter)
            .expect("colliding identities must compose");
        let entries = &loaded.raw.contradictionary.entries;
        assert_eq!(entries.len(), 1, "one identity, one rule");
        assert_eq!(
            entries[0].reason.as_deref(),
            Some("sidecar wins"),
            "the sidecar copy's fields are the ones that survive"
        );
    }

    #[test]
    fn empty_sidecar_entries_leave_inline_entries_untouched() {
        let mut raw = Config::default();
        raw.contradictionary.entries.push(entry("inline"));
        let counter = AtomicU64::new(1);
        let loaded =
            compose_candidate(raw, Vec::new(), &counter).expect("config with entries must compose");
        assert_eq!(loaded.raw.contradictionary.entries.len(), 1);
    }

    #[test]
    fn exhausted_generation_counter_is_a_typed_error() {
        let counter = AtomicU64::new(u64::MAX);
        assert!(matches!(
            compose_candidate(Config::default(), Vec::new(), &counter),
            Err(ConfigGenerationError)
        ));
    }

    #[test]
    fn zero_length_pattern_sidecar_entry_composes_and_is_carried_through() {
        let mut raw = Config::default();
        raw.contradictionary.enabled = true;
        let counter = AtomicU64::new(1);
        let loaded = compose_candidate(raw, vec![entry("")], &counter)
            .expect("an empty-pattern sidecar entry must compose without error");
        assert_eq!(loaded.raw.contradictionary.entries.len(), 1);
        assert_eq!(loaded.raw.contradictionary.entries[0].pattern, "");
        assert!(
            loaded.contradictionary.is_some(),
            "an enabled config with one (empty-pattern) entry still builds a concordance"
        );
    }

    #[test_case("", "/state/contradictionary.toml"; "empty means default sibling")]
    #[test_case("side.toml", "/state/side.toml"; "relative resolves against config dir")]
    #[test_case("/abs/side.toml", "/abs/side.toml"; "absolute is used as is")]
    fn sidecar_path_resolution(sidecar_path: &str, expected: &str) {
        let resolved = resolve_sidecar_path(Utf8Path::new("/state/config.toml"), sidecar_path);
        assert_eq!(resolved, Utf8PathBuf::from(expected));
    }

    // A bare `config.toml` has `parent() == Some("")` (camino returns the empty
    // path, not `None`), so resolution goes through the empty-parent join
    // rather than the `unwrap_or_else(".")` fallback. Joining onto the empty
    // path yields a bare relative sidecar path; this pins that behavior.
    #[test_case("", "contradictionary.toml"; "empty means bare default sibling")]
    #[test_case("side.toml", "side.toml"; "relative stays bare")]
    #[test_case("/abs/side.toml", "/abs/side.toml"; "absolute is used as is")]
    fn sidecar_path_resolution_for_parentless_config(sidecar_path: &str, expected: &str) {
        let resolved = resolve_sidecar_path(Utf8Path::new("config.toml"), sidecar_path);
        assert_eq!(resolved, Utf8PathBuf::from(expected));
    }
}
