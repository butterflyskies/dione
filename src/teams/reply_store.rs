//! Bounded, owner-only persistence for authenticated Teams reply authority.

use super::{ActivityId, ActorId, ApplicationId, BotId, ConversationId, TenantId};
use crate::teams::{
    AdmissionPolicy, AuthenticatedConversationReference, AuthenticatedTeamsEnvelope, ReplyHandle,
};
use camino::{Utf8Path, Utf8PathBuf};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    sync::{Arc, Mutex},
};
use thiserror::Error;
use uuid::Uuid;

const LOCK_NAME: &str = "teams-reply-authorities.lock";
const FILE_NAME: &str = "teams-reply-authorities.json";
const SCHEMA_VERSION: u32 = 1;
const MAX_PENDING: usize = 256;
const MAX_RECORD_BYTES: usize = 16 * 1024;
const MAX_STORE_BYTES: usize = MAX_PENDING * MAX_RECORD_BYTES * 2;

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub(crate) struct TeamsEventKey {
    pub(crate) tenant_id: TenantId,
    pub(crate) bot_id: BotId,
    pub(crate) conversation_id: ConversationId,
    pub(crate) activity_id: ActivityId,
}

impl TeamsEventKey {
    pub(crate) fn from_envelope(envelope: &AuthenticatedTeamsEnvelope) -> Self {
        let reference = &envelope.reference;
        Self {
            tenant_id: reference.dione_authorized_tenant_id.clone(),
            bot_id: reference.bot_id.clone(),
            conversation_id: reference.conversation_id.clone(),
            activity_id: reference.incoming_activity_id.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SealedReplyAuthority {
    pub(crate) event_key: TeamsEventKey,
    pub(crate) service_url: String,
    pub(crate) channel_id: String,
    pub(crate) sender_id: ActorId,
    pub(crate) app_id: ApplicationId,
    pub(crate) expires_at: DateTime<Utc>,
}

impl SealedReplyAuthority {
    pub(crate) fn seal(
        envelope: &AuthenticatedTeamsEnvelope,
        app_id: &ApplicationId,
        expires_at: DateTime<Utc>,
    ) -> Self {
        Self {
            event_key: TeamsEventKey::from_envelope(envelope),
            service_url: envelope.reference.service_url.to_string(),
            channel_id: envelope.reference.channel_id.clone(),
            sender_id: envelope.reference.sender_id.clone(),
            app_id: app_id.to_owned(),
            expires_at,
        }
    }

    pub(crate) fn open(
        &self,
        policy: &AdmissionPolicy,
    ) -> Result<AuthenticatedTeamsEnvelope, ReplyStoreError> {
        self.validate(policy)?;
        let service_url = Url::parse(&self.service_url).map_err(|source| ReplyStoreError {
            kind: ReplyStoreErrorKind::ServiceUrl(source),
        })?;
        Ok(AuthenticatedTeamsEnvelope {
            reference: AuthenticatedConversationReference {
                service_url,
                channel_id: self.channel_id.clone(),
                conversation_id: self.event_key.conversation_id.clone(),
                incoming_activity_id: self.event_key.activity_id.clone(),
                bot_id: self.event_key.bot_id.clone(),
                sender_id: self.sender_id.clone(),
                dione_authorized_tenant_id: self.event_key.tenant_id.clone(),
            },
            // The authenticated message text is not needed for a reply.
            text: String::new(),
        })
    }

    fn validate(&self, policy: &AdmissionPolicy) -> Result<(), ReplyStoreError> {
        if self.app_id != policy.app_id
            || self.event_key.tenant_id != policy.tenant_id
            || !policy
                .allowed_conversations
                .contains(&self.event_key.conversation_id)
            || !policy.channels.contains_key(&self.channel_id)
        {
            return Err(ReplyStoreError {
                kind: ReplyStoreErrorKind::InvalidRecord(InvalidRecordKind::PolicyMismatch),
            });
        }
        let url = Url::parse(&self.service_url).map_err(|source| ReplyStoreError {
            kind: ReplyStoreErrorKind::ServiceUrl(source),
        })?;
        if url.scheme() != "https"
            || url.cannot_be_a_base()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.port_or_known_default() != Some(443)
            || url
                .host_str()
                .is_none_or(|host| !policy.allowed_service_hosts.contains(host))
        {
            return Err(ReplyStoreError {
                kind: ReplyStoreErrorKind::InvalidRecord(InvalidRecordKind::ServiceOriginDenied),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReplyAuthorityState {
    schema_version: u32,
    pending: BTreeMap<ReplyHandle, SealedReplyAuthority>,
}

impl Default for ReplyAuthorityState {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            pending: BTreeMap::new(),
        }
    }
}

#[derive(Debug)]
pub(crate) enum StoreReceipt<T> {
    Persisted(T),
    VisibleDurabilityUncertain(T, io::Error),
}

#[cfg(test)]
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum PersistFailure {
    BeforeWrite,
    BeforeRename,
    AfterRename,
}

/// A failed sealed reply-authority operation.
#[derive(Debug, Error)]
#[error("{kind}")]
pub struct ReplyStoreError {
    /// The semantic classification and retained original cause.
    #[source]
    pub kind: ReplyStoreErrorKind,
}

impl From<ReplyStoreErrorKind> for ReplyStoreError {
    fn from(kind: ReplyStoreErrorKind) -> Self {
        Self { kind }
    }
}
impl From<io::Error> for ReplyStoreError {
    fn from(source: io::Error) -> Self {
        Self {
            kind: source.into(),
        }
    }
}
impl From<serde_json::Error> for ReplyStoreError {
    fn from(source: serde_json::Error) -> Self {
        Self {
            kind: source.into(),
        }
    }
}

/// Exhaustive failures from sealed reply-authority operations.
#[derive(Debug, Error)]
pub enum ReplyStoreErrorKind {
    /// A persisted service URL could not be parsed.
    #[error("invalid stored Teams service URL")]
    ServiceUrl(#[source] url::ParseError),
    /// The bounded offload semaphore was closed.
    #[error("reply store offload closed")]
    OffloadClosed(#[source] tokio::sync::AcquireError),
    /// The offload task panicked or was cancelled.
    #[error("reply store offload task failed")]
    Task(#[source] tokio::task::JoinError),
    /// The in-process store mutex was poisoned.
    #[error("reply store lock poisoned")]
    LockPoisoned,
    /// Another process owns the reply authority registry.
    #[error("teams reply authority store already owned")]
    Owned(#[source] io::Error),
    /// Teams reply authority I/O failed.
    #[error("teams reply authority I/O failed")]
    Io(#[from] io::Error),
    /// Teams reply authority JSON failed.
    #[error("teams reply authority JSON failed")]
    Json(#[source] serde_path_to_error::Error<serde_json::Error>),
    /// Teams reply authority encoding failed.
    #[error("teams reply authority encoding failed")]
    Encode(#[from] serde_json::Error),
    /// Teams reply authority directory durability is uncertain.
    #[error("teams reply authority directory durability is uncertain")]
    DurabilityUncertain(#[source] io::Error),
    /// Invalid Teams reply authority.
    #[error("invalid Teams reply authority")]
    InvalidRecord(#[source] InvalidRecordKind),
    /// Teams reply registry is full.
    #[error("teams reply registry is full")]
    Full,
    /// Teams reply handle is unknown, expired or already consumed.
    #[error("teams reply handle is unknown, expired or already consumed")]
    UnknownHandle,
}

/// Semantic failures while reopening or mutating sealed authority.
#[derive(Debug, Error)]
pub enum InvalidRecordKind {
    /// The source is not an owned bounded mode-0600 regular file.
    #[error("reply authority source is not a safe owned file")]
    UnsafeSource,
    /// The opened inode differs from the checked source.
    #[error("reply authority file changed during open")]
    SourceChanged,
    /// Teams reply authority is outside current admission policy.
    #[error("teams reply authority is outside current admission policy")]
    PolicyMismatch,
    /// Teams reply service host is outside current admission policy.
    #[error("teams reply service host is outside current admission policy")]
    ServiceOriginDenied,
    /// Reply handle collision.
    #[error("reply handle collision")]
    HandleCollision,
    /// Teams event already has reply authority.
    #[error("teams event already has reply authority")]
    EventCollision,
    /// Reply authority file exceeds its size limit.
    #[error("reply authority file exceeds its size limit")]
    FileTooLarge,
    /// Reply store has no directory.
    #[error("reply store has no directory")]
    MissingDirectory,
    /// The persisted schema version is unsupported.
    #[error("unsupported reply authority schema version")]
    SchemaVersion,
    /// The persisted authority count exceeds the bound.
    #[error("reply authority state exceeds its count limit")]
    StateTooLarge,
    /// Reply authority record exceeds its size limit.
    #[error("reply authority record exceeds its size limit")]
    RecordTooLarge,
}

pub(crate) struct TeamsReplyStore {
    _lock_file: File,
    path: Utf8PathBuf,
    temporary_path: Utf8PathBuf,
    state: ReplyAuthorityState,
    policy: AdmissionPolicy,
    directory_sync_pending: bool,
    #[cfg(test)]
    persist_failure: Option<PersistFailure>,
    #[cfg(test)]
    fail_directory_sync: bool,
}

/// A single bounded offload lane for the file-backed authority registry. Once
/// an operation starts, caller cancellation does not cancel its disk commit.
#[derive(Clone)]
pub(crate) struct AsyncTeamsReplyStore {
    inner: Arc<Mutex<TeamsReplyStore>>,
    io_slot: Arc<tokio::sync::Semaphore>,
}

impl AsyncTeamsReplyStore {
    #[cfg(test)]
    pub(super) async fn set_persist_failure_for_test(
        &self,
        failure: Option<PersistFailure>,
        fail_directory_sync: bool,
    ) {
        self.run(move |store| {
            store.persist_failure = failure;
            store.fail_directory_sync = fail_directory_sync;
            Ok(())
        })
        .await
        .unwrap();
    }

    pub(crate) async fn load(
        state_dir: &Utf8Path,
        policy: AdmissionPolicy,
    ) -> Result<Self, ReplyStoreError> {
        let state_dir = state_dir.to_owned();
        let store = tokio::task::spawn_blocking(move || TeamsReplyStore::load(&state_dir, policy))
            .await
            .map_err(join_error)??;
        Ok(Self {
            inner: Arc::new(Mutex::new(store)),
            io_slot: Arc::new(tokio::sync::Semaphore::new(1)),
        })
    }

    pub(crate) async fn find_handle(
        &self,
        key: TeamsEventKey,
        now: DateTime<Utc>,
    ) -> Result<Option<ReplyHandle>, ReplyStoreError> {
        self.run(move |store| store.find_handle(&key, now)).await
    }

    pub(crate) async fn insert(
        &self,
        handle: ReplyHandle,
        authority: SealedReplyAuthority,
        now: DateTime<Utc>,
    ) -> Result<StoreReceipt<()>, ReplyStoreError> {
        self.run(move |store| store.insert(handle, authority, now))
            .await
    }

    pub(crate) async fn get(
        &self,
        handle: ReplyHandle,
        now: DateTime<Utc>,
    ) -> Result<SealedReplyAuthority, ReplyStoreError> {
        self.run(move |store| store.get(&handle, now)).await
    }

    pub(crate) async fn take(
        &self,
        handle: ReplyHandle,
        now: DateTime<Utc>,
    ) -> Result<StoreReceipt<SealedReplyAuthority>, ReplyStoreError> {
        self.run(move |store| store.take(&handle, now)).await
    }

    pub(crate) async fn discard(
        &self,
        handle: ReplyHandle,
        now: DateTime<Utc>,
    ) -> Result<StoreReceipt<()>, ReplyStoreError> {
        self.run(move |store| store.discard(&handle, now)).await
    }

    async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut TeamsReplyStore) -> Result<T, ReplyStoreError> + Send + 'static,
    ) -> Result<T, ReplyStoreError> {
        let permit = self
            .io_slot
            .clone()
            .acquire_owned()
            .await
            .map_err(|source| ReplyStoreError {
                kind: ReplyStoreErrorKind::OffloadClosed(source),
            })?;
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut store = inner.lock().map_err(|_| ReplyStoreError {
                kind: ReplyStoreErrorKind::LockPoisoned,
            })?;
            operation(&mut store)
        })
        .await
        .map_err(join_error)?
    }
}

fn join_error(error: tokio::task::JoinError) -> ReplyStoreError {
    ReplyStoreError {
        kind: ReplyStoreErrorKind::Task(error),
    }
}

impl TeamsReplyStore {
    pub(crate) fn load(
        state_dir: &Utf8Path,
        policy: AdmissionPolicy,
    ) -> Result<Self, ReplyStoreError> {
        std::fs::create_dir_all(state_dir)?;
        // This lock belongs to the authority registry, independently of either
        // harness queue. Retain its handle before reading any snapshot bytes.
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(state_dir.join(LOCK_NAME))?;
        let metadata = lock_file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.nlink() != 1
            || metadata.permissions().mode() & 0o777 != 0o600
        {
            return Err(ReplyStoreError {
                kind: ReplyStoreErrorKind::InvalidRecord(InvalidRecordKind::UnsafeSource),
            });
        }
        FileExt::try_lock_exclusive(&lock_file).map_err(|source| ReplyStoreError {
            kind: if source.kind() == io::ErrorKind::WouldBlock {
                ReplyStoreErrorKind::Owned(source)
            } else {
                ReplyStoreErrorKind::Io(source)
            },
        })?;
        let path = state_dir.join(FILE_NAME);
        // A fresh create_new path cannot truncate a pre-existing hard link in the
        // state directory, even when a stale temporary file remains after a crash.
        let temporary_path = state_dir.join(format!("{FILE_NAME}.{}.tmp", Uuid::new_v4()));
        let (state, directory_sync_pending) = match read_no_follow(&path) {
            Ok(bytes) => (
                super::json::decode_json(&bytes).map_err(|source| ReplyStoreError {
                    kind: ReplyStoreErrorKind::Json(source),
                })?,
                true,
            ),
            Err(ReplyStoreError {
                kind: ReplyStoreErrorKind::Io(error),
            }) if error.kind() == io::ErrorKind::NotFound => {
                (ReplyAuthorityState::default(), false)
            }
            Err(error) => return Err(error),
        };
        let mut store = Self {
            _lock_file: lock_file,
            path,
            temporary_path,
            state,
            policy,
            directory_sync_pending,
            #[cfg(test)]
            persist_failure: None,
            #[cfg(test)]
            fail_directory_sync: false,
        };
        store.validate_state()?;
        store.sync_pending_directory()?;
        if store
            .state
            .pending
            .values()
            .any(|authority| authority.expires_at <= Utc::now())
        {
            match store.prune_expired(Utc::now())? {
                StoreReceipt::Persisted(()) => {}
                StoreReceipt::VisibleDurabilityUncertain((), source) => {
                    return Err(ReplyStoreError {
                        kind: ReplyStoreErrorKind::DurabilityUncertain(source),
                    });
                }
            }
        }
        Ok(store)
    }

    pub(crate) fn find_handle(
        &mut self,
        event_key: &TeamsEventKey,
        now: DateTime<Utc>,
    ) -> Result<Option<ReplyHandle>, ReplyStoreError> {
        self.sync_pending_directory()?;
        Ok(self
            .state
            .pending
            .iter()
            .find(|(_, authority)| authority.expires_at > now && authority.event_key == *event_key)
            .map(|(handle, _)| handle.clone()))
    }

    pub(crate) fn get(
        &mut self,
        handle: &ReplyHandle,
        now: DateTime<Utc>,
    ) -> Result<SealedReplyAuthority, ReplyStoreError> {
        self.sync_pending_directory()?;
        self.state
            .pending
            .get(handle)
            .filter(|authority| authority.expires_at > now)
            .cloned()
            .ok_or(ReplyStoreError {
                kind: ReplyStoreErrorKind::UnknownHandle,
            })
    }

    pub(crate) fn insert(
        &mut self,
        handle: ReplyHandle,
        authority: SealedReplyAuthority,
        now: DateTime<Utc>,
    ) -> Result<StoreReceipt<()>, ReplyStoreError> {
        authority.validate(&self.policy)?;
        self.transaction(|state| {
            state
                .pending
                .retain(|_, authority| authority.expires_at > now);
            if state.pending.len() >= MAX_PENDING {
                return Err(ReplyStoreError {
                    kind: ReplyStoreErrorKind::Full,
                });
            }
            if state.pending.contains_key(&handle) {
                return Err(ReplyStoreError {
                    kind: ReplyStoreErrorKind::InvalidRecord(InvalidRecordKind::HandleCollision),
                });
            }
            if state
                .pending
                .values()
                .any(|pending| pending.event_key == authority.event_key)
            {
                return Err(ReplyStoreError {
                    kind: ReplyStoreErrorKind::InvalidRecord(InvalidRecordKind::EventCollision),
                });
            }
            state.pending.insert(handle, authority);
            Ok(())
        })
    }

    pub(crate) fn take(
        &mut self,
        handle: &ReplyHandle,
        now: DateTime<Utc>,
    ) -> Result<StoreReceipt<SealedReplyAuthority>, ReplyStoreError> {
        self.transaction(|state| {
            state
                .pending
                .retain(|_, authority| authority.expires_at > now);
            state.pending.remove(handle).ok_or(ReplyStoreError {
                kind: ReplyStoreErrorKind::UnknownHandle,
            })
        })
    }

    pub(crate) fn discard(
        &mut self,
        handle: &ReplyHandle,
        now: DateTime<Utc>,
    ) -> Result<StoreReceipt<()>, ReplyStoreError> {
        self.transaction(|state| {
            state
                .pending
                .retain(|_, authority| authority.expires_at > now);
            state.pending.remove(handle);
            Ok(())
        })
    }

    fn prune_expired(&mut self, now: DateTime<Utc>) -> Result<StoreReceipt<()>, ReplyStoreError> {
        self.transaction(|state| {
            state
                .pending
                .retain(|_, authority| authority.expires_at > now);
            Ok(())
        })
    }

    fn transaction<T>(
        &mut self,
        mutate: impl FnOnce(&mut ReplyAuthorityState) -> Result<T, ReplyStoreError>,
    ) -> Result<StoreReceipt<T>, ReplyStoreError> {
        self.sync_pending_directory()?;
        let previous = self.state.clone();
        let value = match mutate(&mut self.state) {
            Ok(value) => value,
            Err(error) => {
                self.state = previous;
                return Err(error);
            }
        };
        match self.persist() {
            Ok(()) => Ok(StoreReceipt::Persisted(value)),
            Err(ReplyStoreError {
                kind: ReplyStoreErrorKind::DurabilityUncertain(source),
            }) => Ok(StoreReceipt::VisibleDurabilityUncertain(value, source)),
            Err(error) => {
                self.state = previous;
                Err(error)
            }
        }
    }

    fn persist(&mut self) -> Result<(), ReplyStoreError> {
        self.validate_state()?;
        let bytes = serde_json::to_vec_pretty(&self.state)?;
        if bytes.len() > MAX_STORE_BYTES {
            return Err(ReplyStoreError {
                kind: ReplyStoreErrorKind::InvalidRecord(InvalidRecordKind::FileTooLarge),
            });
        }
        #[cfg(test)]
        if self.persist_failure == Some(PersistFailure::BeforeWrite) {
            return Err(io::Error::other("injected reply-store prewrite failure").into());
        }
        let mut options = OpenOptions::new();
        options
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW);
        let mut temporary = options.open(&self.temporary_path)?;
        if let Err(error) = temporary
            .write_all(&bytes)
            .and_then(|()| temporary.sync_all())
        {
            drop(temporary);
            let _ = std::fs::remove_file(&self.temporary_path);
            return Err(error.into());
        }
        drop(temporary);
        #[cfg(test)]
        if self.persist_failure == Some(PersistFailure::BeforeRename) {
            let _ = std::fs::remove_file(&self.temporary_path);
            return Err(io::Error::other("injected reply-store prerename failure").into());
        }
        if let Err(error) = std::fs::rename(&self.temporary_path, &self.path) {
            let _ = std::fs::remove_file(&self.temporary_path);
            return Err(error.into());
        }
        self.directory_sync_pending = true;
        #[cfg(test)]
        if self.persist_failure == Some(PersistFailure::AfterRename) {
            return Err(ReplyStoreError {
                kind: ReplyStoreErrorKind::DurabilityUncertain(io::Error::other(
                    "injected reply-store postrename failure",
                )),
            });
        }
        self.sync_pending_directory()
    }

    fn sync_pending_directory(&mut self) -> Result<(), ReplyStoreError> {
        if !self.directory_sync_pending {
            return Ok(());
        }
        #[cfg(test)]
        if self.fail_directory_sync {
            return Err(ReplyStoreError {
                kind: ReplyStoreErrorKind::DurabilityUncertain(io::Error::other(
                    "injected reply-store parent sync failure",
                )),
            });
        }
        let parent = self.path.parent().ok_or(ReplyStoreError {
            kind: ReplyStoreErrorKind::InvalidRecord(InvalidRecordKind::MissingDirectory),
        })?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| ReplyStoreError {
                kind: ReplyStoreErrorKind::DurabilityUncertain(source),
            })?;
        self.directory_sync_pending = false;
        Ok(())
    }

    fn validate_state(&self) -> Result<(), ReplyStoreError> {
        if self.state.schema_version != SCHEMA_VERSION {
            return Err(ReplyStoreError {
                kind: ReplyStoreErrorKind::InvalidRecord(InvalidRecordKind::SchemaVersion),
            });
        }
        if self.state.pending.len() > MAX_PENDING {
            return Err(ReplyStoreError {
                kind: ReplyStoreErrorKind::InvalidRecord(InvalidRecordKind::StateTooLarge),
            });
        }
        for (handle, authority) in &self.state.pending {
            authority.validate(&self.policy)?;
            if serde_json::to_vec(&(handle, authority))?.len() > MAX_RECORD_BYTES {
                return Err(ReplyStoreError {
                    kind: ReplyStoreErrorKind::InvalidRecord(InvalidRecordKind::RecordTooLarge),
                });
            }
        }
        Ok(())
    }
}

fn read_no_follow(path: &Utf8Path) -> Result<Vec<u8>, ReplyStoreError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o777 != 0o600
        || metadata.nlink() != 1
        || metadata.len() > MAX_STORE_BYTES as u64
    {
        return Err(ReplyStoreError {
            kind: ReplyStoreErrorKind::InvalidRecord(InvalidRecordKind::UnsafeSource),
        });
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = options.open(path)?;
    let opened = file.metadata()?;
    if !opened.file_type().is_file()
        || opened.uid() != metadata.uid()
        || opened.ino() != metadata.ino()
        || opened.nlink() != 1
        || opened.len() > MAX_STORE_BYTES as u64
    {
        return Err(ReplyStoreError {
            kind: ReplyStoreErrorKind::InvalidRecord(InvalidRecordKind::SourceChanged),
        });
    }
    let mut bytes = Vec::with_capacity(opened.len() as usize);
    file.take((MAX_STORE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_STORE_BYTES {
        return Err(ReplyStoreError {
            kind: ReplyStoreErrorKind::InvalidRecord(InvalidRecordKind::FileTooLarge),
        });
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::teams::ChannelPolicy;
    use std::collections::{BTreeMap, BTreeSet};
    use tempfile::TempDir;

    fn state_dir(temp: &TempDir) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(temp.path().to_owned()).expect("UTF-8 state path")
    }

    fn policy() -> AdmissionPolicy {
        AdmissionPolicy {
            app_id: "app".parse().unwrap(),
            tenant_id: "tenant".parse().unwrap(),
            allowed_conversations: BTreeSet::from(["conversation".parse().unwrap()]),
            allowed_service_hosts: BTreeSet::from(["connector.test".to_owned()]),
            channels: BTreeMap::from([(
                "msteams".to_owned(),
                ChannelPolicy {
                    requires_key_endorsement: true,
                },
            )]),
        }
    }

    fn authority(now: DateTime<Utc>) -> SealedReplyAuthority {
        let envelope = AuthenticatedTeamsEnvelope {
            reference: AuthenticatedConversationReference {
                service_url: "https://connector.test/".parse().unwrap(),
                channel_id: "msteams".to_owned(),
                conversation_id: "conversation".parse().unwrap(),
                incoming_activity_id: "activity".parse().unwrap(),
                bot_id: "bot".parse().unwrap(),
                sender_id: "sender".parse().unwrap(),
                dione_authorized_tenant_id: "tenant".parse().unwrap(),
            },
            text: "never persisted".to_owned(),
        };
        SealedReplyAuthority::seal(
            &envelope,
            &"app".parse().unwrap(),
            now + chrono::TimeDelta::minutes(15),
        )
    }

    fn handle() -> ReplyHandle {
        ReplyHandle::generate()
    }

    #[test]
    fn authority_store_excludes_independent_owners_before_loading_snapshot() {
        let temp = TempDir::new().unwrap();
        let directory = state_dir(&temp);
        let first = TeamsReplyStore::load(&directory, policy()).unwrap();
        // Invalid snapshot bytes must not be inspected by a competing owner.
        let snapshot = directory.join(FILE_NAME);
        std::fs::write(&snapshot, b"private malformed snapshot").unwrap();
        std::fs::set_permissions(&snapshot, std::fs::Permissions::from_mode(0o600)).unwrap();
        let second = TeamsReplyStore::load(&directory, policy());
        assert!(
            second.is_err(),
            "two authority owners opened the same directory"
        );
        let error = second.err().unwrap();
        let mut source = std::error::Error::source(&error);
        let mut contended = false;
        while let Some(cause) = source {
            contended |= cause
                .downcast_ref::<io::Error>()
                .is_some_and(|source| source.kind() == io::ErrorKind::WouldBlock);
            source = cause.source();
        }
        assert!(
            contended,
            "the authority lock must refuse ownership before decoding the snapshot: {error:?}"
        );
        drop(first);
        std::fs::remove_file(snapshot).unwrap();
        assert!(TeamsReplyStore::load(&directory, policy()).is_ok());
    }

    #[test]
    fn authority_store_lock_preserves_single_use_handle_across_reopen() {
        let temp = TempDir::new().unwrap();
        let directory = state_dir(&temp);
        let now = Utc::now();
        let handle = handle();
        let mut first = TeamsReplyStore::load(&directory, policy()).unwrap();
        first.insert(handle.clone(), authority(now), now).unwrap();
        assert!(
            TeamsReplyStore::load(&directory, policy()).is_err(),
            "unused authority must have one owner"
        );
        drop(first);
        let mut reopened = TeamsReplyStore::load(&directory, policy()).unwrap();
        assert!(reopened.take(&handle, now).is_ok());
        drop(reopened);
        let mut consumed = TeamsReplyStore::load(&directory, policy()).unwrap();
        assert!(matches!(
            consumed.take(&handle, now).unwrap_err().kind,
            ReplyStoreErrorKind::UnknownHandle
        ));
    }

    #[tokio::test]
    async fn offload_failures_preserve_task_and_semaphore_causes() {
        let temp = TempDir::new().unwrap();
        let store = AsyncTeamsReplyStore::load(&state_dir(&temp), policy())
            .await
            .unwrap();
        let error = store
            .run::<()>(|_| panic!("private panic sentinel"))
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "reply store offload task failed");
        assert!(matches!(error.kind, ReplyStoreErrorKind::Task(source) if source.is_panic()));
        let poisoned = store.run(|_| Ok(())).await.unwrap_err();
        assert!(matches!(poisoned.kind, ReplyStoreErrorKind::LockPoisoned));
        let temp = TempDir::new().unwrap();
        let healthy = AsyncTeamsReplyStore::load(&state_dir(&temp), policy())
            .await
            .unwrap();
        assert!(healthy.run(|_| Ok(())).await.is_ok());
        healthy.io_slot.close();
        let closed = healthy.run(|_| Ok(())).await.unwrap_err();
        assert!(matches!(closed.kind, ReplyStoreErrorKind::OffloadClosed(_)));
    }

    #[test]
    fn malformed_stored_coordinates_preserve_paths_and_url_causes() {
        let temp = TempDir::new().unwrap();
        let path = state_dir(&temp);
        let file = path.join(FILE_NAME);
        let fixture = include_str!("../../tests/fixtures/teams/reply-authorities-v1.json");
        std::fs::write(&file, fixture.replace("\"sender\"", "\"\"")).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let error = TeamsReplyStore::load(&path, policy()).err().unwrap();
        assert_eq!(error.to_string(), "teams reply authority JSON failed");
        assert!(
            matches!(error.kind, ReplyStoreErrorKind::Json(source) if source.path().to_string().ends_with(".sender_id"))
        );
        let mut sealed = authority(Utc::now());
        sealed.service_url = "private-url-sentinel".into();
        let error = sealed.open(&policy()).unwrap_err();
        assert_eq!(error.to_string(), "invalid stored Teams service URL");
        assert!(matches!(
            error.kind,
            ReplyStoreErrorKind::ServiceUrl(url::ParseError::RelativeUrlWithoutBase)
        ));
        assert!(authority(Utc::now()).open(&policy()).is_ok());
    }

    #[test]
    fn literal_v1_handles_restore_exact_spelling_and_consume_once() {
        let temp = TempDir::new().unwrap();
        let path = state_dir(&temp);
        let fixture = include_bytes!("../../tests/fixtures/teams/reply-authorities-v1.json");
        let file = path.join(FILE_NAME);
        std::fs::write(&file, fixture).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let now = Utc::now();
        let handle = ReplyHandle::parse("teams-ABCDEF0123456789ABCDEF0123456789").unwrap();
        let mut store = TeamsReplyStore::load(&path, policy()).unwrap();
        assert_eq!(
            store.find_handle(&authority(now).event_key, now).unwrap(),
            Some(handle.clone())
        );
        let encoded = serde_json::to_value(&store.state).unwrap();
        assert_eq!(
            encoded,
            serde_json::from_slice::<serde_json::Value>(fixture).unwrap()
        );
        assert!(matches!(
            store.take(&handle, now).unwrap(),
            StoreReceipt::Persisted(_)
        ));
        drop(store);
        let mut restored = TeamsReplyStore::load(&path, policy()).unwrap();
        assert!(matches!(
            restored.take(&handle, now),
            Err(ReplyStoreError {
                kind: ReplyStoreErrorKind::UnknownHandle
            })
        ));
    }

    #[test]
    fn persisted_malformed_handle_is_rejected_before_authority_is_exposed() {
        let temp = TempDir::new().unwrap();
        let path = state_dir(&temp);
        let fixture = include_str!("../../tests/fixtures/teams/reply-authorities-v1.json");
        let file = path.join(FILE_NAME);
        std::fs::write(
            &file,
            fixture.replace("teams-ABCDEF0123456789ABCDEF0123456789", "teams-invalid"),
        )
        .unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            TeamsReplyStore::load(&path, policy()),
            Err(ReplyStoreError {
                kind: ReplyStoreErrorKind::Json(_)
            })
        ));
    }

    #[test]
    fn literal_v1_identity_fields_reject_malformed_values_without_rewriting_store() {
        let fixture = include_str!("../../tests/fixtures/teams/reply-authorities-v1.json");
        let prefix = "/pending/teams-ABCDEF0123456789ABCDEF0123456789";
        for field in [
            "event_key/tenant_id",
            "event_key/bot_id",
            "event_key/conversation_id",
            "event_key/activity_id",
            "sender_id",
            "app_id",
        ] {
            for invalid in [String::new(), "bad\n".to_owned(), "x".repeat(513)] {
                let temp = TempDir::new().unwrap();
                let path = state_dir(&temp);
                let file = path.join(FILE_NAME);
                let mut value: serde_json::Value = serde_json::from_str(fixture).unwrap();
                *value.pointer_mut(&format!("{prefix}/{field}")).unwrap() =
                    serde_json::json!(invalid);
                let bytes = serde_json::to_vec(&value).unwrap();
                std::fs::write(&file, &bytes).unwrap();
                std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
                assert!(
                    matches!(
                        TeamsReplyStore::load(&path, policy()),
                        Err(ReplyStoreError {
                            kind: ReplyStoreErrorKind::Json(_)
                        })
                    ),
                    "{field}"
                );
                assert_eq!(std::fs::read(file).unwrap(), bytes);
            }
        }
    }

    #[test]
    fn restored_authority_requires_current_conversation_scope() {
        let temp = TempDir::new().unwrap();
        let dir = state_dir(&temp);
        let now = Utc::now();
        let handle = handle();
        {
            let mut store = TeamsReplyStore::load(&dir, policy()).unwrap();
            assert!(matches!(
                store.insert(handle.clone(), authority(now), now),
                Ok(StoreReceipt::Persisted(()))
            ));
        }
        let bytes = std::fs::read(dir.join(FILE_NAME)).unwrap();
        let mut restored = TeamsReplyStore::load(&dir, policy()).unwrap();
        let authority = restored.get(&handle, now).unwrap();
        assert_eq!(
            authority
                .open(&policy())
                .unwrap()
                .reference
                .conversation_id
                .as_str(),
            "conversation"
        );
        let mut narrowed = policy();
        narrowed.allowed_conversations = BTreeSet::from(["other/room".parse().unwrap()]);
        assert!(matches!(
            authority.open(&narrowed),
            Err(ReplyStoreError {
                kind: ReplyStoreErrorKind::InvalidRecord(InvalidRecordKind::PolicyMismatch)
            })
        ));
        drop(restored);
        assert!(matches!(
            TeamsReplyStore::load(&dir, narrowed),
            Err(ReplyStoreError {
                kind: ReplyStoreErrorKind::InvalidRecord(InvalidRecordKind::PolicyMismatch)
            })
        ));
        assert_eq!(std::fs::read(dir.join(FILE_NAME)).unwrap(), bytes);
    }

    #[test]
    fn sealed_authority_survives_restart_and_consumption_does_not() {
        let temp = TempDir::new().expect("create state directory");
        let dir = state_dir(&temp);
        let now = Utc::now();
        let handle = handle();
        {
            let mut store = TeamsReplyStore::load(&dir, policy()).expect("load empty store");
            assert!(matches!(
                store.insert(handle.clone(), authority(now), now),
                Ok(StoreReceipt::Persisted(()))
            ));
        }
        let path = dir.join(FILE_NAME);
        assert_eq!(
            std::fs::metadata(&path)
                .expect("store metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let bytes = std::fs::read(&path).expect("read store bytes");
        assert!(
            !bytes
                .windows(b"never persisted".len())
                .any(|window| window == b"never persisted")
        );
        {
            let mut store = TeamsReplyStore::load(&dir, policy()).expect("reload authority");
            let restored = store.get(&handle, now).expect("same handle available");
            let envelope = restored.open(&policy()).expect("open under current policy");
            assert_eq!(envelope.reference.incoming_activity_id.as_str(), "activity");
            assert!(matches!(
                store.take(&handle, now),
                Ok(StoreReceipt::Persisted(_))
            ));
        }
        let mut store = TeamsReplyStore::load(&dir, policy()).expect("reload consumed store");
        assert!(matches!(
            store.get(&handle, now),
            Err(ReplyStoreError {
                kind: ReplyStoreErrorKind::UnknownHandle
            })
        ));
    }

    #[test]
    fn precommit_failure_rolls_back_and_postrename_failure_remains_visible() {
        let temp = TempDir::new().expect("create state directory");
        let dir = state_dir(&temp);
        let now = Utc::now();
        let rejected = handle();
        let retained = handle();
        let mut store = TeamsReplyStore::load(&dir, policy()).expect("load empty store");
        store.persist_failure = Some(PersistFailure::BeforeRename);
        assert!(matches!(
            store.insert(rejected.clone(), authority(now), now),
            Err(ReplyStoreError { kind: ReplyStoreErrorKind::Io(source) }) if source.kind() == io::ErrorKind::Other
        ));
        assert!(matches!(
            store.get(&rejected, now),
            Err(ReplyStoreError {
                kind: ReplyStoreErrorKind::UnknownHandle
            })
        ));

        store.persist_failure = Some(PersistFailure::AfterRename);
        assert!(matches!(
            store.insert(retained.clone(), authority(now), now),
            Ok(StoreReceipt::VisibleDurabilityUncertain((), _))
        ));
        store.fail_directory_sync = true;
        assert!(matches!(
            store.get(&retained, now),
            Err(ReplyStoreError { kind: ReplyStoreErrorKind::DurabilityUncertain(source) }) if source.kind() == io::ErrorKind::Other
        ));
        assert_eq!(store.state.pending.len(), 1);
        store.fail_directory_sync = false;
        assert!(store.get(&retained, now).is_ok());
        drop(store);

        let mut reloaded = TeamsReplyStore::load(&dir, policy()).expect("reload visible rename");
        assert!(reloaded.get(&retained, now).is_ok());
        assert!(matches!(
            reloaded.get(&rejected, now),
            Err(ReplyStoreError {
                kind: ReplyStoreErrorKind::UnknownHandle
            })
        ));
    }

    #[test]
    fn replayed_file_under_other_app_or_host_is_rejected() {
        let temp = TempDir::new().expect("create state directory");
        let dir = state_dir(&temp);
        let now = Utc::now();
        let mut store = TeamsReplyStore::load(&dir, policy()).expect("load empty store");
        assert!(matches!(
            store.insert(handle(), authority(now), now),
            Ok(StoreReceipt::Persisted(()))
        ));
        drop(store);

        let mut other_app = policy();
        other_app.app_id = "other-app".parse().unwrap();
        assert!(matches!(
            TeamsReplyStore::load(&dir, other_app),
            Err(ReplyStoreError {
                kind: ReplyStoreErrorKind::InvalidRecord(_)
            })
        ));
        let mut other_host = policy();
        other_host.allowed_service_hosts.clear();
        assert!(matches!(
            TeamsReplyStore::load(&dir, other_host),
            Err(ReplyStoreError {
                kind: ReplyStoreErrorKind::InvalidRecord(_)
            })
        ));
    }

    #[test]
    fn same_authenticated_activity_cannot_mint_two_reply_handles() {
        let temp = TempDir::new().expect("create state directory");
        let dir = state_dir(&temp);
        let now = Utc::now();
        let first = handle();
        let second = handle();
        let mut store = TeamsReplyStore::load(&dir, policy()).expect("load empty store");
        assert!(matches!(
            store.insert(first.clone(), authority(now), now),
            Ok(StoreReceipt::Persisted(()))
        ));
        assert!(matches!(
            store.insert(second.clone(), authority(now), now),
            Err(ReplyStoreError {
                kind: ReplyStoreErrorKind::InvalidRecord(_)
            })
        ));
        drop(store);
        let mut restored = TeamsReplyStore::load(&dir, policy()).expect("reload authority");
        assert!(restored.get(&first, now).is_ok());
        assert!(matches!(
            restored.get(&second, now),
            Err(ReplyStoreError {
                kind: ReplyStoreErrorKind::UnknownHandle
            })
        ));
    }

    #[test]
    fn wrong_mode_and_symlink_file_are_rejected_before_loading() {
        let temp = TempDir::new().expect("create state directory");
        let dir = state_dir(&temp);
        let now = Utc::now();
        let mut store = TeamsReplyStore::load(&dir, policy()).expect("load empty store");
        assert!(matches!(
            store.insert(handle(), authority(now), now),
            Ok(StoreReceipt::Persisted(()))
        ));
        drop(store);
        let path = dir.join(FILE_NAME);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("change fixture mode");
        assert!(matches!(
            TeamsReplyStore::load(&dir, policy()),
            Err(ReplyStoreError {
                kind: ReplyStoreErrorKind::InvalidRecord(InvalidRecordKind::UnsafeSource)
            })
        ));

        let target = dir.join("reply-target.json");
        std::fs::rename(&path, &target).expect("move fixture target");
        std::os::unix::fs::symlink(&target, &path).expect("create fixture symlink");
        assert!(matches!(
            TeamsReplyStore::load(&dir, policy()),
            Err(ReplyStoreError {
                kind: ReplyStoreErrorKind::InvalidRecord(InvalidRecordKind::UnsafeSource)
            })
        ));
    }

    #[tokio::test]
    async fn cancelled_caller_does_not_cancel_started_store_commit_or_reorder_read() {
        let temp = TempDir::new().expect("create state directory");
        let dir = state_dir(&temp);
        let now = Utc::now();
        let handle = handle();
        let store = AsyncTeamsReplyStore::load(&dir, policy())
            .await
            .expect("load store off runtime");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let writer = tokio::spawn({
            let store = store.clone();
            let handle = handle.clone();
            async move {
                store
                    .run(move |inner| {
                        started_tx.send(()).expect("signal started store operation");
                        release_rx.recv().expect("release store operation");
                        inner.insert(handle, authority(now), now)
                    })
                    .await
            }
        });
        started_rx
            .await
            .expect("store operation started on blocking lane");
        writer.abort();
        assert!(
            writer
                .await
                .expect_err("caller task aborted")
                .is_cancelled()
        );
        let mut reader = tokio::spawn({
            let store = store.clone();
            let handle = handle.clone();
            async move { store.get(handle, now).await }
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut reader)
                .await
                .is_err(),
            "read must wait for the started commit"
        );
        release_tx.send(()).expect("release blocking lane");
        assert!(reader.await.expect("reader task completed").is_ok());
        assert!(store.get(handle, now).await.is_ok());
    }
}
