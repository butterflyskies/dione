//! Bounded, owner-only persistence for authenticated Teams reply authority.

use crate::teams::{
    AdmissionPolicy, AuthenticatedConversationReference, AuthenticatedTeamsEnvelope,
};
use camino::{Utf8Path, Utf8PathBuf};
use chrono::{DateTime, Utc};
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

const FILE_NAME: &str = "teams-reply-authorities.json";
const SCHEMA_VERSION: u32 = 1;
const MAX_PENDING: usize = 256;
const MAX_RECORD_BYTES: usize = 16 * 1024;
const MAX_STORE_BYTES: usize = MAX_PENDING * MAX_RECORD_BYTES * 2;

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub(crate) struct TeamsEventKey {
    pub(crate) tenant_id: String,
    pub(crate) bot_id: String,
    pub(crate) conversation_id: String,
    pub(crate) activity_id: String,
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

    fn validate(&self) -> Result<(), ReplyStoreError> {
        if [
            &self.tenant_id,
            &self.bot_id,
            &self.conversation_id,
            &self.activity_id,
        ]
        .into_iter()
        .any(|value| value.is_empty() || value.len() > 512 || value.chars().any(char::is_control))
        {
            return Err(ReplyStoreError::InvalidRecord("invalid Teams event key"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SealedReplyAuthority {
    pub(crate) event_key: TeamsEventKey,
    pub(crate) service_url: String,
    pub(crate) channel_id: String,
    pub(crate) sender_id: String,
    pub(crate) app_id: String,
    pub(crate) expires_at: DateTime<Utc>,
}

impl SealedReplyAuthority {
    pub(crate) fn seal(
        envelope: &AuthenticatedTeamsEnvelope,
        app_id: &str,
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
        let service_url = Url::parse(&self.service_url)
            .map_err(|_| ReplyStoreError::InvalidRecord("invalid Teams service URL"))?;
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
        self.event_key.validate()?;
        if self.app_id != policy.app_id
            || self.event_key.tenant_id != policy.tenant_id
            || !policy.channels.contains_key(&self.channel_id)
            || self.sender_id.is_empty()
            || self.sender_id.len() > 512
            || self.sender_id.chars().any(char::is_control)
        {
            return Err(ReplyStoreError::InvalidRecord(
                "Teams reply authority is outside current admission policy",
            ));
        }
        let url = Url::parse(&self.service_url)
            .map_err(|_| ReplyStoreError::InvalidRecord("invalid Teams service URL"))?;
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
            return Err(ReplyStoreError::InvalidRecord(
                "Teams reply service host is outside current admission policy",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReplyAuthorityState {
    schema_version: u32,
    pending: BTreeMap<String, SealedReplyAuthority>,
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
enum PersistFailure {
    BeforeWrite,
    BeforeRename,
    AfterRename,
}

#[derive(Debug, Error)]
pub(crate) enum ReplyStoreError {
    #[error("teams reply authority I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("teams reply authority JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("teams reply authority directory durability is uncertain: {0}")]
    DurabilityUncertain(io::Error),
    #[error("invalid Teams reply authority: {0}")]
    InvalidRecord(&'static str),
    #[error("teams reply registry is full")]
    Full,
    #[error("teams reply handle is unknown, expired or already consumed")]
    UnknownHandle,
}

pub(crate) struct TeamsReplyStore {
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
    ) -> Result<Option<String>, ReplyStoreError> {
        self.run(move |store| store.find_handle(&key, now)).await
    }

    pub(crate) async fn insert(
        &self,
        handle: String,
        authority: SealedReplyAuthority,
        now: DateTime<Utc>,
    ) -> Result<StoreReceipt<()>, ReplyStoreError> {
        self.run(move |store| store.insert(handle, authority, now))
            .await
    }

    pub(crate) async fn get(
        &self,
        handle: String,
        now: DateTime<Utc>,
    ) -> Result<SealedReplyAuthority, ReplyStoreError> {
        self.run(move |store| store.get(&handle, now)).await
    }

    pub(crate) async fn take(
        &self,
        handle: String,
        now: DateTime<Utc>,
    ) -> Result<StoreReceipt<SealedReplyAuthority>, ReplyStoreError> {
        self.run(move |store| store.take(&handle, now)).await
    }

    pub(crate) async fn discard(
        &self,
        handle: String,
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
            .map_err(|_| ReplyStoreError::Io(io::Error::other("reply store offload closed")))?;
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut store = inner
                .lock()
                .map_err(|_| ReplyStoreError::Io(io::Error::other("reply store lock poisoned")))?;
            operation(&mut store)
        })
        .await
        .map_err(join_error)?
    }
}

fn join_error(error: tokio::task::JoinError) -> ReplyStoreError {
    ReplyStoreError::Io(io::Error::other(format!(
        "reply store offload task failed: {error}"
    )))
}

impl TeamsReplyStore {
    pub(crate) fn load(
        state_dir: &Utf8Path,
        policy: AdmissionPolicy,
    ) -> Result<Self, ReplyStoreError> {
        std::fs::create_dir_all(state_dir)?;
        let path = state_dir.join(FILE_NAME);
        // A fresh create_new path cannot truncate a pre-existing hard link in the
        // state directory, even when a stale temporary file remains after a crash.
        let temporary_path = state_dir.join(format!("{FILE_NAME}.{}.tmp", Uuid::new_v4()));
        let (state, directory_sync_pending) = match read_no_follow(&path) {
            Ok(bytes) => (serde_json::from_slice(&bytes)?, true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                (ReplyAuthorityState::default(), false)
            }
            Err(error) => return Err(error.into()),
        };
        let mut store = Self {
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
                    return Err(ReplyStoreError::DurabilityUncertain(source));
                }
            }
        }
        Ok(store)
    }

    pub(crate) fn find_handle(
        &mut self,
        event_key: &TeamsEventKey,
        now: DateTime<Utc>,
    ) -> Result<Option<String>, ReplyStoreError> {
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
        handle: &str,
        now: DateTime<Utc>,
    ) -> Result<SealedReplyAuthority, ReplyStoreError> {
        self.sync_pending_directory()?;
        self.state
            .pending
            .get(handle)
            .filter(|authority| authority.expires_at > now)
            .cloned()
            .ok_or(ReplyStoreError::UnknownHandle)
    }

    pub(crate) fn insert(
        &mut self,
        handle: String,
        authority: SealedReplyAuthority,
        now: DateTime<Utc>,
    ) -> Result<StoreReceipt<()>, ReplyStoreError> {
        authority.validate(&self.policy)?;
        self.transaction(|state| {
            state
                .pending
                .retain(|_, authority| authority.expires_at > now);
            if state.pending.len() >= MAX_PENDING {
                return Err(ReplyStoreError::Full);
            }
            if state.pending.contains_key(&handle) {
                return Err(ReplyStoreError::InvalidRecord("reply handle collision"));
            }
            if state
                .pending
                .values()
                .any(|pending| pending.event_key == authority.event_key)
            {
                return Err(ReplyStoreError::InvalidRecord(
                    "Teams event already has reply authority",
                ));
            }
            state.pending.insert(handle, authority);
            Ok(())
        })
    }

    pub(crate) fn take(
        &mut self,
        handle: &str,
        now: DateTime<Utc>,
    ) -> Result<StoreReceipt<SealedReplyAuthority>, ReplyStoreError> {
        self.transaction(|state| {
            state
                .pending
                .retain(|_, authority| authority.expires_at > now);
            state
                .pending
                .remove(handle)
                .ok_or(ReplyStoreError::UnknownHandle)
        })
    }

    pub(crate) fn discard(
        &mut self,
        handle: &str,
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
            Err(ReplyStoreError::DurabilityUncertain(source)) => {
                Ok(StoreReceipt::VisibleDurabilityUncertain(value, source))
            }
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
            return Err(ReplyStoreError::InvalidRecord(
                "reply authority file exceeds its size limit",
            ));
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
            return Err(ReplyStoreError::DurabilityUncertain(io::Error::other(
                "injected reply-store postrename failure",
            )));
        }
        self.sync_pending_directory()
    }

    fn sync_pending_directory(&mut self) -> Result<(), ReplyStoreError> {
        if !self.directory_sync_pending {
            return Ok(());
        }
        #[cfg(test)]
        if self.fail_directory_sync {
            return Err(ReplyStoreError::DurabilityUncertain(io::Error::other(
                "injected reply-store parent sync failure",
            )));
        }
        let parent = self.path.parent().ok_or(ReplyStoreError::InvalidRecord(
            "reply store has no directory",
        ))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(ReplyStoreError::DurabilityUncertain)?;
        self.directory_sync_pending = false;
        Ok(())
    }

    fn validate_state(&self) -> Result<(), ReplyStoreError> {
        if self.state.schema_version != SCHEMA_VERSION || self.state.pending.len() > MAX_PENDING {
            return Err(ReplyStoreError::InvalidRecord(
                "unsupported or oversized reply authority state",
            ));
        }
        for (handle, authority) in &self.state.pending {
            let uuid = handle
                .strip_prefix("teams-")
                .ok_or(ReplyStoreError::InvalidRecord("invalid reply handle"))?;
            Uuid::parse_str(uuid)
                .map_err(|_| ReplyStoreError::InvalidRecord("invalid reply handle"))?;
            authority.validate(&self.policy)?;
            if serde_json::to_vec(&(handle, authority))?.len() > MAX_RECORD_BYTES {
                return Err(ReplyStoreError::InvalidRecord(
                    "reply authority record exceeds its size limit",
                ));
            }
        }
        Ok(())
    }
}

fn read_no_follow(path: &Utf8Path) -> io::Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o777 != 0o600
        || metadata.nlink() != 1
        || metadata.len() > MAX_STORE_BYTES as u64
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "reply authority source is not an owned bounded mode-0600 regular file",
        ));
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
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "reply authority file changed during open",
        ));
    }
    let mut bytes = Vec::with_capacity(opened.len() as usize);
    file.take((MAX_STORE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_STORE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "reply authority file exceeds its size limit",
        ));
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
            app_id: "app".to_owned(),
            tenant_id: "tenant".to_owned(),
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
                conversation_id: "conversation".to_owned(),
                incoming_activity_id: "activity".to_owned(),
                bot_id: "bot".to_owned(),
                sender_id: "sender".to_owned(),
                dione_authorized_tenant_id: "tenant".to_owned(),
            },
            text: "never persisted".to_owned(),
        };
        SealedReplyAuthority::seal(&envelope, "app", now + chrono::TimeDelta::minutes(15))
    }

    fn handle() -> String {
        format!("teams-{}", Uuid::new_v4())
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
            assert_eq!(envelope.reference.incoming_activity_id, "activity");
            assert!(matches!(
                store.take(&handle, now),
                Ok(StoreReceipt::Persisted(_))
            ));
        }
        let mut store = TeamsReplyStore::load(&dir, policy()).expect("reload consumed store");
        assert!(matches!(
            store.get(&handle, now),
            Err(ReplyStoreError::UnknownHandle)
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
            Err(ReplyStoreError::Io(_))
        ));
        assert!(matches!(
            store.get(&rejected, now),
            Err(ReplyStoreError::UnknownHandle)
        ));

        store.persist_failure = Some(PersistFailure::AfterRename);
        assert!(matches!(
            store.insert(retained.clone(), authority(now), now),
            Ok(StoreReceipt::VisibleDurabilityUncertain((), _))
        ));
        store.fail_directory_sync = true;
        assert!(matches!(
            store.get(&retained, now),
            Err(ReplyStoreError::DurabilityUncertain(_))
        ));
        assert_eq!(store.state.pending.len(), 1);
        store.fail_directory_sync = false;
        assert!(store.get(&retained, now).is_ok());
        drop(store);

        let mut reloaded = TeamsReplyStore::load(&dir, policy()).expect("reload visible rename");
        assert!(reloaded.get(&retained, now).is_ok());
        assert!(matches!(
            reloaded.get(&rejected, now),
            Err(ReplyStoreError::UnknownHandle)
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
        other_app.app_id = "other-app".to_owned();
        assert!(matches!(
            TeamsReplyStore::load(&dir, other_app),
            Err(ReplyStoreError::InvalidRecord(_))
        ));
        let mut other_host = policy();
        other_host.allowed_service_hosts.clear();
        assert!(matches!(
            TeamsReplyStore::load(&dir, other_host),
            Err(ReplyStoreError::InvalidRecord(_))
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
            Err(ReplyStoreError::InvalidRecord(_))
        ));
        drop(store);
        let mut restored = TeamsReplyStore::load(&dir, policy()).expect("reload authority");
        assert!(restored.get(&first, now).is_ok());
        assert!(matches!(
            restored.get(&second, now),
            Err(ReplyStoreError::UnknownHandle)
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
            Err(ReplyStoreError::Io(_))
        ));

        let target = dir.join("reply-target.json");
        std::fs::rename(&path, &target).expect("move fixture target");
        std::os::unix::fs::symlink(&target, &path).expect("create fixture symlink");
        assert!(matches!(
            TeamsReplyStore::load(&dir, policy()),
            Err(ReplyStoreError::Io(_))
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
