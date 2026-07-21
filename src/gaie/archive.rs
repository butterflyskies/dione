use crate::gaie::{CorpusId, Event};
use camino::{Utf8Path, Utf8PathBuf};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
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
            corpus_id: corpus.as_str().to_owned(),
            data_dir,
        };
        Ok(paths)
    }
}

/// A durable incremental cursor for the configured parent channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub corpus_id: String,
    pub channel_id: String,
    pub after_message_id: String,
    pub updated_at: String,
}

/// Committed archive contents and whether an incomplete tail was ignored.
#[derive(Debug)]
pub struct ReadResult {
    pub events: Vec<Event>,
    pub torn_or_uncommitted_tail: bool,
    pub last_sequence: u64,
    committed_prefix: Vec<u8>,
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
        let read = read_committed_unlocked(&paths.events)?;
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
        if checkpoint.corpus_id != self.corpus.as_str() {
            return Err(ArchiveError::Integrity(
                "checkpoint corpus does not match archive".to_owned(),
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
    pub fn load_checkpoint(&self) -> Result<Option<Checkpoint>, ArchiveError> {
        reject_symlink(&self.paths.checkpoint)?;
        let bytes = match fs::read(self.paths.checkpoint.as_std_path()) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(io_error(&self.paths.checkpoint, source)),
        };
        let checkpoint: Checkpoint = serde_json::from_slice(&bytes)
            .map_err(|source| ArchiveError::Json { line: 0, source })?;
        if checkpoint.corpus_id != self.corpus.as_str() {
            return Err(ArchiveError::Integrity(
                "checkpoint corpus does not match archive".to_owned(),
            ));
        }
        Ok(Some(checkpoint))
    }

    /// Reads the current committed snapshot while retaining the writer lock.
    pub fn read_committed(&self) -> Result<ReadResult, ArchiveError> {
        read_committed_unlocked(&self.paths.events)
    }
}

fn read_committed_unlocked(path: &Utf8Path) -> Result<ReadResult, ArchiveError> {
    reject_symlink(path)?;
    let bytes = match fs::read(path.as_std_path()) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Ok(ReadResult {
                events: Vec::new(),
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
                committed.push(
                    serde_json::from_value(value)
                        .map_err(|source| ArchiveError::Json { line: 0, source })?,
                );
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
        torn_or_uncommitted_tail: final_line_lacks_newline
            || torn_physical_tail
            || !pending.is_empty(),
        last_sequence,
        committed_prefix,
    })
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
    use crate::gaie::{EventKind, Ingest, Lineage, Payload, Relations, Source};
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
        }
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
            corpus_id: "fixture".into(),
            channel_id: "2".into(),
            after_message_id: "3".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        };
        archive.save_checkpoint(&checkpoint).unwrap();
        archive.save_checkpoint(&checkpoint).unwrap();
        assert_eq!(archive.load_checkpoint().unwrap(), Some(checkpoint));
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
            corpus_id: "fixture".into(),
            channel_id: "2".into(),
            after_message_id: "3".into(),
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
