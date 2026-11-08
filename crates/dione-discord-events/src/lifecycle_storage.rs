//! Bounded lifecycle metadata storage. Retained records grant no admission authority.
//!
//! Callers retain their private lineage and current policy checks inside a single
//! borrowed storage transaction. This module never compares or constructs lineage.

use serenity::model::{Timestamp, id::{ChannelId, MessageId, UserId}};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Mutex, time::{Duration, Instant}};

/// Default active-record retention.
pub const ACTIVE_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Default terminal tombstone retention.
pub const TOMBSTONE_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// Retention and capacity settings for ordinary lifecycle metadata.
#[derive(Debug, Clone, Copy)]
pub struct StorageLimits {
    active_retention: Duration,
    tombstone_retention: Duration,
    max_active: usize,
    max_tombstones: usize,
}
impl Default for StorageLimits {
    fn default() -> Self {
        Self { active_retention: ACTIVE_RETENTION, tombstone_retention: TOMBSTONE_RETENTION,
            max_active: 16_384, max_tombstones: 16_384 }
    }
}
impl StorageLimits {
    /// Sets the independent record and tombstone capacity limits.
    pub fn with_capacity(mut self, active: usize, tombstones: usize) -> Self {
        self.max_active = active;
        self.max_tombstones = tombstones;
        self
    }
}

/// A storage operation could not safely complete.
#[derive(Debug, thiserror::Error)]
#[error("{kind}")]
pub struct StorageError {
    #[source]
    kind: StorageErrorKind,
}
impl StorageError {
    fn new(kind: StorageErrorKind) -> Self { Self { kind } }
    /// Borrows the failure category without exposing mutable error state.
    pub fn kind(&self) -> &StorageErrorKind { &self.kind }
}
/// Storage failure category, independent of application permission decisions.
#[derive(Debug, thiserror::Error)]
pub enum StorageErrorKind {
    /// The shared storage mutex was poisoned.
    #[error("lifecycle storage lock failed")]
    LockPoisoned,
    /// The monotonic admission order cannot advance.
    #[error("lifecycle storage admission order exhausted")]
    SequenceExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ContentHash([u8;32]);
fn hash_content(content: &str) -> ContentHash { ContentHash(Sha256::digest(content.as_bytes()).into()) }

struct Record<C, P> {
    channel_id: ChannelId,
    context: C,
    actor_id: UserId,
    thread_parent_id: Option<ChannelId>,
    content_hash: ContentHash,
    last_version: Timestamp,
    admission_order: u64,
    expires_at: Instant,
    payload: P,
}
struct Tombstone<C> {
    channel_id: ChannelId,
    context: C,
    lineage_confirmed: bool,
    terminal_at: Instant,
    expires_at: Instant,
}
impl<C: Copy + Eq> Tombstone<C> {
    fn binding_matches(&self, channel: ChannelId, context: C) -> bool {
        self.channel_id == channel && self.context == context
    }
}
struct State<C, P> {
    active: HashMap<MessageId, Record<C,P>>,
    tombstones: HashMap<MessageId, Tombstone<C>>,
    next_admission_order: u64,
}
impl<C, P> Default for State<C,P> {
    fn default() -> Self { Self { active: HashMap::new(), tombstones: HashMap::new(), next_admission_order: 1 } }
}

/// Serialized bounded storage with opaque caller-owned context and lineage.
///
/// No equality, clone or policy strategy is required for the lineage payload.
pub struct LifecycleStorage<C, P> {
    state: Mutex<State<C,P>>,
    limits: StorageLimits,
}
impl<C: Copy + Eq, P> LifecycleStorage<C,P> {
    /// Starts empty storage without reading a clock or constructing application proof.
    pub fn new(limits: StorageLimits) -> Self {
        Self { state: Mutex::new(State::default()), limits }
    }
    /// Runs a synchronous storage operation under the sole state mutex.
    ///
    /// The fixed result type cannot borrow a record or guard from this operation.
    pub fn with_locked<R>(&self,
        operation: impl for<'a> FnOnce(&mut LockedLedger<'a,C,P>) -> R,
    ) -> Result<R,StorageError> {
        let mut state = self.state.lock().map_err(|_| StorageError::new(StorageErrorKind::LockPoisoned))?;
        let mut locked = LockedLedger { state: &mut state, limits: &self.limits };
        Ok(operation(&mut locked))
    }
}

/// Borrowed storage operations while the sole mutex remains held.
pub struct LockedLedger<'a,C,P> { state: &'a mut State<C,P>, limits: &'a StorageLimits }
impl<C: Copy + Eq, P> LockedLedger<'_,C,P> {
    /// Sweeps expired metadata once using the caller's correctly positioned clock sample.
    pub fn transitions(&mut self, now: Instant) -> Transition<'_,C,P> {
        cleanup(self.state,self.limits,now);
        Transition { state: self.state, limits: self.limits, now }
    }
    /// Inspects retained metadata without cleanup or any claim of current authority.
    pub fn inspect_retained(&self, id: MessageId) -> Option<RecordView<'_,C,P>> {
        self.state.active.get(&id).map(|record| RecordView { record })
    }
    /// Applies only the requested record's legacy expiry check, without a global sweep.
    pub fn verify_target(&mut self, now: Instant, id: MessageId, claimed_channel: ChannelId) -> Verification {
        if self.state.active.get(&id).is_some_and(|record| record.expires_at <= now) {
            if let Some(record) = self.state.active.remove(&id) {
                insert_tombstone(self.state,self.limits,id,record.channel_id,record.context,true,record.expires_at,now);
            }
            Verification::Expired
        } else {
            match self.state.active.get(&id) {
                None => Verification::Unknown,
                Some(record) if record.channel_id == claimed_channel => Verification::Found(record.channel_id),
                Some(record) => Verification::ChannelMismatch(record.channel_id),
            }
        }
    }
}

/// Ordinary target lookup metadata, never a permission verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verification {
    /// No active record exists.
    Unknown,
    /// The requested active record expired during this lookup.
    Expired,
    /// The requested channel matches the retained record.
    Found(ChannelId),
    /// The record belongs to another channel.
    ChannelMismatch(ChannelId),
}

/// Transition operations after cleanup, borrowing the same held storage mutex.
pub struct Transition<'a,C,P> { state: &'a mut State<C,P>, limits: &'a StorageLimits, now: Instant }
impl<C: Copy + Eq, P> Transition<'_,C,P> {
    /// Resolves the existing tombstone and record before a possible insertion.
    pub fn create_entry(&mut self,id:MessageId,channel:ChannelId,context:C,content:&str,version:Timestamp) -> CreateEntry<'_,C,P> {
        if let Some(tombstone)=self.state.tombstones.get(&id) {
            if tombstone.binding_matches(channel,context) || tombstone.lineage_confirmed { return CreateEntry::Blocked; }
            self.state.tombstones.remove(&id);
        }
        let content_hash=hash_content(content);
        if self.state.active.contains_key(&id) {
            // This map is private and the mutex is held; the entry cannot disappear.
            let record=&self.state.active[&id];
            return CreateEntry::Existing { record: RecordView { record },
                content_and_version_match: record.content_hash==content_hash && record.last_version==version };
        }
        CreateEntry::Vacant(VacantCreate { state:self.state,limits:self.limits,now:self.now,id,channel,context,content_hash,version })
    }
    /// Borrows the real record for caller-private binding and policy checks before applying an edit.
    pub fn edit_entry(&mut self,id:MessageId) -> Option<EditEntry<'_,C,P>> {
        self.state.active.get_mut(&id).map(|record| EditEntry { record })
    }
    /// Removes one exact-bound record or records an unknown deletion tombstone.
    pub fn delete(&mut self,id:MessageId,channel:ChannelId,context:C) -> DeleteOutcome<C,P> {
        let Some(record)=self.state.active.get(&id) else {
            if let Some(tombstone)=self.state.tombstones.get(&id) {
                return if tombstone.binding_matches(channel,context) { DeleteOutcome::Duplicate } else { DeleteOutcome::Rejected };
            }
            insert_tombstone(self.state,self.limits,id,channel,context,false,self.now,self.now);
            return DeleteOutcome::Rejected;
        };
        if record.channel_id!=channel || record.context!=context { return DeleteOutcome::Rejected; }
        match self.state.active.remove(&id) {
            Some(record) => {
                insert_tombstone(self.state,self.limits,id,record.channel_id,record.context,true,self.now,self.now);
                DeleteOutcome::Removed(RemovedRecord { record })
            }
            None => DeleteOutcome::Unavailable,
        }
    }
    /// Looks up cleaned metadata, retaining the distinction between terminal and unknown.
    pub fn lookup(&self,id:MessageId) -> Lookup<'_,C,P> {
        if self.state.tombstones.contains_key(&id) { Lookup::Terminal }
        else { self.state.active.get(&id).map_or(Lookup::Missing,|record| Lookup::Active(RecordView { record })) }
    }
}

/// Existing metadata or a private insertion slot. No variant grants admission authority.
pub enum CreateEntry<'a,C,P> {
    /// A tombstone blocks the proposed create.
    Blocked,
    /// The caller must compare its own binding and opaque lineage.
    Existing {
        /// The actual retained record under the held mutex.
        record: RecordView<'a,C,P>,
        /// Exact content hash and timestamp comparison only.
        content_and_version_match: bool,
    },
    /// A slot that alone can allocate order and perform capacity eviction/insertion.
    Vacant(VacantCreate<'a,C,P>),
}
/// A private-state insertion opportunity under the original transaction borrow.
pub struct VacantCreate<'a,C,P> {
    state:&'a mut State<C,P>,limits:&'a StorageLimits,now:Instant,id:MessageId,
    channel:ChannelId,context:C,content_hash:ContentHash,version:Timestamp,
}
impl<'a,C:Copy+Eq,P> VacantCreate<'a,C,P> {
    /// Checks sequence exhaustion before eviction, then inserts ordinary metadata.
    pub fn insert(self,actor:UserId,parent:Option<ChannelId>,payload:P) -> Result<RecordView<'a,C,P>,StorageError> {
        let next=self.state.next_admission_order.checked_add(1)
            .ok_or_else(||StorageError::new(StorageErrorKind::SequenceExhausted))?;
        let admission_order=self.state.next_admission_order;
        self.state.next_admission_order=next;
        if self.state.active.len()>=self.limits.max_active
            && let Some(oldest)=self.state.active.iter().min_by_key(|(_,r)|r.admission_order).map(|(id,_)|*id)
            && let Some(record)=self.state.active.remove(&oldest) {
                insert_tombstone(self.state,self.limits,oldest,record.channel_id,record.context,true,self.now,self.now);
        }
        self.state.active.insert(self.id,Record { channel_id:self.channel,context:self.context,actor_id:actor,
            thread_parent_id:parent,content_hash:self.content_hash,last_version:self.version,
            admission_order,expires_at:self.now+self.limits.active_retention,payload });
        Ok(RecordView { record:&self.state.active[&self.id] })
    }
}
/// A borrowed record that keeps caller validation and edit mutation in one critical section.
pub struct EditEntry<'a,C,P> { record:&'a mut Record<C,P> }
impl<'a,C:Copy+Eq,P> EditEntry<'a,C,P> {
    /// Borrows immutable metadata for the caller's private policy predicate.
    pub fn record(&self) -> RecordView<'_,C,P> { RecordView { record:self.record } }
    /// Applies the original timestamp/hash ordering; duplicate content never replaces payload.
    pub fn apply(self,content:&str,version:Timestamp,payload:PayloadChange<P>) -> EditOutcome<'a,C,P> {
        let content_hash=hash_content(content);
        if version<self.record.last_version { return EditOutcome::Rejected; }
        if version==self.record.last_version {
            return if content_hash==self.record.content_hash { EditOutcome::Duplicate } else { EditOutcome::Rejected };
        }
        if content_hash==self.record.content_hash {
            self.record.last_version=version;
            return EditOutcome::Duplicate;
        }
        self.record.last_version=version;
        self.record.content_hash=content_hash;
        if let PayloadChange::Replace(payload)=payload { self.record.payload=payload; }
        EditOutcome::Changed(RecordView { record:self.record })
    }
}
/// Describes ordinary payload bookkeeping after caller-private validation.
pub enum PayloadChange<P> {
    /// Retain the existing payload.
    Keep,
    /// Replace payload only for a newer changed-content edit.
    Replace(P),
}
/// Storage edit result, not an application admission verdict.
pub enum EditOutcome<'a,C,P> {
    /// Timestamp/content conflict rejected the edit.
    Rejected,
    /// No new content was accepted.
    Duplicate,
    /// Content metadata changed under the held borrow.
    Changed(RecordView<'a,C,P>),
}
/// Storage deletion result, not a delivery proof.
pub enum DeleteOutcome<C,P> {
    /// Binding or absence prevented removal.
    Rejected,
    /// The same deletion was already observed.
    Duplicate,
    /// The expected record unexpectedly disappeared.
    Unavailable,
    /// The actual removed record, without cloning its payload.
    Removed(RemovedRecord<C,P>),
}
/// Cleaned metadata lookup result.
pub enum Lookup<'a,C,P> {
    /// No active record or tombstone exists.
    Missing,
    /// A terminal tombstone exists.
    Terminal,
    /// The actual retained record remains borrowed under the mutex.
    Active(RecordView<'a,C,P>),
}
/// The actual record moved out by a successful deletion.
pub struct RemovedRecord<C,P> { record:Record<C,P> }
impl<C:Copy+Eq,P> RemovedRecord<C,P> {
    /// Borrows removed metadata without constructing application proof.
    pub fn view(&self) -> RecordView<'_,C,P> { RecordView { record:&self.record } }
}
/// Immutable borrowed storage metadata, with no authority or freshness guarantee.
pub struct RecordView<'a,C,P> { record:&'a Record<C,P> }
impl<C:Copy+Eq,P> RecordView<'_,C,P> {
    /// Returns the stored channel.
    pub fn channel_id(&self) -> ChannelId { self.record.channel_id }
    /// Returns ordinary caller context metadata.
    pub fn context(&self) -> C { self.record.context }
    /// Returns the observed actor identifier.
    pub fn actor_id(&self) -> UserId { self.record.actor_id }
    /// Returns the retained thread parent coordinate.
    pub fn thread_parent_id(&self) -> Option<ChannelId> { self.record.thread_parent_id }
    /// Borrows opaque caller-owned lineage without comparison or cloning.
    pub fn payload(&self) -> &P { &self.record.payload }
    /// Compares exact lowercase SHA256 spelling; this is not a permission check.
    pub fn matches_hash_hex(&self,expected:&str) -> bool {
        const HEX:&[u8;16]=b"0123456789abcdef";
        expected.len()==64 && self.record.content_hash.0.iter().zip(expected.as_bytes().as_chunks::<2>().0)
            .all(|(byte,pair)|pair[0]==HEX[(byte>>4) as usize] && pair[1]==HEX[(byte&15) as usize])
    }
}

fn cleanup<C:Copy+Eq,P>(state:&mut State<C,P>,limits:&StorageLimits,now:Instant) {
    let expired:Vec<_>=state.active.iter().filter_map(|(id,record)|
        (record.expires_at<=now).then_some((*id,record.channel_id,record.context,record.expires_at))).collect();
    for (id,channel,context,terminal_at) in expired {
        state.active.remove(&id);
        insert_tombstone(state,limits,id,channel,context,true,terminal_at,now);
    }
    state.tombstones.retain(|_,t|t.expires_at>now);
}
#[expect(clippy::too_many_arguments,reason="tombstones retain exact terminal binding and timing")]
fn insert_tombstone<C:Copy+Eq,P>(state:&mut State<C,P>,limits:&StorageLimits,id:MessageId,
    channel:ChannelId,context:C,lineage_confirmed:bool,terminal_at:Instant,now:Instant) {
    let expires_at=terminal_at+limits.tombstone_retention;
    if expires_at<=now { return; }
    if state.tombstones.len()>=limits.max_tombstones
        && let Some(oldest)=state.tombstones.iter().min_by_key(|(_,t)|t.terminal_at).map(|(id,_)|*id) {
            state.tombstones.remove(&oldest);
    }
    state.tombstones.insert(id,Tombstone { channel_id:channel,context,lineage_confirmed,terminal_at,expires_at });
}
