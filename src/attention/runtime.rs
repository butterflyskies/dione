//! Recipient-local attention execution. Network work never owns the ingress loop.

use super::{
    admission::JudgmentWork,
    config::AttentionMode,
    learning::FixedThresholdPolicy,
    provider::{JudgmentInput, ProviderFailure, TypeSafeProvider},
    source::{SourceFailure, SourceResolver, now_ms},
    store::{AttentionStore, StoreLimits, ValidationFact, ValidationStatus},
    types::*,
};
use crate::{
    config::{attention_effect_guard, load_config, subscribe_generation},
    discord::events::{MessageTargeting, NotificationEvent},
    mcp::tools::messaging::{self, MessagingCtx},
    no_rly::consent::ConsentGate,
};
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// An inspectable replay baseline, never an implicitly promoted enforcement policy.
pub const SHADOW_BASELINE: FixedThresholdPolicy = FixedThresholdPolicy {
    wanted: 0.5,
    prompt: 0.5,
    participation: 0.5,
    change: 0.5,
};

/// Inspectable degraded-state counters and notice bookkeeping, without source excerpts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AttentionHealth {
    pub degraded: Option<String>,
    pub last_unknown: Option<String>,
    pub failures: u64,
    pub recoveries: u64,
    pub retry_after_ms: u64,
    pub last_notice_ms: Option<u64>,
    pub noticed_failures: u64,
    pub noticed_recoveries: u64,
    pub status_write_error: Option<String>,
    pub notice_failures: u64,
    pub last_notice_error: Option<String>,
}

/// Coordinates source validation, durable decisions, provider work and guarded delivery.
pub struct AttentionRuntime {
    pub resolver: SourceResolver,
    store: Mutex<Result<AttentionStore, String>>,
    health: Mutex<AttentionHealth>,
    health_dirty: AtomicBool,
    health_epoch_ms: u64,
    health_epoch: tokio::time::Instant,
    provider: Mutex<Option<(u64, Arc<TypeSafeProvider>)>>,
    #[cfg(test)]
    test_provider: Option<Arc<TypeSafeProvider>>,
    #[cfg(test)]
    test_completed_judgment: Mutex<Option<CompletedJudgmentPause>>,
    pub enforcement_supported: bool,
}

#[cfg(test)]
struct CompletedJudgmentPause {
    captured: tokio::sync::oneshot::Sender<DecisionRecord>,
    resume: tokio::sync::oneshot::Receiver<()>,
}

impl fmt::Debug for AttentionRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AttentionRuntime")
            .field("enforcement_supported", &self.enforcement_supported)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
enum EvaluationFailure {
    #[error("{0}")]
    Source(#[from] SourceFailure),
    #[error("{0}")]
    Provider(#[from] ProviderFailure),
    #[error("attention metadata persistence failed")]
    Store,
    #[error("attention evaluation exceeded its total deadline")]
    Deadline,
    #[error("attention evaluation was cancelled or became stale")]
    Stale,
}

impl AttentionRuntime {
    /// Initializes persistent attention state without blocking the async runtime.
    pub async fn new(resolver: SourceResolver, enforcement_supported: bool) -> Self {
        let config = load_config(&resolver.state_dir);
        let attention = &config.raw.attention;
        let limits = StoreLimits {
            max_records: attention.max_records,
            max_record_ttl_ms: attention.retention_ms,
            revalidation_interval_ms: attention.revalidate_ms,
            ..StoreLimits::default()
        };
        let outage_retry_ms = attention.outage_retry_ms;
        let store_path = resolver.state_dir.join("attention").join("records.json");
        let health_path = resolver.state_dir.join("attention-status.json");
        let initialized =
            tokio::task::spawn_blocking(move || {
                let store = AttentionStore::open_with_limits(store_path, limits)
                    .map_err(|error| error.to_string());
                let health =
                    match std::fs::read(health_path) {
                        Ok(bytes) => serde_json::from_slice::<AttentionHealth>(&bytes)
                            .unwrap_or_else(|_| AttentionHealth {
                                degraded: Some("attention status is corrupt".to_owned()),
                                ..AttentionHealth::default()
                            }),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            AttentionHealth::default()
                        }
                        Err(_) => AttentionHealth {
                            degraded: Some("attention status is unreadable".to_owned()),
                            ..AttentionHealth::default()
                        },
                    };
                (store, health)
            })
            .await;
        let (store, mut health) = match initialized {
            Ok(initialized) => initialized,
            Err(error) => (
                Err(format!("attention initialization worker failed: {error}")),
                AttentionHealth {
                    degraded: Some("attention metadata is unavailable".to_owned()),
                    ..AttentionHealth::default()
                },
            ),
        };
        health.retry_after_ms = health
            .retry_after_ms
            .min(now_ms().saturating_add(outage_retry_ms));
        health.last_notice_ms = health.last_notice_ms.map(|time| time.min(now_ms()));
        if store.is_err() {
            health.degraded = Some("attention metadata is unavailable".to_owned());
        }
        Self {
            resolver,
            store: Mutex::new(store),
            health: Mutex::new(health),
            health_dirty: AtomicBool::new(true),
            health_epoch_ms: now_ms(),
            health_epoch: tokio::time::Instant::now(),
            provider: Mutex::new(None),
            #[cfg(test)]
            test_provider: None,
            #[cfg(test)]
            test_completed_judgment: Mutex::new(None),
            enforcement_supported,
        }
    }

    /// Disk transactions run outside Tokio's async worker threads. Errors never
    /// replace a corrupt store with an apparently successful empty one.
    pub async fn with_store<R, F>(self: &Arc<Self>, operation: F) -> Result<R, String>
    where
        R: Send + 'static,
        F: FnOnce(&mut AttentionStore) -> Result<R, String> + Send + 'static,
    {
        let runtime = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = runtime
                .store
                .lock()
                .map_err(|_| "attention store lock failed".to_owned())?;
            let store = guard.as_mut().map_err(|error| error.clone())?;
            operation(store)
        })
        .await
        .map_err(|_| "attention store worker failed".to_owned())?
    }

    /// Persists dirty health state off the async worker and reports persistence failure.
    pub async fn persist_health(self: &Arc<Self>) -> Result<(), String> {
        if !self.health_dirty.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        let runtime = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut health = runtime
                .health
                .lock()
                .map_err(|_| "attention health lock failed".to_owned())?;
            let path = runtime.resolver.state_dir.join("attention-status.json");
            let result = (|| -> std::io::Result<()> {
                std::fs::create_dir_all(&runtime.resolver.state_dir)?;
                let mut temporary = tempfile::NamedTempFile::new_in(&runtime.resolver.state_dir)?;
                health.status_write_error = None;
                serde_json::to_writer(&mut temporary, &*health).map_err(std::io::Error::other)?;
                temporary.as_file().sync_all()?;
                temporary.persist(&path).map_err(|error| error.error)?;
                std::fs::File::open(&runtime.resolver.state_dir)?.sync_all()
            })();
            if result.is_err() {
                health.status_write_error = Some("attention status persistence failed".to_owned());
                runtime.health_dirty.store(true, Ordering::Release);
                return Err("attention status persistence failed".to_owned());
            }
            Ok(())
        })
        .await
        .map_err(|_| "attention status worker failed".to_owned())?
    }

    /// Publishes a bounded opt-in notice under the current route and publication guard.
    pub async fn publish_notice(
        self: Arc<Self>,
        no_rly: Arc<ConsentGate>,
        event_tx: Option<tokio::sync::mpsc::Sender<NotificationEvent>>,
    ) {
        // This bounded publication serializes with configuration changes. A muted
        // or removed route cannot become an asynchronous stale send.
        let publication = attention_effect_guard().await;
        let config = load_config(&self.resolver.state_dir);
        let text = {
            let Ok(mut health) = self.health.lock() else {
                return;
            };
            let before = (
                health.last_notice_ms,
                health.noticed_failures,
                health.noticed_recoveries,
            );
            let text = health.notice_text(&config.raw.attention, self.health_now());
            if before
                != (
                    health.last_notice_ms,
                    health.noticed_failures,
                    health.noticed_recoveries,
                )
            {
                self.health_dirty.store(true, Ordering::Release);
            }
            text
        };
        if let Some(text) = text {
            let outcome = match config
                .raw
                .attention
                .notice_destination(config.raw.attention_notice_route.as_ref())
            {
                Ok(Some(channel)) => {
                    let mut ctx = MessagingCtx::new(
                        self.resolver.http.clone(),
                        self.resolver.state.clone(),
                        config.clone(),
                        self.resolver.state_dir.clone(),
                        no_rly,
                        self.resolver.ledger.clone(),
                    );
                    ctx.event_tx = event_tx;
                    match tokio::time::timeout(
                        Duration::from_millis(config.raw.attention.request_timeout_ms.min(5_000)),
                        messaging::reply(&ctx, channel, &text, None, true),
                    )
                    .await
                    {
                        Ok(result) if result["ok"].as_bool() == Some(true) => Ok(()),
                        _ => Err("attention notice delivery failed"),
                    }
                }
                Ok(None) => Err("attention notice route is not configured"),
                Err(error) => Err(error),
            };
            if let Ok(mut health) = self.health.lock() {
                match outcome {
                    Ok(()) => health.last_notice_error = None,
                    Err(error) => {
                        health.notice_failures = health.notice_failures.saturating_add(1);
                        health.last_notice_error = Some(error.to_owned());
                    }
                }
                self.health_dirty.store(true, Ordering::Release);
            }
        }
        drop(publication);
        let _ = self.persist_health().await;
    }

    /// Returns health, visibly degraded if its synchronization state is unavailable.
    pub fn health(&self) -> AttentionHealth {
        self.health
            .lock()
            .map(|health| health.clone())
            .unwrap_or_else(|_| AttentionHealth {
                degraded: Some("attention health lock failed".to_owned()),
                ..AttentionHealth::default()
            })
    }

    /// Records an unknown outcome without treating absent evidence as a negative judgment.
    pub fn unknown(&self, reason: impl Into<String>) {
        let reason = reason.into();
        if let Ok(mut health) = self.health.lock()
            && health.last_unknown.as_deref() != Some(reason.as_str())
        {
            health.last_unknown = Some(reason);
            self.health_dirty.store(true, Ordering::Release);
        }
    }

    fn health_now(&self) -> u64 {
        self.health_epoch_ms.saturating_add(
            self.health_epoch
                .elapsed()
                .as_millis()
                .min(u64::MAX as u128) as u64,
        )
    }

    /// Reports whether the bounded provider recovery backoff has elapsed.
    pub fn probe_due(&self) -> bool {
        self.health().retry_after_ms <= self.health_now()
    }

    fn failed(&self, reason: String) {
        let config = load_config(&self.resolver.state_dir);
        if let Ok(mut health) = self.health.lock() {
            health.degraded = Some(reason);
            health.failures = health.failures.saturating_add(1);
            health.retry_after_ms = self
                .health_now()
                .saturating_add(config.raw.attention.outage_retry_ms);
            self.health_dirty.store(true, Ordering::Release);
        }
    }

    // Healthy scoring must continue while an explicitly selected calibration is unavailable.
    fn policy_unavailable(&self, reason: String) {
        if let Ok(mut health) = self.health.lock() {
            let changed = health.degraded.as_ref() != Some(&reason);
            if changed || health.retry_after_ms != 0 {
                if changed {
                    health.failures = health.failures.saturating_add(1);
                }
                health.degraded = Some(reason);
                health.retry_after_ms = 0;
                self.health_dirty.store(true, Ordering::Release);
            }
        }
    }

    fn recovered(&self) {
        if let Ok(mut health) = self.health.lock() {
            if health.degraded.take().is_some() {
                health.recoveries = health.recoveries.saturating_add(1);
                self.health_dirty.store(true, Ordering::Release);
            }
            if health.retry_after_ms != 0 {
                health.retry_after_ms = 0;
                self.health_dirty.store(true, Ordering::Release);
            }
        }
    }

    fn provider(
        &self,
        config: &crate::config::LoadedConfig,
    ) -> Result<Arc<TypeSafeProvider>, ProviderFailure> {
        #[cfg(test)]
        if let Some(provider) = &self.test_provider {
            return Ok(provider.clone());
        }
        let mut cached = self
            .provider
            .lock()
            .map_err(|_| ProviderFailure::InvalidConfiguration)?;
        if let Some((generation, provider)) = cached.as_ref()
            && *generation == config.generation()
        {
            return Ok(provider.clone());
        }
        let settings = &config.raw.attention;
        let key = std::env::var(&settings.api_key_env)
            .map_err(|_| ProviderFailure::InvalidConfiguration)?;
        let provider = Arc::new(TypeSafeProvider::new(
            key,
            Duration::from_millis(settings.request_timeout_ms),
        )?);
        *cached = Some((config.generation(), provider.clone()));
        Ok(provider)
    }

    #[cfg(test)]
    pub(crate) fn with_test_provider(mut self, provider: TypeSafeProvider) -> Self {
        self.test_provider = Some(Arc::new(provider));
        self
    }

    /// Pauses one successfully parsed HTTP judgment before its real publication fence.
    #[cfg(test)]
    pub(crate) fn pause_completed_judgment(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<DecisionRecord>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (captured, completed) = tokio::sync::oneshot::channel();
        let (resume, released) = tokio::sync::oneshot::channel();
        let previous =
            self.test_completed_judgment
                .lock()
                .unwrap()
                .replace(CompletedJudgmentPause {
                    captured,
                    resume: released,
                });
        assert!(previous.is_none(), "only one completion pause may be armed");
        (completed, resume)
    }

    /// This predicate is also called by the HTTP client before each retry. Source
    /// versions and room export grants are authority; the model's judgment is not.
    fn export_current(&self, record: &DecisionRecord) -> bool {
        let config = load_config(&self.resolver.state_dir);
        let attention = &config.raw.attention;
        record.config_generation == config.generation()
            && record.compatibility == attention.compatibility()
            && attention
                .brief
                .as_ref()
                .is_some_and(|brief| brief.usable(now_ms()))
            && record.sources.iter().all(|source| {
                attention.effective_mode(source.key.channel_id) != AttentionMode::Off
                    && attention.provider_eligible(source.key.channel_id)
                    && self
                        .resolver
                        .ledger
                        .delivery_check(
                            source.key.message_id,
                            source.key.channel_id,
                            &source.content_hash,
                            &config,
                            MessageTargeting::Ambient,
                        )
                        .is_ok()
            })
    }

    /// Evaluates authorized work, rejecting stale configuration and preserving ordinary fallback.
    pub async fn evaluate(self: Arc<Self>, work: JudgmentWork) -> DecisionRecord {
        let mut record = work.record;
        let mut changes = subscribe_generation();
        if work.cancel.is_cancelled() || !self.export_current(&record) {
            record.actual = Admission::Ordinary;
            return record;
        }
        let initial = record.clone();
        if self
            .with_store(move |store| {
                store
                    .insert_record(initial)
                    .map_err(|error| error.to_string())
            })
            .await
            .is_err()
        {
            self.failed(EvaluationFailure::Store.to_string());
            record.actual = Admission::Ordinary;
            return record;
        }
        let timeout = Duration::from_millis(
            load_config(&self.resolver.state_dir)
                .raw
                .attention
                .request_timeout_ms,
        );
        let result = tokio::select! {
            biased;
            _ = work.cancel.cancelled() => Err(EvaluationFailure::Stale),
            _ = changes.changed() => Err(EvaluationFailure::Stale),
            result = tokio::time::timeout(timeout, self.evaluate_inner(&mut record, &work.cancel)) => {
                result.unwrap_or(Err(EvaluationFailure::Deadline))
            }
        };
        match result {
            Ok(()) => {
                #[cfg(test)]
                {
                    let pause = self.test_completed_judgment.lock().unwrap().take();
                    if let Some(pause) = pause {
                        pause.captured.send(record.clone()).unwrap();
                        pause.resume.await.unwrap();
                    }
                }
                let publication = attention_effect_guard().await;
                let runtime = self.clone();
                let cancelled = work.cancel.clone();
                let mode = work.mode;
                let mut finalized = record.clone();
                match self
                    .with_store(move |store| {
                        // Move the publication guard into the blocking transaction: cancelling
                        // its async waiter must not release authority while fsync is running.
                        let _publication = publication;
                        if cancelled.is_cancelled() || !runtime.export_current(&finalized) {
                            finalized.actual = Admission::Ordinary;
                            finalized.judgment = None;
                            return Ok((finalized, None));
                        }
                        finalized.actual = Admission::Ordinary;
                        let mut policy_failure = None;
                        let current = load_config(&runtime.resolver.state_dir);
                        if let Some(selected) = store.selected_artifact(&finalized.recipient) {
                            if selected.compatibility != finalized.compatibility {
                                policy_failure = Some(
                                    super::learning::LearningError::IncompatibleArtifact
                                        .to_string(),
                                );
                            } else {
                                let membership_current = store
                                    .active_artifact(&finalized.recipient)
                                    .is_some_and(|artifact| {
                                        artifact
                                            .members
                                            .iter()
                                            .chain(&artifact.promotion_sources)
                                            .all(|source| {
                                                runtime
                                                    .resolver
                                                    .ledger
                                                    .delivery_check(
                                                        source.key.message_id,
                                                        source.key.channel_id,
                                                        &source.content_hash,
                                                        &current,
                                                        MessageTargeting::Ambient,
                                                    )
                                                    .is_ok()
                                            })
                                    });
                                if !membership_current {
                                    policy_failure = Some(
                                        super::learning::LearningError::InvalidMembership
                                            .to_string(),
                                    );
                                } else if finalized.hypothetical != Admission::Unknown {
                                    let scores = &finalized
                                        .judgment
                                        .as_ref()
                                        .ok_or_else(|| {
                                            "evaluated record has no judgment".to_owned()
                                        })?
                                        .scores;
                                    match store.predict_active(
                                        &finalized.recipient,
                                        &finalized.compatibility,
                                        scores,
                                        now_ms(),
                                    ) {
                                        Ok(Some(admission)) => {
                                            finalized.hypothetical = admission;
                                            if mode == AttentionMode::On
                                                && runtime.enforcement_supported
                                            {
                                                finalized.actual = admission;
                                            }
                                            finalized.policy_digest = Some(selected.digest.clone());
                                        }
                                        Ok(None) => {}
                                        Err(error) => policy_failure = Some(error.to_string()),
                                    }
                                }
                            }
                        } else {
                            runtime.unknown("no current explicitly selected policy membership");
                        }
                        finalized.delivery = match finalized.actual {
                            Admission::Prompt | Admission::NextTurn => DeliveryState::Admitted,
                            Admission::RetrievalOnly => DeliveryState::Deferred,
                            _ => DeliveryState::Observed,
                        };
                        store
                            .finalize_record(finalized.clone())
                            .map_err(|error| error.to_string())?;
                        Ok((finalized, policy_failure))
                    })
                    .await
                {
                    Ok((finalized, policy_failure)) => {
                        record = finalized;
                        if let Some(reason) = policy_failure {
                            self.policy_unavailable(reason);
                        } else if record.judgment.is_some() {
                            self.recovered();
                        }
                    }
                    Err(_) => {
                        self.failed(EvaluationFailure::Store.to_string());
                        record.actual = Admission::Ordinary;
                    }
                }
            }
            Err(
                EvaluationFailure::Stale | EvaluationFailure::Provider(ProviderFailure::Cancelled),
            ) => {
                record.actual = Admission::Ordinary;
                record.judgment = None;
            }
            Err(EvaluationFailure::Source(error)) => {
                self.unknown(error.to_string());
                record.actual = Admission::Ordinary;
                if error != SourceFailure::Unknown {
                    record.delivery = DeliveryState::Invalidated;
                    let fact = ValidationFact {
                        source: record.sources[0].clone(),
                        status: ValidationStatus::Invalid,
                        checked_at_ms: now_ms(),
                    };
                    if self
                        .with_store(move |store| {
                            store
                                .revalidate_batch(vec![fact])
                                .map(|_| ())
                                .map_err(|error| error.to_string())
                        })
                        .await
                        .is_err()
                    {
                        self.failed(EvaluationFailure::Store.to_string());
                    }
                }
            }
            Err(error) => {
                self.failed(error.to_string());
                record.actual = Admission::Ordinary;
            }
        }
        let _ = self.persist_health().await;
        record
    }

    async fn evaluate_inner(
        &self,
        record: &mut DecisionRecord,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(), EvaluationFailure> {
        let config = load_config(&self.resolver.state_dir);
        let attention = &config.raw.attention;
        let (trigger, antecedents, mut missing_context) = self
            .resolver
            .segment(&record.sources[0], attention.max_antecedents)
            .await?;
        let total_bytes = trigger.text.len().saturating_add(
            antecedents
                .iter()
                .map(|source| source.text.len())
                .sum::<usize>(),
        );
        if total_bytes > attention.max_segment_bytes {
            return Err(SourceFailure::Unknown.into());
        }
        // Arbitrary links are evidence handles, not permission to fetch their targets.
        missing_context |= std::iter::once(&trigger)
            .chain(&antecedents)
            .any(|excerpt| excerpt.text.contains("http://") || excerpt.text.contains("https://"));
        record
            .sources
            .extend(antecedents.iter().map(|excerpt| excerpt.source.clone()));
        let brief = attention
            .brief
            .as_ref()
            .filter(|brief| brief.usable(now_ms()))
            .ok_or(EvaluationFailure::Stale)?;
        let input = JudgmentInput {
            model: attention.model.clone(),
            brief: brief.text.clone(),
            trigger,
            antecedents,
            missing_context,
        };
        let provider = self.provider(&config)?;
        let judgment = provider
            .judge_guarded(&input, cancel, || self.export_current(record))
            .await?;
        record.hypothetical = if missing_context || judgment.context_sufficient.get() <= 0.5 {
            self.unknown("insufficient authorized conversation evidence");
            Admission::Unknown
        } else {
            SHADOW_BASELINE
                .decide(&judgment.scores)
                .map_err(|_| EvaluationFailure::Stale)?
        };
        record.judgment = Some(judgment);
        Ok(())
    }

    /// Revalidation is an observed fact, never inferred from a durable row.
    pub async fn validate_record(
        self: &Arc<Self>,
        record: &DecisionRecord,
    ) -> Result<(), SourceFailure> {
        if record.expires_at_ms <= now_ms() {
            return Err(SourceFailure::Invalidated);
        }
        self.validate_sources(&record.sources).await
    }

    /// Revalidates exact sources within one shared deadline and persists the observed facts.
    pub async fn validate_sources(
        self: &Arc<Self>,
        sources: &[SourceVersion],
    ) -> Result<(), SourceFailure> {
        let mut unique = std::collections::BTreeMap::new();
        for source in sources {
            unique
                .entry((source.key, source.content_hash.as_str()))
                .or_insert(source);
        }
        if unique.len() > 8_192 {
            return Err(SourceFailure::Unknown);
        }
        let config = load_config(&self.resolver.state_dir);
        let deadline =
            tokio::time::Instant::now() + Duration::from_millis(config.raw.attention.revalidate_ms);
        let mut facts = Vec::with_capacity(unique.len());
        let mut failure = None;
        for source in unique.into_values() {
            let result = tokio::time::timeout_at(deadline, self.resolver.fetch(source, false))
                .await
                .unwrap_or(Err(SourceFailure::Unknown));
            let status = match &result {
                Ok(_) => ValidationStatus::Known,
                Err(SourceFailure::Unknown) => ValidationStatus::Unknown,
                Err(_) => ValidationStatus::Invalid,
            };
            if let Err(error) = result {
                failure = Some(error);
            }
            facts.push(ValidationFact {
                source: source.clone(),
                status,
                checked_at_ms: now_ms(),
            });
        }
        self.with_store(move |store| {
            store
                .revalidate_batch(facts)
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
        .await
        .map_err(|_| SourceFailure::Unknown)?;
        failure.map_or(Ok(()), Err)
    }

    /// Prunes evidence, applies storage bounds, and revalidates the selected policy.
    pub async fn maintain(self: Arc<Self>) {
        let config = load_config(&self.resolver.state_dir);
        let recipient = config.raw.attention.recipient.clone();
        let limits = StoreLimits {
            max_records: config.raw.attention.max_records,
            max_record_ttl_ms: config.raw.attention.retention_ms,
            revalidation_interval_ms: config.raw.attention.revalidate_ms,
            ..StoreLimits::default()
        };
        let sources = self
            .with_store(move |store| {
                store.prune(now_ms()).map_err(|error| error.to_string())?;
                store
                    .update_limits(limits)
                    .map_err(|error| error.to_string())?;
                Ok(store
                    .selected_artifact(&recipient)
                    .map(|artifact| {
                        artifact
                            .members
                            .iter()
                            .chain(&artifact.promotion_sources)
                            .cloned()
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default())
            })
            .await;
        match sources {
            Ok(sources) if !sources.is_empty() => {
                if let Err(error) = self.validate_sources(&sources).await {
                    self.unknown(error.to_string());
                }
            }
            Ok(_) => {}
            Err(_) => self.failed(EvaluationFailure::Store.to_string()),
        }
    }

    /// Recovers eligible held work from earlier lifetimes without replaying uncertain receipts.
    pub async fn recover_pending(
        self: Arc<Self>,
        live_incarnation: IncarnationId,
    ) -> Vec<super::admission::AdmissionResult> {
        let config = load_config(&self.resolver.state_dir);
        let recipient = config.raw.attention.recipient.clone();
        let live_for_selection = live_incarnation.clone();
        let rows = self
            .with_store(move |store| {
                let mut rows: Vec<_> = store
                    .list_records(&recipient)
                    .into_iter()
                    .filter(|record| {
                        matches!(
                            record.delivery,
                            DeliveryState::Admitted | DeliveryState::ReceiptUncertain
                        ) || (record.delivery == DeliveryState::Held
                            && record.incarnation != live_for_selection)
                    })
                    .cloned()
                    .collect();
                rows.sort_by(|left, right| {
                    (left.created_at_ms, &left.id).cmp(&(right.created_at_ms, &right.id))
                });
                Ok(rows)
            })
            .await;
        let Ok(mut rows) = rows else {
            self.failed(EvaluationFailure::Store.to_string());
            return Vec::new();
        };
        if rows.is_empty() {
            return Vec::new();
        }
        // A bounded rotating cycle also refreshes sources still held by a busy consumer.
        let start = ((now_ms() / config.raw.attention.revalidate_ms).wrapping_mul(64) as usize)
            % rows.len();
        rows.rotate_left(start);
        rows.truncate(64);
        rows.sort_by(|left, right| {
            (left.created_at_ms, &left.id).cmp(&(right.created_at_ms, &right.id))
        });
        let deadline =
            tokio::time::Instant::now() + Duration::from_millis(config.raw.attention.revalidate_ms);
        let mut recovered = Vec::new();
        for mut record in rows {
            if !matches!(
                tokio::time::timeout_at(deadline, self.validate_record(&record)).await,
                Ok(Ok(()))
            ) {
                self.unknown("pending source is unavailable during recovery");
                continue;
            }
            if record.incarnation == live_incarnation {
                continue;
            }
            let Some(source) = record.sources.first() else {
                continue;
            };
            let Ok(Ok(event)) =
                tokio::time::timeout_at(deadline, self.resolver.recover_event(source)).await
            else {
                continue;
            };
            if record.delivery == DeliveryState::Held {
                record.actual = Admission::Ordinary;
            } else {
                // Admission alone is not a receipt. The durable adapter keeps its
                // stable logical message ID; no exactly-once claim is made here.
                record.delivery = DeliveryState::ReceiptUncertain;
                if self
                    .mark_delivery(record.id.clone(), DeliveryState::ReceiptUncertain)
                    .await
                    .is_err()
                {
                    continue;
                }
            }
            recovered.push(super::admission::AdmissionResult { event, record });
        }
        recovered
    }

    /// Returns a recipient-owned record and excerpts after current exact-source revalidation.
    pub async fn retrieve(self: &Arc<Self>, id: RecordId) -> Result<serde_json::Value, String> {
        let recipient = load_config(&self.resolver.state_dir)
            .raw
            .attention
            .recipient
            .clone();
        let record = self
            .with_store(move |store| {
                store
                    .record(&id)
                    .filter(|record| record.recipient == recipient)
                    .cloned()
                    .ok_or_else(|| "attention source unavailable".to_owned())
            })
            .await?;
        self.validate_record(&record)
            .await
            .map_err(|_| "attention source unavailable".to_owned())?;
        let mut excerpts = Vec::with_capacity(record.sources.len());
        for source in &record.sources {
            excerpts.push(
                self.resolver
                    .fetch(source, false)
                    .await
                    .map_err(|_| "attention source unavailable".to_owned())?,
            );
        }
        let current = load_config(&self.resolver.state_dir);
        if current.raw.attention.recipient != record.recipient
            || !record.sources.iter().all(|source| {
                self.resolver
                    .ledger
                    .delivery_check(
                        source.key.message_id,
                        source.key.channel_id,
                        &source.content_hash,
                        &current,
                        MessageTargeting::Ambient,
                    )
                    .is_ok()
            })
        {
            return Err("attention source unavailable".to_owned());
        }
        Ok(serde_json::json!({"record": record, "sources": excerpts}))
    }

    /// Invalidates retained evidence and dependent artifacts for a Discord source.
    pub async fn invalidate(self: &Arc<Self>, source: SourceKey) -> Result<(), String> {
        self.with_store(move |store| {
            store
                .invalidate_source(source, None)
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
        .await
    }

    /// Persists delivery state off the async worker before callers acknowledge durability.
    pub async fn mark_delivery(
        self: &Arc<Self>,
        id: RecordId,
        state: DeliveryState,
    ) -> Result<(), String> {
        self.with_store(move |store| {
            store
                .update_delivery(&id, state)
                .map_err(|error| error.to_string())
        })
        .await
    }
}

impl crate::codex::AttentionDeliveryGuard for AttentionRuntime {
    fn check(
        &self,
        notification: &serde_json::Value,
    ) -> Result<(), crate::codex::AttentionGuardFailure> {
        use crate::{
            codex::AttentionGuardFailure as Failure, ingress_ledger::SourceDeliveryFailure,
        };
        let id = notification
            .pointer("/params/meta/attention_record")
            .and_then(serde_json::Value::as_str)
            .ok_or(Failure::Invalidated)?;
        // A concurrent durable write must defer this synchronous guard, not block Tokio on fsync.
        let guard = self.store.try_lock().map_err(|_| Failure::Unavailable)?;
        let store = guard.as_ref().map_err(|_| Failure::Unavailable)?;
        let record = store.state.records.get(id).ok_or(Failure::Invalidated)?;
        let config = load_config(&self.resolver.state_dir);
        if record.recipient != config.raw.attention.recipient
            || record.expires_at_ms <= now_ms()
            || !matches!(
                record.delivery,
                DeliveryState::Admitted | DeliveryState::ReceiptUncertain
            )
        {
            return Err(Failure::Invalidated);
        }
        for source in &record.sources {
            self.resolver
                .ledger
                .delivery_check(
                    source.key.message_id,
                    source.key.channel_id,
                    &source.content_hash,
                    &config,
                    MessageTargeting::Ambient,
                )
                .map_err(|error| match error {
                    SourceDeliveryFailure::Unavailable => Failure::Unavailable,
                    SourceDeliveryFailure::Invalidated => Failure::Invalidated,
                })?;
        }
        if !store.sources_current(
            &record.sources,
            now_ms(),
            config.raw.attention.revalidate_ms,
        ) {
            return Err(Failure::Unavailable);
        }
        let trigger = record.sources.first().ok_or(Failure::Invalidated)?;
        let content = notification
            .pointer("/params/content")
            .and_then(serde_json::Value::as_str)
            .ok_or(Failure::Invalidated)?;
        let meta = notification
            .pointer("/params/meta")
            .ok_or(Failure::Invalidated)?;
        if !trigger.matches_text(content)
            || meta
                .get("chat_id")
                .and_then(serde_json::Value::as_str)
                .and_then(|id| id.parse::<u64>().ok())
                != Some(trigger.key.channel_id.get())
            || meta
                .get("message_id")
                .and_then(serde_json::Value::as_str)
                .and_then(|id| id.parse::<u64>().ok())
                != Some(trigger.key.message_id.get())
            || meta
                .get("user_id")
                .and_then(serde_json::Value::as_str)
                .and_then(|id| id.parse::<u64>().ok())
                != Some(trigger.author_id.get())
        {
            return Err(Failure::Invalidated);
        }
        // Already-admitted work survives mode changes, but not access or source loss.
        Ok(())
    }

    fn receipt<'a>(
        self: Arc<Self>,
        notification: &'a serde_json::Value,
        outcome: crate::codex::AttentionDeliveryReceipt,
    ) -> futures_util::future::BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let id = notification
                .pointer("/params/meta/attention_record")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "managed delivery lacks a decision record".to_owned())?
                .into();
            let state = match outcome {
                crate::codex::AttentionDeliveryReceipt::Accepted => DeliveryState::Dispatched,
                crate::codex::AttentionDeliveryReceipt::Uncertain => {
                    DeliveryState::ReceiptUncertain
                }
            };
            self.mark_delivery(id, state).await
        })
    }
}

impl AttentionHealth {
    fn notice_text(&mut self, config: &super::config::AttentionConfig, now: u64) -> Option<String> {
        use super::config::NoticeMode;
        if config.notices == NoticeMode::Off {
            self.noticed_failures = self.failures;
            self.noticed_recoveries = self.recoveries;
            return None;
        }
        if self
            .last_notice_ms
            .is_some_and(|last| now.saturating_sub(last) < config.notice_cooldown_ms)
        {
            return None;
        }
        let text = if self.degraded.is_some() && self.failures > self.noticed_failures {
            Some(format!(
                "Attention is degraded: {}. Eligible messages are using ordinary delivery.",
                self.degraded.as_deref().unwrap_or("evaluation unavailable")
            ))
        } else if self.degraded.is_none() && self.recoveries > self.noticed_recoveries {
            if config.notices == NoticeMode::FailuresAndRecovery {
                Some("Attention evaluation recovered. Existing modes and room settings are unchanged.".to_owned())
            } else {
                self.noticed_recoveries = self.recoveries;
                None
            }
        } else {
            None
        };
        if text.is_some() {
            self.last_notice_ms = Some(now);
            self.noticed_failures = self.failures;
            self.noticed_recoveries = self.recoveries;
        }
        text
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        attention::{
            admission::{AdmissionController, Submission},
            config::{AttentionBrief, AttentionConfig, NoticeMode, RoomAttention},
            provider::TypeSafeProvider,
            source::now_ms,
        },
        config::ConfigRuntime,
        discord::events::NotificationEvent,
    };
    use camino::Utf8PathBuf;
    use parking_lot::Mutex as SyncMutex;
    use serde_json::{Value, json};
    use serenity::model::id::{ChannelId, MessageId, UserId};
    use std::{collections::BTreeMap, future::Future};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

    fn scenario<F: Future<Output = ()>>(run: impl FnOnce() -> F) {
        let _configuration = crate::config::config_cache_guard();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run());
    }

    struct NoticeNetworkState {
        messages: BTreeMap<(u64, u64), Value>,
        requests: Vec<(String, String, Value)>,
        delivered_notices: Vec<String>,
        provider_malformed: bool,
        notice_status: u16,
    }

    struct NoticeNetwork {
        address: std::net::SocketAddr,
        state: Arc<SyncMutex<NoticeNetworkState>>,
        listener: tokio::task::JoinHandle<()>,
    }

    impl Drop for NoticeNetwork {
        fn drop(&mut self) {
            self.listener.abort();
        }
    }

    impl NoticeNetwork {
        async fn start(notice_status: u16) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let state = Arc::new(SyncMutex::new(NoticeNetworkState {
                messages: BTreeMap::new(),
                requests: Vec::new(),
                delivered_notices: Vec::new(),
                provider_malformed: false,
                notice_status,
            }));
            let shared = state.clone();
            let listener = tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    tokio::spawn(serve_notice_request(stream, shared.clone()));
                }
            });
            Self {
                address,
                state,
                listener,
            }
        }

        fn http(&self) -> Arc<serenity::http::Http> {
            Arc::new(
                serenity::http::HttpBuilder::new("fixture-discord")
                    .proxy(format!("http://{}", self.address))
                    .ratelimiter_disabled(true)
                    .build(),
            )
        }

        fn provider(&self) -> TypeSafeProvider {
            TypeSafeProvider::with_test_endpoint(
                "fixture-key".into(),
                Duration::from_secs(3),
                reqwest::Url::parse(&format!("http://{}/v1/systemone", self.address)).unwrap(),
            )
            .unwrap()
        }

        fn insert(&self, channel: u64, id: u64, text: &str) {
            self.state
                .lock()
                .messages
                .insert((channel, id), wire_message(channel, id, text));
        }

        fn set_provider_malformed(&self, malformed: bool) {
            self.state.lock().provider_malformed = malformed;
        }

        fn provider_calls(&self) -> usize {
            self.state
                .lock()
                .requests
                .iter()
                .filter(|(_, path, _)| path.ends_with("/systemone"))
                .count()
        }

        fn notice_attempts(&self) -> usize {
            self.state
                .lock()
                .requests
                .iter()
                .filter(|(method, path, _)| {
                    method == "POST" && path.contains("/channels/100/messages")
                })
                .count()
        }

        fn delivered_notices(&self) -> Vec<String> {
            self.state.lock().delivered_notices.clone()
        }
    }

    fn wire_message(channel: u64, id: u64, text: &str) -> Value {
        json!({
            "id": id.to_string(), "channel_id": channel.to_string(), "guild_id": "500",
            "author": {"id": "7", "username": "fixture", "discriminator": "0", "avatar": null, "bot": false},
            "content": text, "timestamp": "2026-09-21T10:00:00.000Z", "edited_timestamp": null,
            "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
            "attachments": [], "embeds": [], "pinned": false, "type": 0, "flags": 0
        })
    }

    async fn serve_notice_request(
        mut stream: TcpStream,
        state: Arc<SyncMutex<NoticeNetworkState>>,
    ) {
        let mut bytes = Vec::new();
        let header_end = loop {
            let mut chunk = [0; 4096];
            let Ok(read) = stream.read(&mut chunk).await else {
                return;
            };
            if read == 0 {
                return;
            }
            bytes.extend_from_slice(&chunk[..read]);
            if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break end + 4;
            }
            if bytes.len() > 16_384 {
                return;
            }
        };
        let header = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
        let length = header
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while bytes.len() < header_end + length {
            let mut chunk = [0; 4096];
            let Ok(read) = stream.read(&mut chunk).await else {
                return;
            };
            if read == 0 {
                return;
            }
            bytes.extend_from_slice(&chunk[..read]);
        }
        let mut request_line = header.lines().next().unwrap().split_whitespace();
        let method = request_line.next().unwrap().to_owned();
        let path = request_line.next().unwrap().to_owned();
        let body = serde_json::from_slice::<Value>(&bytes[header_end..header_end + length])
            .unwrap_or(Value::Null);
        let (status, response) = {
            let mut state = state.lock();
            state
                .requests
                .push((method.clone(), path.clone(), body.clone()));
            if path.ends_with("/systemone") {
                if state.provider_malformed {
                    (200, "not JSON".to_owned())
                } else {
                    let mut answers = serde_json::Map::new();
                    for (name, score) in [
                        ("wanted", 0.95),
                        ("prompt", 0.05),
                        ("participation", 0.05),
                        ("change", 0.05),
                        ("context_sufficient", 0.95),
                    ] {
                        answers.insert(name.into(), json!({"type": "noul", "noul": score}));
                    }
                    (
                        200,
                        json!({"model": body["model"], "answers": answers,
                        "usage": {"input_tokens": 100, "output_tokens": 10}})
                        .to_string(),
                    )
                }
            } else {
                let route = path.strip_prefix("/api/v10").unwrap_or(&path);
                let parts: Vec<_> = route.trim_matches('/').split('/').collect();
                let channel = parts
                    .get(1)
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(0);
                match (method.as_str(), parts.as_slice()) {
                    ("GET", ["channels", _]) => (
                        200,
                        json!({
                            "id": channel.to_string(), "type": 0, "guild_id": "500", "position": 0,
                            "permission_overwrites": [], "name": "fixture", "nsfw": false,
                            "parent_id": null, "topic": null, "last_message_id": null
                        })
                        .to_string(),
                    ),
                    ("GET", ["channels", _, "messages", id]) => {
                        let id = id.parse().unwrap();
                        match state.messages.get(&(channel, id)) {
                            Some(message) => (200, message.to_string()),
                            None => (
                                404,
                                json!({"code": 10008, "message": "Unknown Message"}).to_string(),
                            ),
                        }
                    }
                    ("POST", ["channels", _, "messages"]) => {
                        let status = state.notice_status;
                        if (200..300).contains(&status) {
                            state
                                .delivered_notices
                                .push(body["content"].as_str().unwrap_or("").to_owned());
                            (
                                status,
                                wire_message(
                                    channel,
                                    9_999,
                                    body["content"].as_str().unwrap_or(""),
                                )
                                .to_string(),
                            )
                        } else {
                            (
                                status,
                                json!({"code": 0, "message": "fixture notice sink failed"})
                                    .to_string(),
                            )
                        }
                    }
                    _ => (404, "{}".into()),
                }
            }
        };
        let response = format!(
            "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
            response.len(),
        );
        let _ = stream.write_all(response.as_bytes()).await;
    }

    struct NoticeFixture {
        _directory: tempfile::TempDir,
        path: Utf8PathBuf,
        network: NoticeNetwork,
        runtime: Arc<AttentionRuntime>,
        admissions: AdmissionController,
        no_rly: Arc<ConsentGate>,
    }

    impl NoticeFixture {
        async fn new(mode: AttentionMode, notices: NoticeMode, notice_status: u16) -> Self {
            let directory = tempfile::TempDir::new().unwrap();
            let path = Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
            let network = NoticeNetwork::start(notice_status).await;
            let mut settings = AttentionConfig {
                mode,
                notices,
                notice_cooldown_ms: 60_000,
                notice_channel: Some(ChannelId::new(100)),
                brief: Some(AttentionBrief {
                    text: "Synthetic declared attention fixture".into(),
                    expires_at_ms: now_ms() + 3_600_000,
                    provider_eligible: true,
                }),
                ..AttentionConfig::default()
            };
            settings.rooms.insert(
                ChannelId::new(100),
                RoomAttention {
                    provider_eligible: true,
                    ..RoomAttention::default()
                },
            );
            publish_settings(&path, &settings).await;
            let runtime = Arc::new(
                AttentionRuntime::new(
                    SourceResolver {
                        http: network.http(),
                        state: crate::state::new_state(),
                        state_dir: path.clone(),
                        ledger: Arc::new(crate::ingress_ledger::IngressLedger::new()),
                    },
                    true,
                )
                .await
                .with_test_provider(network.provider()),
            );
            let no_rly = Arc::new(ConsentGate::new(&path));
            Self {
                _directory: directory,
                path,
                network,
                runtime,
                admissions: AdmissionController::default(),
                no_rly,
            }
        }

        async fn publish_notice(&self) {
            self.runtime
                .clone()
                .publish_notice(self.no_rly.clone(), None)
                .await;
        }

        async fn configure(&self, update: impl FnOnce(&mut AttentionConfig)) {
            let mut settings = load_config(&self.path).raw.attention.clone();
            update(&mut settings);
            publish_settings(&self.path, &settings).await;
        }

        async fn classify(&mut self, id: u64, text: &str) -> Option<DecisionRecord> {
            self.network.insert(100, id, text);
            let event = self
                .runtime
                .resolver
                .recover_event(&SourceVersion {
                    key: SourceKey {
                        channel_id: ChannelId::new(100),
                        message_id: MessageId::new(id),
                    },
                    author_id: UserId::new(7),
                    author_kind: crate::attention::types::SourceAuthorKind::DirectHuman,
                    conversation: "channel:100".into(),
                    content_hash: content_hash(text),
                    observed_at_ms: now_ms(),
                })
                .await
                .unwrap();
            if !self.runtime.probe_due() {
                return None;
            }
            let config = load_config(&self.path);
            let Submission::Judge { work, .. } = self.admissions.submit(
                NotificationEvent::Message(event),
                &config,
                now_ms(),
                self.runtime.enforcement_supported,
            ) else {
                return None;
            };
            let record = self.runtime.clone().evaluate(*work).await;
            let _ = self
                .admissions
                .complete(record.clone(), &load_config(&self.path));
            Some(record)
        }
    }

    async fn publish_settings(path: &Utf8PathBuf, settings: &AttentionConfig) {
        let channel = "[[channels]]\nid = \"100\"\nrequire_mention = false\nallow_from = [\"7\"]\n[[channels]]\nid = \"101\"\nrequire_mention = false\nallow_from = [\"7\"]\n";
        let text = format!(
            "[attention_notice_route]\nrecipient = \"default\"\nchannel = \"100\"\n[access]\nallow_from = [\"7\"]\nadmin_only_mutations = false\n{channel}\n{}",
            toml::to_string(&std::collections::BTreeMap::from([("attention", settings)])).unwrap(),
        );
        std::fs::write(path.join("config.toml"), text).unwrap();
        let (_, warning) = ConfigRuntime::new(path.clone()).reload().await;
        assert!(warning.is_none(), "{warning:?}");
    }

    async fn advance_health(duration: Duration) {
        tokio::time::pause();
        tokio::time::advance(duration).await;
        tokio::time::resume();
    }

    #[test]
    fn notice_destination_requires_independent_recipient_authority() {
        scenario(|| async {
            use crate::attention::control::{AttentionCommand, execute};

            let fixture = NoticeFixture::new(AttentionMode::Log, NoticeMode::Failures, 200).await;
            let current = load_config(&fixture.path);
            assert!(crate::gate::OutboundGate::check_channel(
                &current,
                101,
                &std::collections::HashSet::new()
            ));
            let mut settings = current.raw.attention.clone();
            settings.notice_channel = Some(ChannelId::new(101));
            assert!(
                execute(
                    fixture.runtime.clone(),
                    AttentionCommand::Configure { settings },
                    true,
                )
                .await
                .is_err()
            );
            assert_eq!(
                load_config(&fixture.path).raw.attention.notice_channel,
                Some(ChannelId::new(100))
            );

            // Direct operator-file mistakes must also fail closed at the actual send boundary.
            let config_path = fixture.path.join("config.toml");
            let mut document: toml_edit::DocumentMut = std::fs::read_to_string(&config_path)
                .unwrap()
                .parse()
                .unwrap();
            document["attention_notice_route"]["recipient"] = toml_edit::value("another-recipient");
            std::fs::write(&config_path, document.to_string()).unwrap();
            let (_, warning) = ConfigRuntime::new(fixture.path.clone()).reload().await;
            assert!(warning.is_none(), "{warning:?}");
            fixture.runtime.failed("fixture failure".into());
            fixture.publish_notice().await;
            assert_eq!(fixture.network.notice_attempts(), 0);
            assert_eq!(fixture.runtime.health().notice_failures, 1);

            let mut settings = load_config(&fixture.path).raw.attention.clone();
            settings.notices = NoticeMode::Off;
            execute(
                fixture.runtime.clone(),
                AttentionCommand::Configure { settings },
                true,
            )
            .await
            .unwrap();
            assert_eq!(
                load_config(&fixture.path).raw.attention.notices,
                NoticeMode::Off
            );
        });
    }

    #[test]
    fn delivery_guard_defers_while_a_durable_store_transaction_owns_the_mutex() {
        scenario(|| async {
            use crate::codex::{AttentionDeliveryGuard, AttentionGuardFailure};

            let fixture = NoticeFixture::new(AttentionMode::Log, NoticeMode::Off, 200).await;
            let held = fixture.runtime.store.lock().unwrap();
            let runtime = fixture.runtime.clone();
            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
            let checker = std::thread::spawn(move || {
                let result = runtime.check(&json!({
                    "params": {"meta": {"attention_record": "held-store-record"}}
                }));
                sender.send(result).unwrap();
            });
            let observed = receiver.recv_timeout(Duration::from_secs(5));
            // Release even on regression so the checker exits before the assertion fails.
            drop(held);
            checker.join().unwrap();
            assert!(matches!(
                observed,
                Ok(Err(AttentionGuardFailure::Unavailable))
            ));
        });
    }

    #[test]
    fn notice_modes_coalesce_flapping_and_expose_recovery_with_fake_time() {
        scenario(|| async {
            for notices in [
                NoticeMode::Off,
                NoticeMode::Failures,
                NoticeMode::FailuresAndRecovery,
            ] {
                let fixture = NoticeFixture::new(AttentionMode::Log, notices, 200).await;
                fixture.runtime.failed("fixture provider outage".into());
                fixture.publish_notice().await;
                fixture.runtime.failed("fixture provider outage".into());
                fixture.publish_notice().await;
                fixture.runtime.recovered();
                fixture.publish_notice().await;
                fixture.runtime.failed("fixture provider outage".into());
                fixture.publish_notice().await;

                let initial = fixture.network.delivered_notices();
                assert_eq!(
                    initial.len(),
                    usize::from(notices != NoticeMode::Off),
                    "initial and cooldown notices for {notices:?}"
                );
                assert!(
                    initial
                        .iter()
                        .all(|text| text.contains("fixture provider outage"))
                );
                let health = fixture.runtime.health();
                assert_eq!((health.failures, health.recoveries), (3, 1));
                assert_eq!(health.degraded.as_deref(), Some("fixture provider outage"));

                advance_health(Duration::from_secs(61)).await;
                fixture.publish_notice().await;
                fixture.runtime.recovered();
                fixture.publish_notice().await;
                advance_health(Duration::from_secs(61)).await;
                fixture.publish_notice().await;

                let expected = match notices {
                    NoticeMode::Off => 0,
                    NoticeMode::Failures => 2,
                    NoticeMode::FailuresAndRecovery => 3,
                };
                let delivered = fixture.network.delivered_notices();
                assert_eq!(
                    delivered.len(),
                    expected,
                    "coalesced notices for {notices:?}"
                );
                if notices == NoticeMode::FailuresAndRecovery {
                    assert!(!delivered[2].contains("fixture provider outage"));
                }
                let health = fixture.runtime.health();
                assert_eq!((health.failures, health.recoveries), (3, 2));
                assert!(health.degraded.is_none());
                assert_eq!(health.notice_failures, 0);
                assert_eq!(
                    fixture
                        .runtime
                        .resolver
                        .state
                        .read()
                        .await
                        .is_own_send(9_999),
                    notices != NoticeMode::Off,
                    "published notices must be tracked by shared outbound delivery"
                );
                assert_eq!(
                    fixture.network.provider_calls(),
                    0,
                    "notices must bypass classification"
                );
            }
        });
    }

    #[test]
    fn failed_notice_sink_stays_local_and_never_recurses_or_calls_the_provider() {
        scenario(|| async {
            let fixture =
                NoticeFixture::new(AttentionMode::Log, NoticeMode::FailuresAndRecovery, 503).await;
            fixture.runtime.failed("fixture provider outage".into());
            fixture.publish_notice().await;
            fixture.publish_notice().await;
            advance_health(Duration::from_secs(61)).await;
            fixture.publish_notice().await;
            fixture.runtime.recovered();
            fixture.publish_notice().await;

            assert_eq!(fixture.network.notice_attempts(), 2);
            assert!(fixture.network.delivered_notices().is_empty());
            assert_eq!(fixture.network.provider_calls(), 0);
            let health = fixture.runtime.health();
            assert_eq!(
                (health.failures, health.recoveries, health.notice_failures),
                (1, 1, 2)
            );
            assert!(health.degraded.is_none());
            assert!(health.last_notice_error.is_some());
        });
    }

    #[test]
    fn compatible_recovery_preserves_modes_while_explicit_off_and_muting_win() {
        scenario(|| async {
            for mode in [AttentionMode::Log, AttentionMode::On] {
                let mut fixture = NoticeFixture::new(mode, NoticeMode::Off, 200).await;
                fixture.network.set_provider_malformed(true);
                let failed = fixture
                    .classify(10, "eligible provider failure")
                    .await
                    .unwrap();
                assert!(failed.judgment.is_none());
                assert_eq!(fixture.network.provider_calls(), 1);
                assert_eq!(load_config(&fixture.path).raw.attention.mode, mode);
                assert!(fixture.runtime.health().degraded.is_some());

                fixture.network.set_provider_malformed(false);
                advance_health(Duration::from_secs(31)).await;
                let recovered = fixture
                    .classify(11, "eligible compatible recovery")
                    .await
                    .unwrap();
                assert!(recovered.judgment.is_some());
                assert_eq!(fixture.network.provider_calls(), 2);
                assert_eq!(load_config(&fixture.path).raw.attention.mode, mode);
                let health = fixture.runtime.health();
                assert!(health.degraded.is_none());
                assert_eq!(health.recoveries, 1);
            }

            let mut explicitly_off =
                NoticeFixture::new(AttentionMode::On, NoticeMode::Off, 200).await;
            explicitly_off.network.set_provider_malformed(true);
            explicitly_off
                .classify(20, "eligible outage before explicit off")
                .await
                .unwrap();
            assert_eq!(explicitly_off.network.provider_calls(), 1);
            explicitly_off
                .configure(|settings| settings.mode = AttentionMode::Off)
                .await;
            explicitly_off.network.set_provider_malformed(false);
            advance_health(Duration::from_secs(31)).await;
            assert!(
                explicitly_off
                    .classify(21, "eligible after explicit off")
                    .await
                    .is_none()
            );
            assert_eq!(explicitly_off.network.provider_calls(), 1);
            assert_eq!(
                load_config(&explicitly_off.path).raw.attention.mode,
                AttentionMode::Off
            );

            let mut muted =
                NoticeFixture::new(AttentionMode::Log, NoticeMode::FailuresAndRecovery, 200).await;
            muted.network.set_provider_malformed(true);
            muted
                .classify(30, "eligible outage before notice mute")
                .await
                .unwrap();
            let calls = muted.network.provider_calls();
            muted.publish_notice().await;
            assert_eq!(muted.network.provider_calls(), calls);
            assert_eq!(muted.network.delivered_notices().len(), 1);
            muted
                .configure(|settings| settings.notices = NoticeMode::Off)
                .await;
            muted.network.set_provider_malformed(false);
            advance_health(Duration::from_secs(31)).await;
            assert!(
                muted
                    .classify(31, "eligible recovery after notice mute")
                    .await
                    .unwrap()
                    .judgment
                    .is_some()
            );
            muted.publish_notice().await;
            assert_eq!(muted.network.delivered_notices().len(), 1);
            assert_eq!(muted.network.provider_calls(), calls + 1);
            let health = muted.runtime.health();
            assert!(health.degraded.is_none());
            assert_eq!(health.recoveries, 1);
            assert_eq!(
                load_config(&muted.path).raw.attention.notices,
                NoticeMode::Off
            );
        });
    }
}
