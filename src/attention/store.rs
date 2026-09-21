//! Atomic, source-bound persistence for attention metadata.
//!
//! The store deliberately has no field capable of retaining Discord message text.  It owns
//! judgments, hashes/handles, attributed feedback, evaluation receipts, and learned-policy
//! membership only.  Every successful mutation replaces the complete JSON document after the
//! replacement has been written and synced.

use super::{
    learning::{ArtifactAvailability, EvaluationReceipt, LearnedArtifact, OpenedEvaluation},
    types::{
        Admission, ArtifactDigest, DecisionRecord, DeliveryState, EvaluationId, Feedback,
        FeedbackLabel, RecipientId, RecordId, SourceKey, SourceVersion,
    },
};
use camino::{Utf8Path, Utf8PathBuf};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, DirBuilder, OpenOptions},
    io::{Read, Write},
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

const STORE_SCHEMA_VERSION: u32 = 1;
const DEFAULT_MAX_RECORDS: usize = 16_384;
const DEFAULT_MAX_FEEDBACK: usize = 32_768;
const DEFAULT_MAX_ARTIFACTS: usize = 256;
const DEFAULT_MAX_EVALUATIONS: usize = 256;
const DEFAULT_MAX_OPENED_EVALUATIONS: usize = 64;
const DEFAULT_MAX_REVIEW_BATCHES: usize = 64;
const DEFAULT_MAX_MEMBERS_PER_ARTIFACT: usize = 4_096;
const DEFAULT_MAX_REVIEW_BATCH: usize = 128;
const DEFAULT_MAX_RECORD_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
const DEFAULT_REVALIDATION_INTERVAL_MS: u64 = 24 * 60 * 60 * 1_000;

/// Hard bounds for one recipient-local metadata store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreLimits {
    pub max_records: usize,
    pub max_feedback: usize,
    pub max_artifacts: usize,
    pub max_evaluations: usize,
    pub max_opened_evaluations: usize,
    pub max_members_per_artifact: usize,
    pub max_review_batch: usize,
    pub max_record_ttl_ms: u64,
    pub revalidation_interval_ms: u64,
}

impl Default for StoreLimits {
    fn default() -> Self {
        Self {
            max_records: DEFAULT_MAX_RECORDS,
            max_feedback: DEFAULT_MAX_FEEDBACK,
            max_artifacts: DEFAULT_MAX_ARTIFACTS,
            max_evaluations: DEFAULT_MAX_EVALUATIONS,
            max_opened_evaluations: DEFAULT_MAX_OPENED_EVALUATIONS,
            max_members_per_artifact: DEFAULT_MAX_MEMBERS_PER_ARTIFACT,
            max_review_batch: DEFAULT_MAX_REVIEW_BATCH,
            max_record_ttl_ms: DEFAULT_MAX_RECORD_TTL_MS,
            revalidation_interval_ms: DEFAULT_REVALIDATION_INTERVAL_MS,
        }
    }
}

/// Current source evidence: unavailable lookup is unknown; confirmed denial or mismatch is invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationStatus {
    Known,
    Unknown,
    Invalid,
}

/// Resolver-observed validation of one exact source version at a recorded time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidationFact {
    pub source: SourceVersion,
    pub status: ValidationStatus,
    pub checked_at_ms: u64,
}

/// Durable consequences of withdrawing a source version and dependent calibration evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InvalidationReport {
    pub affected_records: usize,
    pub removed_feedback: usize,
    pub withdrawn_artifacts: Vec<ArtifactDigest>,
    pub active_withdrawn: bool,
}

/// Durable consequences of retention expiry, including dependent artifact withdrawal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PruneReport {
    pub expired_records: usize,
    pub removed_feedback: usize,
    pub withdrawn_artifacts: Vec<ArtifactDigest>,
    pub active_withdrawn: bool,
}

/// Artifact availability changes caused by one current-source observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RevalidationReport {
    pub status: ValidationStatus,
    pub affected_artifacts: Vec<ArtifactDigest>,
    pub active_withdrawn: bool,
}

/// A sampled review candidate with its true inclusion probability for unbiased estimates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewItem {
    pub record_id: RecordId,
    pub wanted_score: f64,
    pub would_be_deferred: bool,
    pub inclusion_probability: f64,
}

/// Private source-bound provenance for one recipient-local review selection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct ReviewBatch {
    pub recipient: RecipientId,
    pub requested: usize,
    pub seed: u64,
    pub items: Vec<ReviewItem>,
    pub source_versions: BTreeMap<RecordId, Vec<SourceVersion>>,
    #[serde(default)]
    pub covers_occupied_strata: bool,
}

/// Explicit recipient labels converted to wantedness and timeliness training targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RecipientTarget {
    pub wanted: bool,
    pub timely: bool,
}

/// Persistence, integrity, ownership or bounded-state failure; never an empty successful store.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("failed to access attention store `{path}`")]
    Io {
        path: Utf8PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("another process owns attention store at {path}")]
    Locked { path: Utf8PathBuf },
    #[error("attention store `{path}` is corrupt")]
    Corrupt {
        path: Utf8PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("unsupported attention store schema {0}")]
    UnsupportedSchema(u32),
    #[error("attention record `{0}` already exists")]
    DuplicateRecord(RecordId),
    #[error("attention record `{0}` was not found")]
    RecordNotFound(RecordId),
    #[error("attention record {0} was already finalized")]
    RecordAlreadyFinalized(RecordId),
    #[error("attention record {0} finalization changed immutable identity")]
    FinalizationMismatch(RecordId),
    #[error("attention record {0} expired before finalization")]
    RecordExpired(RecordId),
    #[error("feedback source versions do not match attention record `{0}`")]
    FeedbackSourceMismatch(RecordId),
    #[error("feedback annotator does not match authenticated actor")]
    FeedbackActorMismatch,
    #[error("attention store capacity exceeded for {0}")]
    Capacity(&'static str),
    #[error("invalid attention metadata: {0}")]
    Invalid(&'static str),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ValidationEntry {
    pub source: SourceVersion,
    pub status: ValidationStatus,
    pub checked_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct StoreState {
    schema_version: u32,
    pub records: BTreeMap<RecordId, DecisionRecord>,
    pub feedback: Vec<Feedback>,
    pub validations: Vec<ValidationEntry>,
    pub artifacts: BTreeMap<ArtifactDigest, LearnedArtifact>,
    pub evaluations: BTreeMap<EvaluationId, EvaluationReceipt>,
    pub opened_evaluations: BTreeMap<EvaluationId, OpenedEvaluation>,
    #[serde(default)]
    pub evaluation_samples: BTreeMap<EvaluationId, ReviewBatch>,
    #[serde(default)]
    pub review_batches: Vec<ReviewBatch>,
    pub active: BTreeMap<RecipientId, ArtifactDigest>,
    pub rollback: BTreeMap<RecipientId, Vec<ArtifactDigest>>,
}

impl Default for StoreState {
    fn default() -> Self {
        Self {
            schema_version: STORE_SCHEMA_VERSION,
            records: BTreeMap::new(),
            feedback: Vec::new(),
            validations: Vec::new(),
            artifacts: BTreeMap::new(),
            evaluations: BTreeMap::new(),
            opened_evaluations: BTreeMap::new(),
            evaluation_samples: BTreeMap::new(),
            review_batches: Vec::new(),
            active: BTreeMap::new(),
            rollback: BTreeMap::new(),
        }
    }
}

/// Single-owner persisted attention metadata.
///
/// Callers must serialize access to this value.  Mutations clone the bounded state, persist the
/// replacement, and only then publish it in memory.
pub struct AttentionStore {
    path: Utf8PathBuf,
    _lock_file: fs::File,
    pub(super) limits: StoreLimits,
    pub(super) state: StoreState,
}

impl AttentionStore {
    /// Opens a single-owner store at the UTF-8 application data path.
    pub fn open(path: Utf8PathBuf) -> Result<Self, StoreError> {
        Self::open_with_limits(path, StoreLimits::default())
    }

    /// Opens a single-owner store with explicit retention and capacity bounds.
    pub fn open_with_limits(path: Utf8PathBuf, limits: StoreLimits) -> Result<Self, StoreError> {
        let path = canonical_store_path(&path)?;
        let lock_file = lock_store(&path)?;
        validate_limits(limits)?;
        let mut state = read_state(&path)?;
        if state.schema_version != STORE_SCHEMA_VERSION {
            return Err(StoreError::UnsupportedSchema(state.schema_version));
        }
        validate_loaded_bounds(&state, limits)?;

        // Restart invalidates proof of current source existence/access. Keep the recipient's
        // explicit selection, but make it unenforceable until every membership source is known.
        let had_known = state
            .validations
            .iter()
            .any(|entry| entry.status == ValidationStatus::Known);
        let had_available = state
            .artifacts
            .values()
            .any(|artifact| artifact.availability == ArtifactAvailability::Available);
        if had_known || had_available {
            for entry in &mut state.validations {
                if entry.status == ValidationStatus::Known {
                    entry.status = ValidationStatus::Unknown;
                }
            }
            for artifact in state.artifacts.values_mut() {
                if artifact.availability == ArtifactAvailability::Available {
                    artifact.availability = ArtifactAvailability::AwaitingRevalidation;
                }
            }
            persist_state(&path, &state).map_err(PersistFailure::into_error)?;
        }
        Ok(Self {
            path,
            _lock_file: lock_file,
            limits,
            state,
        })
    }

    /// Returns the canonical UTF-8 path backing this store.
    pub fn path(&self) -> &Utf8Path {
        &self.path
    }
    /// Returns the currently enforced storage and retention bounds.
    pub fn limits(&self) -> StoreLimits {
        self.limits
    }

    /// Apply live configuration bounds after checking the retained document against them.
    pub fn update_limits(&mut self, limits: StoreLimits) -> Result<(), StoreError> {
        validate_limits(limits)?;
        validate_loaded_bounds(&self.state, limits)?;
        self.limits = limits;
        Ok(())
    }

    /// Looks up metadata only; callers must revalidate sources before exposing derived content.
    pub fn record(&self, id: &RecordId) -> Option<&DecisionRecord> {
        self.state.records.get(id)
    }

    /// Lists recipient-owned metadata without granting current source access.
    pub fn list_records(&self, recipient: &RecipientId) -> Vec<&DecisionRecord> {
        self.state
            .records
            .values()
            .filter(|record| &record.recipient == recipient)
            .collect()
    }

    /// Persists a bounded decision whose source versions the caller has actually observed.
    /// This method records those observations; it does not fetch Discord or grant access.
    pub fn insert_record(&mut self, record: DecisionRecord) -> Result<(), StoreError> {
        self.update(|state, limits| {
            if state.records.contains_key(&record.id) {
                return Err(StoreError::DuplicateRecord(record.id.clone()));
            }
            if state.records.len() >= limits.max_records {
                return Err(StoreError::Capacity("records"));
            }
            if !record_policy_is_coherent(&record) {
                return Err(StoreError::Invalid(
                    "judgment and policy provenance must be recorded together",
                ));
            }
            if record.sources.is_empty() {
                return Err(StoreError::Invalid(
                    "a record must have at least one source",
                ));
            }
            if record.sources.len() > limits.max_members_per_artifact {
                return Err(StoreError::Capacity("record source membership"));
            }
            if record
                .selection_probability
                .is_some_and(|probability| probability.get() == 0.0)
            {
                return Err(StoreError::Invalid(
                    "sampling probability must be greater than zero",
                ));
            }
            if record.expires_at_ms <= record.created_at_ms
                || record.expires_at_ms - record.created_at_ms > limits.max_record_ttl_ms
            {
                return Err(StoreError::Invalid(
                    "record TTL is outside the configured bound",
                ));
            }
            for source in &record.sources {
                upsert_validation(
                    &mut state.validations,
                    ValidationFact {
                        source: source.clone(),
                        status: ValidationStatus::Known,
                        checked_at_ms: record.created_at_ms,
                    },
                );
            }
            state.records.insert(record.id.clone(), record);
            Ok(())
        })
    }
    /// Atomically attach the one immutable provider judgment and chosen outcome to a persisted
    /// pre-await record. Existing trigger identity is immutable; callers may append authorized
    /// antecedent source versions resolved for the judgment.
    pub fn finalize_record(&mut self, finalized: DecisionRecord) -> Result<(), StoreError> {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| StoreError::Invalid("system clock predates Unix epoch"))?
            .as_millis()
            .try_into()
            .map_err(|_| StoreError::Invalid("system clock does not fit milliseconds"))?;
        self.update(|state, limits| {
            let existing = state
                .records
                .get(&finalized.id)
                .ok_or_else(|| StoreError::RecordNotFound(finalized.id.clone()))?;
            if existing.judgment.is_some() {
                return Err(StoreError::RecordAlreadyFinalized(finalized.id.clone()));
            }
            if existing.delivery == DeliveryState::Invalidated
                || finalized.delivery == DeliveryState::Invalidated
            {
                return Err(StoreError::FinalizationMismatch(finalized.id.clone()));
            }
            if existing.expires_at_ms <= now_ms {
                return Err(StoreError::RecordExpired(finalized.id.clone()));
            }
            let identity_matches = existing.recipient == finalized.recipient
                && existing.compatibility == finalized.compatibility
                && existing.config_generation == finalized.config_generation
                && existing.incarnation == finalized.incarnation
                && existing.created_at_ms == finalized.created_at_ms
                && existing.expires_at_ms == finalized.expires_at_ms
                && existing.selection_probability == finalized.selection_probability
                && finalized.sources.len() >= existing.sources.len()
                && finalized
                    .sources
                    .iter()
                    .zip(&existing.sources)
                    .all(|(new, original)| new == original);
            if !identity_matches
                || existing.policy_digest.is_some()
                || finalized.judgment.is_none()
                || finalized
                    .policy_digest
                    .as_ref()
                    .is_some_and(|digest| digest.as_str().trim().is_empty())
                || (matches!(
                    finalized.actual,
                    Admission::Prompt | Admission::NextTurn | Admission::RetrievalOnly
                ) && finalized.policy_digest.is_none())
            {
                return Err(StoreError::FinalizationMismatch(finalized.id.clone()));
            }
            if finalized.sources.len() > limits.max_members_per_artifact {
                return Err(StoreError::Capacity("record source membership"));
            }
            if finalized.sources.iter().enumerate().any(|(index, source)| {
                finalized.sources[..index]
                    .iter()
                    .any(|prior| same_source(prior, source))
            }) {
                return Err(StoreError::Invalid(
                    "record source membership contains duplicates",
                ));
            }
            if finalized.judgment.as_ref().is_some_and(|judgment| {
                judgment.model.as_str() != finalized.compatibility.model.as_str()
            }) {
                return Err(StoreError::FinalizationMismatch(finalized.id.clone()));
            }
            for source in finalized.sources.iter().skip(existing.sources.len()) {
                upsert_validation(
                    &mut state.validations,
                    ValidationFact {
                        source: source.clone(),
                        status: ValidationStatus::Known,
                        checked_at_ms: now_ms,
                    },
                );
            }
            state.records.insert(finalized.id.clone(), finalized);
            Ok(())
        })
    }

    /// Persists delivery state without allowing an invalidated record to be revived.
    pub fn update_delivery(
        &mut self,
        record_id: &RecordId,
        delivery: DeliveryState,
    ) -> Result<(), StoreError> {
        self.update(|state, _| {
            let record = state
                .records
                .get_mut(record_id)
                .ok_or_else(|| StoreError::RecordNotFound(record_id.to_owned()))?;
            if record.delivery == DeliveryState::Invalidated
                && delivery != DeliveryState::Invalidated
            {
                return Err(StoreError::Invalid(
                    "an invalidated record cannot be revived",
                ));
            }
            record.delivery = delivery;
            Ok(())
        })
    }

    /// Persist attributed feedback.  The actor is explicit so callers cannot smuggle a different
    /// annotator into a trusted control request.
    pub fn label(&mut self, actor: &RecipientId, feedback: Feedback) -> Result<(), StoreError> {
        self.update(|state, limits| {
            if actor != &feedback.annotator {
                return Err(StoreError::FeedbackActorMismatch);
            }
            if state.feedback.len() >= limits.max_feedback {
                return Err(StoreError::Capacity("feedback"));
            }
            let record = state
                .records
                .get(&feedback.record_id)
                .ok_or_else(|| StoreError::RecordNotFound(feedback.record_id.clone()))?;
            if record.delivery == DeliveryState::Invalidated {
                return Err(StoreError::Invalid(
                    "feedback cannot target an invalidated record",
                ));
            }
            if feedback.source_versions != record.sources {
                return Err(StoreError::FeedbackSourceMismatch(
                    feedback.record_id.clone(),
                ));
            }
            state.feedback.push(feedback);
            Ok(())
        })
    }

    /// Returns attributed assessments, including non-recipient feedback retained for audit.
    pub fn feedback(&self, record_id: &RecordId) -> Vec<&Feedback> {
        self.state
            .feedback
            .iter()
            .filter(|feedback| &feedback.record_id == record_id)
            .collect()
    }

    /// Return the recipient's latest assessment.  Other annotators never override it, and an
    /// explicit `Unsure` (like silence) yields no fitting target.
    pub fn recipient_target(&self, record_id: &RecordId) -> Option<RecipientTarget> {
        let record = self.state.records.get(record_id)?;
        latest_recipient_feedback(&self.state.feedback, record).and_then(feedback_target)
    }

    /// Deterministically stratify review candidates across the complete wanted-score range.
    /// Selected records retain their true per-stratum inclusion probability for later estimates.
    pub fn sample_review(
        &mut self,
        recipient: &RecipientId,
        requested: usize,
        seed: u64,
    ) -> Result<Vec<ReviewItem>, StoreError> {
        Ok(self.sample_review_batch(recipient, requested, seed)?.items)
    }

    /// Samples and durably records recipient-local representative review provenance.
    pub(super) fn sample_review_batch(
        &mut self,
        recipient: &RecipientId,
        requested: usize,
        seed: u64,
    ) -> Result<ReviewBatch, StoreError> {
        if requested == 0 {
            return Ok(ReviewBatch {
                recipient: recipient.to_owned(),
                requested,
                seed,
                items: Vec::new(),
                source_versions: BTreeMap::new(),
                covers_occupied_strata: false,
            });
        }
        self.update(|state, limits| {
            if requested > limits.max_review_batch {
                return Err(StoreError::Capacity("review batch"));
            }
            let mut strata: [Vec<RecordId>; 4] = std::array::from_fn(|_| Vec::new());
            for record in state.records.values() {
                if &record.recipient != recipient
                    || record.delivery == DeliveryState::Invalidated
                    || latest_recipient_feedback(&state.feedback, record)
                        .is_some_and(|feedback| feedback.label != FeedbackLabel::Unsure)
                {
                    continue;
                }
                let Some(judgment) = &record.judgment else {
                    continue;
                };
                let score = judgment.scores.wanted.get();
                let index = ((score * 4.0).floor() as usize).min(3);
                strata[index].push(record.id.clone());
            }
            for stratum in &mut strata {
                stratum.sort_by_key(|id| deterministic_rank(seed, id.as_str()));
            }

            let target = requested.min(strata.iter().map(Vec::len).sum());
            let mut allocations = [0usize; 4];
            let mut remaining = target;
            while remaining > 0 {
                let mut advanced = false;
                // Low-to-high order guarantees low-score examples enter any batch large enough
                // to cover the occupied strata; deterministic ranking prevents cherry-picking.
                for index in 0..4 {
                    if remaining == 0 {
                        break;
                    }
                    if allocations[index] < strata[index].len() {
                        allocations[index] += 1;
                        remaining -= 1;
                        advanced = true;
                    }
                }
                if !advanced {
                    break;
                }
            }
            let covers_occupied_strata = strata
                .iter()
                .zip(allocations)
                .all(|(stratum, allocation)| stratum.is_empty() || allocation > 0);

            let mut sampled = Vec::with_capacity(target);
            let mut source_versions = BTreeMap::new();
            for (index, stratum) in strata.iter().enumerate() {
                if allocations[index] == 0 {
                    continue;
                }
                let probability = allocations[index] as f64 / stratum.len() as f64;
                let selection_probability = probability.try_into().map_err(|_| {
                    StoreError::Invalid("sampling probability is not finite and bounded")
                })?;
                for id in stratum.iter().take(allocations[index]) {
                    let record = state
                        .records
                        .get_mut(id)
                        .ok_or_else(|| StoreError::RecordNotFound(id.clone()))?;
                    record.selection_probability = Some(selection_probability);
                    let score = record
                        .judgment
                        .as_ref()
                        .ok_or(StoreError::Invalid("review candidate has no judgment"))?
                        .scores
                        .wanted
                        .get();
                    source_versions.insert(id.clone(), record.sources.clone());
                    sampled.push(ReviewItem {
                        record_id: id.clone(),
                        wanted_score: score,
                        would_be_deferred: matches!(
                            record.hypothetical,
                            super::types::Admission::RetrievalOnly
                        ),
                        inclusion_probability: probability,
                    });
                }
            }
            sampled.sort_by_key(|item| deterministic_rank(seed, item.record_id.as_str()));
            let batch = ReviewBatch {
                recipient: recipient.to_owned(),
                requested,
                seed,
                items: sampled,
                source_versions,
                covers_occupied_strata,
            };
            state.review_batches.retain(|existing| {
                existing.recipient != batch.recipient
                    || existing.requested != batch.requested
                    || existing.seed != batch.seed
            });
            if state.review_batches.len() >= DEFAULT_MAX_REVIEW_BATCHES {
                state.review_batches.remove(0);
            }
            state.review_batches.push(batch.clone());
            Ok(batch)
        })
    }

    /// Invalidate a source key, optionally only one content version.  Scores and labels disappear;
    /// every artifact containing that source is removed and an active match is withdrawn.
    pub fn invalidate_source(
        &mut self,
        key: SourceKey,
        content_hash: Option<&str>,
    ) -> Result<InvalidationReport, StoreError> {
        self.update(|state, _| {
            let matches = |source: &SourceVersion| {
                source.key == key
                    && content_hash.is_none_or(|hash| source.content_hash.as_str() == hash)
            };
            let affected_ids: BTreeSet<RecordId> = state
                .records
                .values_mut()
                .filter(|record| record.sources.iter().any(&matches))
                .map(|record| {
                    record.judgment = None;
                    record.policy_digest = None;
                    record.hypothetical = super::types::Admission::Unknown;
                    record.actual = super::types::Admission::Unknown;
                    record.delivery = DeliveryState::Invalidated;
                    record.id.clone()
                })
                .collect();
            let feedback_before = state.feedback.len();
            state
                .feedback
                .retain(|feedback| !affected_ids.contains(&feedback.record_id));
            let includes_affected_record = |batch: &ReviewBatch| {
                batch
                    .items
                    .iter()
                    .any(|item| affected_ids.contains(&item.record_id))
            };
            state
                .review_batches
                .retain(|batch| !includes_affected_record(batch));
            state
                .evaluation_samples
                .retain(|_, batch| !includes_affected_record(batch));
            for entry in &mut state.validations {
                if matches(&entry.source) {
                    entry.status = ValidationStatus::Invalid;
                }
            }
            let (withdrawn_artifacts, active_withdrawn) =
                remove_affected_artifacts(state, &matches);
            Ok(InvalidationReport {
                affected_records: affected_ids.len(),
                removed_feedback: feedback_before - state.feedback.len(),
                withdrawn_artifacts,
                active_withdrawn,
            })
        })
    }

    /// Remove expired decision evidence.  Artifacts with expired members cannot survive expiry,
    /// including an active artifact.
    pub fn prune(&mut self, now_ms: u64) -> Result<PruneReport, StoreError> {
        self.update(|state, _| {
            let expired_sources: Vec<SourceVersion> = state
                .records
                .values()
                .filter(|record| record.expires_at_ms <= now_ms)
                .flat_map(|record| record.sources.iter().cloned())
                .collect();
            let expired_ids: BTreeSet<RecordId> = state
                .records
                .iter()
                .filter(|(_, record)| record.expires_at_ms <= now_ms)
                .map(|(id, _)| id.clone())
                .collect();
            let matches = |source: &SourceVersion| {
                expired_sources
                    .iter()
                    .any(|expired| same_source(expired, source))
            };
            // Mark opened holdouts while their record-to-source membership is still present.
            // Removing records first would leave an apparently usable opened evaluation whose
            // source evidence had expired.
            let (mut withdrawn_artifacts, mut active_withdrawn) =
                remove_affected_artifacts(state, &matches);
            for id in &expired_ids {
                state.records.remove(id);
            }
            let feedback_before = state.feedback.len();
            state
                .feedback
                .retain(|feedback| !expired_ids.contains(&feedback.record_id));
            let includes_expired_record = |batch: &ReviewBatch| {
                batch
                    .items
                    .iter()
                    .any(|item| expired_ids.contains(&item.record_id))
            };
            state
                .review_batches
                .retain(|batch| !includes_expired_record(batch));
            state
                .evaluation_samples
                .retain(|_, batch| !includes_expired_record(batch));
            state.validations.retain(|entry| {
                !expired_sources
                    .iter()
                    .any(|source| same_source(source, &entry.source))
            });

            let expired_candidates: BTreeSet<ArtifactDigest> = state
                .artifacts
                .iter()
                .filter(|(_, artifact)| !artifact.ever_promoted && artifact.expires_at_ms <= now_ms)
                .map(|(digest, _)| digest.clone())
                .collect();
            if !expired_candidates.is_empty() {
                for digest in &expired_candidates {
                    state.artifacts.remove(digest);
                    withdrawn_artifacts.push(digest.clone());
                }
                let active_before = state.active.len();
                state
                    .active
                    .retain(|_, digest| !expired_candidates.contains(digest));
                active_withdrawn |= active_before != state.active.len();
                remove_artifact_references(state, &expired_candidates);
            }
            withdrawn_artifacts.sort();
            withdrawn_artifacts.dedup();
            Ok(PruneReport {
                expired_records: expired_ids.len(),
                removed_feedback: feedback_before - state.feedback.len(),
                withdrawn_artifacts,
                active_withdrawn,
            })
        })
    }

    /// Apply exactly the fact supplied by the resolver.  Unknown current state makes dependent
    /// artifacts unavailable and withdraws enforcement; it is never upgraded to valid.
    pub fn revalidate(&mut self, fact: ValidationFact) -> Result<RevalidationReport, StoreError> {
        let mut reports = self.revalidate_batch(vec![fact])?;
        reports
            .pop()
            .ok_or(StoreError::Invalid("revalidation produced no report"))
    }

    /// Apply a coherent, bounded resolver snapshot in one atomic store replacement.
    pub fn revalidate_batch(
        &mut self,
        facts: Vec<ValidationFact>,
    ) -> Result<Vec<RevalidationReport>, StoreError> {
        if facts.is_empty() {
            return Ok(Vec::new());
        }
        if facts.len() > self.limits.max_members_per_artifact.saturating_mul(2) {
            return Err(StoreError::Capacity("revalidation batch"));
        }
        let mut seen = BTreeSet::new();
        if facts
            .iter()
            .any(|fact| !seen.insert((fact.source.key, fact.source.content_hash.clone())))
        {
            return Err(StoreError::Invalid(
                "revalidation batch contains duplicate sources",
            ));
        }
        let checked_at_ms = facts
            .iter()
            .map(|fact| fact.checked_at_ms)
            .max()
            .ok_or(StoreError::Invalid("validation batch has no timestamp"))?;
        self.update(|state, limits| {
            facts
                .into_iter()
                .map(|fact| apply_revalidation(state, limits, fact, checked_at_ms))
                .collect()
        })
    }

    /// Checks that every exact source has known evidence within the configured freshness window.
    pub fn sources_current(
        &self,
        sources: &[SourceVersion],
        now_ms: u64,
        maximum_age_ms: u64,
    ) -> bool {
        artifact_availability(sources, &self.state.validations, now_ms, maximum_age_ms)
            == ArtifactAvailability::Available
    }

    /// The explicitly selected artifact, only while all source evidence is currently available.
    pub fn active_artifact(&self, recipient: &RecipientId) -> Option<&LearnedArtifact> {
        self.selected_artifact(recipient)
            .filter(|artifact| artifact.availability == ArtifactAvailability::Available)
    }

    /// The recipient's explicit selection, including a policy awaiting bounded revalidation.
    pub fn selected_artifact(&self, recipient: &RecipientId) -> Option<&LearnedArtifact> {
        self.state
            .active
            .get(recipient)
            .and_then(|digest| self.state.artifacts.get(digest))
    }
    /// Looks up artifact metadata without asserting source freshness or promotion eligibility.
    pub fn artifact(&self, digest: &ArtifactDigest) -> Option<&LearnedArtifact> {
        self.state.artifacts.get(digest)
    }

    /// Returns the stored result of a held-out evaluation, not an authorization to promote.
    pub fn evaluation(&self, evaluation_id: &EvaluationId) -> Option<&EvaluationReceipt> {
        self.state.evaluations.get(evaluation_id)
    }

    /// Returns the predeclared holdout, including any recorded source or tuning invalidation.
    pub fn opened_evaluation(&self, evaluation_id: &EvaluationId) -> Option<&OpenedEvaluation> {
        self.state.opened_evaluations.get(evaluation_id)
    }

    pub(super) fn update<R, E>(
        &mut self,
        mutation: impl FnOnce(&mut StoreState, StoreLimits) -> Result<R, E>,
    ) -> Result<R, E>
    where
        E: From<StoreError>,
    {
        self.update_with_persistence(mutation, persist_state)
    }

    fn update_with_persistence<R, E>(
        &mut self,
        mutation: impl FnOnce(&mut StoreState, StoreLimits) -> Result<R, E>,
        persistence: impl FnOnce(&Utf8Path, &StoreState) -> Result<(), PersistFailure>,
    ) -> Result<R, E>
    where
        E: From<StoreError>,
    {
        let mut next = self.state.clone();
        let result = mutation(&mut next, self.limits)?;
        match persistence(&self.path, &next) {
            Ok(()) => {
                self.state = next;
                Ok(result)
            }
            Err(failure) => {
                if failure.replacement_committed {
                    // The rename already made this exact state authoritative. Keep memory
                    // coherent while surfacing the unresolved directory durability failure.
                    self.state = next;
                }
                Err(E::from(failure.error))
            }
        }
    }

    pub(super) fn sources_are_known(&self, sources: &[SourceVersion], now_ms: u64) -> bool {
        artifact_availability(
            sources,
            &self.state.validations,
            now_ms,
            self.limits.revalidation_interval_ms,
        ) == ArtifactAvailability::Available
    }
}

fn apply_revalidation(
    state: &mut StoreState,
    limits: StoreLimits,
    fact: ValidationFact,
    evaluated_at_ms: u64,
) -> Result<RevalidationReport, StoreError> {
    if !state.records.values().any(|record| {
        record
            .sources
            .iter()
            .any(|source| same_source(source, &fact.source))
    }) {
        return Err(StoreError::Invalid(
            "validation fact has no retained source",
        ));
    }
    upsert_validation(&mut state.validations, fact.clone());
    if fact.status == ValidationStatus::Invalid {
        let affected_ids: BTreeSet<RecordId> = state
            .records
            .values_mut()
            .filter(|record| {
                record
                    .sources
                    .iter()
                    .any(|source| same_source(source, &fact.source))
            })
            .map(|record| {
                record.judgment = None;
                record.policy_digest = None;
                record.hypothetical = super::types::Admission::Unknown;
                record.actual = super::types::Admission::Unknown;
                record.delivery = DeliveryState::Invalidated;
                record.id.clone()
            })
            .collect();
        state
            .feedback
            .retain(|feedback| !affected_ids.contains(&feedback.record_id));
        let (affected_artifacts, active_withdrawn) =
            remove_affected_artifacts(state, &|source| same_source(source, &fact.source));
        return Ok(RevalidationReport {
            status: fact.status,
            affected_artifacts,
            active_withdrawn,
        });
    }

    let mut affected_artifacts = Vec::new();
    let active_available_before = state
        .active
        .values()
        .filter(|digest| {
            state
                .artifacts
                .get(*digest)
                .is_some_and(|artifact| artifact.availability == ArtifactAvailability::Available)
        })
        .count();
    for artifact in state.artifacts.values_mut() {
        if artifact
            .members
            .iter()
            .chain(&artifact.promotion_sources)
            .any(|source| same_source(source, &fact.source))
        {
            affected_artifacts.push(artifact.digest.clone());
            artifact.availability = artifact_availability(
                &artifact.members,
                &state.validations,
                evaluated_at_ms,
                limits.revalidation_interval_ms,
            );
            if artifact.availability == ArtifactAvailability::Available {
                artifact.availability = artifact_availability(
                    &artifact.promotion_sources,
                    &state.validations,
                    evaluated_at_ms,
                    limits.revalidation_interval_ms,
                );
            }
        }
    }
    let active_available_after = state
        .active
        .values()
        .filter(|digest| {
            state
                .artifacts
                .get(*digest)
                .is_some_and(|artifact| artifact.availability == ArtifactAvailability::Available)
        })
        .count();
    Ok(RevalidationReport {
        status: fact.status,
        affected_artifacts,
        active_withdrawn: active_available_after < active_available_before,
    })
}

fn validate_limits(limits: StoreLimits) -> Result<(), StoreError> {
    if limits.max_records == 0
        || limits.max_feedback == 0
        || limits.max_artifacts == 0
        || limits.max_evaluations == 0
        || limits.max_opened_evaluations == 0
        || limits.max_members_per_artifact == 0
        || limits.max_review_batch == 0
        || limits.max_record_ttl_ms == 0
        || limits.revalidation_interval_ms == 0
    {
        return Err(StoreError::Invalid("store limits must be nonzero"));
    }
    Ok(())
}

fn validate_loaded_bounds(state: &StoreState, limits: StoreLimits) -> Result<(), StoreError> {
    if state.records.len() > limits.max_records {
        return Err(StoreError::Capacity("records"));
    }
    if state.records.values().any(|record| {
        record.expires_at_ms <= record.created_at_ms
            || record.expires_at_ms - record.created_at_ms > limits.max_record_ttl_ms
    }) {
        return Err(StoreError::Invalid(
            "retained record TTL exceeds the configured bound",
        ));
    }
    if state.records.values().any(|record| {
        record
            .selection_probability
            .is_some_and(|probability| probability.get() == 0.0)
    }) {
        return Err(StoreError::Invalid("retained sampling probability is zero"));
    }
    if state.feedback.len() > limits.max_feedback {
        return Err(StoreError::Capacity("feedback"));
    }
    if state.feedback.iter().any(|feedback| {
        !state
            .records
            .get(&feedback.record_id)
            .is_some_and(|record| feedback.source_versions == record.sources)
    }) {
        return Err(StoreError::Invalid(
            "retained feedback does not match a valid record",
        ));
    }
    if state.artifacts.len() > limits.max_artifacts {
        return Err(StoreError::Capacity("artifacts"));
    }
    if state.evaluations.len() > limits.max_evaluations {
        return Err(StoreError::Capacity("evaluations"));
    }
    if state.opened_evaluations.len() > limits.max_opened_evaluations {
        return Err(StoreError::Capacity("opened evaluations"));
    }
    if state.review_batches.len() > DEFAULT_MAX_REVIEW_BATCHES {
        return Err(StoreError::Capacity("review batches"));
    }
    if state.evaluation_samples.len() > limits.max_opened_evaluations {
        return Err(StoreError::Capacity("evaluation samples"));
    }
    if state
        .review_batches
        .iter()
        .chain(state.evaluation_samples.values())
        .any(|batch| !review_batch_is_valid(batch, state, limits))
        || state
            .evaluation_samples
            .iter()
            .any(|(evaluation_id, batch)| {
                !batch.covers_occupied_strata
                    || !state
                        .opened_evaluations
                        .get(evaluation_id)
                        .is_some_and(|opened| {
                            opened.recipient == batch.recipient
                                && opened.record_ids.len() == batch.items.len()
                                && opened.record_ids.iter().all(|record_id| {
                                    batch.items.iter().any(|item| &item.record_id == record_id)
                                })
                        })
            })
    {
        return Err(StoreError::Invalid("retained review provenance is invalid"));
    }
    if state.artifacts.values().any(|artifact| {
        artifact.members.len() > limits.max_members_per_artifact
            || artifact.promotion_sources.len() > limits.max_members_per_artifact
    }) {
        return Err(StoreError::Capacity("artifact membership"));
    }
    if state.records.iter().any(|(id, record)| {
        id != &record.id
            || record.sources.is_empty()
            || record.sources.len() > limits.max_members_per_artifact
    }) {
        return Err(StoreError::Invalid(
            "retained record identity or membership is invalid",
        ));
    }
    if state
        .records
        .values()
        .any(|record| !record_policy_is_coherent(record))
    {
        return Err(StoreError::Invalid(
            "retained judgment and policy provenance disagree",
        ));
    }
    if state
        .opened_evaluations
        .values()
        .any(|opened| opened.record_ids.len() > limits.max_members_per_artifact)
        || state
            .evaluations
            .values()
            .any(|receipt| receipt.evaluation_sources.len() > limits.max_members_per_artifact)
    {
        return Err(StoreError::Capacity("evaluation membership"));
    }
    if state.active.iter().any(|(recipient, digest)| {
        !state.artifacts.get(digest).is_some_and(|artifact| {
            &artifact.recipient == recipient
                && artifact.ever_promoted
                && artifact.availability != ArtifactAvailability::Invalid
        })
    }) {
        return Err(StoreError::Invalid("active artifact reference is invalid"));
    }
    Ok(())
}

fn review_batch_is_valid(batch: &ReviewBatch, state: &StoreState, limits: StoreLimits) -> bool {
    if batch.requested == 0
        || batch.requested > limits.max_review_batch
        || batch.items.len() > batch.requested
        || batch.source_versions.len() != batch.items.len()
    {
        return false;
    }
    let mut ids = BTreeSet::new();
    !batch.items.iter().any(|item| {
        let Some(source_versions) = batch.source_versions.get(&item.record_id) else {
            return true;
        };
        !ids.insert(&item.record_id)
            || source_versions.is_empty()
            || source_versions.len() > limits.max_members_per_artifact
            || !item.wanted_score.is_finite()
            || !(0.0..=1.0).contains(&item.wanted_score)
            || !item.inclusion_probability.is_finite()
            || !(0.0 < item.inclusion_probability && item.inclusion_probability <= 1.0)
            || !state.records.get(&item.record_id).is_some_and(|record| {
                record.recipient == batch.recipient
                    && &record.sources == source_versions
                    && record
                        .judgment
                        .as_ref()
                        .is_some_and(|judgment| judgment.scores.wanted.get() == item.wanted_score)
                    && matches!(record.hypothetical, Admission::RetrievalOnly)
                        == item.would_be_deferred
            })
    })
}
fn record_policy_is_coherent(record: &DecisionRecord) -> bool {
    match (&record.judgment, &record.policy_digest) {
        (None, None) => true,
        (Some(_), Some(digest)) => !digest.as_str().trim().is_empty(),
        (Some(_), None) => !matches!(
            record.actual,
            Admission::Prompt | Admission::NextTurn | Admission::RetrievalOnly
        ),
        (None, Some(_)) => false,
    }
}

#[derive(Debug)]
struct PersistFailure {
    error: StoreError,
    replacement_committed: bool,
}

impl PersistFailure {
    fn before_commit(error: StoreError) -> Self {
        Self {
            error,
            replacement_committed: false,
        }
    }

    fn after_commit(error: StoreError) -> Self {
        Self {
            error,
            replacement_committed: true,
        }
    }

    fn into_error(self) -> StoreError {
        self.error
    }
}

fn canonical_store_path(path: &Utf8Path) -> Result<Utf8PathBuf, StoreError> {
    let file_name = path
        .file_name()
        .ok_or(StoreError::Invalid("attention store path must name a file"))?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_str().is_empty())
        .unwrap_or_else(|| Utf8Path::new("."));
    create_private_directory(parent)?;
    let canonical_parent =
        Utf8PathBuf::from_path_buf(fs::canonicalize(parent).map_err(|source| StoreError::Io {
            path: parent.to_path_buf(),
            source,
        })?)
        .map_err(|_| StoreError::Invalid("attention store path must resolve to UTF-8"))?;
    let canonical = canonical_parent.join(file_name);
    reject_symlink(&canonical)?;
    Ok(canonical)
}

fn create_private_directory(path: &Utf8Path) -> Result<(), StoreError> {
    reject_symlink_ancestors(path)?;
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(path).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    reject_symlink_ancestors(path)?;
    let metadata = fs::metadata(path).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.is_dir() {
        return Err(StoreError::Invalid(
            "attention store parent must be a directory",
        ));
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(StoreError::Invalid(
            "attention store directory must be owner-private",
        ));
    }
    Ok(())
}

fn reject_symlink_ancestors(path: &Utf8Path) -> Result<(), StoreError> {
    for ancestor in path.ancestors() {
        reject_symlink(ancestor)?;
    }
    Ok(())
}

fn reject_symlink(path: &Utf8Path) -> Result<(), StoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(StoreError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::other("symlink rejected"),
        }),
        Ok(_) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(StoreError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn harden_file(file: &fs::File, path: &Utf8Path) -> Result<(), StoreError> {
    let metadata = file.metadata().map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.file_type().is_file() {
        return Err(StoreError::Invalid(
            "attention store artifact is not a regular file",
        ));
    }
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(())
}

fn read_state(path: &Utf8Path) -> Result<StoreState, StoreError> {
    reject_symlink(path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StoreState::default());
        }
        Err(source) => {
            return Err(StoreError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    harden_file(&file, path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    serde_json::from_slice(&bytes).map_err(|source| StoreError::Corrupt {
        path: path.to_path_buf(),
        source,
    })
}

fn lock_store(path: &Utf8Path) -> Result<fs::File, StoreError> {
    let file_name = path
        .file_name()
        .ok_or(StoreError::Invalid("attention store path must name a file"))?;
    let lock_path = path.with_file_name(format!(".{file_name}.lock"));
    reject_symlink(&lock_path)?;
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let lock_file = options.open(&lock_path).map_err(|source| StoreError::Io {
        path: lock_path.clone(),
        source,
    })?;
    harden_file(&lock_file, &lock_path)?;
    lock_file
        .try_lock_exclusive()
        .map_err(|_| StoreError::Locked { path: lock_path })?;
    Ok(lock_file)
}

fn persist_state(path: &Utf8Path, state: &StoreState) -> Result<(), PersistFailure> {
    persist_state_with_parent_sync(path, state, sync_directory)
}

fn persist_state_with_parent_sync(
    path: &Utf8Path,
    state: &StoreState,
    sync_parent: impl FnOnce(&Utf8Path) -> std::io::Result<()>,
) -> Result<(), PersistFailure> {
    let bytes = serde_json::to_vec_pretty(state).map_err(|source| {
        PersistFailure::before_commit(StoreError::Corrupt {
            path: path.to_path_buf(),
            source,
        })
    })?;
    reject_symlink(path).map_err(PersistFailure::before_commit)?;
    let file_name = path.file_name().unwrap_or("attention.json");
    let temporary = path.with_file_name(format!(".{file_name}.tmp"));
    reject_symlink(&temporary).map_err(PersistFailure::before_commit)?;
    match open_existing_private_file(&temporary) {
        Ok(Some(file)) => {
            drop(file);
            fs::remove_file(&temporary).map_err(|source| {
                PersistFailure::before_commit(StoreError::Io {
                    path: temporary.clone(),
                    source,
                })
            })?;
        }
        Ok(None) => {}
        Err(error) => return Err(PersistFailure::before_commit(error)),
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let mut file = options.open(&temporary).map_err(|source| {
        PersistFailure::before_commit(StoreError::Io {
            path: temporary.clone(),
            source,
        })
    })?;
    harden_file(&file, &temporary).map_err(PersistFailure::before_commit)?;
    if let Err(source) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(PersistFailure::before_commit(StoreError::Io {
            path: temporary,
            source,
        }));
    }
    drop(file);
    reject_symlink(path).map_err(PersistFailure::before_commit)?;
    if let Err(source) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(PersistFailure::before_commit(StoreError::Io {
            path: path.to_path_buf(),
            source,
        }));
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_str().is_empty())
        .unwrap_or_else(|| Utf8Path::new("."));
    sync_parent(parent).map_err(|source| {
        PersistFailure::after_commit(StoreError::Io {
            path: parent.to_path_buf(),
            source,
        })
    })
}

fn open_existing_private_file(path: &Utf8Path) -> Result<Option<fs::File>, StoreError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    match options.open(path) {
        Ok(file) => {
            harden_file(&file, path)?;
            Ok(Some(file))
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(StoreError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn sync_directory(path: &Utf8Path) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW);
    let directory = options.open(path)?;
    if !directory.metadata()?.is_dir() {
        return Err(std::io::Error::other("store parent is not a directory"));
    }
    directory.sync_all()
}

fn upsert_validation(entries: &mut Vec<ValidationEntry>, fact: ValidationFact) {
    if let Some(entry) = entries
        .iter_mut()
        .find(|entry| same_source(&entry.source, &fact.source))
    {
        entry.status = fact.status;
        entry.checked_at_ms = fact.checked_at_ms;
    } else {
        entries.push(ValidationEntry {
            source: fact.source,
            status: fact.status,
            checked_at_ms: fact.checked_at_ms,
        });
    }
}

pub(super) fn same_source(left: &SourceVersion, right: &SourceVersion) -> bool {
    left.key == right.key && left.content_hash == right.content_hash
}

pub(super) fn artifact_availability(
    members: &[SourceVersion],
    validations: &[ValidationEntry],
    now_ms: u64,
    revalidation_interval_ms: u64,
) -> ArtifactAvailability {
    let mut unknown = false;
    for member in members {
        let Some(entry) = validations
            .iter()
            .find(|entry| same_source(&entry.source, member))
        else {
            unknown = true;
            continue;
        };
        match entry.status {
            ValidationStatus::Invalid => return ArtifactAvailability::Invalid,
            ValidationStatus::Unknown => unknown = true,
            ValidationStatus::Known => {
                if now_ms.saturating_sub(entry.checked_at_ms) > revalidation_interval_ms {
                    unknown = true;
                }
            }
        }
    }
    if unknown {
        ArtifactAvailability::AwaitingRevalidation
    } else {
        ArtifactAvailability::Available
    }
}

fn latest_recipient_feedback<'a>(
    feedback: &'a [Feedback],
    record: &DecisionRecord,
) -> Option<&'a Feedback> {
    feedback
        .iter()
        .filter(|feedback| {
            feedback.record_id == record.id && feedback.annotator == record.recipient
        })
        .max_by_key(|feedback| feedback.assessed_at_ms)
}

fn feedback_target(feedback: &Feedback) -> Option<RecipientTarget> {
    match feedback.label {
        FeedbackLabel::WantedPromptly => Some(RecipientTarget {
            wanted: true,
            timely: true,
        }),
        FeedbackLabel::WantedLater => Some(RecipientTarget {
            wanted: true,
            timely: false,
        }),
        FeedbackLabel::NotNeeded => Some(RecipientTarget {
            wanted: false,
            timely: false,
        }),
        FeedbackLabel::Unsure => None,
    }
}

fn deterministic_rank(seed: u64, value: &str) -> u64 {
    // Stable FNV-1a, seeded explicitly.  `DefaultHasher` is intentionally avoided because its
    // reproducibility is not part of the standard-library contract.
    let mut hash = 0xcbf29ce484222325u64 ^ seed;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn remove_affected_artifacts(
    state: &mut StoreState,
    matches: &impl Fn(&SourceVersion) -> bool,
) -> (Vec<ArtifactDigest>, bool) {
    let affected_record_ids: BTreeSet<&str> = state
        .records
        .values()
        .filter(|record| record.sources.iter().any(matches))
        .map(|record| record.id.as_str())
        .collect();
    for opened in state.opened_evaluations.values_mut() {
        if opened
            .record_ids
            .iter()
            .any(|record_id| affected_record_ids.contains(record_id.as_str()))
        {
            opened.invalidated_source = true;
        }
    }
    state
        .evaluations
        .retain(|_, receipt| !receipt.evaluation_sources.iter().any(matches));
    let affected: BTreeSet<ArtifactDigest> = state
        .artifacts
        .iter()
        .filter(|(_, artifact)| {
            artifact.members.iter().any(matches) || artifact.promotion_sources.iter().any(matches)
        })
        .map(|(digest, _)| digest.clone())
        .collect();
    for digest in &affected {
        state.artifacts.remove(digest);
    }
    let active_before = state.active.len();
    state.active.retain(|_, digest| !affected.contains(digest));
    remove_artifact_references(state, &affected);
    (
        affected.into_iter().collect(),
        active_before != state.active.len(),
    )
}

fn remove_artifact_references(state: &mut StoreState, affected: &BTreeSet<ArtifactDigest>) {
    state
        .evaluations
        .retain(|_, receipt| !affected.contains(&receipt.artifact_digest));
    for history in state.rollback.values_mut() {
        history.retain(|digest| !affected.contains(digest));
    }
    state.rollback.retain(|_, history| !history.is_empty());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attention::types::{
        Admission, Compatibility, Probability, RawJudgment, Scores, content_hash,
    };
    use serenity::model::id::{ChannelId, MessageId, UserId};
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    fn store_path(directory: &tempfile::TempDir) -> Utf8PathBuf {
        #[cfg(unix)]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("fixture store directory is owner-private regardless of runner umask");
        Utf8PathBuf::from_path_buf(directory.path().join("attention.json"))
            .expect("temporary paths are UTF-8")
    }

    fn source(id: u64, conversation: &str) -> SourceVersion {
        SourceVersion {
            key: SourceKey {
                channel_id: ChannelId::new(7),
                message_id: MessageId::new(id),
            },
            author_id: UserId::new(8),
            author_kind: crate::attention::types::SourceAuthorKind::DirectHuman,
            conversation: conversation.to_owned(),
            content_hash: content_hash(&format!("message-{id}")),
            observed_at_ms: id,
        }
    }

    fn probability(value: f64) -> Probability {
        value.try_into().unwrap()
    }

    fn record(id: u64, wanted: f64) -> DecisionRecord {
        DecisionRecord {
            id: format!("r{id}").into(),
            recipient: "recipient".into(),
            sources: vec![source(id, &format!("c{id}"))],
            compatibility: Compatibility {
                model: "jev-1.13.0".into(),
                rubric: "r".into(),
                features: "f".into(),
                brief_version: "b".into(),
            },
            config_generation: 1,
            incarnation: "i".into(),
            created_at_ms: 100,
            expires_at_ms: 1_000,
            judgment: Some(RawJudgment {
                model: "jev-1.13.0".into(),
                scores: Scores {
                    wanted: probability(wanted),
                    prompt: probability(0.5),
                    participation: probability(0.5),
                    change: probability(0.5),
                },
                context_sufficient: probability(1.0),
                telemetry: None,
            }),
            hypothetical: if wanted >= 0.5 {
                Admission::Prompt
            } else {
                Admission::RetrievalOnly
            },
            policy_digest: Some("fixture-fixed-v1".into()),
            actual: Admission::Ordinary,
            delivery: DeliveryState::Observed,
            selection_probability: None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn persisted_store_artifacts_are_owner_private() {
        let directory = tempfile::tempdir().unwrap();
        let parent = Utf8PathBuf::from_path_buf(directory.path().join("attention")).unwrap();
        let path = parent.join("records.json");
        let mut store = AttentionStore::open(path.clone()).unwrap();
        store.insert_record(record(1, 0.5)).unwrap();
        let lock = parent.join(".records.json.lock");

        assert_eq!(
            fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&lock).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn store_refuses_directory_metadata_lock_and_temporary_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let target_directory = directory.path().join("unrelated");
        fs::create_dir(&target_directory).unwrap();
        fs::set_permissions(&target_directory, fs::Permissions::from_mode(0o755)).unwrap();
        let alias = directory.path().join("alias");
        symlink(&target_directory, &alias).unwrap();
        let aliased_path = Utf8PathBuf::from_path_buf(alias.join("records.json")).unwrap();
        assert!(matches!(
            AttentionStore::open(aliased_path),
            Err(StoreError::Io { .. })
        ));
        assert_eq!(
            fs::metadata(&target_directory)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755,
            "rejecting an alias must not chmod an unrelated target"
        );
        assert!(!target_directory.join(".records.json.lock").exists());

        let private = directory.path().join("attention");
        fs::create_dir(&private).unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        let path = Utf8PathBuf::from_path_buf(private.join("records.json")).unwrap();
        let victim = directory.path().join("victim");
        fs::write(&victim, b"unchanged").unwrap();

        symlink(&victim, path.as_std_path()).unwrap();
        assert!(matches!(
            AttentionStore::open(path.clone()),
            Err(StoreError::Io { .. })
        ));
        assert_eq!(fs::read(&victim).unwrap(), b"unchanged");
        fs::remove_file(&path).unwrap();

        let lock = private.join(".records.json.lock");
        symlink(&victim, &lock).unwrap();
        assert!(matches!(
            AttentionStore::open(path.clone()),
            Err(StoreError::Io { .. })
        ));
        assert_eq!(fs::read(&victim).unwrap(), b"unchanged");
        fs::remove_file(&lock).unwrap();

        let mut store = AttentionStore::open(path).unwrap();
        let temporary = private.join(".records.json.tmp");
        symlink(&victim, &temporary).unwrap();
        assert!(matches!(
            store.insert_record(record(1, 0.5)),
            Err(StoreError::Io { .. })
        ));
        assert!(store.record(&"r1".into()).is_none());
        assert_eq!(fs::read(&victim).unwrap(), b"unchanged");
    }

    #[test]
    fn post_rename_sync_failure_keeps_memory_at_committed_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = store_path(&directory);
        let mut store = AttentionStore::open(path.clone()).unwrap();
        let first = record(1, 0.5);
        let first_for_mutation = first.clone();
        let result: Result<(), StoreError> = store.update_with_persistence(
            move |state, _| {
                for source in &first_for_mutation.sources {
                    upsert_validation(
                        &mut state.validations,
                        ValidationFact {
                            source: source.clone(),
                            status: ValidationStatus::Known,
                            checked_at_ms: first_for_mutation.created_at_ms,
                        },
                    );
                }
                state
                    .records
                    .insert(first_for_mutation.id.clone(), first_for_mutation);
                Ok(())
            },
            |path, state| {
                persist_state_with_parent_sync(path, state, |_| {
                    Err(std::io::Error::other("injected parent fsync failure"))
                })
            },
        );
        assert!(matches!(result, Err(StoreError::Io { .. })));
        assert_eq!(store.record(&first.id), Some(&first));
        assert!(read_state(&path).unwrap().records.contains_key(&first.id));

        store.insert_record(record(2, 0.7)).unwrap();
        drop(store);
        let reopened = AttentionStore::open(path).unwrap();
        assert!(reopened.record(&"r1".into()).is_some());
        assert!(reopened.record(&"r2".into()).is_some());
    }

    #[test]
    fn corrupt_store_is_an_explicit_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = store_path(&directory);
        fs::write(&path, b"{broken").unwrap();
        assert!(matches!(
            AttentionStore::open(path),
            Err(StoreError::Corrupt { .. })
        ));
    }
    #[test]
    fn logically_inconsistent_feedback_is_an_explicit_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = store_path(&directory);
        let mut store = AttentionStore::open(path.clone()).unwrap();
        let item = record(1, 0.5);
        let sources = item.sources.clone();
        store.insert_record(item).unwrap();
        store
            .label(
                &"recipient".into(),
                Feedback {
                    record_id: "r1".into(),
                    annotator: "recipient".into(),
                    label: FeedbackLabel::WantedLater,
                    assessed_at_ms: 101,
                    source_versions: sources,
                },
            )
            .unwrap();
        drop(store);

        let mut state: StoreState = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        state.feedback[0].record_id = "missing".into();
        fs::write(&path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
        assert!(matches!(
            AttentionStore::open(path),
            Err(StoreError::Invalid(
                "retained feedback does not match a valid record"
            ))
        ));
    }
    #[test]
    fn concurrent_owner_is_rejected_without_touching_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = store_path(&directory);
        let mut first = AttentionStore::open(path.clone()).unwrap();
        first.insert_record(record(1, 0.5)).unwrap();
        let before = fs::read(&path).unwrap();

        assert!(matches!(
            AttentionStore::open(path.clone()),
            Err(StoreError::Locked { .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), before);

        drop(first);
        AttentionStore::open(path).unwrap();
    }

    #[test]
    fn failed_persistence_does_not_publish_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let parent = Utf8PathBuf::from_path_buf(directory.path().join("metadata"))
            .expect("temporary paths are UTF-8");
        let path = parent.join("attention.json");
        let mut store = AttentionStore::open(path).unwrap();
        fs::create_dir(parent.join(".attention.json.tmp")).unwrap();

        assert!(matches!(
            store.insert_record(record(1, 0.5)),
            Err(StoreError::Io { .. })
        ));
        assert!(store.record(&"r1".into()).is_none());
    }

    #[test]
    fn feedback_keeps_attribution_while_unsure_and_silence_stay_unlabeled() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = AttentionStore::open(store_path(&directory)).unwrap();
        for id in 1..=3 {
            store.insert_record(record(id, 0.5)).unwrap();
        }
        let x_sources = store.record(&"r1".into()).unwrap().sources.clone();
        store
            .label(
                &"other".into(),
                Feedback {
                    record_id: "r1".into(),
                    annotator: "other".into(),
                    label: FeedbackLabel::NotNeeded,
                    assessed_at_ms: 101,
                    source_versions: x_sources.clone(),
                },
            )
            .unwrap();
        store
            .label(
                &"recipient".into(),
                Feedback {
                    record_id: "r1".into(),
                    annotator: "recipient".into(),
                    label: FeedbackLabel::WantedLater,
                    assessed_at_ms: 102,
                    source_versions: x_sources.clone(),
                },
            )
            .unwrap();
        assert_eq!(
            store.recipient_target(&"r1".into()),
            Some(RecipientTarget {
                wanted: true,
                timely: false,
            })
        );
        let x_feedback = store.feedback(&"r1".into());
        assert_eq!(x_feedback.len(), 2);
        assert_eq!(x_feedback[0].annotator.as_str(), "other");
        assert_eq!(x_feedback[0].source_versions, x_sources);
        assert_eq!(x_feedback[1].annotator.as_str(), "recipient");

        // y is unreviewed: silence is not a negative target.
        assert!(store.feedback(&"r2".into()).is_empty());
        assert_eq!(store.recipient_target(&"r2".into()), None);

        // z has an explicit recipient assessment, but unsure is still not a fitting target.
        let z_sources = store.record(&"r3".into()).unwrap().sources.clone();
        store
            .label(
                &"recipient".into(),
                Feedback {
                    record_id: "r3".into(),
                    annotator: "recipient".into(),
                    label: FeedbackLabel::Unsure,
                    assessed_at_ms: 103,
                    source_versions: z_sources,
                },
            )
            .unwrap();
        assert_eq!(store.feedback(&"r3".into()).len(), 1);
        assert_eq!(store.recipient_target(&"r3".into()), None);
    }

    #[test]
    fn review_sampling_spans_score_strata_and_records_actual_propensity() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = AttentionStore::open(store_path(&directory)).unwrap();
        for (id, wanted) in [0.1, 0.2, 0.6, 0.7, 0.9].into_iter().enumerate() {
            store.insert_record(record(id as u64 + 1, wanted)).unwrap();
        }
        let sampled = store.sample_review(&"recipient".into(), 3, 42).unwrap();
        assert_eq!(sampled.len(), 3);
        assert!(
            sampled
                .iter()
                .any(|item| item.would_be_deferred && item.wanted_score < 0.25)
        );
        for item in sampled {
            let expected = if item.wanted_score >= 0.75 { 1.0 } else { 0.5 };
            assert_eq!(item.inclusion_probability, expected);
            assert_eq!(
                store
                    .record(&item.record_id)
                    .unwrap()
                    .selection_probability
                    .unwrap()
                    .get(),
                expected
            );
        }
    }

    #[test]
    fn invalidation_removes_scores_and_attributed_labels() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = AttentionStore::open(store_path(&directory)).unwrap();
        let record = record(1, 0.8);
        let source = record.sources[0].clone();
        store.insert_record(record).unwrap();
        store
            .label(
                &"recipient".into(),
                Feedback {
                    record_id: "r1".into(),
                    annotator: "recipient".into(),
                    label: FeedbackLabel::WantedPromptly,
                    assessed_at_ms: 101,
                    source_versions: vec![source.clone()],
                },
            )
            .unwrap();

        let report = store
            .invalidate_source(source.key, Some(&source.content_hash))
            .unwrap();
        assert_eq!(report.affected_records, 1);
        let invalidated = store.record(&"r1".into()).unwrap();
        assert_eq!(invalidated.delivery, DeliveryState::Invalidated);
        assert!(invalidated.judgment.is_none());
        assert!(store.feedback(&"r1".into()).is_empty());
    }
    #[test]
    fn revalidation_batch_is_atomic_when_any_fact_is_invalid() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = AttentionStore::open(store_path(&directory)).unwrap();
        let first = record(1, 0.8);
        let first_source = first.sources[0].clone();
        let second = record(2, 0.2);
        let second_source = second.sources[0].clone();
        store.insert_record(first).unwrap();
        store.insert_record(second).unwrap();

        let failed = store.revalidate_batch(vec![
            ValidationFact {
                source: first_source.clone(),
                status: ValidationStatus::Unknown,
                checked_at_ms: 200,
            },
            ValidationFact {
                source: source(99, "missing"),
                status: ValidationStatus::Known,
                checked_at_ms: 200,
            },
        ]);
        assert!(matches!(failed, Err(StoreError::Invalid(_))));
        assert!(store.sources_are_known(std::slice::from_ref(&first_source), 200));

        let reports = store
            .revalidate_batch(vec![
                ValidationFact {
                    source: first_source.clone(),
                    status: ValidationStatus::Unknown,
                    checked_at_ms: 201,
                },
                ValidationFact {
                    source: second_source,
                    status: ValidationStatus::Known,
                    checked_at_ms: 201,
                },
            ])
            .unwrap();
        assert_eq!(reports.len(), 2);
        assert!(!store.sources_are_known(&[first_source], 201));
    }

    #[test]
    fn record_finalization_is_one_time_and_preserves_trigger_identity() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = AttentionStore::open(store_path(&directory)).unwrap();
        let now_ms: u64 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
            .try_into()
            .unwrap();
        let mut initial = record(1, 0.9);
        initial.created_at_ms = now_ms;
        initial.expires_at_ms = now_ms + 10_000;
        initial.judgment = None;
        initial.policy_digest = None;
        initial.delivery = DeliveryState::Held;
        store.insert_record(initial.clone()).unwrap();

        let mut finalized = initial;
        finalized.sources.push(source(99, "c1"));
        finalized.judgment = record(2, 0.9).judgment;
        finalized.policy_digest = Some("fixture-fixed-v1".into());
        finalized.hypothetical = Admission::Prompt;
        finalized.actual = Admission::Prompt;
        finalized.delivery = DeliveryState::Admitted;
        store.finalize_record(finalized.clone()).unwrap();
        assert_eq!(store.record(&"r1".into()).unwrap(), &finalized);

        assert!(matches!(
            store.finalize_record(finalized),
            Err(StoreError::RecordAlreadyFinalized(id)) if id.as_str() == "r1"
        ));
        assert_eq!(store.record(&"r1".into()).unwrap().sources.len(), 2);
    }
    #[test]
    fn ordinary_finalization_without_policy_digest_survives_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = store_path(&directory);
        let now_ms: u64 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
            .try_into()
            .unwrap();
        let mut store = AttentionStore::open(path.clone()).unwrap();
        let mut initial = record(1, 0.9);
        initial.created_at_ms = now_ms;
        initial.expires_at_ms = now_ms + 10_000;
        initial.judgment = None;
        initial.policy_digest = None;
        initial.hypothetical = Admission::Unknown;
        initial.actual = Admission::Unknown;
        initial.delivery = DeliveryState::Held;
        store.insert_record(initial.clone()).unwrap();

        let mut shadow = initial;
        shadow.judgment = record(2, 0.9).judgment;
        shadow.hypothetical = Admission::Prompt;
        shadow.actual = Admission::Ordinary;
        shadow.delivery = DeliveryState::Observed;
        store.finalize_record(shadow.clone()).unwrap();
        assert_eq!(store.record(&"r1".into()).unwrap(), &shadow);
        drop(store);

        let reopened = AttentionStore::open(path).unwrap();
        assert_eq!(reopened.record(&"r1".into()).unwrap(), &shadow);
    }

    #[test]
    fn learned_finalization_without_policy_digest_is_rejected_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = AttentionStore::open(store_path(&directory)).unwrap();
        let now_ms: u64 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
            .try_into()
            .unwrap();
        let mut initial = record(1, 0.9);
        initial.created_at_ms = now_ms;
        initial.expires_at_ms = now_ms + 10_000;
        initial.judgment = None;
        initial.policy_digest = None;
        initial.delivery = DeliveryState::Held;
        store.insert_record(initial.clone()).unwrap();

        let mut learned = initial.clone();
        learned.judgment = record(2, 0.9).judgment;
        learned.hypothetical = Admission::Prompt;
        learned.actual = Admission::Prompt;
        learned.delivery = DeliveryState::Admitted;
        assert!(matches!(
            store.finalize_record(learned),
            Err(StoreError::FinalizationMismatch(id)) if id.as_str() == "r1"
        ));
        assert_eq!(store.record(&"r1".into()).unwrap(), &initial);
    }
}
