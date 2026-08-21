//! Thin adapter from the cingulate classifier to Dione's pre-send hook API.
//!
//! The classifier logic lives in the [`cingulate`] crate. This module wraps it
//! as a [`PreSendHook`] so it can participate in the outbound pipeline.

use crate::pre_send::{
    Assessment, AuditTrail, ConstructFeedback, HookContext, HookDecision, HookName, HookOutput,
    PreSendHook,
};
use cingulate::{PatternAction, PatternSet};
use thiserror::Error;

const HOOK_NAME: &str = "tier-1";

/// Failure to initialize the tier-1 hook from the embedded pattern artifact.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Tier1Error {
    #[error("failed to load cingulate pattern set")]
    Classifier(#[source] cingulate::CingulateError),
    #[error("failed to construct tier-1 hook name")]
    InvalidHookName,
}

/// Adapter from the immutable tier-1 regex artifact to Dione's pre-send hook API.
///
/// The embedded artifact is still a draft. Production registers this hook in
/// Observe mode only; its `block` actions are retained so a future reviewed
/// Enforce rollout has the intended behavior without changing the data model.
pub struct Tier1Hook {
    name: HookName,
    classifier: PatternSet,
}

impl Tier1Hook {
    /// Parse and compile the vendored, content-addressed tier-1 artifact.
    pub fn from_embedded() -> Result<Self, Tier1Error> {
        let classifier = PatternSet::from_embedded().map_err(Tier1Error::Classifier)?;
        let name = HookName::parse(HOOK_NAME).map_err(|_| Tier1Error::InvalidHookName)?;
        Ok(Self { name, classifier })
    }
}

impl PreSendHook for Tier1Hook {
    fn name(&self) -> HookName {
        self.name.clone()
    }

    fn execute(&self, context: &HookContext) -> HookOutput {
        let matches = self.classifier.matching_patterns(context.text());
        let mut blocked_ids = Vec::new();
        let mut to_construct = Vec::new();
        let mut to_audit = Vec::with_capacity(matches.len());

        for pattern in matches {
            // A confidence of 1.0 describes deterministic regex-match certainty,
            // not the semantic precision of the pattern. The artifact's expected
            // false-positive risk remains explicit in the assessment detail.
            let assessment = Assessment::new(
                pattern.category(),
                1.0,
                format!(
                    "pattern={}; action={}; fp_risk={}; {}",
                    pattern.id(),
                    pattern.action(),
                    pattern.fp_risk(),
                    pattern.rationale()
                ),
            );
            to_audit.push(assessment.clone());
            if pattern.action() == PatternAction::Block {
                blocked_ids.push(pattern.id().to_owned());
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
