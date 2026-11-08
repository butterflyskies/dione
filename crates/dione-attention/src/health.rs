//! Attention health lifecycle, notice bookkeeping, and durable status persistence.
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

/// Inspectable degraded-state counters and notice bookkeeping, without source excerpts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AttentionHealth {
    /// Current degradation reason.
    pub degraded: Option<String>,
    /// Most recent unknown outcome.
    pub last_unknown: Option<String>,
    /// Count of degradation events.
    pub failures: u64,
    /// Count of recoveries.
    pub recoveries: u64,
    /// Monotonic recovery deadline in epoch milliseconds.
    pub retry_after_ms: u64,
    /// Time of the last proposed notice.
    pub last_notice_ms: Option<u64>,
    /// Failures already consumed by notice bookkeeping.
    pub noticed_failures: u64,
    /// Recoveries already consumed by notice bookkeeping.
    pub noticed_recoveries: u64,
    /// Most recent status persistence failure.
    pub status_write_error: Option<String>,
    /// Count of failed notice attempts.
    pub notice_failures: u64,
    /// Most recent caller-reported notice error.
    pub last_notice_error: Option<String>,
}

/// A status persistence failure with its original causal error retained.
#[derive(Debug, thiserror::Error)]
#[error("{kind}")]
pub struct HealthPersistenceError {
    /// The failed persistence boundary.
    #[source]
    pub kind: HealthPersistenceErrorKind,
}
/// The status persistence boundary that failed.
#[derive(Debug, thiserror::Error)]
pub enum HealthPersistenceErrorKind {
    /// The status mutex was poisoned.
    #[error("attention health lock failed")]
    LockPoisoned,
    /// A filesystem or serialization operation failed.
    #[error("attention status persistence failed")]
    Io(#[source] std::io::Error),
    /// The blocking worker did not complete normally.
    #[error("attention status worker failed")]
    Worker(#[source] tokio::task::JoinError),
}
/// Serialized health state; it owns no configuration publication or transport authority.
pub struct AttentionHealthController {
    state: Mutex<AttentionHealth>,
    dirty: AtomicBool,
    epoch_ms: u64,
    epoch: tokio::time::Instant,
    state_dir: Utf8PathBuf,
}
impl AttentionHealthController {
    /// Restores status synchronously inside the caller's existing initialization worker.
    pub fn load(path: &Utf8Path) -> AttentionHealth {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice::<AttentionHealth>(&bytes).unwrap_or_else(|_| {
                AttentionHealth {
                    degraded: Some("attention status is corrupt".to_owned()),
                    ..AttentionHealth::default()
                }
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                AttentionHealth::default()
            }
            Err(_) => AttentionHealth {
                degraded: Some("attention status is unreadable".to_owned()),
                ..AttentionHealth::default()
            },
        }
    }
    /// Retains a restored snapshot after the application applies its startup clock clamps.
    pub fn from_restored(state: AttentionHealth, state_dir: Utf8PathBuf, epoch_ms: u64) -> Self {
        Self {
            state: Mutex::new(state),
            dirty: AtomicBool::new(true),
            epoch_ms,
            epoch: tokio::time::Instant::now(),
            state_dir,
        }
    }
    /// Writes dirty status on a blocking worker, retaining retry state on filesystem failure.
    pub async fn persist(self: &Arc<Self>) -> Result<(), HealthPersistenceError> {
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        let controller = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut health = controller
                .state
                .lock()
                .map_err(|_| HealthPersistenceError {
                    kind: HealthPersistenceErrorKind::LockPoisoned,
                })?;
            let path = controller.state_dir.join("attention-status.json");
            let result = (|| -> std::io::Result<()> {
                std::fs::create_dir_all(&controller.state_dir)?;
                let mut temporary = tempfile::NamedTempFile::new_in(&controller.state_dir)?;
                health.status_write_error = None;
                serde_json::to_writer(&mut temporary, &*health).map_err(std::io::Error::other)?;
                temporary.as_file().sync_all()?;
                temporary.persist(&path).map_err(|error| error.error)?;
                std::fs::File::open(&controller.state_dir)?.sync_all()
            })();
            if let Err(error) = result {
                health.status_write_error = Some("attention status persistence failed".to_owned());
                controller.dirty.store(true, Ordering::Release);
                return Err(HealthPersistenceError {
                    kind: HealthPersistenceErrorKind::Io(error),
                });
            }
            Ok(())
        })
        .await
        .map_err(|error| HealthPersistenceError {
            kind: HealthPersistenceErrorKind::Worker(error),
        })?
    }
    /// Returns health, visibly degraded if its synchronization state is unavailable.
    pub fn snapshot(&self) -> AttentionHealth {
        self.state
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
        if let Ok(mut health) = self.state.lock()
            && health.last_unknown.as_deref() != Some(reason.as_str())
        {
            health.last_unknown = Some(reason);
            self.dirty.store(true, Ordering::Release);
        }
    }

    fn now(&self) -> u64 {
        self.epoch_ms
            .saturating_add(self.epoch.elapsed().as_millis().min(u64::MAX as u128) as u64)
    }

    /// Reports whether the bounded provider recovery backoff has elapsed.
    pub fn probe_due(&self) -> bool {
        self.snapshot().retry_after_ms <= self.now()
    }

    /// Records unavailable calibration while allowing healthy scoring to continue.
    pub fn policy_unavailable(&self, reason: String) {
        if let Ok(mut health) = self.state.lock() {
            let changed = health.degraded.as_ref() != Some(&reason);
            if changed || health.retry_after_ms != 0 {
                if changed {
                    health.failures = health.failures.saturating_add(1);
                }
                health.degraded = Some(reason);
                health.retry_after_ms = 0;
                self.dirty.store(true, Ordering::Release);
            }
        }
    }

    /// Clears degradation and backoff while counting actual recoveries.
    pub fn recovered(&self) {
        if let Ok(mut health) = self.state.lock() {
            if health.degraded.take().is_some() {
                health.recoveries = health.recoveries.saturating_add(1);
                self.dirty.store(true, Ordering::Release);
            }
            if health.retry_after_ms != 0 {
                health.retry_after_ms = 0;
                self.dirty.store(true, Ordering::Release);
            }
        }
    }

    /// Records failure and a bounded recovery backoff.
    pub fn failed(&self, reason: String, outage_retry_ms: u64) {
        if let Ok(mut health) = self.state.lock() {
            health.degraded = Some(reason);
            health.failures = health.failures.saturating_add(1);
            health.retry_after_ms = self.now().saturating_add(outage_retry_ms);
            self.dirty.store(true, Ordering::Release);
        }
    }

    /// Coalesces descriptive notice text without selecting a destination or sending.
    pub fn propose_notice(
        &self,
        config: &crate::config::AttentionConfig,
    ) -> Result<Option<String>, HealthPersistenceError> {
        let text = {
            let Ok(mut health) = self.state.lock() else {
                return Err(HealthPersistenceError {
                    kind: HealthPersistenceErrorKind::LockPoisoned,
                });
            };
            let before = (
                health.last_notice_ms,
                health.noticed_failures,
                health.noticed_recoveries,
            );
            let text = health.notice_text(config, self.now());
            if before
                != (
                    health.last_notice_ms,
                    health.noticed_failures,
                    health.noticed_recoveries,
                )
            {
                self.dirty.store(true, Ordering::Release);
            }
            text
        };
        Ok(text)
    }
    /// Records the caller-reported notice outcome without establishing delivery authority.
    pub fn record_notice_outcome(&self, outcome: Result<(), &str>) {
        if let Ok(mut health) = self.state.lock() {
            match outcome {
                Ok(()) => health.last_notice_error = None,
                Err(error) => {
                    health.notice_failures = health.notice_failures.saturating_add(1);
                    health.last_notice_error = Some(error.to_owned());
                }
            }
            self.dirty.store(true, Ordering::Release);
        }
    }
}
impl AttentionHealth {
    fn notice_text(&mut self, config: &crate::config::AttentionConfig, now: u64) -> Option<String> {
        use crate::config::NoticeMode;
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
