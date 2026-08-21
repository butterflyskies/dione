use crate::gaie::{CorpusId, Event, backfill::parse_or_migrate_checkpoint};
use camino::{Utf8Path, Utf8PathBuf};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{self, Read as _, Write as _},
};
use thiserror::Error;

const FORMAT_VERSION: &str = "2";

#[derive(Serialize)]
struct FormatHeader<'a> {
    event_kind: &'static str,
    format_version: &'static str,
    created_at: &'a str,
}

#[derive(Serialize)]
struct BatchCommit<'a> {
    event_kind: &'static str,
    message_id: &'a str,
    event_count: usize,
    batch_sha256: String,
    committed_at: &'a str,
}

/// The validated filesystem locations belonging to one corpus.
#[derive(Debug, Clone)]
pub struct ArchivePaths {
    data_dir: Utf8PathBuf,
    events: Utf8PathBuf,
    checkpoint: Utf8PathBuf,
    lock: Utf8PathBuf,
    attachments: Utf8PathBuf,
    origin_evidence: Utf8PathBuf,
    corpus_id: String,
}

impl ArchivePaths {
    /// Constructs corpus paths below an absolute, traversal-free data directory.
    pub fn new(data_dir: Utf8PathBuf, corpus: &CorpusId) -> Result<Self, ArchiveError> {
        if !data_dir.is_absolute()
            || data_dir
                .components()
                .any(|component| component.as_str() == "..")
        {
            return Err(ArchiveError::UnsafePath(data_dir));
        }
        let paths = Self {
            events: data_dir.join(format!("{}.ndjson", corpus.as_str())),
            checkpoint: data_dir.join(format!("{}.checkpoint.json", corpus.as_str())),
            lock: data_dir.join(format!("{}.lock", corpus.as_str())),
            attachments: data_dir.join("attachments"),
            origin_evidence: data_dir.join("origin-evidence"),
            corpus_id: corpus.as_str().to_owned(),
            data_dir,
        };
        Ok(paths)
    }
}

/// A durable incremental cursor for one verified capture stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct StreamCheckpoint {
    pub after_message_id: Option<String>,
}

/// A durable, root-bound set of independent capture-stream cursors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Checkpoint {
    pub version: u8,
    pub corpus_id: String,
    pub guild_id: String,
    pub parent_channel_id: String,
    pub streams: BTreeMap<String, StreamCheckpoint>,
    pub updated_at: String,
}

/// Committed archive contents and whether an incomplete tail was ignored.
#[derive(Debug)]
pub struct ReadResult {
    /// Structurally committed events whose origin evidence is absent or verified.
    pub events: Vec<Event>,
    /// Structurally committed events whose claimed origin evidence did not verify.
    /// These are never mixed into `events`; callers must opt in to handling them.
    pub quarantined_events: Vec<QuarantinedEvent>,
    pub torn_or_uncommitted_tail: bool,
    pub last_sequence: u64,
    committed_prefix: Vec<u8>,
}

/// A structurally committed event whose claimed origin evidence failed validation.
#[derive(Debug)]
pub struct QuarantinedEvent {
    pub event: Event,
    pub origin_error: String,
}

/// A stored byte-exact origin observation awaiting event-specific selectors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredOriginEvidence {
    pub(crate) sha256: String,
}

impl StoredOriginEvidence {
    pub(crate) fn location(&self) -> String {
        format!("origin-evidence/{}", self.sha256)
    }
}

/// Errors raised by archive validation or durable I/O.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ArchiveError {
    #[error("unsafe archive path `{0}`")]
    UnsafePath(Utf8PathBuf),
    #[error("another archive process owns `{0}`")]
    Locked(Utf8PathBuf),
    #[error("archive I/O failed for `{path}`")]
    Io {
        path: Utf8PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("archive JSON is invalid at physical line {line}")]
    Json {
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("archive integrity failure: {0}")]
    Integrity(String),
}

/// An exclusively locked format-v2 archive writer and reader.
pub struct Archive {
    paths: ArchivePaths,
    corpus: CorpusId,
    _lock: File,
    events: File,
    sequence: u64,
    event_ids: HashSet<String>,
    sync_directory: fn(&Utf8Path) -> Result<(), ArchiveError>,
}

impl Archive {
    /// Opens one corpus, recovering only a final torn or uncommitted tail.
    pub fn open(
        paths: ArchivePaths,
        corpus: CorpusId,
        created_at: &str,
    ) -> Result<Self, ArchiveError> {
        if paths.corpus_id != corpus.as_str() {
            return Err(ArchiveError::Integrity(
                "archive paths belong to another corpus".to_owned(),
            ));
        }
        create_hardened_dir(&paths.data_dir)?;
        create_hardened_dir(&paths.attachments)?;
        create_hardened_dir(&paths.origin_evidence)?;
        sync_directory(&paths.data_dir)?;
        reject_symlink(&paths.lock)?;
        reject_symlink(&paths.events)?;
        let mut lock_options = OpenOptions::new();
        lock_options
            .create(true)
            .read(true)
            .write(true)
            .truncate(false);
        #[cfg(unix)]
        lock_options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        let lock = lock_options
            .open(paths.lock.as_std_path())
            .map_err(|source| io_error(&paths.lock, source))?;
        harden_file(&lock, &paths.lock)?;
        lock.try_lock_exclusive()
            .map_err(|_| ArchiveError::Locked(paths.lock.clone()))?;
        let read = read_committed_structure_unlocked(&paths)?;
        if read.torn_or_uncommitted_tail {
            recover_committed_prefix(&paths, &read.committed_prefix)?;
        }
        let mut event_options = OpenOptions::new();
        event_options.create(true).append(true).read(true);
        #[cfg(unix)]
        event_options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        let mut events = event_options
            .open(paths.events.as_std_path())
            .map_err(|source| io_error(&paths.events, source))?;
        harden_file(&events, &paths.events)?;
        if events
            .metadata()
            .map_err(|source| io_error(&paths.events, source))?
            .len()
            == 0
        {
            let header = FormatHeader {
                event_kind: "format_header",
                format_version: FORMAT_VERSION,
                created_at,
            };
            writeln!(events, "{}", compact(&header)?)
                .and_then(|()| events.sync_all())
                .map_err(|source| io_error(&paths.events, source))?;
        }
        let event_ids = read
            .events
            .iter()
            .map(|event| event.event_id.clone())
            .collect();
        Ok(Self {
            paths,
            corpus,
            _lock: lock,
            events,
            sequence: read.last_sequence,
            event_ids,
            sync_directory,
        })
    }

    /// Returns the highest committed archive sequence.
    pub fn last_sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the hardened content-addressed attachment directory.
    pub fn attachments_dir(&self) -> &Utf8Path {
        &self.paths.attachments
    }

    /// Durably stores one exact HTTP response body in the origin-evidence CAS.
    pub(crate) fn store_origin_evidence(
        &self,
        bytes: &[u8],
    ) -> Result<StoredOriginEvidence, ArchiveError> {
        let digest = hex_sha256(bytes);
        let destination = self.paths.origin_evidence.join(&digest);
        validate_evidence_directory(&self.paths.origin_evidence)?;
        reject_symlink(&destination)?;
        if destination.exists() {
            let stored = read_regular_file(&destination)?;
            if stored != bytes || hex_sha256(&stored) != digest {
                return Err(ArchiveError::Integrity(
                    "origin-evidence object does not match its digest".to_owned(),
                ));
            }
            // Re-check immediately before fsync so a directory substitution between
            // the initial lookup and durability barrier fails closed.
            validate_evidence_directory(&self.paths.origin_evidence)?;
            (self.sync_directory)(&self.paths.origin_evidence)?;
            return Ok(StoredOriginEvidence {
                sha256: digest.clone(),
            });
        }

        let temporary = self
            .paths
            .origin_evidence
            .join(format!(".observation-{}", uuid::Uuid::new_v4().simple()));
        reject_symlink(&temporary)?;
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        let mut file = options
            .open(temporary.as_std_path())
            .map_err(|source| io_error(&temporary, source))?;
        if let Err(source) = file.write_all(bytes).and_then(|()| file.sync_all()) {
            let _ = fs::remove_file(temporary.as_std_path());
            return Err(io_error(&temporary, source));
        }
        let written = match fs::read(temporary.as_std_path()) {
            Ok(written) => written,
            Err(source) => {
                let _ = fs::remove_file(temporary.as_std_path());
                return Err(io_error(&temporary, source));
            }
        };
        if written != bytes || hex_sha256(&written) != digest {
            let _ = fs::remove_file(temporary.as_std_path());
            return Err(ArchiveError::Integrity(
                "origin-evidence write verification failed".to_owned(),
            ));
        }

        match fs::hard_link(temporary.as_std_path(), destination.as_std_path()) {
            Ok(()) => {
                fs::remove_file(temporary.as_std_path())
                    .map_err(|source| io_error(&temporary, source))?;
            }
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                fs::remove_file(temporary.as_std_path())
                    .map_err(|source| io_error(&temporary, source))?;
                let stored = read_regular_file(&destination)?;
                if stored != bytes || hex_sha256(&stored) != digest {
                    return Err(ArchiveError::Integrity(
                        "origin-evidence object does not match its digest".to_owned(),
                    ));
                }
            }
            Err(source) => {
                let _ = fs::remove_file(temporary.as_std_path());
                return Err(io_error(&destination, source));
            }
        }
        // Re-check immediately before fsync so a directory substitution during
        // publication cannot redirect the durability barrier.
        validate_evidence_directory(&self.paths.origin_evidence)?;
        (self.sync_directory)(&self.paths.origin_evidence)?;
        Ok(StoredOriginEvidence {
            sha256: digest.clone(),
        })
    }

    /// Appends and fsyncs one message-scoped event batch and its commit marker.
    pub fn append_batch(
        &mut self,
        events: &[Event],
        message_id: &str,
        committed_at: &str,
    ) -> Result<(), ArchiveError> {
        if events.is_empty()
            || events
                .iter()
                .any(|event| event.source.message_id != message_id)
        {
            return Err(ArchiveError::Integrity(
                "a batch must contain events for exactly one message".to_owned(),
            ));
        }
        let mut content = String::new();
        let mut previous = self.sequence;
        let mut batch_ids = HashSet::new();
        let has_origin_evidence = events.iter().any(|event| event.origin_evidence.is_some());
        if has_origin_evidence {
            validate_evidence_directory(&self.paths.origin_evidence)?;
        }
        let mut origin_cache = HashMap::new();
        for event in events {
            if event.corpus_id != self.corpus.as_str() {
                return Err(ArchiveError::Integrity(
                    "event corpus does not match archive".to_owned(),
                ));
            }
            if event.archive_seq <= previous {
                return Err(ArchiveError::Integrity(
                    "archive_seq must increase monotonically".to_owned(),
                ));
            }
            if self.event_ids.contains(&event.event_id) || !batch_ids.insert(event.event_id.clone())
            {
                return Err(ArchiveError::Integrity("duplicate event_id".to_owned()));
            }
            if event.origin_evidence.is_some() {
                validate_event_origin(&self.paths, event, &mut origin_cache)?;
            }
            previous = event.archive_seq;
            content.push_str(&compact(event)?);
            content.push('\n');
        }
        let hash = hex_sha256(content.as_bytes());
        let commit = BatchCommit {
            event_kind: "batch_commit",
            message_id,
            event_count: events.len(),
            batch_sha256: hash,
            committed_at,
        };
        let commit_line = compact(&commit)?;
        if has_origin_evidence {
            validate_evidence_directory(&self.paths.origin_evidence)?;
            (self.sync_directory)(&self.paths.origin_evidence)?;
        }
        self.events
            .write_all(content.as_bytes())
            .and_then(|()| writeln!(self.events, "{commit_line}"))
            .and_then(|()| self.events.sync_all())
            .map_err(|source| io_error(&self.paths.events, source))?;
        self.sequence = previous;
        self.event_ids.extend(batch_ids);
        Ok(())
    }

    /// Atomically replaces and fsyncs the incremental checkpoint.
    pub fn save_checkpoint(&self, checkpoint: &Checkpoint) -> Result<(), ArchiveError> {
        if checkpoint.version != 2 || checkpoint.corpus_id != self.corpus.as_str() {
            return Err(ArchiveError::Integrity(
                "checkpoint corpus does not match archive".to_owned(),
            ));
        }
        if checkpoint.streams.iter().any(|(stream_id, stream)| {
            !is_nonzero_snowflake(stream_id)
                || stream
                    .after_message_id
                    .as_deref()
                    .is_some_and(|cursor| !is_nonzero_snowflake(cursor))
        }) {
            return Err(ArchiveError::Integrity(
                "checkpoint stream identifiers must be nonzero Discord snowflakes".to_owned(),
            ));
        }
        reject_symlink(&self.paths.checkpoint)?;
        let temporary = self
            .paths
            .data_dir
            .join(format!(".{}.checkpoint.tmp", self.corpus.as_str()));
        reject_symlink(&temporary)?;
        let bytes = serde_json::to_vec(checkpoint)
            .map_err(|source| ArchiveError::Json { line: 0, source })?;
        let mut options = OpenOptions::new();
        options.create(true).write(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        let mut file = options
            .open(temporary.as_std_path())
            .map_err(|source| io_error(&temporary, source))?;
        harden_file(&file, &temporary)?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|source| io_error(&temporary, source))?;
        fs::rename(temporary.as_std_path(), self.paths.checkpoint.as_std_path())
            .map_err(|source| io_error(&self.paths.checkpoint, source))?;
        sync_directory(&self.paths.data_dir)
    }

    /// Loads a checkpoint after verifying that it belongs to this corpus.
    pub fn load_checkpoint(
        &self,
        guild_id: &str,
        parent_channel_id: &str,
    ) -> Result<Option<Checkpoint>, ArchiveError> {
        reject_symlink(&self.paths.checkpoint)?;
        let bytes = match fs::read(self.paths.checkpoint.as_std_path()) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(io_error(&self.paths.checkpoint, source)),
        };
        parse_or_migrate_checkpoint(&bytes, self.corpus.as_str(), guild_id, parent_channel_id)
            .map(Some)
            .map_err(|error| ArchiveError::Integrity(error.to_string()))
    }

    /// Reports whether the durable checkpoint is the exact legacy v1 shape.
    pub(crate) fn checkpoint_is_legacy(&self) -> Result<bool, ArchiveError> {
        reject_symlink(&self.paths.checkpoint)?;
        let bytes = match fs::read(self.paths.checkpoint.as_std_path()) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(source) => return Err(io_error(&self.paths.checkpoint, source)),
        };
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|source| ArchiveError::Json { line: 0, source })?;
        let Some(object) = value.as_object() else {
            return Ok(false);
        };
        let expected: BTreeSet<_> = ["after_message_id", "channel_id", "corpus_id", "updated_at"]
            .into_iter()
            .collect();
        let actual: BTreeSet<_> = object.keys().map(String::as_str).collect();
        Ok(actual == expected && object.values().all(Value::is_string))
    }

    /// Reads the current committed snapshot while retaining the writer lock.
    pub fn read_committed(&self) -> Result<ReadResult, ArchiveError> {
        read_committed_unlocked(&self.paths)
    }
}

fn is_nonzero_snowflake(value: &str) -> bool {
    value.parse::<u64>().is_ok_and(|value| value != 0)
}

fn read_committed_unlocked(paths: &ArchivePaths) -> Result<ReadResult, ArchiveError> {
    let read = read_committed_structure_unlocked(paths)?;
    Ok(partition_origin_validation(paths, read))
}

fn read_committed_structure_unlocked(paths: &ArchivePaths) -> Result<ReadResult, ArchiveError> {
    let path = &paths.events;
    reject_symlink(path)?;
    let bytes = match fs::read(path.as_std_path()) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Ok(ReadResult {
                events: Vec::new(),
                quarantined_events: Vec::new(),
                torn_or_uncommitted_tail: false,
                last_sequence: 0,
                committed_prefix: Vec::new(),
            });
        }
        Err(source) => return Err(io_error(path, source)),
    };
    let final_line_lacks_newline = !bytes.is_empty() && !bytes.ends_with(b"\n");
    let mut torn_physical_tail = false;
    let physical_lines: Vec<_> = bytes.split(|byte| *byte == b'\n').collect();
    let mut raw_lines = Vec::new();
    for (index, raw) in physical_lines.iter().enumerate() {
        if raw.is_empty() && index + 1 == physical_lines.len() {
            continue;
        }
        let line = match std::str::from_utf8(raw) {
            Ok(line) => line.to_owned(),
            Err(_) if final_line_lacks_newline && index + 1 == physical_lines.len() => {
                torn_physical_tail = true;
                continue;
            }
            Err(_) => {
                return Err(ArchiveError::Integrity(format!(
                    "non-UTF-8 data at physical line {}",
                    index + 1
                )));
            }
        };
        if line.is_empty() {
            return Err(ArchiveError::Integrity(format!(
                "blank physical line {}",
                index + 1
            )));
        }
        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) if final_line_lacks_newline && index + 1 == physical_lines.len() => {
                torn_physical_tail = true;
                continue;
            }
            Err(source) => {
                return Err(ArchiveError::Json {
                    line: index + 1,
                    source,
                });
            }
        };
        raw_lines.push((line, value));
    }
    if raw_lines.is_empty() {
        return Ok(ReadResult {
            events: Vec::new(),
            quarantined_events: Vec::new(),
            torn_or_uncommitted_tail: torn_physical_tail,
            last_sequence: 0,
            committed_prefix: Vec::new(),
        });
    }
    if raw_lines[0].1.get("event_kind").and_then(Value::as_str) != Some("format_header")
        || raw_lines[0].1.get("format_version").and_then(Value::as_str) != Some(FORMAT_VERSION)
    {
        return Err(ArchiveError::Integrity(
            "format-v2 header must be physical line 1".to_owned(),
        ));
    }
    let mut committed = Vec::new();
    let mut pending = Vec::new();
    let mut ids = HashSet::new();
    let mut last_sequence = 0;
    let mut committed_line_count = 1;
    for (index, (text, value)) in raw_lines.iter().enumerate().skip(1) {
        if value.get("event_kind").and_then(Value::as_str) == Some("format_header") {
            return Err(ArchiveError::Integrity(
                "duplicate format header".to_owned(),
            ));
        }
        if value.get("event_kind").and_then(Value::as_str) == Some("batch_commit") {
            validate_batch(&pending, value, &mut ids, &mut last_sequence)?;
            for (_, value) in pending.drain(..) {
                let event: Event = serde_json::from_value(value)
                    .map_err(|source| ArchiveError::Json { line: 0, source })?;
                committed.push(event);
            }
            committed_line_count = index + 1;
        } else {
            pending.push((text.clone(), value.clone()));
        }
    }
    let mut committed_prefix = raw_lines[..committed_line_count]
        .iter()
        .map(|(line, _)| line.as_str())
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes();
    committed_prefix.push(b'\n');
    Ok(ReadResult {
        events: committed,
        quarantined_events: Vec::new(),
        torn_or_uncommitted_tail: final_line_lacks_newline
            || torn_physical_tail
            || !pending.is_empty(),
        last_sequence,
        committed_prefix,
    })
}

fn partition_origin_validation(paths: &ArchivePaths, mut read: ReadResult) -> ReadResult {
    let mut page_cache = HashMap::new();
    let mut verified = Vec::with_capacity(read.events.len());
    let directory_error = validate_evidence_directory(&paths.origin_evidence)
        .err()
        .map(|error| error.to_string());
    for event in read.events.drain(..) {
        if event.origin_evidence.is_none() {
            verified.push(event);
            continue;
        }
        if let Some(error) = &directory_error {
            read.quarantined_events.push(QuarantinedEvent {
                event,
                origin_error: error.clone(),
            });
            continue;
        }
        match validate_event_origin(paths, &event, &mut page_cache) {
            Ok(()) => verified.push(event),
            Err(error) => read.quarantined_events.push(QuarantinedEvent {
                event,
                origin_error: error.to_string(),
            }),
        }
    }
    read.events = verified;
    read
}

fn validate_event_origin(
    paths: &ArchivePaths,
    event: &Event,
    page_cache: &mut HashMap<String, Value>,
) -> Result<(), ArchiveError> {
    let Some(origin) = &event.origin_evidence else {
        return Ok(());
    };
    if origin.adapter.name != crate::gaie::origin::DISCORD_HTTP_ADAPTER_NAME
        || origin.media_type != "application/json"
        || origin.harness.is_some()
        || !is_sha256(&origin.sha256)
    {
        return Err(ArchiveError::Integrity(
            "origin-evidence reference has an unsupported or malformed contract".to_owned(),
        ));
    }
    let validate_projection: fn(&Value, &str, &Event) -> Result<(), &'static str> =
        match origin.adapter.version.as_str() {
            "1" => crate::gaie::origin::validate_discord_projection,
            version => {
                return Err(ArchiveError::Integrity(format!(
                    "unsupported origin-evidence adapter version `{version}`"
                )));
            }
        };
    let canonical_location = format!("origin-evidence/{}", origin.sha256);
    if origin.location != canonical_location {
        return Err(ArchiveError::Integrity(
            "origin-evidence location is not canonical for its digest".to_owned(),
        ));
    }
    if !page_cache.contains_key(&origin.sha256) {
        let destination = paths.origin_evidence.join(&origin.sha256);
        let bytes = read_regular_file(&destination)?;
        if hex_sha256(&bytes) != origin.sha256 {
            return Err(ArchiveError::Integrity(
                "origin-evidence object does not match its digest".to_owned(),
            ));
        }
        let page: Value = serde_json::from_slice(&bytes)
            .map_err(|source| ArchiveError::Json { line: 0, source })?;
        page_cache.insert(origin.sha256.clone(), page);
    }
    let page = page_cache
        .get(&origin.sha256)
        .expect("origin page was inserted above");
    validate_projection(page, &origin.selector, event)
        .map_err(|message| ArchiveError::Integrity(format!("origin evidence: {message}")))?;
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn validate_evidence_directory(path: &Utf8Path) -> Result<(), ArchiveError> {
    reject_symlink_ancestors(path)?;
    reject_symlink(path)?;
    let metadata = fs::metadata(path.as_std_path()).map_err(|source| io_error(path, source))?;
    if !metadata.is_dir() {
        return Err(ArchiveError::Integrity(format!(
            "origin-evidence path `{path}` is not a directory"
        )));
    }
    Ok(())
}

fn read_regular_file(path: &Utf8Path) -> Result<Vec<u8>, ArchiveError> {
    reject_symlink(path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let mut file = options
        .open(path.as_std_path())
        .map_err(|source| io_error(path, source))?;
    let metadata = file.metadata().map_err(|source| io_error(path, source))?;
    if !metadata.file_type().is_file() {
        return Err(ArchiveError::Integrity(format!(
            "origin-evidence object `{path}` is not a regular file"
        )));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|source| io_error(path, source))?;
    Ok(bytes)
}

fn validate_batch(
    pending: &[(String, Value)],
    commit: &Value,
    ids: &mut HashSet<String>,
    last: &mut u64,
) -> Result<(), ArchiveError> {
    let message_id = commit
        .get("message_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ArchiveError::Integrity("batch commit lacks message_id".to_owned()))?;
    if commit.get("event_count").and_then(Value::as_u64) != Some(pending.len() as u64) {
        return Err(ArchiveError::Integrity(
            "batch event_count mismatch".to_owned(),
        ));
    }
    let content: String = pending
        .iter()
        .map(|(line, _)| format!("{line}\n"))
        .collect();
    if commit.get("batch_sha256").and_then(Value::as_str)
        != Some(hex_sha256(content.as_bytes()).as_str())
    {
        return Err(ArchiveError::Integrity("batch SHA-256 mismatch".to_owned()));
    }
    for (_, event) in pending {
        if event.pointer("/source/message_id").and_then(Value::as_str) != Some(message_id) {
            return Err(ArchiveError::Integrity(
                "batch contains another message".to_owned(),
            ));
        }
        let id = event
            .get("event_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ArchiveError::Integrity("event lacks event_id".to_owned()))?;
        if !ids.insert(id.to_owned()) {
            return Err(ArchiveError::Integrity("duplicate event_id".to_owned()));
        }
        let sequence = event
            .get("archive_seq")
            .and_then(Value::as_u64)
            .ok_or_else(|| ArchiveError::Integrity("event lacks archive_seq".to_owned()))?;
        if sequence <= *last {
            return Err(ArchiveError::Integrity(
                "non-monotonic archive_seq".to_owned(),
            ));
        }
        *last = sequence;
    }
    Ok(())
}

fn recover_committed_prefix(paths: &ArchivePaths, prefix: &[u8]) -> Result<(), ArchiveError> {
    let backup = paths
        .events
        .with_extension(format!("pre-repair-{}.bak", uuid::Uuid::new_v4().simple()));
    fs::copy(paths.events.as_std_path(), backup.as_std_path())
        .map_err(|source| io_error(&backup, source))?;
    let backup_file =
        File::open(backup.as_std_path()).map_err(|source| io_error(&backup, source))?;
    harden_file(&backup_file, &backup)?;
    backup_file
        .sync_all()
        .map_err(|source| io_error(&backup, source))?;
    sync_directory(&paths.data_dir)?;
    let temporary = paths.events.with_extension("repair");
    reject_symlink(&temporary)?;
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let mut file = options
        .open(temporary.as_std_path())
        .map_err(|source| io_error(&temporary, source))?;
    harden_file(&file, &temporary)?;
    file.write_all(prefix)
        .map_err(|source| io_error(&temporary, source))?;
    file.sync_all()
        .map_err(|source| io_error(&temporary, source))?;
    fs::rename(temporary.as_std_path(), paths.events.as_std_path())
        .map_err(|source| io_error(&paths.events, source))?;
    sync_directory(&paths.data_dir)
}

fn create_hardened_dir(path: &Utf8Path) -> Result<(), ArchiveError> {
    reject_symlink_ancestors(path)?;
    reject_symlink(path)?;
    fs::create_dir_all(path.as_std_path()).map_err(|source| io_error(path, source))?;
    reject_symlink(path)?;
    #[cfg(unix)]
    fs::set_permissions(path.as_std_path(), fs::Permissions::from_mode(0o700))
        .map_err(|source| io_error(path, source))?;
    Ok(())
}
fn reject_symlink_ancestors(path: &Utf8Path) -> Result<(), ArchiveError> {
    for ancestor in path.ancestors().skip(1) {
        match fs::symlink_metadata(ancestor.as_std_path()) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ArchiveError::UnsafePath(ancestor.to_owned()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error(ancestor, source)),
        }
    }
    Ok(())
}
fn harden_file(file: &File, path: &Utf8Path) -> Result<(), ArchiveError> {
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|source| io_error(path, source))?;
    Ok(())
}
fn reject_symlink(path: &Utf8Path) -> Result<(), ArchiveError> {
    match fs::symlink_metadata(path.as_std_path()) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(ArchiveError::UnsafePath(path.to_owned()))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error(path, source)),
    }
}
fn sync_directory(path: &Utf8Path) -> Result<(), ArchiveError> {
    File::open(path.as_std_path())
        .and_then(|file| file.sync_all())
        .map_err(|source| io_error(path, source))
}
fn io_error(path: &Utf8Path, source: io::Error) -> ArchiveError {
    ArchiveError::Io {
        path: path.to_owned(),
        source,
    }
}
fn compact<T: Serialize>(value: &T) -> Result<String, ArchiveError> {
    serde_json::to_string(value).map_err(|source| ArchiveError::Json { line: 0, source })
}
pub(crate) fn hex_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gaie::{
        EventKind, Ingest, Lineage, MessageContext, Payload, Relations, Source,
        service::{MessageOriginEvidence, message_batch_with_origin},
    };
    use proptest::prelude::*;

    fn fixture_event(sequence: u64) -> Event {
        Event {
            schema_version: "1".into(),
            corpus_id: "fixture".into(),
            archive_seq: sequence,
            event_id: format!("00000000-0000-4000-8000-{sequence:012}"),
            event_kind: EventKind::MessageCreate,
            observed_at: "2026-01-01T00:00:00Z".into(),
            source: Source {
                platform: "discord".into(),
                guild_id: "1".into(),
                channel_id: "2".into(),
                thread_id: None,
                message_id: "3".into(),
                actor_id: Some("4".into()),
                created_at: Some("2026-01-01T00:00:00Z".into()),
                edited_at: None,
            },
            payload: Payload {
                content: Some("hello".into()),
                content_sha256: None,
                attachments: vec![],
                emoji_id: None,
                count: None,
                normal_count: None,
                burst_count: None,
            },
            relations: Relations {
                reply_to_message_id: None,
                thread_parent_channel_id: None,
            },
            lineage: Lineage {
                observed_version_ordinal: None,
                predecessor_event_id: None,
                history_status: "complete".into(),
            },
            ingest: Ingest {
                collector_version: "gaie-alpha-0.8.0".into(),
                raw_payload_sha256: "00".into(),
            },
            origin_evidence: None,
        }
    }

    fn fixture_event_with_evidence(
        sequence: u64,
        stored: &StoredOriginEvidence,
        page: &Value,
        selector: &str,
    ) -> Event {
        let message_index = selector.trim_start_matches('/').parse::<usize>().unwrap();
        let message = page.get(message_index).unwrap();
        let origin = MessageOriginEvidence {
            page,
            message_index,
            stored,
        };
        let mut event = message_batch_with_origin(
            message,
            MessageContext {
                corpus_id: "fixture",
                guild_id: "1",
                channel_id: "2",
                thread_id: None,
                thread_parent_channel_id: None,
                observed_at: "2026-01-01T00:00:00Z",
            },
            sequence,
            &std::collections::HashMap::new(),
            Some(&origin),
        )
        .unwrap()
        .remove(0);
        event.archive_seq = sequence;
        event
    }

    fn injected_sync_failure(path: &Utf8Path) -> Result<(), ArchiveError> {
        Err(io_error(
            path,
            io::Error::other("injected directory sync failure"),
        ))
    }

    #[test]
    fn test_compact_batch_hash_includes_exact_newline() {
        let event = fixture_event(1);
        let line = serde_json::to_string(&event).unwrap();
        assert_eq!(hex_sha256(format!("{line}\n").as_bytes()).len(), 64);
        let commit = BatchCommit {
            event_kind: "batch_commit",
            message_id: "3",
            event_count: 1,
            batch_sha256: "abc".to_owned(),
            committed_at: "2026-01-01T00:00:00Z",
        };
        assert_eq!(
            compact(&commit).unwrap(),
            r#"{"event_kind":"batch_commit","message_id":"3","event_count":1,"batch_sha256":"abc","committed_at":"2026-01-01T00:00:00Z"}"#
        );
    }

    #[test]
    fn test_reader_ignores_uncommitted_and_torn_tail() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(root, &corpus).unwrap();
        {
            let mut archive =
                Archive::open(paths.clone(), corpus.clone(), "2026-01-01T00:00:00Z").unwrap();
            archive
                .append_batch(&[fixture_event(1)], "3", "2026-01-01T00:00:00Z")
                .unwrap();
        }
        let committed_prefix = fs::read(paths.events.as_std_path()).unwrap();
        let mut file = OpenOptions::new()
            .append(true)
            .open(paths.events.as_std_path())
            .unwrap();
        file.write_all(b"{\"archive_seq\":2").unwrap();
        drop(file);
        let archive = Archive::open(paths, corpus, "2026-01-01T00:00:00Z").unwrap();
        let read = archive.read_committed().unwrap();
        assert_eq!(read.events.len(), 1);
        assert!(!read.torn_or_uncommitted_tail);
        assert_eq!(
            fs::read(archive.paths.events.as_std_path()).unwrap(),
            committed_prefix
        );
        assert!(
            fs::read_dir(archive.paths.data_dir.as_std_path())
                .unwrap()
                .any(|entry| {
                    entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .contains("pre-repair-")
                })
        );
    }

    #[test]
    fn test_checkpoint_round_trip_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(root, &corpus).unwrap();
        let archive = Archive::open(paths, corpus, "2026-01-01T00:00:00Z").unwrap();
        let checkpoint = Checkpoint {
            version: 2,
            corpus_id: "fixture".into(),
            guild_id: "1".into(),
            parent_channel_id: "2".into(),
            streams: BTreeMap::from([(
                "2".into(),
                StreamCheckpoint {
                    after_message_id: Some("3".into()),
                },
            )]),
            updated_at: "2026-01-01T00:00:00Z".into(),
        };
        archive.save_checkpoint(&checkpoint).unwrap();
        archive.save_checkpoint(&checkpoint).unwrap();
        assert_eq!(archive.load_checkpoint("1", "2").unwrap(), Some(checkpoint));
    }

    #[test]
    fn test_origin_evidence_store_preserves_exact_bytes_and_reuses_verified_object() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(root, &corpus).unwrap();
        let archive = Archive::open(paths, corpus, "2026-01-01T00:00:00Z").unwrap();
        let bytes = br#"[ { "z":null, "a": 1 } ]"#;

        let first = archive.store_origin_evidence(bytes).unwrap();
        let second = archive.store_origin_evidence(bytes).unwrap();

        assert_eq!(first, second);
        assert_eq!(
            first.location(),
            format!("origin-evidence/{}", first.sha256)
        );
        assert_eq!(
            fs::read(archive.paths.data_dir.join(first.location()).as_std_path()).unwrap(),
            bytes
        );
    }

    #[test]
    fn test_origin_evidence_sync_failure_blocks_fast_path_and_append_until_retry() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(root, &corpus).unwrap();
        let mut archive = Archive::open(paths, corpus, "2026-01-01T00:00:00Z").unwrap();
        let bytes = br#"[{"id":"3","content":"hello"}]"#;
        let page: Value = serde_json::from_slice(bytes).unwrap();
        let stored = archive.store_origin_evidence(bytes).unwrap();
        let event = fixture_event_with_evidence(1, &stored, &page, "/0");

        archive.sync_directory = injected_sync_failure;
        assert!(matches!(
            archive.store_origin_evidence(bytes),
            Err(ArchiveError::Io { .. })
        ));
        assert!(matches!(
            archive.append_batch(std::slice::from_ref(&event), "3", "2026-01-01T00:00:00Z"),
            Err(ArchiveError::Io { .. })
        ));
        assert!(archive.read_committed().unwrap().events.is_empty());

        archive.sync_directory = sync_directory;
        assert_eq!(archive.store_origin_evidence(bytes).unwrap(), stored);
        archive
            .append_batch(&[event], "3", "2026-01-01T00:00:00Z")
            .unwrap();
        assert_eq!(archive.read_committed().unwrap().events.len(), 1);
    }

    #[test]
    fn test_append_rejects_fabricated_origin_reference() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(root, &corpus).unwrap();
        let mut archive = Archive::open(paths, corpus, "2026-01-01T00:00:00Z").unwrap();
        let bytes = br#"[{"id":"3","content":"hello"}]"#;
        let page: Value = serde_json::from_slice(bytes).unwrap();
        let stored = archive.store_origin_evidence(bytes).unwrap();
        let mut event = fixture_event_with_evidence(1, &stored, &page, "/0");
        event.origin_evidence.as_mut().unwrap().location = "origin-evidence/fabricated".into();

        assert!(matches!(
            archive.append_batch(&[event], "3", "2026-01-01T00:00:00Z"),
            Err(ArchiveError::Integrity(_))
        ));
        assert!(archive.read_committed().unwrap().events.is_empty());
    }

    #[test]
    fn test_origin_validation_does_not_pin_collector_version() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(root, &corpus).unwrap();
        let mut archive = Archive::open(paths, corpus, "2026-01-01T00:00:00Z").unwrap();
        let bytes = br#"[{"id":"3","content":"hello"}]"#;
        let page: Value = serde_json::from_slice(bytes).unwrap();
        let stored = archive.store_origin_evidence(bytes).unwrap();
        let mut event = fixture_event_with_evidence(1, &stored, &page, "/0");
        event.ingest.collector_version = "future-collector-version".into();

        archive
            .append_batch(&[event], "3", "2026-01-01T00:00:00Z")
            .unwrap();
        assert_eq!(
            archive.read_committed().unwrap().events[0]
                .ingest
                .collector_version,
            "future-collector-version"
        );
    }

    #[test]
    fn test_origin_validation_dispatches_by_recorded_adapter_version() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(root, &corpus).unwrap();
        let mut archive = Archive::open(paths, corpus, "2026-01-01T00:00:00Z").unwrap();
        let bytes = br#"[{"id":"3","content":"hello"}]"#;
        let page: Value = serde_json::from_slice(bytes).unwrap();
        let stored = archive.store_origin_evidence(bytes).unwrap();
        let mut event = fixture_event_with_evidence(1, &stored, &page, "/0");
        event.origin_evidence.as_mut().unwrap().adapter.version = "2".into();

        assert!(matches!(
            archive.append_batch(&[event], "3", "2026-01-01T00:00:00Z"),
            Err(ArchiveError::Integrity(message))
                if message == "unsupported origin-evidence adapter version `2`"
        ));
    }

    #[test]
    fn test_read_quarantines_unsupported_adapter_version() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(root, &corpus).unwrap();
        let archive = Archive::open(paths.clone(), corpus.clone(), "2026-01-01T00:00:00Z").unwrap();
        let bytes = br#"[{"id":"3","content":"hello"}]"#;
        let page: Value = serde_json::from_slice(bytes).unwrap();
        let stored = archive.store_origin_evidence(bytes).unwrap();
        let mut event = fixture_event_with_evidence(1, &stored, &page, "/0");
        event.origin_evidence.as_mut().unwrap().adapter.version = "2".into();
        drop(archive);

        let event_line = compact(&event).unwrap();
        let commit = BatchCommit {
            event_kind: "batch_commit",
            message_id: "3",
            event_count: 1,
            batch_sha256: hex_sha256(format!("{event_line}\n").as_bytes()),
            committed_at: "2026-01-01T00:00:00Z",
        };
        let header = FormatHeader {
            event_kind: "format_header",
            format_version: FORMAT_VERSION,
            created_at: "2026-01-01T00:00:00Z",
        };
        fs::write(
            paths.events.as_std_path(),
            format!(
                "{}\n{event_line}\n{}\n",
                compact(&header).unwrap(),
                compact(&commit).unwrap()
            ),
        )
        .unwrap();

        let archive = Archive::open(paths, corpus, "2026-01-01T00:00:00Z").unwrap();
        let read = archive.read_committed().unwrap();
        assert!(read.events.is_empty());
        assert_eq!(read.quarantined_events.len(), 1);
        assert_eq!(
            read.quarantined_events[0].origin_error,
            "archive integrity failure: unsupported origin-evidence adapter version `2`"
        );
    }

    #[test]
    fn test_origin_validation_accepts_embedded_thread_on_thread_target() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(root, &corpus).unwrap();
        let mut archive = Archive::open(paths, corpus, "2026-01-01T00:00:00Z").unwrap();
        let bytes = br#"[{"id":"3","channel_id":"2","guild_id":"1","content":"hello","thread":{"id":"2"}}]"#;
        let page: Value = serde_json::from_slice(bytes).unwrap();
        let stored = archive.store_origin_evidence(bytes).unwrap();
        let origin = MessageOriginEvidence {
            page: &page,
            message_index: 0,
            stored: &stored,
        };
        let event = message_batch_with_origin(
            &page[0],
            MessageContext {
                corpus_id: "fixture",
                guild_id: "1",
                channel_id: "2",
                thread_id: Some("2"),
                thread_parent_channel_id: Some("1"),
                observed_at: "2026-01-01T00:00:00Z",
            },
            1,
            &std::collections::HashMap::new(),
            Some(&origin),
        )
        .unwrap()
        .remove(0);

        archive
            .append_batch(std::slice::from_ref(&event), "3", "2026-01-01T00:00:00Z")
            .unwrap();
        assert_eq!(archive.read_committed().unwrap().events.len(), 1);
    }

    #[test]
    fn test_append_rejects_mutated_message_projection_fields() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(root, &corpus).unwrap();
        let mut archive = Archive::open(paths, corpus, "2026-01-01T00:00:00Z").unwrap();
        let bytes = br#"[{"id":"3","content":"hello","author":{"id":"4"},"timestamp":"2026-01-01T00:00:00Z","edited_timestamp":null,"attachments":[{"id":"8","filename":"proof.txt","content_type":"text/plain","size":5,"url":"https://cdn.discordapp.com/proof.txt"}],"message_reference":{"message_id":"2"}}]"#;
        let page: Value = serde_json::from_slice(bytes).unwrap();
        let stored = archive.store_origin_evidence(bytes).unwrap();
        let event = fixture_event_with_evidence(1, &stored, &page, "/0");
        let mut content = event.clone();
        content.payload.content = Some("changed".into());
        let mut actor = event.clone();
        actor.source.actor_id = Some("999".into());
        let mut attachment = event;
        attachment.payload.attachments[0].filename = "changed.txt".into();

        for mutated in [content, actor, attachment] {
            assert!(matches!(
                archive.append_batch(&[mutated], "3", "2026-01-01T00:00:00Z"),
                Err(ArchiveError::Integrity(_))
            ));
        }
        assert!(archive.read_committed().unwrap().events.is_empty());
    }

    #[test]
    fn test_append_rejects_mutated_reaction_key_and_count() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(root, &corpus).unwrap();
        let mut archive = Archive::open(paths, corpus, "2026-01-01T00:00:00Z").unwrap();
        let bytes = br#"[{"id":"3","attachments":[],"reactions":[{"emoji":{"id":"7","name":"spark"},"count":4,"count_details":{"normal":3,"burst":1}}]}]"#;
        let page: Value = serde_json::from_slice(bytes).unwrap();
        let stored = archive.store_origin_evidence(bytes).unwrap();
        let message = page.get(0).unwrap();
        let origin = MessageOriginEvidence {
            page: &page,
            message_index: 0,
            stored: &stored,
        };
        let reaction = message_batch_with_origin(
            message,
            MessageContext {
                corpus_id: "fixture",
                guild_id: "1",
                channel_id: "2",
                thread_id: None,
                thread_parent_channel_id: None,
                observed_at: "2026-01-01T00:00:00Z",
            },
            1,
            &std::collections::HashMap::new(),
            Some(&origin),
        )
        .unwrap()
        .remove(1);
        let mut key = reaction.clone();
        key.payload.content_sha256 = Some("0".repeat(64));
        let mut count = reaction;
        count.payload.count = Some(99);

        for mutated in [key, count] {
            assert!(matches!(
                archive.append_batch(&[mutated], "3", "2026-01-01T00:00:00Z"),
                Err(ArchiveError::Integrity(_))
            ));
        }
        assert!(archive.read_committed().unwrap().events.is_empty());
    }

    #[test]
    fn test_append_authenticates_response_supplied_channel_and_guild_context() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(root, &corpus).unwrap();
        let mut archive = Archive::open(paths, corpus, "2026-01-01T00:00:00Z").unwrap();
        let bytes = br#"[{"id":"3","channel_id":"2","guild_id":"1","attachments":[],"reactions":[{"emoji":{"id":"7","name":"spark"},"count":1}]}]"#;
        let page: Value = serde_json::from_slice(bytes).unwrap();
        let stored = archive.store_origin_evidence(bytes).unwrap();
        let message = page.get(0).unwrap();
        let origin = MessageOriginEvidence {
            page: &page,
            message_index: 0,
            stored: &stored,
        };
        let batch = message_batch_with_origin(
            message,
            MessageContext {
                corpus_id: "fixture",
                guild_id: "1",
                channel_id: "2",
                thread_id: None,
                thread_parent_channel_id: None,
                observed_at: "2026-01-01T00:00:00Z",
            },
            1,
            &std::collections::HashMap::new(),
            Some(&origin),
        )
        .unwrap();
        let mut message_with_wrong_channel = batch[0].clone();
        message_with_wrong_channel.source.channel_id = "999".into();
        let mut reaction_with_wrong_guild = batch[1].clone();
        reaction_with_wrong_guild.source.guild_id = "999".into();

        for mutated in [message_with_wrong_channel, reaction_with_wrong_guild] {
            assert!(matches!(
                archive.append_batch(&[mutated], "3", "2026-01-01T00:00:00Z"),
                Err(ArchiveError::Integrity(_))
            ));
        }
        assert!(archive.read_committed().unwrap().events.is_empty());
    }

    #[test]
    fn test_read_quarantines_origin_selector_not_matching_event_source() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(root, &corpus).unwrap();
        let archive = Archive::open(paths.clone(), corpus, "2026-01-01T00:00:00Z").unwrap();
        let bytes = br#"[{"id":"3","content":"hello"},{"id":"4","content":"other"}]"#;
        let page: Value = serde_json::from_slice(bytes).unwrap();
        let stored = archive.store_origin_evidence(bytes).unwrap();
        let mut event = fixture_event_with_evidence(1, &stored, &page, "/0");
        event.origin_evidence.as_mut().unwrap().selector = "/1".into();
        drop(archive);

        let event_line = compact(&event).unwrap();
        let commit = BatchCommit {
            event_kind: "batch_commit",
            message_id: "3",
            event_count: 1,
            batch_sha256: hex_sha256(format!("{event_line}\n").as_bytes()),
            committed_at: "2026-01-01T00:00:00Z",
        };
        let header = FormatHeader {
            event_kind: "format_header",
            format_version: FORMAT_VERSION,
            created_at: "2026-01-01T00:00:00Z",
        };
        fs::write(
            paths.events.as_std_path(),
            format!(
                "{}\n{event_line}\n{}\n",
                compact(&header).unwrap(),
                compact(&commit).unwrap()
            ),
        )
        .unwrap();

        let read = read_committed_unlocked(&paths).unwrap();
        assert!(read.events.is_empty());
        assert_eq!(read.quarantined_events.len(), 1);
        assert!(
            read.quarantined_events[0]
                .origin_error
                .contains("selector does not identify the event message")
        );
    }

    #[test]
    fn test_read_quarantines_forged_response_supplied_channel_projection() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(root, &corpus).unwrap();
        let archive = Archive::open(paths.clone(), corpus, "2026-01-01T00:00:00Z").unwrap();
        let bytes = br#"[{"id":"3","channel_id":"2","guild_id":"1","content":"hello"}]"#;
        let page: Value = serde_json::from_slice(bytes).unwrap();
        let stored = archive.store_origin_evidence(bytes).unwrap();
        let mut event = fixture_event_with_evidence(1, &stored, &page, "/0");
        event.source.channel_id = "999".into();
        drop(archive);

        let event_line = compact(&event).unwrap();
        let commit = BatchCommit {
            event_kind: "batch_commit",
            message_id: "3",
            event_count: 1,
            batch_sha256: hex_sha256(format!("{event_line}\n").as_bytes()),
            committed_at: "2026-01-01T00:00:00Z",
        };
        let header = FormatHeader {
            event_kind: "format_header",
            format_version: FORMAT_VERSION,
            created_at: "2026-01-01T00:00:00Z",
        };
        fs::write(
            paths.events.as_std_path(),
            format!(
                "{}\n{event_line}\n{}\n",
                compact(&header).unwrap(),
                compact(&commit).unwrap()
            ),
        )
        .unwrap();

        let read = read_committed_unlocked(&paths).unwrap();
        assert!(read.events.is_empty());
        assert_eq!(read.quarantined_events.len(), 1);
        assert!(
            read.quarantined_events[0]
                .origin_error
                .contains("capture context does not match")
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_append_rejects_and_read_quarantines_evidence_path_substitution() {
        use std::os::unix::fs::symlink;

        for during_read in [false, true] {
            for substitute_ancestor in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let temp_root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
                let root = temp_root.join("archive");
                let corpus = CorpusId::parse("fixture").unwrap();
                let paths = ArchivePaths::new(root.clone(), &corpus).unwrap();
                let mut archive =
                    Archive::open(paths.clone(), corpus, "2026-01-01T00:00:00Z").unwrap();
                let bytes = br#"[{"id":"3","content":"hello"}]"#;
                let page: Value = serde_json::from_slice(bytes).unwrap();
                let stored = archive.store_origin_evidence(bytes).unwrap();
                let event = fixture_event_with_evidence(1, &stored, &page, "/0");
                if during_read {
                    archive
                        .append_batch(std::slice::from_ref(&event), "3", "2026-01-01T00:00:00Z")
                        .unwrap();
                }

                let target = if substitute_ancestor {
                    paths.data_dir.clone()
                } else {
                    paths.origin_evidence.clone()
                };
                let moved = temp_root.join(if substitute_ancestor {
                    "real-archive"
                } else {
                    "real-origin-evidence"
                });
                fs::rename(target.as_std_path(), moved.as_std_path()).unwrap();
                symlink(moved.as_std_path(), target.as_std_path()).unwrap();

                if during_read {
                    drop(archive);
                    let read = read_committed_unlocked(&paths).unwrap();
                    assert!(read.events.is_empty());
                    assert_eq!(read.quarantined_events.len(), 1);
                    assert!(
                        read.quarantined_events[0]
                            .origin_error
                            .contains("unsafe archive path")
                    );
                } else {
                    assert!(matches!(
                        archive.append_batch(&[event], "3", "2026-01-01T00:00:00Z"),
                        Err(ArchiveError::UnsafePath(_))
                    ));
                }
            }
        }
    }

    #[test]
    fn test_origin_evidence_corruption_fails_before_event_append() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(root, &corpus).unwrap();
        let archive = Archive::open(paths, corpus, "2026-01-01T00:00:00Z").unwrap();
        let bytes = br#"[{"id":"3"}]"#;
        let digest = hex_sha256(bytes);
        fs::write(
            archive.paths.origin_evidence.join(digest).as_std_path(),
            b"substituted",
        )
        .unwrap();

        assert!(matches!(
            archive.store_origin_evidence(bytes),
            Err(ArchiveError::Integrity(_))
        ));
        assert!(archive.read_committed().unwrap().events.is_empty());
    }

    #[test]
    fn test_structural_tail_recovers_before_committed_origin_validation() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(root, &corpus).unwrap();
        let mut archive =
            Archive::open(paths.clone(), corpus.clone(), "2026-01-01T00:00:00Z").unwrap();
        let bytes = br#"[{"id":"3","content":"hello"}]"#;
        let page: Value = serde_json::from_slice(bytes).unwrap();
        let stored = archive.store_origin_evidence(bytes).unwrap();
        let event = fixture_event_with_evidence(1, &stored, &page, "/0");
        archive
            .append_batch(&[event], "3", "2026-01-01T00:00:00Z")
            .unwrap();
        let committed = fs::read(paths.events.as_std_path()).unwrap();
        drop(archive);

        OpenOptions::new()
            .append(true)
            .open(paths.events.as_std_path())
            .unwrap()
            .write_all(br#"{"event_kind""#)
            .unwrap();
        fs::write(
            paths.origin_evidence.join(&stored.sha256).as_std_path(),
            b"corrupt",
        )
        .unwrap();

        let archive = Archive::open(paths.clone(), corpus, "2026-01-01T00:00:00Z").unwrap();
        let read = archive.read_committed().unwrap();
        assert!(read.events.is_empty());
        assert_eq!(read.quarantined_events.len(), 1);
        assert_eq!(fs::read(paths.events.as_std_path()).unwrap(), committed);
    }

    #[test]
    fn test_corrupt_origin_quarantines_only_the_affected_event() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(root, &corpus).unwrap();
        let mut archive =
            Archive::open(paths.clone(), corpus.clone(), "2026-01-01T00:00:00Z").unwrap();
        let good_bytes = br#"[{"id":"3","content":"good"}]"#;
        let bad_bytes = br#"[{"id":"4","content":"damaged"}]"#;
        let good_page: Value = serde_json::from_slice(good_bytes).unwrap();
        let bad_page: Value = serde_json::from_slice(bad_bytes).unwrap();
        let good_stored = archive.store_origin_evidence(good_bytes).unwrap();
        let bad_stored = archive.store_origin_evidence(bad_bytes).unwrap();
        let good = fixture_event_with_evidence(1, &good_stored, &good_page, "/0");
        let bad = fixture_event_with_evidence(2, &bad_stored, &bad_page, "/0");
        archive
            .append_batch(&[good], "3", "2026-01-01T00:00:00Z")
            .unwrap();
        archive
            .append_batch(&[bad], "4", "2026-01-01T00:00:00Z")
            .unwrap();
        drop(archive);
        fs::write(
            paths.origin_evidence.join(&bad_stored.sha256).as_std_path(),
            b"cosmic-ray",
        )
        .unwrap();

        let archive = Archive::open(paths, corpus, "2026-01-01T00:00:00Z").unwrap();
        let read = archive.read_committed().unwrap();
        assert_eq!(read.events.len(), 1);
        assert_eq!(read.events[0].source.message_id, "3");
        assert_eq!(read.quarantined_events.len(), 1);
        assert_eq!(read.quarantined_events[0].event.source.message_id, "4");
        assert!(
            read.quarantined_events[0]
                .origin_error
                .contains("does not match its digest")
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_origin_evidence_rejects_symlinked_directory_and_destination() {
        use std::os::unix::fs::symlink;

        for link_destination in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
            let corpus = CorpusId::parse("fixture").unwrap();
            let paths = ArchivePaths::new(root, &corpus).unwrap();
            let archive = Archive::open(paths, corpus, "2026-01-01T00:00:00Z").unwrap();
            let bytes = br#"[{"id":"3"}]"#;
            let outside = archive.paths.data_dir.join("outside");
            fs::write(outside.as_std_path(), b"outside").unwrap();
            if link_destination {
                symlink(
                    outside.as_std_path(),
                    archive
                        .paths
                        .origin_evidence
                        .join(hex_sha256(bytes))
                        .as_std_path(),
                )
                .unwrap();
            } else {
                fs::remove_dir(archive.paths.origin_evidence.as_std_path()).unwrap();
                symlink(
                    outside.as_std_path(),
                    archive.paths.origin_evidence.as_std_path(),
                )
                .unwrap();
            }
            assert!(matches!(
                archive.store_origin_evidence(bytes),
                Err(ArchiveError::UnsafePath(_))
            ));
        }
    }

    #[test]
    fn test_checkpoint_save_rejects_invalid_stream_snowflakes() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(root, &corpus).unwrap();
        let archive = Archive::open(paths, corpus, "2026-01-01T00:00:00Z").unwrap();
        for streams in [
            BTreeMap::from([(
                "0".into(),
                StreamCheckpoint {
                    after_message_id: None,
                },
            )]),
            BTreeMap::from([(
                "2".into(),
                StreamCheckpoint {
                    after_message_id: Some("not-a-snowflake".into()),
                },
            )]),
        ] {
            let error = archive
                .save_checkpoint(&Checkpoint {
                    version: 2,
                    corpus_id: "fixture".into(),
                    guild_id: "1".into(),
                    parent_channel_id: "2".into(),
                    streams,
                    updated_at: "2026-01-01T00:00:00Z".into(),
                })
                .unwrap_err();
            assert!(matches!(error, ArchiveError::Integrity(_)));
        }
    }

    #[test]
    fn test_checkpoint_exact_v1_fixture_migrates_and_saves_exact_v2_shape() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(root, &corpus).unwrap();
        let archive = Archive::open(paths, corpus, "2026-01-01T00:00:00Z").unwrap();
        fs::write(
            archive.paths.checkpoint.as_std_path(),
            br#"{"corpus_id":"fixture","channel_id":"2","after_message_id":"3","updated_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();

        let migrated = archive.load_checkpoint("1", "2").unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(&migrated).unwrap(),
            serde_json::json!({
                "version":2,
                "corpus_id":"fixture",
                "guild_id":"1",
                "parent_channel_id":"2",
                "streams":{"2":{"after_message_id":"3"}},
                "updated_at":"2026-01-01T00:00:00Z"
            })
        );
        archive.save_checkpoint(&migrated).unwrap();
        let saved: Value =
            serde_json::from_slice(&fs::read(archive.paths.checkpoint.as_std_path()).unwrap())
                .unwrap();
        assert_eq!(saved, serde_json::to_value(migrated).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn test_archive_rejects_symlink_data_directory() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = temp.path().join("link");
        symlink(&target, &link).unwrap();
        let root = Utf8PathBuf::from_path_buf(link).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(root, &corpus).unwrap();
        assert!(matches!(
            Archive::open(paths, corpus, "2026-01-01T00:00:00Z"),
            Err(ArchiveError::UnsafePath(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn test_archive_rejects_symlink_ancestor() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = temp.path().join("link");
        symlink(&target, &link).unwrap();
        let root = Utf8PathBuf::from_path_buf(link.join("archive")).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(root, &corpus).unwrap();
        assert!(matches!(
            Archive::open(paths, corpus, "2026-01-01T00:00:00Z"),
            Err(ArchiveError::UnsafePath(_))
        ));
    }

    #[test]
    fn test_writer_rejects_duplicate_event_ids_within_and_across_batches() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(root, &corpus).unwrap();
        let mut archive = Archive::open(paths, corpus, "2026-01-01T00:00:00Z").unwrap();
        let first = fixture_event(1);
        archive
            .append_batch(std::slice::from_ref(&first), "3", "2026-01-01T00:00:00Z")
            .unwrap();
        let mut duplicate = fixture_event(2);
        duplicate.event_id.clone_from(&first.event_id);
        assert!(matches!(
            archive.append_batch(&[duplicate], "3", "2026-01-01T00:00:00Z"),
            Err(ArchiveError::Integrity(_))
        ));
        let same_batch = vec![fixture_event(2), fixture_event(3)];
        let mut same_batch = same_batch;
        let duplicate_id = same_batch[0].event_id.clone();
        same_batch[1].event_id = duplicate_id;
        assert!(matches!(
            archive.append_batch(&same_batch, "3", "2026-01-01T00:00:00Z"),
            Err(ArchiveError::Integrity(_))
        ));
    }

    #[test]
    fn test_gaie_archive_recovers_header_only_missing_terminal_newline() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(root.clone(), &corpus).unwrap();
        fs::write(root.join("fixture.ndjson").as_std_path(),
            br#"{"event_kind":"format_header","format_version":"2","created_at":"2026-01-01T00:00:00Z"}"#).unwrap();
        let archive = Archive::open(paths, corpus, "2026-01-01T00:00:00Z").unwrap();
        assert!(
            fs::read(archive.paths.events.as_std_path())
                .unwrap()
                .ends_with(b"\n")
        );
    }

    #[test]
    fn test_gaie_archive_recovers_committed_final_json_missing_terminal_newline() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(root, &corpus).unwrap();
        {
            let mut archive =
                Archive::open(paths.clone(), corpus.clone(), "2026-01-01T00:00:00Z").unwrap();
            archive
                .append_batch(&[fixture_event(1)], "3", "2026-01-01T00:00:00Z")
                .unwrap();
        }
        let expected = fs::read(paths.events.as_std_path()).unwrap();
        let file = OpenOptions::new()
            .write(true)
            .open(paths.events.as_std_path())
            .unwrap();
        file.set_len(expected.len() as u64 - 1).unwrap();
        drop(file);
        let archive = Archive::open(paths, corpus, "2026-01-01T00:00:00Z").unwrap();
        assert_eq!(
            fs::read(archive.paths.events.as_std_path()).unwrap(),
            expected
        );
    }

    #[test]
    fn test_gaie_archive_interior_corruption_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(root, &corpus).unwrap();
        {
            let mut archive =
                Archive::open(paths.clone(), corpus.clone(), "2026-01-01T00:00:00Z").unwrap();
            archive
                .append_batch(&[fixture_event(1)], "3", "2026-01-01T00:00:00Z")
                .unwrap();
        }
        let bytes = fs::read(paths.events.as_std_path()).unwrap();
        let mut lines: Vec<_> = String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        lines[1] = "{broken".to_owned();
        fs::write(
            paths.events.as_std_path(),
            format!("{}\n", lines.join("\n")),
        )
        .unwrap();
        assert!(matches!(
            Archive::open(paths, corpus, "2026-01-01T00:00:00Z"),
            Err(ArchiveError::Json { line: 2, .. })
        ));
    }

    #[test]
    fn test_gaie_archive_commit_survives_checkpoint_failure() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(root, &corpus).unwrap();
        let mut archive = Archive::open(paths, corpus, "2026-01-01T00:00:00Z").unwrap();
        archive
            .append_batch(&[fixture_event(1)], "3", "2026-01-01T00:00:00Z")
            .unwrap();
        fs::create_dir(archive.paths.checkpoint.as_std_path()).unwrap();
        let checkpoint = Checkpoint {
            version: 2,
            corpus_id: "fixture".into(),
            guild_id: "1".into(),
            parent_channel_id: "2".into(),
            streams: BTreeMap::from([(
                "2".into(),
                StreamCheckpoint {
                    after_message_id: Some("3".into()),
                },
            )]),
            updated_at: "2026-01-01T00:00:00Z".into(),
        };
        assert!(archive.save_checkpoint(&checkpoint).is_err());
        assert_eq!(archive.read_committed().unwrap().events.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn test_archive_artifacts_use_private_modes() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().join("private")).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(root, &corpus).unwrap();
        let archive = Archive::open(paths, corpus, "2026-01-01T00:00:00Z").unwrap();
        assert_eq!(
            fs::metadata(archive.paths.data_dir.as_std_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(archive.paths.events.as_std_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(archive.paths.lock.as_std_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    // Property invariants: accepted batches round-trip with validated framing;
    // duplicate identities/non-monotonic sequences fail before durable write;
    // and an arbitrary torn suffix never becomes visible or rewrites the prefix.
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        #[test]
        fn prop_gaie_archive_valid_batch_round_trips(
            message_id in "[1-9][0-9]{0,8}",
            contents in prop::collection::vec("[a-zA-Z0-9 ]{0,24}", 1..6),
        ) {
            let temp = tempfile::tempdir().unwrap();
            let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
            let corpus = CorpusId::parse("property").unwrap();
            let paths = ArchivePaths::new(root, &corpus).unwrap();
            let mut archive = Archive::open(paths, corpus, "2026-01-01T00:00:00Z").unwrap();
            let events: Vec<_> = contents.iter().enumerate().map(|(index, content)| {
                let mut event = fixture_event(index as u64 + 1);
                event.corpus_id = "property".to_owned();
                event.event_id = format!("property-{index}");
                event.source.message_id.clone_from(&message_id);
                event.payload.content = Some(content.clone());
                event
            }).collect();
            let expected = serde_json::to_value(&events).unwrap();
            archive.append_batch(&events, &message_id, "2026-01-01T00:00:00Z").unwrap();
            let actual = serde_json::to_value(archive.read_committed().unwrap().events).unwrap();
            prop_assert_eq!(actual, expected);
            let archive_text = fs::read_to_string(archive.paths.events.as_std_path()).unwrap();
            let lines: Vec<_> = archive_text.lines().collect();
            let framed = lines[1..=events.len()].iter().map(|line| format!("{line}\n")).collect::<String>();
            let commit: Value = serde_json::from_str(lines[events.len() + 1]).unwrap();
            let independent_hash = format!("{:x}", Sha256::digest(framed.as_bytes()));
            prop_assert_eq!(commit["batch_sha256"].as_str(), Some(independent_hash.as_str()));
        }

        #[test]
        fn prop_gaie_archive_rejects_archive_duplicate_id(previous in 1_u64..1000) {
            let temp = tempfile::tempdir().unwrap();
            let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
            let corpus = CorpusId::parse("property").unwrap();
            let paths = ArchivePaths::new(root, &corpus).unwrap();
            let mut archive = Archive::open(paths, corpus, "2026-01-01T00:00:00Z").unwrap();
            let mut first = fixture_event(previous);
            first.corpus_id = "property".to_owned();
            archive.append_batch(std::slice::from_ref(&first), "3", "2026-01-01T00:00:00Z").unwrap();
            let mut rejected = fixture_event(previous + 1);
            rejected.corpus_id = "property".to_owned();
            rejected.event_id.clone_from(&first.event_id);
            let result = archive.append_batch(&[rejected], "3", "2026-01-01T00:00:00Z");
            prop_assert!(matches!(result, Err(ArchiveError::Integrity(reason)) if reason == "duplicate event_id"));
            prop_assert_eq!(archive.read_committed().unwrap().events.len(), 1);
        }

        #[test]
        fn prop_gaie_archive_rejects_nonmonotonic_sequence(
            (previous, next) in (1_u64..1000).prop_flat_map(|previous| (Just(previous), 0..=previous)),
        ) {
            let temp = tempfile::tempdir().unwrap();
            let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
            let corpus = CorpusId::parse("property").unwrap();
            let paths = ArchivePaths::new(root, &corpus).unwrap();
            let mut archive = Archive::open(paths, corpus, "2026-01-01T00:00:00Z").unwrap();
            let mut first = fixture_event(previous);
            first.corpus_id = "property".to_owned();
            first.event_id = "fresh-previous-id".to_owned();
            archive.append_batch(std::slice::from_ref(&first), "3", "2026-01-01T00:00:00Z").unwrap();
            let mut rejected = fixture_event(next);
            rejected.corpus_id = "property".to_owned();
            rejected.event_id = "fresh-next-id".to_owned();
            let result = archive.append_batch(&[rejected], "3", "2026-01-01T00:00:00Z");
            prop_assert!(matches!(result, Err(ArchiveError::Integrity(reason)) if reason == "archive_seq must increase monotonically"));
            prop_assert_eq!(archive.read_committed().unwrap().events.len(), 1);
        }

        #[test]
        fn prop_gaie_archive_arbitrary_torn_tail_preserves_committed_prefix(
            tail in prop::collection::vec(any::<u8>().prop_filter("no physical newline", |byte| *byte != b'\n'), 0..64),
        ) {
            let temp = tempfile::tempdir().unwrap();
            let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
            let corpus = CorpusId::parse("property").unwrap();
            let paths = ArchivePaths::new(root, &corpus).unwrap();
            {
                let mut archive = Archive::open(paths.clone(), corpus.clone(), "2026-01-01T00:00:00Z").unwrap();
                let mut event = fixture_event(1);
                event.corpus_id = "property".to_owned();
                archive.append_batch(&[event], "3", "2026-01-01T00:00:00Z").unwrap();
            }
            let prefix = fs::read(paths.events.as_std_path()).unwrap();
            let mut file = OpenOptions::new().append(true).open(paths.events.as_std_path()).unwrap();
            file.write_all(b"{broken:").unwrap();
            file.write_all(&tail).unwrap();
            drop(file);
            let archive = Archive::open(paths, corpus, "2026-01-01T00:00:00Z").unwrap();
            prop_assert_eq!(archive.read_committed().unwrap().events.len(), 1);
            prop_assert_eq!(fs::read(archive.paths.events.as_std_path()).unwrap(), prefix);
        }

        #[test]
        fn prop_gaie_archive_arbitrary_interior_line_prefix_mutation_fails_closed(
            replacement in any::<u8>().prop_filter("not a valid object opener or newline", |byte| !matches!(*byte, b'{' | b'\n')),
        ) {
            let temp = tempfile::tempdir().unwrap();
            let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
            let corpus = CorpusId::parse("property").unwrap();
            let paths = ArchivePaths::new(root, &corpus).unwrap();
            {
                let mut archive = Archive::open(paths.clone(), corpus.clone(), "2026-01-01T00:00:00Z").unwrap();
                let mut event = fixture_event(1);
                event.corpus_id = "property".to_owned();
                archive.append_batch(&[event], "3", "2026-01-01T00:00:00Z").unwrap();
            }
            let mut bytes = fs::read(paths.events.as_std_path()).unwrap();
            let event_start = bytes.iter().position(|byte| *byte == b'\n').unwrap() + 1;
            bytes[event_start] = replacement;
            fs::write(paths.events.as_std_path(), bytes).unwrap();
            prop_assert!(Archive::open(paths, corpus, "2026-01-01T00:00:00Z").is_err());
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        #[test]
        fn prop_gaie_archive_valid_corpus_paths_stay_under_root(corpus in "[A-Za-z0-9_-]{1,32}") {
            let root = Utf8PathBuf::from("/tmp/gaie-property-root");
            let corpus = CorpusId::parse(corpus).unwrap();
            let paths = ArchivePaths::new(root.clone(), &corpus).unwrap();
            prop_assert_eq!(paths.events.parent(), Some(root.as_path()));
            prop_assert_eq!(paths.checkpoint.parent(), Some(root.as_path()));
            prop_assert!(ArchivePaths::new(Utf8PathBuf::from("relative/path"), &corpus).is_err());
        }
    }
}
