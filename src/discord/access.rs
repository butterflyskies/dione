//! Pending Discord access requests and configuration-backed decisions.

use crate::{
    config::{ConfigDurability, ConfigRuntime, LoadedConfig},
    config_store::BoxError,
    queue::AccessQueue,
    timestamp::Timestamp,
};
use camino::Utf8PathBuf;
use serenity::model::id::UserId;
use std::{fmt, sync::Arc};
use thiserror::Error;
use tokio::sync::Mutex;

/// An observed persisted user ID, including historical zero values.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StoredAccessUserId(u64);

impl fmt::Display for StoredAccessUserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A pending request localized to the caller's configuration snapshot.
pub(crate) struct PendingAccessRequest {
    user_id: StoredAccessUserId,
    username: String,
    message_preview: String,
    timestamp: Timestamp,
}

impl PendingAccessRequest {
    /// The user ID observed in the persisted request.
    pub(crate) fn user_id(&self) -> StoredAccessUserId {
        self.user_id
    }

    /// The requesting user's recorded name.
    pub(crate) fn username(&self) -> &str {
        &self.username
    }

    /// The recorded message preview.
    pub(crate) fn message_preview(&self) -> &str {
        &self.message_preview
    }

    /// The localized request timestamp.
    pub(crate) fn timestamp(&self) -> &Timestamp {
        &self.timestamp
    }
}

/// An applied access grant and its configuration durability.
pub(crate) struct AccessApproval {
    user_id: UserId,
    username: String,
    generation: u64,
    durability: ConfigDurability,
}

impl AccessApproval {
    /// The user whose access was granted.
    pub(crate) fn user_id(&self) -> UserId {
        self.user_id
    }

    /// The name recorded on the pending request.
    pub(crate) fn username(&self) -> &str {
        &self.username
    }

    /// The published configuration generation.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// The durability of the configuration mutation.
    pub(crate) fn durability(&self) -> &ConfigDurability {
        &self.durability
    }
}

/// A request removed from the in-memory queue with best-effort persistence.
pub(crate) struct AccessDenial {
    user_id: UserId,
    username: String,
}

impl AccessDenial {
    /// The user whose request was denied.
    pub(crate) fn user_id(&self) -> UserId {
        self.user_id
    }

    /// The name recorded on the removed request.
    pub(crate) fn username(&self) -> &str {
        &self.username
    }
}

/// A failed access decision with its requested user ID.
#[derive(Debug, Error)]
#[error("access decision failed for user {user_id}: {kind}")]
pub(crate) struct AccessError {
    pub(crate) user_id: UserId,
    #[source]
    pub(crate) kind: AccessErrorKind,
}

/// The reason an access decision failed.
#[derive(Debug, Error)]
pub(crate) enum AccessErrorKind {
    #[error("no pending request")]
    NoPendingRequest,
    #[error("failed to persist config: {0}")]
    PersistConfig(#[source] BoxError),
}

/// Owns access-request decisions across the queue and configuration store.
pub(crate) struct AccessCtx {
    queue: Arc<Mutex<AccessQueue>>,
    config: Arc<LoadedConfig>,
    state_dir: Utf8PathBuf,
}

impl AccessCtx {
    /// Construct an access owner using the caller's configuration snapshot.
    pub(crate) fn new(
        queue: Arc<Mutex<AccessQueue>>,
        config: Arc<LoadedConfig>,
        state_dir: Utf8PathBuf,
    ) -> Self {
        Self {
            queue,
            config,
            state_dir,
        }
    }

    /// List pending requests in the queue's existing chronological order.
    pub(crate) async fn list(&self) -> Vec<PendingAccessRequest> {
        let queue = self.queue.lock().await;
        queue
            .list()
            .iter()
            .map(|request| PendingAccessRequest {
                user_id: StoredAccessUserId(request.user_id),
                username: request.username.clone(),
                message_preview: request.message_preview.clone(),
                timestamp: self.config.localize_utc(&request.timestamp),
            })
            .collect()
    }

    /// Apply an access grant, retaining the request when durability is unknown.
    pub(crate) async fn approve(&self, user_id: UserId) -> Result<AccessApproval, AccessError> {
        let uid = user_id.get();
        let request = {
            let queue = self.queue.lock().await;
            queue.peek(uid).cloned()
        }
        .ok_or(AccessError {
            user_id,
            kind: AccessErrorKind::NoPendingRequest,
        })?;
        let mutation_user_id = uid.to_string();
        let outcome = ConfigRuntime::new(self.state_dir.clone())
            .mutate(move |editor| editor.ensure_in_allow_from(&mutation_user_id))
            .await
            .map_err(|error| {
                tracing::warn!(user_id = uid, error = %error, "failed to persist allow_from update");
                AccessError { user_id, kind: AccessErrorKind::PersistConfig(error) }
            })?;

        match &outcome.durability {
            ConfigDurability::Unknown { warning } => {
                tracing::warn!(
                    user_id = uid,
                    generation = outcome.generation,
                    warning,
                    "access approval applied with unknown durability; retaining pending request"
                );
            }
            ConfigDurability::Durable => {
                let mut queue = self.queue.lock().await;
                queue.approve(uid);
            }
        }
        Ok(AccessApproval {
            user_id,
            username: request.username,
            generation: outcome.generation,
            durability: outcome.durability,
        })
    }

    /// Deny a request with the queue's existing best-effort persistence.
    pub(crate) async fn deny(&self, user_id: UserId) -> Result<AccessDenial, AccessError> {
        let removed = {
            let mut queue = self.queue.lock().await;
            queue.deny(user_id.get())
        };
        let request = removed.ok_or(AccessError {
            user_id,
            kind: AccessErrorKind::NoPendingRequest,
        })?;
        Ok(AccessDenial {
            user_id,
            username: request.username,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{error::Error as _, io};

    #[test]
    fn access_error_retains_config_source() {
        let error = AccessError {
            user_id: UserId::new(42),
            kind: AccessErrorKind::PersistConfig(Box::new(io::Error::other(
                "config source fixture",
            ))),
        };
        assert_eq!(error.user_id, UserId::new(42));
        let source = error.source().unwrap().source().unwrap();
        let source = source.downcast_ref::<io::Error>().unwrap();
        assert_eq!(source.kind(), io::ErrorKind::Other);
        assert_eq!(source.to_string(), "config source fixture");
    }
}
