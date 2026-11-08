//! Descriptive commands for an authenticated application's Attention control surface.
//!
//! Decoding and mutation classification confer no permission. The application
//! retains authentication, recipient identity, current source access and durable
//! effect admission.

use crate::{
    config::AttentionConfig,
    learning::{EvaluationPlan, FitOptions, FixedThresholdPolicy, TemporalPartitionSpec},
    types::{ArtifactDigest, EvaluationId, FeedbackLabel, RecordId},
};
use serde::Deserialize;
#[cfg(any(test, feature = "test-support"))]
use serde::Serialize;
use serde_json::{Value, json};

/// Typed operations accepted only through the authenticated local control surface.
#[derive(Debug, Deserialize)]
#[cfg_attr(any(test, feature = "test-support"), derive(Serialize))]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum AttentionCommand {
    /// Read current settings and engine status.
    Status,
    /// Replace the recipient settings.
    Configure {
        /// Complete replacement settings.
        settings: AttentionConfig,
    },
    /// Retrieve an authorized decision record.
    Retrieve {
        /// Selected decision record.
        record_id: RecordId,
    },
    /// Select records for attributed review.
    Review {
        /// Requested number of review records.
        requested: usize,
        /// Deterministic selection seed.
        seed: u64,
    },
    /// Record an attributed review label.
    Feedback {
        /// Selected decision record.
        record_id: RecordId,
        /// Attributed feedback label.
        label: FeedbackLabel,
    },
    /// Describe temporal training and validation partitions.
    Partitions {
        /// Temporal partition boundaries.
        spec: TemporalPartitionSpec,
    },
    /// Replay records under an explicit fixed policy.
    Replay {
        /// Selected decision records in caller order.
        record_ids: Vec<RecordId>,
        /// Explicit replay policy.
        policy: FixedThresholdPolicy,
    },
    /// Fit a candidate from selected records.
    Fit {
        /// Selected decision records in caller order.
        record_ids: Vec<RecordId>,
        /// Explicit fitting options.
        options: FitOptions,
    },
    /// Open a predeclared held-out evaluation.
    OpenEvaluation {
        /// Predeclared evaluation plan.
        plan: EvaluationPlan,
    },
    /// Evaluate a candidate digest.
    Evaluate {
        /// Opened evaluation identity.
        evaluation_id: EvaluationId,
        /// Candidate artifact digest.
        digest: ArtifactDigest,
    },
    /// Promote an evaluated digest.
    Promote {
        /// Opened evaluation identity.
        evaluation_id: EvaluationId,
        /// Candidate artifact digest.
        digest: ArtifactDigest,
    },
    /// Restore a previously supported digest.
    Rollback {
        /// Candidate artifact digest.
        digest: ArtifactDigest,
    },
}

impl AttentionCommand {
    /// Identifies commands requiring mutation authority, including durable review selection.
    pub fn is_mutating(&self) -> bool {
        !matches!(
            self,
            Self::Status | Self::Retrieve { .. } | Self::Partitions { .. } | Self::Replay { .. }
        )
    }
}

/// Describes the same closed command surface advertised by the MCP attention tool.
pub fn schema() -> Value {
    json!({"type": "object", "required": ["operation"], "additionalProperties": false,
    "properties": {
        "operation": {"type": "string", "enum": ["status", "configure", "retrieve", "review", "feedback", "partitions", "replay", "fit", "open_evaluation", "evaluate", "promote", "rollback"]},
        "settings": {"type": "object", "description": "Complete attention settings from status. Replacement preserves no omitted values; recipient cannot change."},
        "record_id": {"type": "string"}, "record_ids": {"type": "array", "items": {"type": "string"}},
        "requested": {"type": "integer", "minimum": 1, "maximum": 128}, "seed": {"type": "integer", "minimum": 0},
        "label": {"type": "string", "enum": ["wanted_promptly", "wanted_later", "not_needed", "unsure"]},
        "spec": {"type": "object", "required": ["training_end_ms", "validation_end_ms"], "properties": {
            "training_end_ms": {"type": "integer"}, "validation_end_ms": {"type": "integer"}}},
        "policy": {"type": "object", "required": ["wanted", "prompt", "participation", "change"], "properties": {
            "wanted": {"type": "number"}, "prompt": {"type": "number"}, "participation": {"type": "number"}, "change": {"type": "number"}}},
        "options": {"type": "object", "description": "Explicit FitOptions: minimum_labels, minimum_wanted, minimum_not_needed, minimum_promptly, minimum_later, regularization, iterations, learning_rate, seed, environment, wanted_threshold, timely_threshold, candidate_ttl_ms."},
        "plan": {"type": "object", "description": "EvaluationPlan: id, record_ids, fixed_baseline, predeclared limits, opened_at_ms (server replaces time)."},
        "evaluation_id": {"type": "string"}, "digest": {"type": "string"}
    }})
}
