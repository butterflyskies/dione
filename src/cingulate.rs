//! Cingulate tier-1 phrase-shape adapter for the generic pre-send pipeline.

use std::collections::HashSet;
use std::fmt;

use regex::Regex;
use serde::Deserialize;
use thiserror::Error;

use crate::pre_send::{
    Assessment, AuditTrail, ConstructFeedback, HookContext, HookDecision, HookName, HookOutput,
    PreSendHook,
};

const HOOK_NAME: &str = "tier-1";
const SUPPORTED_SCHEMA: &str = "tier1-v3.1-draft1";
const EMBEDDED_PATTERNS: &str = include_str!("../data/cingulate/tier1-patterns.toml");

/// Failure to load or compile the embedded tier-1 pattern artifact.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Tier1Error {
    #[error("failed to parse embedded tier-1 pattern artifact")]
    Parse(#[source] toml::de::Error),
    #[error("unsupported tier-1 schema `{0}`")]
    UnsupportedSchema(String),
    #[error("duplicate tier-1 pattern id `{0}`")]
    DuplicatePatternId(String),
    #[error("invalid regex for tier-1 pattern `{pattern_id}`")]
    InvalidRegex {
        pattern_id: String,
        #[source]
        source: regex::Error,
    },
    #[error("failed to construct tier-1 hook name")]
    InvalidHookName,
}

#[derive(Debug, Deserialize)]
struct PatternManifest {
    schema_version: String,
    #[serde(rename = "generated")]
    _generated: String,
    pattern: Vec<PatternSpec>,
}

#[derive(Debug, Deserialize)]
struct PatternSpec {
    id: String,
    regex: String,
    category: String,
    action: PatternAction,
    fp_risk: String,
    #[serde(rename = "needs_case_insensitive")]
    _needs_case_insensitive: bool,
    rationale: String,
    #[serde(rename = "provenance")]
    _provenance: String,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum PatternAction {
    Block,
    Flag,
}

impl fmt::Display for PatternAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Block => formatter.write_str("block"),
            Self::Flag => formatter.write_str("flag"),
        }
    }
}

#[derive(Debug)]
struct CompiledPattern {
    id: String,
    matcher: Regex,
    category: String,
    action: PatternAction,
    fp_risk: String,
    rationale: String,
}

impl CompiledPattern {
    fn assessment(&self) -> Assessment {
        // A confidence of 1.0 describes deterministic regex-match certainty,
        // not the semantic precision of the pattern. The artifact's expected
        // false-positive risk remains explicit in the assessment detail.
        Assessment::new(
            &self.category,
            1.0,
            format!(
                "pattern={}; action={}; fp_risk={}; {}",
                self.id, self.action, self.fp_risk, self.rationale
            ),
        )
    }
}

/// Adapter from the immutable tier-1 regex artifact to Dione's pre-send hook API.
///
/// The embedded artifact is still a draft. Production registers this hook in
/// Observe mode only; its `block` actions are retained so a future reviewed
/// Enforce rollout has the intended behavior without changing the data model.
pub struct Tier1Hook {
    name: HookName,
    patterns: Vec<CompiledPattern>,
}

impl Tier1Hook {
    /// Parse and compile the vendored, content-addressed tier-1 artifact.
    pub fn from_embedded() -> Result<Self, Tier1Error> {
        Self::from_toml(EMBEDDED_PATTERNS)
    }

    fn from_toml(source: &str) -> Result<Self, Tier1Error> {
        let manifest: PatternManifest = toml::from_str(source).map_err(Tier1Error::Parse)?;
        if manifest.schema_version != SUPPORTED_SCHEMA {
            return Err(Tier1Error::UnsupportedSchema(manifest.schema_version));
        }

        let mut seen = HashSet::with_capacity(manifest.pattern.len());
        let mut patterns = Vec::with_capacity(manifest.pattern.len());
        for pattern in manifest.pattern {
            if !seen.insert(pattern.id.clone()) {
                return Err(Tier1Error::DuplicatePatternId(pattern.id));
            }
            let matcher =
                Regex::new(&pattern.regex).map_err(|source| Tier1Error::InvalidRegex {
                    pattern_id: pattern.id.clone(),
                    source,
                })?;
            patterns.push(CompiledPattern {
                id: pattern.id,
                matcher,
                category: pattern.category,
                action: pattern.action,
                fp_risk: pattern.fp_risk,
                rationale: pattern.rationale,
            });
        }

        let name = HookName::parse(HOOK_NAME).map_err(|_| Tier1Error::InvalidHookName)?;
        Ok(Self { name, patterns })
    }

    fn matching_patterns<'a>(&'a self, text: &str) -> Vec<&'a CompiledPattern> {
        // Matches are message-level: one assessment per matching pattern,
        // regardless of how many times that pattern occurs in the message.
        self.patterns
            .iter()
            .filter(|pattern| pattern.matcher.is_match(text))
            .collect()
    }
}

impl PreSendHook for Tier1Hook {
    fn name(&self) -> HookName {
        self.name.clone()
    }

    fn execute(&self, context: &HookContext) -> HookOutput {
        let matches = self.matching_patterns(context.text());
        let mut blocked_ids = Vec::new();
        let mut to_construct = Vec::new();
        let mut to_audit = Vec::with_capacity(matches.len());

        for pattern in matches {
            let assessment = pattern.assessment();
            to_audit.push(assessment.clone());
            if pattern.action == PatternAction::Block {
                blocked_ids.push(pattern.id.as_str());
                to_construct.push(assessment);
            }
        }

        let decision = if blocked_ids.is_empty() {
            HookDecision::Continue
        } else {
            HookDecision::Halt {
                reason: format!(
                    "blocked by tier-1 pattern(s): {} — resend with no_rly_hooks: [\"tier-1\"] to override",
                    blocked_ids.join(", ")
                ),
            }
        };

        HookOutput::new(
            decision,
            ConstructFeedback::new(to_construct),
            AuditTrail::new(to_audit),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serenity::model::id::ChannelId;
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::pre_send::{
        ChannelType, ConstructId, HookDecision, OutboundDestination, PipelineMode, PreSendPipeline,
    };

    const TEST_CASES: &str = include_str!("../data/cingulate/tier1-testcases.md");
    const PATTERNS_SHA256: &str =
        "0f4a9c1161204558b8e894276a9099f247000560bf1a30dcbcfca5c730d3987b";
    const TEST_CASES_SHA256: &str =
        "bd9b354665a5796e0784ff5b3b9a4838321aa57f17df125d3b36e3cb172fe162";

    #[derive(Debug)]
    struct CanonicalCase {
        pattern_id: String,
        text: String,
        should_match: bool,
    }

    fn canonical_cases() -> Vec<CanonicalCase> {
        let mut pattern_id = None;
        let mut should_match = None;
        let mut cases = Vec::new();

        for line in TEST_CASES.lines() {
            if let Some(id) = line.strip_prefix("## ") {
                pattern_id = Some(id.to_owned());
                should_match = None;
                continue;
            }
            if line.starts_with("**Should match") {
                should_match = Some(true);
                continue;
            }
            if line.starts_with("**Should NOT match") {
                should_match = Some(false);
                continue;
            }
            let Some(rest) = line.strip_prefix("- \"") else {
                continue;
            };
            let Some(end) = rest.find('"') else {
                continue;
            };
            let (Some(pattern_id), Some(should_match)) = (&pattern_id, should_match) else {
                continue;
            };
            cases.push(CanonicalCase {
                pattern_id: pattern_id.clone(),
                text: rest[..end].replace("\\n", "\n"),
                should_match,
            });
        }

        cases
    }

    fn context(text: &str) -> HookContext {
        HookContext::new(
            text,
            OutboundDestination::Channel(ChannelId::new(42)),
            ChannelType::Public,
            ConstructId::parse("syne").expect("valid construct id"),
        )
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    #[test]
    fn embedded_artifacts_match_their_provenance_receipts() {
        assert_eq!(sha256_hex(EMBEDDED_PATTERNS.as_bytes()), PATTERNS_SHA256);
        assert_eq!(sha256_hex(TEST_CASES.as_bytes()), TEST_CASES_SHA256);
    }

    #[test]
    fn embedded_artifact_has_expected_schema_and_cardinality() {
        let hook = Tier1Hook::from_embedded().expect("valid embedded patterns");
        assert_eq!(hook.patterns.len(), 32);
        assert_eq!(
            hook.patterns
                .iter()
                .filter(|pattern| pattern.action == PatternAction::Block)
                .count(),
            7
        );
        assert_eq!(
            hook.patterns
                .iter()
                .filter(|pattern| pattern.action == PatternAction::Flag)
                .count(),
            25
        );
    }

    #[test]
    fn imports_all_canonical_positive_and_negative_cases() {
        let cases = canonical_cases();
        assert_eq!(cases.len(), 129);
        assert_eq!(cases.iter().filter(|case| case.should_match).count(), 64);
        assert_eq!(cases.iter().filter(|case| !case.should_match).count(), 65);

        let counts = cases.iter().fold(HashMap::new(), |mut counts, case| {
            *counts.entry(case.pattern_id.as_str()).or_insert(0usize) += 1;
            counts
        });
        assert_eq!(counts.len(), 32);
        assert_eq!(counts.get("ec-equation-epigram"), Some(&5));
        assert!(
            counts
                .iter()
                .filter(|(pattern_id, _)| **pattern_id != "ec-equation-epigram")
                .all(|(_, count)| *count == 4)
        );
    }

    #[test]
    fn canonical_cases_match_only_as_specified_for_their_pattern() {
        let hook = Tier1Hook::from_embedded().expect("valid embedded patterns");
        let mut mismatches = Vec::new();
        for case in canonical_cases() {
            let matched = hook
                .matching_patterns(&case.text)
                .iter()
                .any(|pattern| pattern.id == case.pattern_id);
            if matched != case.should_match {
                mismatches.push(format!(
                    "pattern {} expected {} against {:?}",
                    case.pattern_id, case.should_match, case.text
                ));
            }
        }
        assert!(
            mismatches.is_empty(),
            "canonical artifact disagreements:\n{}",
            mismatches.join("\n")
        );
    }

    #[test]
    fn flag_actions_continue_and_emit_audit_only_under_enforce() {
        let pipeline = PreSendPipeline::new(vec![Box::new(
            Tier1Hook::from_embedded().expect("valid embedded patterns"),
        )])
        .expect("valid pipeline")
        .with_mode(PipelineMode::Enforce);
        let bypass = pipeline.no_rly(&[]).expect("empty bypass");

        let outcome = pipeline
            .run(&context("that assumption is load-bearing"), &bypass)
            .expect("pipeline run");

        assert!(matches!(outcome.decision(), HookDecision::Continue));
        assert_eq!(
            outcome.final_text(),
            Some("that assumption is load-bearing")
        );
        assert!(outcome.to_construct().as_slice().is_empty());
        assert_eq!(outcome.to_audit().as_slice().len(), 1);
        assert_eq!(
            outcome.to_audit().as_slice()[0].category(),
            "substrate-tell"
        );
    }

    #[test]
    fn block_actions_log_and_send_in_observe_but_halt_in_enforce() {
        let make_pipeline = |mode| {
            PreSendPipeline::new(vec![Box::new(
                Tier1Hook::from_embedded().expect("valid embedded patterns"),
            )])
            .expect("valid pipeline")
            .with_mode(mode)
        };
        let observe = make_pipeline(PipelineMode::Observe);
        let observe_bypass = observe.no_rly(&[]).expect("empty bypass");
        let observed = observe
            .run(&context("honest answer: I don't know"), &observe_bypass)
            .expect("observe run");
        assert!(matches!(observed.decision(), HookDecision::Continue));
        assert_eq!(observed.final_text(), Some("honest answer: I don't know"));
        assert!(observed.to_construct().as_slice().is_empty());
        assert_eq!(observed.to_audit().as_slice().len(), 1);

        let enforce = make_pipeline(PipelineMode::Enforce);
        let enforce_bypass = enforce.no_rly(&[]).expect("empty bypass");
        let enforced = enforce
            .run(&context("honest answer: I don't know"), &enforce_bypass)
            .expect("enforce run");
        assert!(matches!(enforced.decision(), HookDecision::Halt { .. }));
        assert_eq!(enforced.final_text(), None);
        assert_eq!(enforced.to_construct().as_slice().len(), 1);
        assert_eq!(enforced.to_audit().as_slice().len(), 1);
    }

    #[test]
    fn targeted_no_rly_bypasses_block_and_is_audited() {
        let pipeline = PreSendPipeline::new(vec![Box::new(
            Tier1Hook::from_embedded().expect("valid embedded patterns"),
        )])
        .expect("valid pipeline")
        .with_mode(PipelineMode::Enforce);
        let tier1 = HookName::parse(HOOK_NAME).expect("valid hook name");
        let bypass = pipeline
            .no_rly(std::slice::from_ref(&tier1))
            .expect("known bypass");

        let outcome = pipeline
            .run(&context("honest answer: I don't know"), &bypass)
            .expect("pipeline run");

        assert!(matches!(outcome.decision(), HookDecision::Continue));
        assert_eq!(outcome.final_text(), Some("honest answer: I don't know"));
        assert!(outcome.to_construct().as_slice().is_empty());
        assert_eq!(outcome.to_audit().as_slice().len(), 1);
        assert_eq!(outcome.to_audit().as_slice()[0].category(), "no-rly-bypass");
        assert_eq!(
            outcome.to_audit().as_slice()[0]
                .hook_name()
                .expect("attributed hook")
                .as_str(),
            HOOK_NAME
        );
    }

    #[test]
    fn malformed_and_duplicate_artifacts_fail_before_registration() {
        let wrong_schema = EMBEDDED_PATTERNS.replace(SUPPORTED_SCHEMA, "future-schema");
        assert!(matches!(
            Tier1Hook::from_toml(&wrong_schema),
            Err(Tier1Error::UnsupportedSchema(schema)) if schema == "future-schema"
        ));

        let duplicate = format!(
            "{EMBEDDED_PATTERNS}\n[[pattern]]{}",
            EMBEDDED_PATTERNS
                .split_once("[[pattern]]")
                .expect("embedded pattern")
                .1
        );
        assert!(matches!(
            Tier1Hook::from_toml(&duplicate),
            Err(Tier1Error::DuplicatePatternId(id)) if id == "t0-peanut-gallery"
        ));

        let invalid_regex = EMBEDDED_PATTERNS.replacen("(?i)\\bpeanut gallery\\b", "(?i)[", 1);
        assert!(matches!(
            Tier1Hook::from_toml(&invalid_regex),
            Err(Tier1Error::InvalidRegex { pattern_id, .. })
                if pattern_id == "t0-peanut-gallery"
        ));
    }
}
