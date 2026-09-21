//! Recipient-local replay, fitting, held-out evaluation, promotion, and rollback.
//!
//! Jev probabilities are features.  Targets come only from the recipient's attributed
//! wanted/timeliness feedback; neither Jev output nor another annotator is a training target.

use super::{
    store::{AttentionStore, StoreError, artifact_availability, same_source},
    types::{
        Admission, ArtifactDigest, Compatibility, DecisionRecord, EvaluationId, Feedback,
        FeedbackLabel, ProviderTelemetry, RecipientId, RecordId, Scores, SourceVersion,
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

const FEATURE_COUNT: usize = 5; // intercept plus four Jev score features
const MAX_FIT_ITERATIONS: usize = 10_000;

/// Fixed-score replay baseline; thresholds do not themselves authorize enforcement.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FixedThresholdPolicy {
    pub wanted: f64,
    pub prompt: f64,
    pub participation: f64,
    pub change: f64,
}

impl FixedThresholdPolicy {
    /// Validates thresholds and returns the hypothetical admission for these scores.
    pub fn decide(self, scores: &Scores) -> Result<Admission, LearningError> {
        self.validate()?;
        let wanted = scores.wanted.get() >= self.wanted
            || scores.participation.get() >= self.participation
            || scores.change.get() >= self.change;
        if !wanted {
            Ok(Admission::RetrievalOnly)
        } else if scores.prompt.get() >= self.prompt {
            Ok(Admission::Prompt)
        } else {
            Ok(Admission::NextTurn)
        }
    }

    fn validate(self) -> Result<(), LearningError> {
        if [self.wanted, self.prompt, self.participation, self.change]
            .into_iter()
            .all(|value| value.is_finite() && (0.0..=1.0).contains(&value))
        {
            Ok(())
        } else {
            Err(LearningError::InvalidPolicy(
                "fixed thresholds must be finite probabilities",
            ))
        }
    }
}

/// Whether all source membership needed by an artifact is presently usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactAvailability {
    Available,
    AwaitingRevalidation,
    Invalid,
}

/// Two recipient-label classifiers with reproducible fitting parameters and thresholds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogisticCandidate {
    pub wanted_weights: [f64; FEATURE_COUNT],
    pub timely_weights: [f64; FEATURE_COUNT],
    pub wanted_threshold: f64,
    pub timely_threshold: f64,
    pub regularization: f64,
    pub iterations: usize,
    pub learning_rate: f64,
    pub seed: u64,
    pub environment: String,
}

impl LogisticCandidate {
    /// Computes a hypothetical outcome; the caller must separately validate artifact authority.
    pub fn predict(&self, scores: &Scores) -> Admission {
        let features = features(scores);
        if sigmoid(dot(&self.wanted_weights, &features)) < self.wanted_threshold {
            Admission::RetrievalOnly
        } else if sigmoid(dot(&self.timely_weights, &features)) >= self.timely_threshold {
            Admission::Prompt
        } else {
            Admission::NextTurn
        }
    }
}

/// Recipient-owned model bound to compatibility, source membership, and expiry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearnedArtifact {
    pub digest: ArtifactDigest,
    pub recipient: RecipientId,
    pub compatibility: Compatibility,
    pub model: LogisticCandidate,
    pub members: Vec<SourceVersion>,
    /// Held-out source versions supporting the most recent explicit promotion. Rollback and
    /// enforcement revalidate these along with training membership.
    pub promotion_sources: Vec<SourceVersion>,
    pub training_records: Vec<RecordId>,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
    pub availability: ArtifactAvailability,
    pub ever_promoted: bool,
}

/// Explicit data sufficiency and optimization settings for fitting an unpromoted candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FitOptions {
    pub minimum_labels: usize,
    pub minimum_wanted: usize,
    pub minimum_not_needed: usize,
    pub minimum_promptly: usize,
    pub minimum_later: usize,
    pub regularization: f64,
    pub iterations: usize,
    pub learning_rate: f64,
    pub seed: u64,
    pub environment: String,
    pub wanted_threshold: f64,
    pub timely_threshold: f64,
    pub candidate_ttl_ms: u64,
}

/// Millisecond cutoffs for conversation-grouped chronological holdouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemporalPartitionSpec {
    pub training_end_ms: u64,
    pub validation_end_ms: u64,
}

/// Record IDs partitioned without splitting a conversation across time boundaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemporalPartitions {
    pub training: Vec<RecordId>,
    pub validation: Vec<RecordId>,
    pub evaluation: Vec<RecordId>,
}

/// Predeclared coverage, recall, noise, reduction, and uncertainty requirements.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromotionLimits {
    pub minimum_labeled: usize,
    pub minimum_wanted: usize,
    pub minimum_promptly: usize,
    pub minimum_wanted_recall: f64,
    pub minimum_timely_recall: f64,
    pub minimum_volume_reduction: f64,
    pub maximum_unwanted_delivery_rate: f64,
    pub maximum_standard_error: f64,
}

impl PromotionLimits {
    fn validate(&self) -> Result<(), LearningError> {
        let probabilities = [
            self.minimum_wanted_recall,
            self.minimum_timely_recall,
            self.minimum_volume_reduction,
            self.maximum_unwanted_delivery_rate,
            self.maximum_standard_error,
        ];
        if self.minimum_labeled == 0
            || self.minimum_wanted == 0
            || self.minimum_promptly == 0
            || !probabilities
                .into_iter()
                .all(|value| value.is_finite() && (0.0..=1.0).contains(&value))
        {
            return Err(LearningError::InvalidLimits);
        }
        Ok(())
    }
}

/// Recipient-supplied holdout membership and limits, declared before evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvaluationPlan {
    pub id: EvaluationId,
    pub record_ids: Vec<RecordId>,
    pub fixed_baseline: FixedThresholdPolicy,
    pub limits: PromotionLimits,
    pub opened_at_ms: u64,
}

/// Persisted holdout declaration whose conversation reuse and source loss are tracked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenedEvaluation {
    pub id: EvaluationId,
    pub recipient: RecipientId,
    pub record_ids: Vec<RecordId>,
    pub conversations: Vec<String>,
    pub fixed_baseline: FixedThresholdPolicy,
    pub limits: PromotionLimits,
    pub opened_at_ms: u64,
    pub invalidated_for_tuning: bool,
    pub invalidated_source: bool,
}

/// A weighted estimate with uncertainty and effective sample size; absent values are unknown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Estimate {
    pub value: Option<f64>,
    pub standard_error: Option<f64>,
    pub effective_sample_size: f64,
}

/// Recall, timeliness, unwanted delivery, and source-volume measurements for one policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyMetrics {
    pub wanted_recall: Estimate,
    pub timely_recall: Estimate,
    pub wanted_delay_rate: Estimate,
    pub unwanted_delivery_rate: Estimate,
    pub admitted_records: usize,
    pub admitted_source_volume: usize,
    pub volume_reduction: f64,
    pub source_volume_reduction: f64,
}

/// Recipient label coverage with explicit unsure and unlabeled populations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewCoverage {
    pub total_records: usize,
    pub recipient_labeled: usize,
    pub unsure: usize,
    pub unlabeled: usize,
    pub labels_with_recorded_propensity: usize,
    pub inverse_probability_population_estimate: f64,
    pub effective_sample_size: f64,
}

/// Reasons held-out evidence cannot authorize promotion under its declared limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationFailure {
    MissingRepresentativeSample,
    InsufficientLabeled,
    InsufficientWanted,
    InsufficientPromptlyWanted,
    WantedRecall,
    TimelyRecall,
    VolumeReduction,
    UnwantedDelivery,
    Uncertainty,
}

/// Exact source-bound rows used to reproduce every reported evaluation metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricInput {
    pub record_id: RecordId,
    pub recipient_label: Option<FeedbackLabel>,
    pub inclusion_probability: Option<f64>,
    pub source_volume: usize,
    pub provider_telemetry: Option<ProviderTelemetry>,
    pub candidate: Admission,
    pub normal_delivery: Admission,
    pub fixed_baseline: Admission,
}

/// Exact aggregates over observed provider fields. Missing rows are counted and never folded
/// into the total as zero; the total is absent when no row reported the field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedU64Aggregate {
    pub observed_records: usize,
    pub missing_records: usize,
    pub total: Option<u64>,
}

/// Aggregated observed provider usage; missing telemetry is not treated as zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluationUsage {
    pub attempts: ObservedU64Aggregate,
    pub input_tokens: ObservedU64Aggregate,
    pub output_tokens: ObservedU64Aggregate,
}

/// The documented API reports token usage, not a billed price. No currency estimate is inferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BilledCost {
    UnavailableNoBilledPrice,
}

/// Provider latency and usage observations, without inferred billing amounts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluationProviderMetrics {
    pub latency_ms: ObservedU64Aggregate,
    pub usage: EvaluationUsage,
    pub billed_cost: BilledCost,
}

/// Source-bound, reproducible comparison against normal delivery and fixed thresholds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvaluationReceipt {
    pub evaluation_id: EvaluationId,
    pub recipient: RecipientId,
    pub artifact_digest: ArtifactDigest,
    pub compatibility: Compatibility,
    pub evaluated_at_ms: u64,
    pub evaluation_sources: Vec<SourceVersion>,
    pub metric_inputs: Vec<MetricInput>,
    pub provider: EvaluationProviderMetrics,
    pub coverage: ReviewCoverage,
    pub candidate: PolicyMetrics,
    pub normal_delivery: PolicyMetrics,
    pub fixed_baseline: PolicyMetrics,
    pub limits: PromotionLimits,
    pub failures: Vec<EvaluationFailure>,
    pub passed: bool,
}

/// The policy selection produced by an explicit promotion or compatible rollback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PromotionResult {
    pub active_digest: ArtifactDigest,
    pub previous_digest: Option<ArtifactDigest>,
}

/// Authority, compatibility, membership, fitting, and held-out evaluation failures.
#[derive(Debug, Error)]
pub enum LearningError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("authenticated actor is not the recipient")]
    ActorMismatch,
    #[error("invalid fixed or learned policy: {0}")]
    InvalidPolicy(&'static str),
    #[error("invalid predeclared promotion limits")]
    InvalidLimits,
    #[error("record `{0}` cannot be used for this recipient or compatibility")]
    IneligibleRecord(RecordId),
    #[error("record `{0}` combines multiple conversation groups")]
    MixedConversationRecord(RecordId),
    #[error("insufficient recipient labels for fitting")]
    InsufficientLabels,
    #[error("evaluation `{0}` already exists")]
    EvaluationExists(EvaluationId),
    #[error("evaluation `{0}` was not found")]
    EvaluationNotFound(EvaluationId),
    #[error("evaluation holdout was already touched by fitting")]
    HoldoutTouched,
    #[error("opened evaluation conversations were reused for tuning: {0:?}")]
    OpenedEvaluationReused(Vec<EvaluationId>),
    #[error("artifact `{0}` was not found")]
    ArtifactNotFound(ArtifactDigest),
    #[error("artifact is not compatible with the current policy identity")]
    IncompatibleArtifact,
    #[error("artifact membership is invalid or awaiting revalidation")]
    InvalidMembership,
    #[error("held-out evidence does not pass its predeclared limits")]
    EvaluationDidNotPass,
    #[error("artifact was never promoted and is not eligible for rollback")]
    NotRollbackArtifact,
    #[error("arithmetic produced a non-finite value")]
    NonFinite,
}

impl AttentionStore {
    /// Replays fixed thresholds with unknown fallback and no routing side effects.
    pub fn replay_fixed(
        &self,
        record_id: &RecordId,
        policy: FixedThresholdPolicy,
    ) -> Result<Admission, LearningError> {
        let record = self
            .record(record_id)
            .ok_or_else(|| StoreError::RecordNotFound(record_id.to_owned()))?;
        let judgment = record
            .judgment
            .as_ref()
            .ok_or_else(|| LearningError::IneligibleRecord(record_id.to_owned()))?;
        Ok(unknown_fallback(record, policy.decide(&judgment.scores)?))
    }

    /// Replays compatible stored scores without selecting an artifact or authorizing delivery.
    pub fn replay_artifact(
        &self,
        artifact_digest: &ArtifactDigest,
        record_id: &RecordId,
    ) -> Result<Admission, LearningError> {
        let artifact = self
            .artifact(artifact_digest)
            .ok_or_else(|| LearningError::ArtifactNotFound(artifact_digest.to_owned()))?;
        let record = self
            .record(record_id)
            .ok_or_else(|| StoreError::RecordNotFound(record_id.to_owned()))?;
        if record.recipient != artifact.recipient || record.compatibility != artifact.compatibility
        {
            return Err(LearningError::IneligibleRecord(record_id.to_owned()));
        }
        let judgment = record
            .judgment
            .as_ref()
            .ok_or_else(|| LearningError::IneligibleRecord(record_id.to_owned()))?;
        Ok(unknown_fallback(
            record,
            artifact.model.predict(&judgment.scores),
        ))
    }

    /// Predict with the active artifact only while its identity and all source memberships remain
    /// current.  `Ok(None)` means no artifact is active; other failures require ordinary delivery.
    pub fn predict_active(
        &self,
        recipient: &RecipientId,
        compatibility: &Compatibility,
        scores: &Scores,
        now_ms: u64,
    ) -> Result<Option<Admission>, LearningError> {
        let Some(artifact) = self.active_artifact(recipient) else {
            return Ok(None);
        };
        if &artifact.compatibility != compatibility {
            return Err(LearningError::IncompatibleArtifact);
        }
        if artifact.availability != ArtifactAvailability::Available
            || !self.sources_are_known(&artifact.members, now_ms)
            || !self.sources_are_known(&artifact.promotion_sources, now_ms)
        {
            return Err(LearningError::InvalidMembership);
        }
        Ok(Some(artifact.model.predict(scores)))
    }

    /// Partition whole conversations by their latest record timestamp.  A conversation spanning a
    /// boundary moves wholly to the later partition, so no earlier example from it leaks backward.
    pub fn temporal_partitions(
        &self,
        recipient: &RecipientId,
        spec: TemporalPartitionSpec,
    ) -> Result<TemporalPartitions, LearningError> {
        if spec.training_end_ms >= spec.validation_end_ms {
            return Err(LearningError::InvalidPolicy(
                "temporal partition boundaries must increase",
            ));
        }
        let mut conversations: BTreeMap<String, (u64, Vec<RecordId>)> = BTreeMap::new();
        for record in self.list_records(recipient) {
            if record.judgment.is_none()
                || record.delivery == super::types::DeliveryState::Invalidated
            {
                continue;
            }
            let conversation = record_conversation(record)?;
            let entry = conversations
                .entry(conversation)
                .or_insert((record.created_at_ms, Vec::new()));
            entry.0 = entry.0.max(record.created_at_ms);
            entry.1.push(record.id.clone());
        }
        let mut partitions = TemporalPartitions {
            training: Vec::new(),
            validation: Vec::new(),
            evaluation: Vec::new(),
        };
        for (_, (latest, mut ids)) in conversations {
            ids.sort();
            if latest <= spec.training_end_ms {
                partitions.training.extend(ids);
            } else if latest <= spec.validation_end_ms {
                partitions.validation.extend(ids);
            } else {
                partitions.evaluation.extend(ids);
            }
        }
        partitions.training.sort();
        partitions.validation.sort();
        partitions.evaluation.sort();
        Ok(partitions)
    }

    /// Seals recipient-owned holdout conversations and limits before any evaluation or tuning.
    pub fn open_evaluation(
        &mut self,
        actor: &RecipientId,
        recipient: &RecipientId,
        mut plan: EvaluationPlan,
    ) -> Result<OpenedEvaluation, LearningError> {
        require_recipient(actor, recipient)?;
        plan.fixed_baseline.validate()?;
        plan.limits.validate()?;
        if plan.id.as_str().is_empty() || plan.record_ids.is_empty() {
            return Err(LearningError::InvalidPolicy(
                "evaluation ID and holdout must be nonempty",
            ));
        }
        plan.record_ids.sort();
        plan.record_ids.dedup();
        self.update(|state, limits| {
            if state.opened_evaluations.contains_key(&plan.id) {
                return Err(LearningError::EvaluationExists(plan.id.clone()));
            }
            if state.opened_evaluations.len() >= limits.max_opened_evaluations {
                return Err(StoreError::Capacity("opened evaluations").into());
            }
            let mut conversations = BTreeSet::new();
            for id in &plan.record_ids {
                let record = state
                    .records
                    .get(id)
                    .ok_or_else(|| StoreError::RecordNotFound(id.clone()))?;
                if &record.recipient != recipient
                    || record.judgment.is_none()
                    || record.delivery == super::types::DeliveryState::Invalidated
                {
                    return Err(LearningError::IneligibleRecord(id.clone()));
                }
                conversations.insert(record_conversation(record)?);
            }
            let fitted_conversations: BTreeSet<&str> = state
                .artifacts
                .values()
                .filter(|artifact| &artifact.recipient == recipient)
                .flat_map(|artifact| {
                    artifact
                        .members
                        .iter()
                        .map(|source| source.conversation.as_str())
                })
                .collect();
            if conversations
                .iter()
                .any(|conversation| fitted_conversations.contains(conversation.as_str()))
            {
                return Err(LearningError::HoldoutTouched);
            }
            if state.opened_evaluations.values().any(|opened| {
                &opened.recipient == recipient
                    && opened
                        .conversations
                        .iter()
                        .any(|conversation| conversations.contains(conversation))
            }) {
                return Err(LearningError::HoldoutTouched);
            }
            let representative_index = state.review_batches.iter().rposition(|batch| {
                batch.covers_occupied_strata
                    && &batch.recipient == recipient
                    && batch.items.len() == plan.record_ids.len()
                    && batch.items.iter().all(|item| {
                        plan.record_ids.binary_search(&item.record_id).is_ok()
                            && state.records.get(&item.record_id).is_some_and(|record| {
                                batch.source_versions.get(&item.record_id) == Some(&record.sources)
                                    && artifact_availability(
                                        &record.sources,
                                        &state.validations,
                                        plan.opened_at_ms,
                                        limits.revalidation_interval_ms,
                                    ) == ArtifactAvailability::Available
                            })
                    })
            });
            let representative_sample =
                representative_index.map(|index| state.review_batches.remove(index));
            let opened = OpenedEvaluation {
                id: plan.id.clone(),
                recipient: recipient.to_owned(),
                record_ids: plan.record_ids,
                conversations: conversations.into_iter().collect(),
                fixed_baseline: plan.fixed_baseline,
                limits: plan.limits,
                opened_at_ms: plan.opened_at_ms,
                invalidated_for_tuning: false,
                invalidated_source: false,
            };
            state
                .opened_evaluations
                .insert(opened.id.clone(), opened.clone());
            if let Some(sample) = representative_sample {
                state.evaluation_samples.insert(opened.id.clone(), sample);
            }
            Ok(opened)
        })
    }

    /// Fits and persists an unpromoted candidate from eligible recipient labels only.
    pub fn fit(
        &mut self,
        actor: &RecipientId,
        recipient: &RecipientId,
        record_ids: &[RecordId],
        compatibility: Compatibility,
        options: FitOptions,
        now_ms: u64,
    ) -> Result<LearnedArtifact, LearningError> {
        require_recipient(actor, recipient)?;
        validate_fit_options(&options, self.limits.max_record_ttl_ms)?;
        let mut unique_ids = record_ids.to_vec();
        unique_ids.sort();
        unique_ids.dedup();
        if unique_ids.len() > self.limits.max_members_per_artifact {
            return Err(StoreError::Capacity("artifact membership").into());
        }

        let training_conversations: BTreeSet<String> = unique_ids
            .iter()
            .map(|id| {
                let record = self
                    .record(id)
                    .ok_or_else(|| StoreError::RecordNotFound(id.clone()))?;
                if &record.recipient != recipient || record.compatibility != compatibility {
                    return Err(LearningError::IneligibleRecord(id.clone()));
                }
                record_conversation(record)
            })
            .collect::<Result<_, _>>()?;
        let reused: Vec<EvaluationId> = self
            .state
            .opened_evaluations
            .values()
            .filter(|opened| {
                &opened.recipient == recipient
                    && opened
                        .conversations
                        .iter()
                        .any(|conversation| training_conversations.contains(conversation))
            })
            .map(|opened| opened.id.clone())
            .collect();
        if !reused.is_empty() {
            let reused_set: BTreeSet<EvaluationId> = reused.iter().cloned().collect();
            self.update(|state, _| {
                for id in &reused_set {
                    if let Some(opened) = state.opened_evaluations.get_mut(id) {
                        opened.invalidated_for_tuning = true;
                    }
                }
                state.evaluations.retain(|id, _| !reused_set.contains(id));
                Ok::<(), LearningError>(())
            })?;
            return Err(LearningError::OpenedEvaluationReused(reused));
        }

        let mut examples = Vec::with_capacity(unique_ids.len());
        let mut members = Vec::new();
        let mut used_ids = Vec::with_capacity(unique_ids.len());
        let mut wanted = 0;
        let mut not_needed = 0;
        let mut promptly = 0;
        let mut later = 0;
        for id in &unique_ids {
            let record = self
                .record(id)
                .ok_or_else(|| StoreError::RecordNotFound(id.clone()))?;
            if &record.recipient != recipient
                || record.compatibility != compatibility
                || record.delivery == super::types::DeliveryState::Invalidated
            {
                return Err(LearningError::IneligibleRecord(id.clone()));
            }
            let judgment = record
                .judgment
                .as_ref()
                .ok_or_else(|| LearningError::IneligibleRecord(id.clone()))?;
            let Some(target) = self.recipient_target(id) else {
                continue;
            };
            if target.wanted {
                wanted += 1;
                if target.timely {
                    promptly += 1;
                } else {
                    later += 1;
                }
            } else {
                not_needed += 1;
            }
            examples.push((features(&judgment.scores), target));
            used_ids.push(id.clone());
            members.extend(record.sources.iter().cloned());
        }
        if examples.len() < options.minimum_labels
            || wanted < options.minimum_wanted
            || not_needed < options.minimum_not_needed
            || promptly < options.minimum_promptly
            || later < options.minimum_later
        {
            return Err(LearningError::InsufficientLabels);
        }
        sort_and_deduplicate_sources(&mut members);
        if members.len() > self.limits.max_members_per_artifact
            || !self.sources_are_known(&members, now_ms)
        {
            return Err(LearningError::InvalidMembership);
        }

        let wanted_examples: Vec<([f64; FEATURE_COUNT], f64)> = examples
            .iter()
            .map(|(input, target)| (*input, if target.wanted { 1.0 } else { 0.0 }))
            .collect();
        let timely_examples: Vec<([f64; FEATURE_COUNT], f64)> = examples
            .iter()
            .filter(|(_, target)| target.wanted)
            .map(|(input, target)| (*input, if target.timely { 1.0 } else { 0.0 }))
            .collect();
        let wanted_weights = fit_logistic(&wanted_examples, &options, 0)?;
        let timely_weights = fit_logistic(&timely_examples, &options, 1)?;
        let model = LogisticCandidate {
            wanted_weights,
            timely_weights,
            wanted_threshold: options.wanted_threshold,
            timely_threshold: options.timely_threshold,
            regularization: options.regularization,
            iterations: options.iterations,
            learning_rate: options.learning_rate,
            seed: options.seed,
            environment: options.environment,
        };
        let expires_at_ms = now_ms
            .checked_add(options.candidate_ttl_ms)
            .ok_or(LearningError::InvalidPolicy("candidate expiry overflow"))?;
        let digest = artifact_digest(
            recipient,
            &compatibility,
            &model,
            &members,
            &used_ids,
            now_ms,
            expires_at_ms,
        )?;
        let artifact = LearnedArtifact {
            digest: digest.clone(),
            recipient: recipient.to_owned(),
            compatibility,
            model,
            members,
            promotion_sources: Vec::new(),
            training_records: used_ids,
            created_at_ms: now_ms,
            expires_at_ms,
            availability: ArtifactAvailability::Available,
            ever_promoted: false,
        };
        self.update(|state, limits| {
            if state.artifacts.len() >= limits.max_artifacts
                && !state.artifacts.contains_key(&digest)
            {
                return Err(StoreError::Capacity("artifacts").into());
            }
            state.artifacts.insert(digest, artifact.clone());
            Ok(artifact)
        })
    }

    /// Persists a reproducible held-out comparison; a passing result alone does not activate it.
    pub fn evaluate(
        &mut self,
        actor: &RecipientId,
        recipient: &RecipientId,
        evaluation_id: &EvaluationId,
        artifact_digest: &ArtifactDigest,
        evaluated_at_ms: u64,
    ) -> Result<EvaluationReceipt, LearningError> {
        require_recipient(actor, recipient)?;
        let opened = self
            .opened_evaluation(evaluation_id)
            .cloned()
            .ok_or_else(|| LearningError::EvaluationNotFound(evaluation_id.to_owned()))?;
        if &opened.recipient != recipient
            || opened.invalidated_for_tuning
            || opened.invalidated_source
        {
            return Err(LearningError::HoldoutTouched);
        }
        let artifact = self
            .artifact(artifact_digest)
            .cloned()
            .ok_or_else(|| LearningError::ArtifactNotFound(artifact_digest.to_owned()))?;
        if &artifact.recipient != recipient
            || artifact.availability != ArtifactAvailability::Available
            || (!artifact.ever_promoted && artifact.expires_at_ms <= evaluated_at_ms)
            || !self.sources_are_known(&artifact.members, evaluated_at_ms)
        {
            return Err(LearningError::InvalidMembership);
        }

        let representative_sample = self.state.evaluation_samples.get(evaluation_id).cloned();
        let mut observations = Vec::with_capacity(opened.record_ids.len());
        let mut evaluation_sources = Vec::new();
        let mut metric_inputs = Vec::with_capacity(opened.record_ids.len());
        for id in &opened.record_ids {
            let record = self
                .record(id)
                .ok_or_else(|| StoreError::RecordNotFound(id.clone()))?;
            if &record.recipient != recipient
                || record.compatibility != artifact.compatibility
                || record.delivery == super::types::DeliveryState::Invalidated
            {
                return Err(LearningError::IneligibleRecord(id.clone()));
            }
            let judgment = record
                .judgment
                .as_ref()
                .ok_or_else(|| LearningError::IneligibleRecord(id.clone()))?;
            let target = latest_recipient_label(&self.state.feedback, record);
            let sampled_item = representative_sample
                .as_ref()
                .and_then(|batch| batch.items.iter().find(|item| &item.record_id == id));
            if representative_sample.as_ref().is_some_and(|batch| {
                sampled_item.is_none()
                    || batch.source_versions.get(&record.id) != Some(&record.sources)
            }) {
                return Err(LearningError::HoldoutTouched);
            }
            // Only server-issued review sampling is a propensity. An inbound record field or
            // hand-picked holdout cannot manufacture representative promotion evidence.
            let propensity = sampled_item.map(|item| item.inclusion_probability);
            let candidate = unknown_fallback(record, artifact.model.predict(&judgment.scores));
            let fixed = unknown_fallback(record, opened.fixed_baseline.decide(&judgment.scores)?);
            observations.push(Observation {
                target,
                propensity,
                source_volume: record.sources.len(),
                candidate,
                fixed,
            });
            metric_inputs.push(MetricInput {
                record_id: record.id.clone(),
                recipient_label: target,
                inclusion_probability: propensity,
                source_volume: record.sources.len(),
                provider_telemetry: judgment.telemetry.clone(),
                candidate,
                normal_delivery: Admission::Ordinary,
                fixed_baseline: fixed,
            });
            evaluation_sources.extend(record.sources.iter().cloned());
        }
        sort_and_deduplicate_sources(&mut evaluation_sources);
        if !self.sources_are_known(&evaluation_sources, evaluated_at_ms) {
            return Err(LearningError::InvalidMembership);
        }

        let provider = evaluation_provider_metrics(&metric_inputs)?;
        let coverage = coverage(&observations);
        let candidate = metrics(&observations, |observation| observation.candidate);
        let normal_delivery = metrics(&observations, |_| Admission::Ordinary);
        let fixed_baseline = metrics(&observations, |observation| observation.fixed);
        let failures = evaluation_failures(
            &coverage,
            &candidate,
            &opened.limits,
            representative_sample.is_some(),
        );
        let receipt = EvaluationReceipt {
            evaluation_id: evaluation_id.to_owned(),
            recipient: recipient.to_owned(),
            artifact_digest: artifact_digest.to_owned(),
            compatibility: artifact.compatibility,
            evaluated_at_ms,
            evaluation_sources,
            metric_inputs,
            provider,
            coverage,
            candidate,
            normal_delivery,
            fixed_baseline,
            limits: opened.limits,
            passed: failures.is_empty(),
            failures,
        };
        self.update(|state, limits| {
            if state.evaluations.contains_key(evaluation_id) {
                return Err(LearningError::EvaluationExists(evaluation_id.to_owned()));
            }
            if state.evaluations.len() >= limits.max_evaluations {
                return Err(StoreError::Capacity("evaluations").into());
            }
            state
                .evaluations
                .insert(evaluation_id.to_owned(), receipt.clone());
            Ok(receipt)
        })
    }

    /// Selects a recipient-authorized candidate only with passing, current held-out membership.
    pub fn promote(
        &mut self,
        actor: &RecipientId,
        recipient: &RecipientId,
        artifact_digest: &ArtifactDigest,
        evaluation_id: &EvaluationId,
        compatibility: &Compatibility,
        promoted_at_ms: u64,
    ) -> Result<PromotionResult, LearningError> {
        require_recipient(actor, recipient)?;
        self.update(|state, limits| {
            let opened = state
                .opened_evaluations
                .get(evaluation_id)
                .ok_or_else(|| LearningError::EvaluationNotFound(evaluation_id.to_owned()))?;
            if &opened.recipient != recipient
                || opened.invalidated_for_tuning
                || opened.invalidated_source
            {
                return Err(LearningError::HoldoutTouched);
            }
            let receipt = state
                .evaluations
                .get(evaluation_id)
                .ok_or_else(|| LearningError::EvaluationNotFound(evaluation_id.to_owned()))?;
            if &receipt.recipient != recipient
                || &receipt.artifact_digest != artifact_digest
                || !state.evaluation_samples.contains_key(evaluation_id)
                || receipt.metric_inputs.len() != opened.record_ids.len()
                || receipt
                    .metric_inputs
                    .iter()
                    .any(|input| input.inclusion_probability.is_none())
                || !receipt.passed
            {
                return Err(LearningError::EvaluationDidNotPass);
            }
            let artifact = state
                .artifacts
                .get(artifact_digest)
                .ok_or_else(|| LearningError::ArtifactNotFound(artifact_digest.to_owned()))?;
            if &artifact.recipient != recipient || &artifact.compatibility != compatibility {
                return Err(LearningError::IncompatibleArtifact);
            }
            if artifact.availability != ArtifactAvailability::Available
                || (!artifact.ever_promoted && artifact.expires_at_ms <= promoted_at_ms)
                || artifact_availability(
                    &artifact.members,
                    &state.validations,
                    promoted_at_ms,
                    limits.revalidation_interval_ms,
                ) != ArtifactAvailability::Available
                || artifact_availability(
                    &artifact.promotion_sources,
                    &state.validations,
                    promoted_at_ms,
                    limits.revalidation_interval_ms,
                ) != ArtifactAvailability::Available
                || artifact_availability(
                    &receipt.evaluation_sources,
                    &state.validations,
                    promoted_at_ms,
                    limits.revalidation_interval_ms,
                ) != ArtifactAvailability::Available
            {
                return Err(LearningError::InvalidMembership);
            }
            let previous_digest = state
                .active
                .insert(recipient.to_owned(), artifact_digest.to_owned());
            if let Some(previous) = previous_digest.as_ref()
                && previous != artifact_digest
            {
                let history = state.rollback.entry(recipient.to_owned()).or_default();
                history.retain(|digest| digest != previous);
                history.push(previous.clone());
            }
            let selected = state
                .artifacts
                .get_mut(artifact_digest)
                .ok_or_else(|| LearningError::ArtifactNotFound(artifact_digest.to_owned()))?;
            selected.promotion_sources = receipt.evaluation_sources.clone();
            selected.ever_promoted = true;
            Ok(PromotionResult {
                active_digest: artifact_digest.to_owned(),
                previous_digest,
            })
        })
    }

    /// Explicitly selects a previously promoted artifact after current compatibility and membership checks.
    pub fn rollback(
        &mut self,
        actor: &RecipientId,
        recipient: &RecipientId,
        artifact_digest: &ArtifactDigest,
        compatibility: &Compatibility,
        rolled_back_at_ms: u64,
    ) -> Result<PromotionResult, LearningError> {
        require_recipient(actor, recipient)?;
        self.update(|state, limits| {
            let artifact = state
                .artifacts
                .get(artifact_digest)
                .ok_or_else(|| LearningError::ArtifactNotFound(artifact_digest.to_owned()))?;
            if &artifact.recipient != recipient || &artifact.compatibility != compatibility {
                return Err(LearningError::IncompatibleArtifact);
            }
            if !artifact.ever_promoted {
                return Err(LearningError::NotRollbackArtifact);
            }
            if artifact.availability != ArtifactAvailability::Available
                || artifact_availability(
                    &artifact.members,
                    &state.validations,
                    rolled_back_at_ms,
                    limits.revalidation_interval_ms,
                ) != ArtifactAvailability::Available
                || artifact_availability(
                    &artifact.promotion_sources,
                    &state.validations,
                    rolled_back_at_ms,
                    limits.revalidation_interval_ms,
                ) != ArtifactAvailability::Available
            {
                return Err(LearningError::InvalidMembership);
            }
            let previous_digest = state
                .active
                .insert(recipient.to_owned(), artifact_digest.to_owned());
            if let Some(previous) = previous_digest.as_ref()
                && previous != artifact_digest
            {
                let history = state.rollback.entry(recipient.to_owned()).or_default();
                history.retain(|digest| digest != previous);
                history.push(previous.clone());
            }
            Ok(PromotionResult {
                active_digest: artifact_digest.to_owned(),
                previous_digest,
            })
        })
    }
}

fn require_recipient(actor: &RecipientId, recipient: &RecipientId) -> Result<(), LearningError> {
    if actor == recipient {
        Ok(())
    } else {
        Err(LearningError::ActorMismatch)
    }
}

fn validate_fit_options(options: &FitOptions, max_ttl_ms: u64) -> Result<(), LearningError> {
    if options.minimum_labels == 0
        || options.minimum_wanted == 0
        || options.minimum_not_needed == 0
        || options.minimum_promptly == 0
        || options.minimum_later == 0
        || options.iterations == 0
        || options.iterations > MAX_FIT_ITERATIONS
        || options.environment.is_empty()
        || options.candidate_ttl_ms == 0
        || options.candidate_ttl_ms > max_ttl_ms
        || !options.regularization.is_finite()
        || options.regularization < 0.0
        || !options.learning_rate.is_finite()
        || options.learning_rate <= 0.0
        || !options.wanted_threshold.is_finite()
        || !(0.0..=1.0).contains(&options.wanted_threshold)
        || !options.timely_threshold.is_finite()
        || !(0.0..=1.0).contains(&options.timely_threshold)
    {
        return Err(LearningError::InvalidPolicy("invalid fitting options"));
    }
    Ok(())
}

fn record_conversation(record: &DecisionRecord) -> Result<String, LearningError> {
    let Some(first) = record.sources.first() else {
        return Err(LearningError::IneligibleRecord(record.id.clone()));
    };
    if record
        .sources
        .iter()
        .any(|source| source.conversation != first.conversation)
    {
        return Err(LearningError::MixedConversationRecord(record.id.clone()));
    }
    Ok(first.conversation.clone())
}

fn features(scores: &Scores) -> [f64; FEATURE_COUNT] {
    let [wanted, prompt, participation, change] = scores.values();
    [1.0, wanted, prompt, participation, change]
}

fn dot(weights: &[f64; FEATURE_COUNT], input: &[f64; FEATURE_COUNT]) -> f64 {
    weights
        .iter()
        .zip(input)
        .map(|(weight, value)| weight * value)
        .sum()
}

fn sigmoid(value: f64) -> f64 {
    if value >= 0.0 {
        1.0 / (1.0 + (-value).exp())
    } else {
        let exp = value.exp();
        exp / (1.0 + exp)
    }
}

fn fit_logistic(
    examples: &[([f64; FEATURE_COUNT], f64)],
    options: &FitOptions,
    stream: u64,
) -> Result<[f64; FEATURE_COUNT], LearningError> {
    if examples.is_empty() {
        return Err(LearningError::InsufficientLabels);
    }
    let mut weights = [0.0; FEATURE_COUNT];
    for (index, weight) in weights.iter_mut().enumerate() {
        let bits = splitmix64(options.seed ^ stream.rotate_left(17) ^ index as u64);
        *weight = ((bits as f64 / u64::MAX as f64) - 0.5) * 0.01;
    }
    let count = examples.len() as f64;
    for _ in 0..options.iterations {
        let mut gradient = [0.0; FEATURE_COUNT];
        for (input, target) in examples {
            let error = sigmoid(dot(&weights, input)) - target;
            for index in 0..FEATURE_COUNT {
                gradient[index] += error * input[index];
            }
        }
        for index in 0..FEATURE_COUNT {
            gradient[index] /= count;
            if index != 0 {
                gradient[index] += options.regularization * weights[index];
            }
            weights[index] -= options.learning_rate * gradient[index];
            if !weights[index].is_finite() {
                return Err(LearningError::NonFinite);
            }
        }
    }
    Ok(weights)
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e3779b97f4a7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

fn artifact_digest(
    recipient: &RecipientId,
    compatibility: &Compatibility,
    model: &LogisticCandidate,
    members: &[SourceVersion],
    training_records: &[RecordId],
    created_at_ms: u64,
    expires_at_ms: u64,
) -> Result<ArtifactDigest, LearningError> {
    let payload = serde_json::to_vec(&(
        recipient,
        compatibility,
        model,
        members,
        training_records,
        created_at_ms,
        expires_at_ms,
    ))
    .map_err(|_| LearningError::InvalidPolicy("artifact serialization failed"))?;
    Ok(format!("{:x}", Sha256::digest(payload)).into())
}

fn sort_and_deduplicate_sources(sources: &mut Vec<SourceVersion>) {
    sources.sort_by(|left, right| {
        left.key
            .channel_id
            .get()
            .cmp(&right.key.channel_id.get())
            .then_with(|| left.key.message_id.get().cmp(&right.key.message_id.get()))
            .then_with(|| left.content_hash.cmp(&right.content_hash))
    });
    sources.dedup_by(|left, right| same_source(left, right));
}

#[derive(Clone, Copy)]
struct Observation {
    target: Option<FeedbackLabel>,
    propensity: Option<f64>,
    source_volume: usize,
    candidate: Admission,
    fixed: Admission,
}

fn latest_recipient_label(feedback: &[Feedback], record: &DecisionRecord) -> Option<FeedbackLabel> {
    feedback
        .iter()
        .filter(|feedback| {
            feedback.record_id == record.id && feedback.annotator == record.recipient
        })
        .max_by_key(|feedback| feedback.assessed_at_ms)
        .map(|feedback| feedback.label)
}

fn evaluation_provider_metrics(
    inputs: &[MetricInput],
) -> Result<EvaluationProviderMetrics, LearningError> {
    let records = inputs.len();
    Ok(EvaluationProviderMetrics {
        latency_ms: observed_u64(
            records,
            inputs.iter().map(|input| {
                input
                    .provider_telemetry
                    .as_ref()
                    .map(|value| value.elapsed_ms)
            }),
        )?,
        usage: EvaluationUsage {
            attempts: observed_u64(
                records,
                inputs.iter().map(|input| {
                    input
                        .provider_telemetry
                        .as_ref()
                        .map(|value| u64::from(value.attempts))
                }),
            )?,
            input_tokens: observed_u64(
                records,
                inputs.iter().map(|input| {
                    input
                        .provider_telemetry
                        .as_ref()
                        .and_then(|value| value.input_tokens)
                }),
            )?,
            output_tokens: observed_u64(
                records,
                inputs.iter().map(|input| {
                    input
                        .provider_telemetry
                        .as_ref()
                        .and_then(|value| value.output_tokens)
                }),
            )?,
        },
        billed_cost: BilledCost::UnavailableNoBilledPrice,
    })
}

fn observed_u64(
    record_count: usize,
    values: impl Iterator<Item = Option<u64>>,
) -> Result<ObservedU64Aggregate, LearningError> {
    let mut observed_records = 0usize;
    let mut total = 0u64;
    for value in values.flatten() {
        observed_records += 1;
        total = total
            .checked_add(value)
            .ok_or(LearningError::InvalidPolicy(
                "provider telemetry total overflow",
            ))?;
    }
    Ok(ObservedU64Aggregate {
        observed_records,
        missing_records: record_count - observed_records,
        total: (observed_records != 0).then_some(total),
    })
}

fn coverage(observations: &[Observation]) -> ReviewCoverage {
    let recipient_labeled = observations
        .iter()
        .filter(|observation| {
            observation
                .target
                .is_some_and(|label| label != FeedbackLabel::Unsure)
        })
        .count();
    let unsure = observations
        .iter()
        .filter(|observation| observation.target == Some(FeedbackLabel::Unsure))
        .count();
    let labeled_weights: Vec<f64> = observations
        .iter()
        .filter(|observation| {
            observation
                .target
                .is_some_and(|label| label != FeedbackLabel::Unsure)
        })
        .filter_map(|observation| observation.propensity.map(|value| 1.0 / value))
        .collect();
    let weight_sum: f64 = labeled_weights.iter().sum();
    let weight_square_sum: f64 = labeled_weights.iter().map(|weight| weight * weight).sum();
    ReviewCoverage {
        total_records: observations.len(),
        recipient_labeled,
        unsure,
        unlabeled: observations.len() - recipient_labeled - unsure,
        labels_with_recorded_propensity: observations
            .iter()
            .filter(|observation| {
                observation.propensity.is_some()
                    && observation
                        .target
                        .is_some_and(|label| label != FeedbackLabel::Unsure)
            })
            .count(),
        inverse_probability_population_estimate: weight_sum,
        effective_sample_size: if weight_square_sum == 0.0 {
            0.0
        } else {
            weight_sum * weight_sum / weight_square_sum
        },
    }
}

fn unknown_fallback(record: &DecisionRecord, predicted: Admission) -> Admission {
    if record.hypothetical == Admission::Unknown {
        Admission::Ordinary
    } else {
        predicted
    }
}

fn admitted(admission: Admission) -> bool {
    matches!(
        admission,
        Admission::Ordinary | Admission::Prompt | Admission::NextTurn
    )
}

fn weighted_estimate(
    observations: &[Observation],
    denominator: impl Fn(FeedbackLabel) -> bool,
    numerator: impl Fn(FeedbackLabel, Admission) -> bool,
    decision: impl Fn(&Observation) -> Admission,
) -> Estimate {
    let mut numerator_sum = 0.0;
    let mut denominator_sum = 0.0;
    let mut square_sum = 0.0;
    for observation in observations {
        let Some(label) = observation.target else {
            continue;
        };
        if label == FeedbackLabel::Unsure || !denominator(label) {
            continue;
        }
        let Some(propensity) = observation.propensity else {
            continue;
        };
        let weight = 1.0 / propensity;
        denominator_sum += weight;
        square_sum += weight * weight;
        if numerator(label, decision(observation)) {
            numerator_sum += weight;
        }
    }
    if denominator_sum == 0.0 {
        return Estimate {
            value: None,
            standard_error: None,
            effective_sample_size: 0.0,
        };
    }
    let value = numerator_sum / denominator_sum;
    let effective_sample_size = denominator_sum * denominator_sum / square_sum;
    let standard_error = (value * (1.0 - value) / effective_sample_size).sqrt();
    Estimate {
        value: Some(value),
        standard_error: Some(standard_error),
        effective_sample_size,
    }
}

fn metrics(
    observations: &[Observation],
    decision: impl Fn(&Observation) -> Admission + Copy,
) -> PolicyMetrics {
    let wanted_recall = weighted_estimate(
        observations,
        |label| {
            matches!(
                label,
                FeedbackLabel::WantedPromptly | FeedbackLabel::WantedLater
            )
        },
        |_, admission| admitted(admission),
        decision,
    );
    let timely_recall = weighted_estimate(
        observations,
        |label| label == FeedbackLabel::WantedPromptly,
        |_, admission| matches!(admission, Admission::Ordinary | Admission::Prompt),
        decision,
    );
    let wanted_delay_rate = weighted_estimate(
        observations,
        |label| {
            matches!(
                label,
                FeedbackLabel::WantedPromptly | FeedbackLabel::WantedLater
            )
        },
        |_, admission| admission == Admission::NextTurn,
        decision,
    );
    let unwanted_delivery_rate = weighted_estimate(
        observations,
        |label| label == FeedbackLabel::NotNeeded,
        |_, admission| admitted(admission),
        decision,
    );
    let admitted_records = observations
        .iter()
        .filter(|observation| admitted(decision(observation)))
        .count();
    let total_source_volume: usize = observations
        .iter()
        .map(|observation| observation.source_volume)
        .sum();
    let admitted_source_volume: usize = observations
        .iter()
        .filter(|observation| admitted(decision(observation)))
        .map(|observation| observation.source_volume)
        .sum();
    PolicyMetrics {
        wanted_recall,
        timely_recall,
        wanted_delay_rate,
        unwanted_delivery_rate,
        admitted_records,
        admitted_source_volume,
        volume_reduction: if observations.is_empty() {
            0.0
        } else {
            1.0 - admitted_records as f64 / observations.len() as f64
        },
        source_volume_reduction: if total_source_volume == 0 {
            0.0
        } else {
            1.0 - admitted_source_volume as f64 / total_source_volume as f64
        },
    }
}

fn evaluation_failures(
    coverage: &ReviewCoverage,
    candidate: &PolicyMetrics,
    limits: &PromotionLimits,
    representative_sample: bool,
) -> Vec<EvaluationFailure> {
    let mut failures = Vec::new();
    if !representative_sample
        || coverage.labels_with_recorded_propensity != coverage.recipient_labeled
    {
        failures.push(EvaluationFailure::MissingRepresentativeSample);
    }
    if coverage.recipient_labeled < limits.minimum_labeled {
        failures.push(EvaluationFailure::InsufficientLabeled);
    }
    let wanted_n = candidate.wanted_recall.effective_sample_size;
    if wanted_n < limits.minimum_wanted as f64 {
        failures.push(EvaluationFailure::InsufficientWanted);
    }
    let promptly_n = candidate.timely_recall.effective_sample_size;
    if promptly_n < limits.minimum_promptly as f64 {
        failures.push(EvaluationFailure::InsufficientPromptlyWanted);
    }
    if candidate.wanted_recall.value.unwrap_or(0.0) < limits.minimum_wanted_recall {
        failures.push(EvaluationFailure::WantedRecall);
    }
    if candidate.timely_recall.value.unwrap_or(0.0) < limits.minimum_timely_recall {
        failures.push(EvaluationFailure::TimelyRecall);
    }
    if candidate.volume_reduction < limits.minimum_volume_reduction {
        failures.push(EvaluationFailure::VolumeReduction);
    }
    if candidate.unwanted_delivery_rate.value.unwrap_or(1.0) > limits.maximum_unwanted_delivery_rate
    {
        failures.push(EvaluationFailure::UnwantedDelivery);
    }
    if [
        &candidate.wanted_recall,
        &candidate.timely_recall,
        &candidate.unwanted_delivery_rate,
    ]
    .into_iter()
    .filter_map(|estimate| estimate.standard_error)
    .any(|error| error > limits.maximum_standard_error)
    {
        failures.push(EvaluationFailure::Uncertainty);
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attention::{
        store::{StoreLimits, ValidationFact, ValidationStatus},
        types::{
            Compatibility, DeliveryState, Probability, ProviderTelemetry, RawJudgment, SourceKey,
            content_hash,
        },
    };
    use serenity::model::id::{ChannelId, MessageId, UserId};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn store_path(directory: &tempfile::TempDir) -> camino::Utf8PathBuf {
        #[cfg(unix)]
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .expect("fixture store directory is owner-private regardless of runner umask");
        camino::Utf8PathBuf::from_path_buf(directory.path().join("attention.json"))
            .expect("temporary paths are UTF-8")
    }

    fn probability(value: f64) -> Probability {
        value.try_into().unwrap()
    }

    fn compatibility() -> Compatibility {
        Compatibility {
            model: "jev-1.13.0".into(),
            rubric: "rubric-v1".into(),
            features: "features-v1".into(),
            brief_version: "brief-v1".into(),
        }
    }

    fn record(id: u64, conversation: &str, at: u64, wanted: f64) -> DecisionRecord {
        DecisionRecord {
            id: format!("r{id}").into(),
            recipient: "recipient".into(),
            sources: vec![SourceVersion {
                key: SourceKey {
                    channel_id: ChannelId::new(1),
                    message_id: MessageId::new(id),
                },
                author_id: UserId::new(2),
                author_kind: crate::attention::types::SourceAuthorKind::DirectHuman,
                conversation: conversation.into(),
                content_hash: content_hash(&format!("source-{id}")),
                observed_at_ms: at,
            }],
            compatibility: compatibility(),
            config_generation: 1,
            incarnation: "incarnation".into(),
            created_at_ms: at,
            expires_at_ms: at + 10_000,
            judgment: Some(RawJudgment {
                model: "jev-1.13.0".into(),
                scores: Scores {
                    wanted: probability(wanted),
                    prompt: probability(wanted),
                    participation: probability(wanted),
                    change: probability(wanted),
                },
                context_sufficient: probability(1.0),
                telemetry: None,
            }),
            policy_digest: Some("fixture-fixed-v1".into()),
            hypothetical: Admission::RetrievalOnly,
            actual: Admission::Ordinary,
            delivery: DeliveryState::Observed,
            selection_probability: None,
        }
    }

    fn label(store: &mut AttentionStore, id: u64, label: FeedbackLabel) {
        let record = store.record(&format!("r{id}").into()).unwrap();
        let sources = record.sources.clone();
        store
            .label(
                &"recipient".into(),
                Feedback {
                    record_id: format!("r{id}").into(),
                    annotator: "recipient".into(),
                    label,
                    assessed_at_ms: 500 + id,
                    source_versions: sources,
                },
            )
            .unwrap();
    }

    fn fit_options() -> FitOptions {
        FitOptions {
            minimum_labels: 4,
            minimum_wanted: 2,
            minimum_not_needed: 1,
            minimum_promptly: 1,
            minimum_later: 1,
            regularization: 0.1,
            iterations: 300,
            learning_rate: 0.2,
            seed: 19,
            environment: "rust-f64-v1".into(),
            wanted_threshold: 0.5,
            timely_threshold: 0.5,
            candidate_ttl_ms: 10_000,
        }
    }

    fn limits() -> StoreLimits {
        StoreLimits {
            max_record_ttl_ms: 20_000,
            revalidation_interval_ms: 20_000,
            ..StoreLimits::default()
        }
    }

    fn populate(store: &mut AttentionStore) {
        let fixtures = [
            (1, "train-a", 100, 0.95, FeedbackLabel::WantedPromptly),
            (2, "train-b", 110, 0.75, FeedbackLabel::WantedLater),
            (3, "train-c", 120, 0.10, FeedbackLabel::NotNeeded),
            (4, "train-d", 130, 0.80, FeedbackLabel::WantedPromptly),
            (5, "eval-a", 300, 0.90, FeedbackLabel::WantedPromptly),
            (6, "eval-b", 310, 0.05, FeedbackLabel::NotNeeded),
        ];
        for (id, conversation, at, score, _) in fixtures {
            let mut record = record(id, conversation, at, score);
            if id == 5 {
                record.judgment.as_mut().unwrap().telemetry = Some(ProviderTelemetry {
                    elapsed_ms: 25,
                    attempts: 2,
                    input_tokens: Some(100),
                    output_tokens: None,
                });
            } else if id == 6 {
                record.hypothetical = Admission::Unknown;
            }
            store.insert_record(record).unwrap();
        }
        for (id, _, _, _, feedback) in fixtures.into_iter().take(4) {
            label(store, id, feedback);
        }
        let batch = store
            .sample_review_batch(&"recipient".into(), 2, 42)
            .unwrap();
        assert!(batch.covers_occupied_strata);
        let mut sampled_ids: Vec<_> = batch
            .items
            .iter()
            .map(|item| item.record_id.clone())
            .collect();
        sampled_ids.sort();
        assert_eq!(
            sampled_ids,
            vec![RecordId::from("r5"), RecordId::from("r6")]
        );
        for (id, _, _, _, feedback) in fixtures.into_iter().skip(4) {
            label(store, id, feedback);
        }
    }

    #[test]
    fn temporal_partition_keeps_a_conversation_whole() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = AttentionStore::open_with_limits(store_path(&directory), limits()).unwrap();
        store.insert_record(record(1, "a", 100, 0.8)).unwrap();
        store.insert_record(record(2, "a", 250, 0.8)).unwrap();
        store.insert_record(record(3, "b", 350, 0.8)).unwrap();
        let partitions = store
            .temporal_partitions(
                &"recipient".into(),
                TemporalPartitionSpec {
                    training_end_ms: 150,
                    validation_end_ms: 300,
                },
            )
            .unwrap();
        assert!(partitions.training.is_empty());
        assert_eq!(
            partitions.validation,
            vec![RecordId::from("r1"), RecordId::from("r2")]
        );
        assert_eq!(partitions.evaluation, vec![RecordId::from("r3")]);
    }

    #[test]
    fn fitting_is_deterministic_and_uses_recipient_labels() {
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        let mut first = AttentionStore::open_with_limits(store_path(&first_dir), limits()).unwrap();
        let mut second =
            AttentionStore::open_with_limits(store_path(&second_dir), limits()).unwrap();
        populate(&mut first);
        populate(&mut second);
        // A disagreeing non-recipient annotation is retained but cannot change the target.
        let sources = first.record(&"r1".into()).unwrap().sources.clone();
        first
            .label(
                &"reviewer".into(),
                Feedback {
                    record_id: "r1".into(),
                    annotator: "reviewer".into(),
                    label: FeedbackLabel::NotNeeded,
                    assessed_at_ms: 999,
                    source_versions: sources,
                },
            )
            .unwrap();
        let ids = ["r1", "r2", "r3", "r4"].map(RecordId::from);
        let a = first
            .fit(
                &"recipient".into(),
                &"recipient".into(),
                &ids,
                compatibility(),
                fit_options(),
                500,
            )
            .unwrap();
        let b = second
            .fit(
                &"recipient".into(),
                &"recipient".into(),
                &ids,
                compatibility(),
                fit_options(),
                500,
            )
            .unwrap();
        assert_eq!(a.digest, b.digest);
        assert_eq!(a.model, b.model);

        let always_prompt = FixedThresholdPolicy {
            wanted: 0.0,
            prompt: 0.0,
            participation: 0.0,
            change: 0.0,
        };
        assert_eq!(
            first.replay_fixed(&"r6".into(), always_prompt).unwrap(),
            Admission::Ordinary
        );
        assert_eq!(
            first.replay_artifact(&a.digest, &"r6".into()).unwrap(),
            Admission::Ordinary
        );
    }
    #[test]
    fn failed_candidate_persistence_does_not_publish_artifact() {
        let directory = tempfile::tempdir().unwrap();
        let path = store_path(&directory);
        let mut store = AttentionStore::open_with_limits(path, limits()).unwrap();
        populate(&mut store);
        let before = store.state.artifacts.len();
        std::fs::create_dir(directory.path().join(".attention.json.tmp")).unwrap();

        let result = store.fit(
            &"recipient".into(),
            &"recipient".into(),
            &["r1".into(), "r2".into(), "r3".into(), "r4".into()],
            compatibility(),
            fit_options(),
            500,
        );
        assert!(matches!(
            result,
            Err(LearningError::Store(StoreError::Io { .. }))
        ));
        assert_eq!(store.state.artifacts.len(), before);
        assert!(store.selected_artifact(&"recipient".into()).is_none());
    }

    #[test]
    fn fitting_rejects_cross_recipient_records_without_committing_candidate() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = AttentionStore::open_with_limits(store_path(&directory), limits()).unwrap();
        populate(&mut store);
        let mut outsider = record(7, "outsider", 140, 0.9);
        outsider.recipient = "other".into();
        let outsider_sources = outsider.sources.clone();
        store.insert_record(outsider).unwrap();
        store
            .label(
                &"other".into(),
                Feedback {
                    record_id: "r7".into(),
                    annotator: "other".into(),
                    label: FeedbackLabel::WantedPromptly,
                    assessed_at_ms: 507,
                    source_versions: outsider_sources,
                },
            )
            .unwrap();
        let before = store.state.artifacts.len();

        let result = store.fit(
            &"recipient".into(),
            &"recipient".into(),
            &["r1".into(), "r2".into(), "r3".into(), "r7".into()],
            compatibility(),
            fit_options(),
            500,
        );
        assert!(matches!(
            result,
            Err(LearningError::IneligibleRecord(id)) if id.as_str() == "r7"
        ));
        assert_eq!(store.state.artifacts.len(), before);
    }

    #[test]
    fn opened_holdout_reuse_invalidates_persisted_evaluation() {
        let directory = tempfile::tempdir().unwrap();
        let path = store_path(&directory);
        let mut store = AttentionStore::open_with_limits(path.clone(), limits()).unwrap();
        populate(&mut store);
        let artifact = store
            .fit(
                &"recipient".into(),
                &"recipient".into(),
                &["r1".into(), "r2".into(), "r3".into(), "r4".into()],
                compatibility(),
                fit_options(),
                500,
            )
            .unwrap();
        store
            .open_evaluation(
                &"recipient".into(),
                &"recipient".into(),
                EvaluationPlan {
                    id: "heldout".into(),
                    record_ids: vec!["r5".into(), "r6".into()],
                    fixed_baseline: FixedThresholdPolicy {
                        wanted: 0.5,
                        prompt: 0.5,
                        participation: 1.0,
                        change: 1.0,
                    },
                    limits: permissive_limits(),
                    opened_at_ms: 600,
                },
            )
            .unwrap();
        assert!(matches!(
            store.open_evaluation(
                &"recipient".into(),
                &"recipient".into(),
                EvaluationPlan {
                    id: "same-heldout-again".into(),
                    record_ids: vec!["r5".into(), "r6".into()],
                    fixed_baseline: FixedThresholdPolicy {
                        wanted: 0.5,
                        prompt: 0.5,
                        participation: 1.0,
                        change: 1.0,
                    },
                    limits: permissive_limits(),
                    opened_at_ms: 601,
                },
            ),
            Err(LearningError::HoldoutTouched)
        ));
        assert!(
            store
                .opened_evaluation(&"same-heldout-again".into())
                .is_none()
        );
        let receipt = store
            .evaluate(
                &"recipient".into(),
                &"recipient".into(),
                &"heldout".into(),
                &artifact.digest,
                700,
            )
            .unwrap();
        assert_eq!(store.evaluation(&"heldout".into()), Some(&receipt));
        assert!(matches!(
            store.evaluate(
                &"recipient".into(),
                &"recipient".into(),
                &"heldout".into(),
                &artifact.digest,
                701,
            ),
            Err(LearningError::EvaluationExists(id)) if id.as_str() == "heldout"
        ));
        assert_eq!(store.evaluation(&"heldout".into()), Some(&receipt));

        let artifact_count = store.state.artifacts.len();
        let result = store.fit(
            &"recipient".into(),
            &"recipient".into(),
            &["r1".into(), "r2".into(), "r3".into(), "r5".into()],
            compatibility(),
            fit_options(),
            702,
        );
        assert!(matches!(
            result,
            Err(LearningError::OpenedEvaluationReused(_))
        ));
        assert_eq!(store.state.artifacts.len(), artifact_count);
        assert!(store.evaluation(&"heldout".into()).is_none());
        assert!(
            store
                .opened_evaluation(&"heldout".into())
                .unwrap()
                .invalidated_for_tuning
        );
        drop(store);
        let reopened = AttentionStore::open_with_limits(path, limits()).unwrap();
        assert!(reopened.evaluation(&"heldout".into()).is_none());
        assert!(
            reopened
                .opened_evaluation(&"heldout".into())
                .unwrap()
                .invalidated_for_tuning
        );
    }

    #[test]
    fn evaluation_reports_weighted_coverage_and_both_baselines() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = AttentionStore::open_with_limits(store_path(&directory), limits()).unwrap();
        populate(&mut store);
        let ids = ["r1", "r2", "r3", "r4"].map(RecordId::from);
        let artifact = store
            .fit(
                &"recipient".into(),
                &"recipient".into(),
                &ids,
                compatibility(),
                fit_options(),
                500,
            )
            .unwrap();
        store
            .open_evaluation(
                &"recipient".into(),
                &"recipient".into(),
                EvaluationPlan {
                    id: "heldout".into(),
                    record_ids: vec!["r5".into(), "r6".into()],
                    fixed_baseline: FixedThresholdPolicy {
                        wanted: 0.5,
                        prompt: 0.5,
                        participation: 1.0,
                        change: 1.0,
                    },
                    limits: permissive_limits(),
                    opened_at_ms: 600,
                },
            )
            .unwrap();
        let receipt = store
            .evaluate(
                &"recipient".into(),
                &"recipient".into(),
                &"heldout".into(),
                &artifact.digest,
                700,
            )
            .unwrap();
        assert_eq!(receipt.coverage.recipient_labeled, 2);
        // Both held-out records came from the complete two-record review population.
        assert_eq!(
            receipt.coverage.inverse_probability_population_estimate,
            2.0
        );
        assert_eq!(receipt.normal_delivery.admitted_records, 2);
        assert_eq!(receipt.fixed_baseline.volume_reduction, 0.0);
        let fallback = receipt
            .metric_inputs
            .iter()
            .find(|input| input.record_id.as_str() == "r6")
            .unwrap();
        assert_eq!(fallback.candidate, Admission::Ordinary);
        assert_eq!(fallback.fixed_baseline, Admission::Ordinary);
        assert_eq!(fallback.normal_delivery, Admission::Ordinary);
        assert_eq!(receipt.provider.latency_ms.observed_records, 1);
        assert_eq!(receipt.provider.latency_ms.missing_records, 1);
        assert_eq!(receipt.provider.latency_ms.total, Some(25));
        assert_eq!(receipt.provider.usage.attempts.total, Some(2));
        assert_eq!(receipt.provider.usage.input_tokens.total, Some(100));
        assert_eq!(receipt.provider.usage.output_tokens.total, None);
        assert_eq!(receipt.provider.usage.output_tokens.missing_records, 2);
        assert_eq!(
            receipt.provider.billed_cost,
            BilledCost::UnavailableNoBilledPrice
        );
    }
    #[test]
    fn incomplete_review_frame_and_record_propensity_cannot_authorize_promotion() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = AttentionStore::open_with_limits(store_path(&directory), limits()).unwrap();
        populate(&mut store);
        let artifact = store
            .fit(
                &"recipient".into(),
                &"recipient".into(),
                &["r1".into(), "r2".into(), "r3".into(), "r4".into()],
                compatibility(),
                fit_options(),
                500,
            )
            .unwrap();
        store.state.review_batches.clear();
        store
            .state
            .feedback
            .retain(|feedback| !matches!(feedback.record_id.as_str(), "r5" | "r6"));
        let short_batch = store
            .sample_review_batch(&"recipient".into(), 1, 43)
            .unwrap();
        assert!(!short_batch.covers_occupied_strata);
        let selected_id = short_batch.items[0].record_id.clone();
        let selected_sources = store.record(&selected_id).unwrap().sources.clone();
        store
            .label(
                &"recipient".into(),
                Feedback {
                    record_id: selected_id.clone(),
                    annotator: "recipient".into(),
                    label: FeedbackLabel::WantedPromptly,
                    assessed_at_ms: 550,
                    source_versions: selected_sources,
                },
            )
            .unwrap();
        assert!(
            store
                .record(&selected_id)
                .unwrap()
                .selection_probability
                .is_some(),
            "the record field alone must not make an incomplete frame representative"
        );
        let opened = store
            .open_evaluation(
                &"recipient".into(),
                &"recipient".into(),
                EvaluationPlan {
                    id: "exploratory".into(),
                    record_ids: vec![selected_id],
                    fixed_baseline: FixedThresholdPolicy {
                        wanted: 0.5,
                        prompt: 0.5,
                        participation: 1.0,
                        change: 1.0,
                    },
                    limits: permissive_limits(),
                    opened_at_ms: 600,
                },
            )
            .unwrap();
        assert!(!store.state.evaluation_samples.contains_key(&opened.id));
        let receipt = store
            .evaluate(
                &"recipient".into(),
                &"recipient".into(),
                &"exploratory".into(),
                &artifact.digest,
                700,
            )
            .unwrap();
        assert!(!receipt.passed);
        assert!(
            receipt
                .failures
                .contains(&EvaluationFailure::MissingRepresentativeSample)
        );
        assert!(
            receipt
                .metric_inputs
                .iter()
                .all(|input| input.inclusion_probability.is_none())
        );
        assert_eq!(receipt.coverage.recipient_labeled, 1);
        assert!(matches!(
            store.promote(
                &"recipient".into(),
                &"recipient".into(),
                &artifact.digest,
                &"exploratory".into(),
                &compatibility(),
                701,
            ),
            Err(LearningError::EvaluationDidNotPass)
        ));
    }

    #[test]
    fn absent_propensity_is_not_treated_as_certainty() {
        let observations = [
            Observation {
                target: Some(FeedbackLabel::WantedPromptly),
                propensity: Some(0.5),
                source_volume: 1,
                candidate: Admission::Prompt,
                fixed: Admission::Prompt,
            },
            Observation {
                target: Some(FeedbackLabel::WantedPromptly),
                propensity: None,
                source_volume: 1,
                candidate: Admission::RetrievalOnly,
                fixed: Admission::RetrievalOnly,
            },
        ];
        let review = coverage(&observations);
        assert_eq!(review.recipient_labeled, 2);
        assert_eq!(review.labels_with_recorded_propensity, 1);
        assert_eq!(review.inverse_probability_population_estimate, 2.0);
        let estimate = weighted_estimate(
            &observations,
            |_| true,
            |_, admission| admitted(admission),
            |observation| observation.candidate,
        );
        assert_eq!(estimate.value, Some(1.0));
        assert_eq!(estimate.effective_sample_size, 1.0);
    }

    #[test]
    fn weighted_estimates_match_hand_calculation_and_report_missing_labels() {
        let observations = [
            Observation {
                target: Some(FeedbackLabel::WantedPromptly),
                propensity: Some(0.25),
                source_volume: 1,
                candidate: Admission::Prompt,
                fixed: Admission::RetrievalOnly,
            },
            Observation {
                target: Some(FeedbackLabel::WantedLater),
                propensity: Some(0.5),
                source_volume: 1,
                candidate: Admission::RetrievalOnly,
                fixed: Admission::RetrievalOnly,
            },
            Observation {
                target: Some(FeedbackLabel::NotNeeded),
                propensity: Some(0.5),
                source_volume: 1,
                candidate: Admission::Prompt,
                fixed: Admission::RetrievalOnly,
            },
            Observation {
                target: Some(FeedbackLabel::Unsure),
                propensity: Some(0.1),
                source_volume: 1,
                candidate: Admission::Prompt,
                fixed: Admission::RetrievalOnly,
            },
            Observation {
                target: None,
                propensity: None,
                source_volume: 1,
                candidate: Admission::RetrievalOnly,
                fixed: Admission::RetrievalOnly,
            },
        ];
        let review = coverage(&observations);
        assert_eq!(review.recipient_labeled, 3);
        assert_eq!(review.unsure, 1);
        assert_eq!(review.unlabeled, 1);
        assert_eq!(review.labels_with_recorded_propensity, 3);
        assert_eq!(review.inverse_probability_population_estimate, 8.0);
        assert!((review.effective_sample_size - 8.0 / 3.0).abs() < f64::EPSILON);

        let measured = metrics(&observations, |observation| observation.candidate);
        let wanted = measured.wanted_recall;
        assert!((wanted.value.unwrap() - 2.0 / 3.0).abs() < f64::EPSILON);
        assert!((wanted.effective_sample_size - 1.8).abs() < f64::EPSILON);
        let hand_standard_error = ((2.0 / 3.0) * (1.0 / 3.0) / 1.8_f64).sqrt();
        assert!((wanted.standard_error.unwrap() - hand_standard_error).abs() < f64::EPSILON);
        assert_eq!(measured.unwanted_delivery_rate.value, Some(1.0));
    }

    #[test]
    fn promotion_requires_recipient_passing_evidence_and_valid_members() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = AttentionStore::open_with_limits(store_path(&directory), limits()).unwrap();
        populate(&mut store);
        let artifact = store
            .fit(
                &"recipient".into(),
                &"recipient".into(),
                &["r1".into(), "r2".into(), "r3".into(), "r4".into()],
                compatibility(),
                fit_options(),
                500,
            )
            .unwrap();
        store
            .open_evaluation(
                &"recipient".into(),
                &"recipient".into(),
                EvaluationPlan {
                    id: "heldout".into(),
                    record_ids: vec!["r5".into(), "r6".into()],
                    fixed_baseline: FixedThresholdPolicy {
                        wanted: 0.5,
                        prompt: 0.5,
                        participation: 1.0,
                        change: 1.0,
                    },
                    limits: permissive_limits(),
                    opened_at_ms: 600,
                },
            )
            .unwrap();
        let receipt = store
            .evaluate(
                &"recipient".into(),
                &"recipient".into(),
                &"heldout".into(),
                &artifact.digest,
                700,
            )
            .unwrap();
        assert!(receipt.passed, "failures: {:?}", receipt.failures);
        assert!(matches!(
            store.promote(
                &"reviewer".into(),
                &"recipient".into(),
                &artifact.digest,
                &"heldout".into(),
                &compatibility(),
                701,
            ),
            Err(LearningError::ActorMismatch)
        ));
        store
            .promote(
                &"recipient".into(),
                &"recipient".into(),
                &artifact.digest,
                &"heldout".into(),
                &compatibility(),
                701,
            )
            .unwrap();
        assert_eq!(
            store.active_artifact(&"recipient".into()).unwrap().digest,
            artifact.digest
        );
        let scores = store
            .record(&"r5".into())
            .unwrap()
            .judgment
            .as_ref()
            .unwrap()
            .scores
            .clone();
        assert!(
            store
                .predict_active(&"recipient".into(), &compatibility(), &scores, 20_100)
                .unwrap()
                .is_some()
        );
        assert!(matches!(
            store.predict_active(&"recipient".into(), &compatibility(), &scores, 20_101),
            Err(LearningError::InvalidMembership)
        ));

        let member = artifact.members[0].clone();
        store
            .revalidate(ValidationFact {
                source: member,
                status: ValidationStatus::Unknown,
                checked_at_ms: 20_102,
            })
            .unwrap();
        assert!(store.active_artifact(&"recipient".into()).is_none());
    }
    #[test]
    fn failed_predeclared_promotion_requirements_remain_unpromoted() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = AttentionStore::open_with_limits(store_path(&directory), limits()).unwrap();
        populate(&mut store);
        let artifact = store
            .fit(
                &"recipient".into(),
                &"recipient".into(),
                &["r1".into(), "r2".into(), "r3".into(), "r4".into()],
                compatibility(),
                fit_options(),
                500,
            )
            .unwrap();
        let mut strict = permissive_limits();
        strict.minimum_labeled = 3;
        strict.minimum_volume_reduction = 1.0;
        store
            .open_evaluation(
                &"recipient".into(),
                &"recipient".into(),
                EvaluationPlan {
                    id: "strict-heldout".into(),
                    record_ids: vec!["r5".into(), "r6".into()],
                    fixed_baseline: FixedThresholdPolicy {
                        wanted: 0.5,
                        prompt: 0.5,
                        participation: 1.0,
                        change: 1.0,
                    },
                    limits: strict,
                    opened_at_ms: 600,
                },
            )
            .unwrap();
        let receipt = store
            .evaluate(
                &"recipient".into(),
                &"recipient".into(),
                &"strict-heldout".into(),
                &artifact.digest,
                700,
            )
            .unwrap();
        assert!(!receipt.passed);
        assert!(
            receipt
                .failures
                .contains(&EvaluationFailure::InsufficientLabeled)
        );
        assert!(
            receipt
                .failures
                .contains(&EvaluationFailure::VolumeReduction)
        );
        assert!(matches!(
            store.promote(
                &"recipient".into(),
                &"recipient".into(),
                &artifact.digest,
                &"strict-heldout".into(),
                &compatibility(),
                701,
            ),
            Err(LearningError::EvaluationDidNotPass)
        ));
        assert!(store.selected_artifact(&"recipient".into()).is_none());
    }

    #[test]
    fn restart_withdraws_active_policy_until_every_member_is_revalidated() {
        let directory = tempfile::tempdir().unwrap();
        let path = store_path(&directory);
        let mut store = AttentionStore::open_with_limits(path.clone(), limits()).unwrap();
        populate(&mut store);
        let artifact = store
            .fit(
                &"recipient".into(),
                &"recipient".into(),
                &["r1".into(), "r2".into(), "r3".into(), "r4".into()],
                compatibility(),
                fit_options(),
                500,
            )
            .unwrap();
        store
            .open_evaluation(
                &"recipient".into(),
                &"recipient".into(),
                EvaluationPlan {
                    id: "heldout".into(),
                    record_ids: vec!["r5".into(), "r6".into()],
                    fixed_baseline: FixedThresholdPolicy {
                        wanted: 0.5,
                        prompt: 0.5,
                        participation: 1.0,
                        change: 1.0,
                    },
                    limits: permissive_limits(),
                    opened_at_ms: 600,
                },
            )
            .unwrap();
        assert!(
            store
                .evaluate(
                    &"recipient".into(),
                    &"recipient".into(),
                    &"heldout".into(),
                    &artifact.digest,
                    700,
                )
                .unwrap()
                .passed
        );
        store
            .promote(
                &"recipient".into(),
                &"recipient".into(),
                &artifact.digest,
                &"heldout".into(),
                &compatibility(),
                701,
            )
            .unwrap();
        drop(store);

        let mut reopened = AttentionStore::open_with_limits(path, limits()).unwrap();
        assert!(reopened.active_artifact(&"recipient".into()).is_none());
        assert_eq!(
            reopened.artifact(&artifact.digest).unwrap().availability,
            ArtifactAvailability::AwaitingRevalidation
        );
        assert_eq!(
            reopened
                .selected_artifact(&"recipient".into())
                .unwrap()
                .digest,
            artifact.digest
        );
        let selected = reopened.selected_artifact(&"recipient".into()).unwrap();
        let sources: Vec<_> = selected
            .members
            .iter()
            .chain(&selected.promotion_sources)
            .cloned()
            .collect();
        let unavailable = sources[0].clone();
        reopened
            .revalidate(ValidationFact {
                source: unavailable.clone(),
                status: ValidationStatus::Unknown,
                checked_at_ms: 702,
            })
            .unwrap();
        let known: Vec<_> = sources
            .into_iter()
            .skip(1)
            .map(|source| ValidationFact {
                source,
                status: ValidationStatus::Known,
                checked_at_ms: 702,
            })
            .collect();
        reopened.revalidate_batch(known).unwrap();
        assert!(reopened.active_artifact(&"recipient".into()).is_none());
        reopened
            .revalidate(ValidationFact {
                source: unavailable,
                status: ValidationStatus::Known,
                checked_at_ms: 703,
            })
            .unwrap();
        assert_eq!(
            reopened
                .active_artifact(&"recipient".into())
                .unwrap()
                .digest,
            artifact.digest
        );
    }
    #[test]
    fn expiration_invalidates_opened_holdout_and_rejects_pending_promotion() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = AttentionStore::open_with_limits(store_path(&directory), limits()).unwrap();
        populate(&mut store);
        let artifact = store
            .fit(
                &"recipient".into(),
                &"recipient".into(),
                &["r1".into(), "r2".into(), "r3".into(), "r4".into()],
                compatibility(),
                fit_options(),
                500,
            )
            .unwrap();
        store
            .open_evaluation(
                &"recipient".into(),
                &"recipient".into(),
                EvaluationPlan {
                    id: "heldout".into(),
                    record_ids: vec!["r5".into(), "r6".into()],
                    fixed_baseline: FixedThresholdPolicy {
                        wanted: 0.5,
                        prompt: 0.5,
                        participation: 1.0,
                        change: 1.0,
                    },
                    limits: permissive_limits(),
                    opened_at_ms: 600,
                },
            )
            .unwrap();
        assert!(
            store
                .evaluate(
                    &"recipient".into(),
                    &"recipient".into(),
                    &"heldout".into(),
                    &artifact.digest,
                    700,
                )
                .unwrap()
                .passed
        );

        let report = store.prune(10_301).unwrap();
        assert_eq!(report.expired_records, 5);
        assert!(report.withdrawn_artifacts.contains(&artifact.digest));
        assert!(store.artifact(&artifact.digest).is_none());
        assert!(store.evaluation(&"heldout".into()).is_none());
        assert!(
            store
                .opened_evaluation(&"heldout".into())
                .unwrap()
                .invalidated_source
        );
        assert!(matches!(
            store.promote(
                &"recipient".into(),
                &"recipient".into(),
                &artifact.digest,
                &"heldout".into(),
                &compatibility(),
                10_302,
            ),
            Err(LearningError::HoldoutTouched)
        ));
        assert!(store.selected_artifact(&"recipient".into()).is_none());
    }

    #[test]
    fn invalidated_active_artifact_can_only_roll_back_to_compatible_unaffected_policy() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = AttentionStore::open_with_limits(store_path(&directory), limits()).unwrap();
        populate(&mut store);
        let first = store
            .fit(
                &"recipient".into(),
                &"recipient".into(),
                &["r1".into(), "r2".into(), "r3".into(), "r4".into()],
                compatibility(),
                fit_options(),
                500,
            )
            .unwrap();
        store
            .open_evaluation(
                &"recipient".into(),
                &"recipient".into(),
                EvaluationPlan {
                    id: "heldout".into(),
                    record_ids: vec!["r5".into(), "r6".into()],
                    fixed_baseline: FixedThresholdPolicy {
                        wanted: 0.5,
                        prompt: 0.5,
                        participation: 1.0,
                        change: 1.0,
                    },
                    limits: permissive_limits(),
                    opened_at_ms: 600,
                },
            )
            .unwrap();
        assert!(
            store
                .evaluate(
                    &"recipient".into(),
                    &"recipient".into(),
                    &"heldout".into(),
                    &first.digest,
                    700
                )
                .unwrap()
                .passed
        );
        store
            .promote(
                &"recipient".into(),
                &"recipient".into(),
                &first.digest,
                &"heldout".into(),
                &compatibility(),
                701,
            )
            .unwrap();

        for (id, score, feedback) in [
            (7, 0.95, FeedbackLabel::WantedPromptly),
            (8, 0.75, FeedbackLabel::WantedLater),
            (9, 0.10, FeedbackLabel::NotNeeded),
            (10, 0.85, FeedbackLabel::WantedPromptly),
        ] {
            store
                .insert_record(record(id, &format!("second-{id}"), 140 + id, score))
                .unwrap();
            label(&mut store, id, feedback);
        }
        let mut second_options = fit_options();
        second_options.seed = 31;
        let second = store
            .fit(
                &"recipient".into(),
                &"recipient".into(),
                &["r7".into(), "r8".into(), "r9".into(), "r10".into()],
                compatibility(),
                second_options,
                702,
            )
            .unwrap();
        let second_evaluation = [
            (
                11,
                "eval-second-a",
                320,
                0.90,
                FeedbackLabel::WantedPromptly,
            ),
            (12, "eval-second-b", 330, 0.05, FeedbackLabel::NotNeeded),
        ];
        for (id, conversation, at, score, _) in second_evaluation {
            store
                .insert_record(record(id, conversation, at, score))
                .unwrap();
        }
        let second_review = store
            .sample_review_batch(&"recipient".into(), 2, 44)
            .unwrap();
        assert!(second_review.covers_occupied_strata);
        for (id, _, _, _, feedback) in second_evaluation {
            label(&mut store, id, feedback);
        }
        store
            .open_evaluation(
                &"recipient".into(),
                &"recipient".into(),
                EvaluationPlan {
                    id: "heldout-second".into(),
                    record_ids: vec!["r11".into(), "r12".into()],
                    fixed_baseline: FixedThresholdPolicy {
                        wanted: 0.5,
                        prompt: 0.5,
                        participation: 1.0,
                        change: 1.0,
                    },
                    limits: permissive_limits(),
                    opened_at_ms: 703,
                },
            )
            .unwrap();
        assert!(
            store
                .evaluate(
                    &"recipient".into(),
                    &"recipient".into(),
                    &"heldout-second".into(),
                    &second.digest,
                    704,
                )
                .unwrap()
                .passed
        );
        store
            .promote(
                &"recipient".into(),
                &"recipient".into(),
                &second.digest,
                &"heldout-second".into(),
                &compatibility(),
                705,
            )
            .unwrap();

        let mut incompatible = compatibility();
        incompatible.features = "changed-features".into();
        assert!(matches!(
            store.rollback(
                &"recipient".into(),
                &"recipient".into(),
                &first.digest,
                &incompatible,
                705,
            ),
            Err(LearningError::IncompatibleArtifact)
        ));

        let invalidated = second.members[0].clone();
        let report = store
            .invalidate_source(invalidated.key, Some(&invalidated.content_hash))
            .unwrap();
        assert!(report.active_withdrawn);
        assert!(store.artifact(&second.digest).is_none());
        assert!(store.artifact(&first.digest).is_some());
        store
            .rollback(
                &"recipient".into(),
                &"recipient".into(),
                &first.digest,
                &compatibility(),
                706,
            )
            .unwrap();
        assert_eq!(
            store.active_artifact(&"recipient".into()).unwrap().digest,
            first.digest
        );
        let pruned = store.prune(20_000).unwrap();
        assert!(pruned.active_withdrawn);
        assert!(store.active_artifact(&"recipient".into()).is_none());
        assert!(store.artifact(&first.digest).is_none());
    }

    fn permissive_limits() -> PromotionLimits {
        PromotionLimits {
            minimum_labeled: 2,
            minimum_wanted: 1,
            minimum_promptly: 1,
            minimum_wanted_recall: 0.0,
            minimum_timely_recall: 0.0,
            minimum_volume_reduction: 0.0,
            maximum_unwanted_delivery_rate: 1.0,
            maximum_standard_error: 1.0,
        }
    }
}
