//! Prepared and running ownership of a harness session.
use super::{
    HarnessConnection,
    codex_session::{CodexSession, CodexSessionOptions, RunningCodexSession},
    omp_session::{OmpAdmission, OmpSession, RunningOmpSession},
};
use crate::{
    attention::delivery::AttentionDeliveryGuard,
    codex::{CodexDeliveryError, CodexQueueError},
    config::DeliveryConfig,
};
use camino::Utf8Path;
use std::{error::Error, sync::Arc};
use thiserror::Error;
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;

/// A prepared connection whose delivery worker has not started.
#[must_use]
pub struct HarnessSession {
    connection: HarnessConnection,
    lifecycle: PreparedLifecycle,
}

enum PreparedLifecycle {
    Claude,
    OhMyPi,
    OmpResident(Box<OmpSession>),
    Codex(Box<CodexSession>),
}

/// Owns the connection until delivery shutdown has completed.
#[must_use]
pub struct RunningHarnessSession {
    connection: HarnessConnection,
    lifecycle: RunningLifecycle,
}

enum RunningLifecycle {
    Claude,
    OhMyPi,
    OmpResident(RunningOmpSession),
    Codex(RunningCodexSession),
}

pub(crate) enum ResidentAdmission {
    Codex(crate::codex::CodexEventQueue),
    Omp(OmpAdmission),
}

impl HarnessSession {
    /// Prepares Claude notifications on the shared MCP writer.
    pub fn claude() -> Self {
        Self {
            connection: HarnessConnection::claude(),
            lifecycle: PreparedLifecycle::Claude,
        }
    }

    /// Prepares Oh My Pi events on the shared MCP writer.
    pub fn oh_my_pi() -> Self {
        Self {
            connection: HarnessConnection::oh_my_pi(),
            lifecycle: PreparedLifecycle::OhMyPi,
        }
    }

    pub(crate) async fn oh_my_pi_resident(
        state_dir: &Utf8Path,
    ) -> Result<Self, HarnessSessionError> {
        let session = OmpSession::open(state_dir)
            .await
            .map_err(|source| HarnessSessionError {
                kind: HarnessSessionErrorKind::Omp(Box::new(source)),
            })?;
        let connection = HarnessConnection::oh_my_pi_with_control(session.control());
        Ok(Self {
            connection,
            lifecycle: PreparedLifecycle::OmpResident(Box::new(session)),
        })
    }

    pub(crate) fn resident_admission(&self) -> Result<ResidentAdmission, HarnessSessionError> {
        match &self.lifecycle {
            PreparedLifecycle::Codex(session) => {
                Ok(ResidentAdmission::Codex(session.codex_admission()))
            }
            PreparedLifecycle::OmpResident(session) => {
                Ok(ResidentAdmission::Omp(session.admission()))
            }
            PreparedLifecycle::Claude | PreparedLifecycle::OhMyPi => Err(HarnessSessionError {
                kind: HarnessSessionErrorKind::TeamsUnavailable,
            }),
        }
    }

    /// Opens the durable inbox without resolving or starting live delivery.
    pub async fn codex(
        state_dir: &Utf8Path,
        options: CodexSessionOptions,
    ) -> Result<Self, HarnessSessionError> {
        let session = CodexSession::prepare(state_dir, options).await?;
        Ok(Self {
            connection: session.connection(),
            lifecycle: PreparedLifecycle::Codex(Box::new(session)),
        })
    }

    /// Returns the same complete connection retained by this session.
    pub fn connection(&self) -> HarnessConnection {
        self.connection.clone()
    }

    /// Returns the prepared Codex admission queue for explicit provider composition.
    pub fn codex_admission(&self) -> Result<crate::codex::CodexEventQueue, HarnessSessionError> {
        match &self.lifecycle {
            PreparedLifecycle::Claude
            | PreparedLifecycle::OhMyPi
            | PreparedLifecycle::OmpResident(_) => Err(HarnessSessionError {
                kind: HarnessSessionErrorKind::TeamsUnavailable,
            }),
            PreparedLifecycle::Codex(session) => Ok(session.codex_admission()),
        }
    }

    /// Starts delivery after the shared attention authority has been created.
    pub async fn start(
        self,
        delivery: &DeliveryConfig,
        attention: Arc<dyn AttentionDeliveryGuard>,
        process_shutdown: CancellationToken,
    ) -> Result<RunningHarnessSession, HarnessSessionError> {
        let lifecycle = match self.lifecycle {
            PreparedLifecycle::Claude => RunningLifecycle::Claude,
            PreparedLifecycle::OhMyPi => RunningLifecycle::OhMyPi,
            PreparedLifecycle::OmpResident(session) => RunningLifecycle::OmpResident(
                session.start(self.connection.output().clone(), process_shutdown),
            ),
            PreparedLifecycle::Codex(session) => {
                RunningLifecycle::Codex(session.start(delivery, attention, process_shutdown).await?)
            }
        };
        Ok(RunningHarnessSession {
            connection: self.connection,
            lifecycle,
        })
    }
}

impl RunningHarnessSession {
    /// Stops delivery after accepted ingress and replies have drained.
    pub async fn shutdown(self) -> Result<(), HarnessSessionError> {
        let result = match self.lifecycle {
            RunningLifecycle::Claude | RunningLifecycle::OhMyPi => Ok(()),
            RunningLifecycle::OmpResident(session) => {
                session
                    .shutdown()
                    .await
                    .map_err(|source| HarnessSessionError {
                        kind: HarnessSessionErrorKind::Omp(Box::new(source)),
                    })
            }
            RunningLifecycle::Codex(session) => session.shutdown().await,
        };
        drop(self.connection);
        result
    }
}

/// A harness preparation, startup, or shutdown failure.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct HarnessSessionError {
    /// The failed lifecycle operation and its original source.
    pub kind: HarnessSessionErrorKind,
}

/// Failures of the concrete session lifecycle.
#[derive(Debug, Error)]
pub enum HarnessSessionErrorKind {
    /// The child-owned OMP inbox or worker failed.
    #[error("OMP resident session failed")]
    Omp(#[source] Box<dyn Error + Send + Sync>),
    /// The durable inbox could not be opened.
    #[error("failed to open Codex event queue")]
    QueueOpen(#[source] CodexQueueError),
    /// No usable delivery configuration could be resolved.
    #[error("failed to configure Codex live delivery")]
    Configure(#[source] CodexDeliveryError),
    /// The environment fallback contained an invalid thread identifier.
    #[error("invalid CODEX_THREAD_ID")]
    EnvironmentThread(#[source] CodexQueueError),
    /// The initial thread binding could not be durably committed.
    #[error("failed to persist initial Codex thread binding")]
    InitialBinding(#[source] CodexQueueError),
    /// This session does not support resident Teams ingress.
    #[error("resident Teams ingress requires a prepared Codex or OMP session")]
    TeamsUnavailable,
    /// The owned worker could not be joined during shutdown.
    #[error("Codex delivery worker panicked during drain")]
    Join(#[source] JoinError),
}

#[cfg(test)]
#[path = "session_tests.rs"]
pub(super) mod tests;
