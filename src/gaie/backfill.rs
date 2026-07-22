//! Verified capture targets and deterministic Atom 1b backfill planning.

use crate::gaie::archive::StoredOriginEvidence;
use crate::gaie::service::{MessageOriginEvidence, message_batch_with_origin};
use crate::gaie::{
    Archive, ArchiveError, ArchivePaths, Checkpoint, CorpusId, DiscordArchiveClient,
    DiscordArchiveError, MessageContext, StreamCheckpoint,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CaptureRoot {
    pub(crate) corpus_id: String,
    pub(crate) guild_id: String,
    pub(crate) parent_channel_id: String,
    pub(crate) kind: RootKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum RootKind {
    Text,
    Announcement,
    Forum,
    Media,
}

impl RootKind {
    fn has_parent_message_stream(self) -> bool {
        matches!(self, Self::Text | Self::Announcement)
    }
}

impl CaptureRoot {
    pub fn parent_target(&self) -> Option<CaptureTarget> {
        self.kind
            .has_parent_message_stream()
            .then(|| CaptureTarget {
                channel_id: self.parent_channel_id.clone(),
                thread_id: None,
                thread_parent_channel_id: None,
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ThreadKind {
    Announcement,
    Public,
    Private,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ThreadCandidate {
    pub channel_id: String,
    pub guild_id: String,
    pub parent_id: String,
    pub kind: ThreadKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiscoveryRoute {
    ActiveSnapshotA,
    PublicArchived,
    PrivateArchived,
    JoinedPrivateArchived,
    ActiveSnapshotB,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiscoveryPage {
    pub route: DiscoveryRoute,
    pub candidates: Vec<ThreadCandidate>,
    pub has_more: bool,
    pub before: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CaptureTarget {
    pub(crate) channel_id: String,
    pub(crate) thread_id: Option<String>,
    pub(crate) thread_parent_channel_id: Option<String>,
}

impl CaptureTarget {
    pub fn channel_id(&self) -> &str {
        &self.channel_id
    }

    pub fn thread_id(&self) -> Option<&str> {
        self.thread_id.as_deref()
    }

    pub fn thread_parent_channel_id(&self) -> Option<&str> {
        self.thread_parent_channel_id.as_deref()
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum BackfillContractError {
    #[error("Discord snowflake is malformed")]
    MalformedSnowflake,
    #[error("Discord returned a thread outside the configured capture root")]
    ForeignThread,
    #[error("checkpoint is not the exact supported v1 or v2 shape")]
    InvalidCheckpoint,
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BackfillRunError {
    #[error(transparent)]
    Discord(#[from] DiscordArchiveError),
    #[error(transparent)]
    Archive(#[from] ArchiveError),
    #[error("Discord message response is malformed")]
    InvalidMessage,
    #[error("parent-only mode is unavailable for forum/media roots")]
    ParentStreamUnavailable,
}

#[non_exhaustive]
pub struct BackfillOptions<'a> {
    corpus_id: &'a str,
    guild_id: &'a str,
    parent_channel_id: &'a str,
    allow_partial: bool,
}

impl<'a> BackfillOptions<'a> {
    pub fn new(
        corpus_id: &'a str,
        guild_id: &'a str,
        parent_channel_id: &'a str,
        allow_partial: bool,
    ) -> Self {
        Self {
            corpus_id,
            guild_id,
            parent_channel_id,
            allow_partial,
        }
    }
}

pub async fn run_backfill(
    client: &DiscordArchiveClient,
    token: &str,
    options: BackfillOptions<'_>,
    paths: ArchivePaths,
    corpus: CorpusId,
    created_at: &str,
) -> Result<usize, BackfillRunError> {
    let root = client
        .capture_root(
            token,
            options.corpus_id,
            options.guild_id,
            options.parent_channel_id,
        )
        .await?;
    let targets = if options.allow_partial {
        vec![
            root.parent_target()
                .ok_or(BackfillRunError::ParentStreamUnavailable)?,
        ]
    } else {
        client.discover_targets(token, &root).await?
    };
    let mut archive = Archive::open(paths, corpus, created_at)?;
    let migration_pending = archive.checkpoint_is_legacy()?;
    let loaded_checkpoint = archive.load_checkpoint(options.guild_id, options.parent_channel_id)?;
    let mut checkpoint = loaded_checkpoint.clone().unwrap_or_else(|| Checkpoint {
        version: 2,
        corpus_id: options.corpus_id.to_owned(),
        guild_id: options.guild_id.to_owned(),
        parent_channel_id: options.parent_channel_id.to_owned(),
        streams: BTreeMap::new(),
        updated_at: String::new(),
    });
    let committed = archive.read_committed()?;
    let mut seen: HashSet<String> = committed
        .events
        .iter()
        .filter(|event| matches!(event.event_kind, crate::gaie::EventKind::MessageCreate))
        .map(|event| event.source.message_id.clone())
        .collect();
    let mut committed_stream_max = BTreeMap::<String, u64>::new();
    for event in committed
        .events
        .iter()
        .filter(|event| matches!(event.event_kind, crate::gaie::EventKind::MessageCreate))
    {
        let message_id = parse_snowflake(&event.source.message_id)
            .map_err(|_| BackfillRunError::InvalidMessage)?;
        committed_stream_max
            .entry(event.source.channel_id.clone())
            .and_modify(|current| *current = (*current).max(message_id))
            .or_insert(message_id);
    }
    let mut sequence = archive.last_sequence() + 1;
    let mut added = 0;
    let mut checkpoint_saved = false;
    for target in targets {
        let previous_stream = checkpoint.streams.get(target.channel_id()).cloned();
        let original_after = previous_stream
            .as_ref()
            .and_then(|stream| stream.after_message_id.clone());
        let mut messages =
            fetch_target_messages(client, token, &target, original_after.as_deref(), &archive)
                .await?;
        messages.sort_by_key(|message| {
            message
                .value
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| id.parse::<u64>().ok())
                .unwrap_or(0)
        });
        let mut stream_max = previous_stream
            .as_ref()
            .and_then(|stream| stream.after_message_id.clone());
        if let Some(committed_max) = committed_stream_max.get(target.channel_id()) {
            stream_max = maximum_message_id(stream_max.as_deref(), Some(*committed_max))?;
        }
        for observed in messages {
            let message = &observed.value;
            let message_id = message
                .get("id")
                .and_then(Value::as_str)
                .ok_or(BackfillRunError::InvalidMessage)?
                .to_owned();
            parse_snowflake(&message_id).map_err(|_| BackfillRunError::InvalidMessage)?;
            stream_max = Some(message_id.clone());
            if seen.contains(&message_id) {
                continue;
            }
            let observed_at = archive_timestamp();
            let mut attachment_hashes = std::collections::HashMap::new();
            for attachment in message
                .get("attachments")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let attachment_id = attachment
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or(BackfillRunError::InvalidMessage)?;
                let url = attachment
                    .get("url")
                    .and_then(Value::as_str)
                    .ok_or(BackfillRunError::InvalidMessage)?;
                let digest = client
                    .download_attachment(url, archive.attachments_dir())
                    .await?;
                attachment_hashes.insert(attachment_id.to_owned(), digest);
            }
            let origin = MessageOriginEvidence {
                page: &observed.page,
                message_index: observed.message_index,
                stored: &observed.stored,
            };
            let batch = message_batch_with_origin(
                message,
                MessageContext {
                    corpus_id: options.corpus_id,
                    guild_id: options.guild_id,
                    channel_id: target.channel_id(),
                    thread_id: target.thread_id(),
                    thread_parent_channel_id: target.thread_parent_channel_id(),
                    observed_at: &observed_at,
                },
                sequence,
                &attachment_hashes,
                Some(&origin),
            )?;
            archive.append_batch(&batch, &message_id, &observed_at)?;
            sequence += batch.len() as u64;
            seen.insert(message_id);
            added += 1;
        }
        let next_stream = StreamCheckpoint {
            after_message_id: stream_max,
        };
        if previous_stream.as_ref() != Some(&next_stream) {
            checkpoint
                .streams
                .insert(target.channel_id().to_owned(), next_stream);
            checkpoint.updated_at = archive_timestamp();
            archive.save_checkpoint(&checkpoint)?;
            checkpoint_saved = true;
        }
    }
    if migration_pending && !checkpoint_saved {
        archive.save_checkpoint(&checkpoint)?;
    }
    Ok(added)
}

struct FetchedMessage {
    value: Value,
    page: Arc<Value>,
    message_index: usize,
    stored: StoredOriginEvidence,
}

async fn fetch_target_messages(
    client: &DiscordArchiveClient,
    token: &str,
    target: &CaptureTarget,
    original_after: Option<&str>,
    archive: &Archive,
) -> Result<Vec<FetchedMessage>, BackfillRunError> {
    let lower_bound = original_after
        .map(parse_snowflake)
        .transpose()
        .map_err(|_| BackfillRunError::InvalidMessage)?;
    let mut before = None::<String>;
    let mut cursors = HashSet::new();
    let mut messages = Vec::new();
    loop {
        let page = if before.is_none() {
            client
                .observed_message_page(token, target, None, original_after)
                .await?
        } else {
            client
                .observed_message_page(token, target, before.as_deref(), None)
                .await?
        };
        let stored = archive.store_origin_evidence(page.exact_bytes())?;
        if page.is_empty() {
            break;
        }
        let parsed = Arc::new(page.into_parsed());
        let page_messages = parsed.as_array().ok_or(BackfillRunError::InvalidMessage)?;
        let request_after = if before.is_none() {
            original_after
        } else {
            None
        };
        validate_message_page_request_bounds(page_messages, before.as_deref(), request_after)?;
        let (minimum, _) = message_page_bounds(page_messages)?;
        let minimum_numeric =
            parse_snowflake(&minimum).map_err(|_| BackfillRunError::InvalidMessage)?;
        let reached_bound = lower_bound.is_some_and(|bound| minimum_numeric <= bound);
        let short_page = page_messages.len() < 100;
        messages.extend(
            page_messages
                .iter()
                .enumerate()
                .filter(|(_, message)| {
                    message
                        .get("id")
                        .and_then(Value::as_str)
                        .and_then(|id| id.parse::<u64>().ok())
                        .is_some_and(|id| lower_bound.is_none_or(|bound| id > bound))
                })
                .map(|(message_index, message)| FetchedMessage {
                    value: message.clone(),
                    page: Arc::clone(&parsed),
                    message_index,
                    stored: stored.clone(),
                }),
        );
        if reached_bound || short_page {
            break;
        }
        before = Some(advance_message_cursor(
            before.as_deref(),
            &mut cursors,
            minimum,
        )?);
    }
    Ok(messages)
}

fn maximum_message_id(
    current: Option<&str>,
    candidate: Option<u64>,
) -> Result<Option<String>, BackfillRunError> {
    let current = current
        .map(parse_snowflake)
        .transpose()
        .map_err(|_| BackfillRunError::InvalidMessage)?;
    Ok(current
        .into_iter()
        .chain(candidate)
        .max()
        .map(|value| value.to_string()))
}

fn archive_timestamp() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn message_page_bounds(page: &[Value]) -> Result<(String, String), BackfillRunError> {
    let mut ids = page
        .iter()
        .map(|message| -> Result<(u64, String), BackfillRunError> {
            let id = message
                .get("id")
                .and_then(Value::as_str)
                .ok_or(BackfillRunError::InvalidMessage)?;
            let numeric = parse_snowflake(id).map_err(|_| BackfillRunError::InvalidMessage)?;
            Ok((numeric, id.to_owned()))
        });
    let first = ids.next().ok_or(BackfillRunError::InvalidMessage)??;
    let mut minimum = first.clone();
    let mut maximum = first;
    for current in ids {
        let current = current?;
        if current.0 < minimum.0 {
            minimum = current.clone();
        }
        if current.0 > maximum.0 {
            maximum = current;
        }
    }
    Ok((minimum.1, maximum.1))
}

fn advance_message_cursor(
    current: Option<&str>,
    seen: &mut HashSet<String>,
    next: String,
) -> Result<String, BackfillRunError> {
    let next_numeric = parse_snowflake(&next).map_err(|_| BackfillRunError::InvalidMessage)?;
    if current
        .map(parse_snowflake)
        .transpose()
        .map_err(|_| BackfillRunError::InvalidMessage)?
        .is_some_and(|current| next_numeric >= current)
        || !seen.insert(next.clone())
    {
        return Err(BackfillRunError::InvalidMessage);
    }
    Ok(next)
}

fn validate_message_page_request_bounds(
    page: &[Value],
    before: Option<&str>,
    after: Option<&str>,
) -> Result<(), BackfillRunError> {
    let before = before
        .map(parse_snowflake)
        .transpose()
        .map_err(|_| BackfillRunError::InvalidMessage)?;
    let after = after
        .map(parse_snowflake)
        .transpose()
        .map_err(|_| BackfillRunError::InvalidMessage)?;
    for message in page {
        let id = message
            .get("id")
            .and_then(Value::as_str)
            .ok_or(BackfillRunError::InvalidMessage)?;
        let id = parse_snowflake(id).map_err(|_| BackfillRunError::InvalidMessage)?;
        if before.is_some_and(|bound| id >= bound) || after.is_some_and(|bound| id <= bound) {
            return Err(BackfillRunError::InvalidMessage);
        }
    }
    Ok(())
}

pub(crate) fn discover_capture_targets(
    root: &CaptureRoot,
    pages: &[DiscoveryPage],
) -> Result<Vec<CaptureTarget>, BackfillContractError> {
    parse_snowflake(&root.parent_channel_id)?;
    let mut threads = BTreeMap::<u64, CaptureTarget>::new();
    for page in pages {
        for candidate in &page.candidates {
            if candidate.guild_id != root.guild_id || candidate.parent_id != root.parent_channel_id
            {
                if matches!(
                    page.route,
                    DiscoveryRoute::ActiveSnapshotA | DiscoveryRoute::ActiveSnapshotB
                ) {
                    continue;
                }
                return Err(BackfillContractError::ForeignThread);
            }
            let root_compatible = matches!(
                (root.kind, candidate.kind),
                (RootKind::Announcement, ThreadKind::Announcement)
                    | (
                        RootKind::Text | RootKind::Forum | RootKind::Media,
                        ThreadKind::Public
                    )
                    | (RootKind::Text, ThreadKind::Private)
            );
            let route_compatible = match page.route {
                DiscoveryRoute::ActiveSnapshotA | DiscoveryRoute::ActiveSnapshotB => true,
                DiscoveryRoute::PublicArchived => {
                    matches!(
                        candidate.kind,
                        ThreadKind::Announcement | ThreadKind::Public
                    )
                }
                DiscoveryRoute::PrivateArchived | DiscoveryRoute::JoinedPrivateArchived => {
                    candidate.kind == ThreadKind::Private
                }
            };
            if !root_compatible || !route_compatible {
                return Err(BackfillContractError::ForeignThread);
            }
            let id = parse_snowflake(&candidate.channel_id)?;
            threads.insert(
                id,
                CaptureTarget {
                    channel_id: candidate.channel_id.clone(),
                    thread_id: Some(candidate.channel_id.clone()),
                    thread_parent_channel_id: Some(root.parent_channel_id.clone()),
                },
            );
        }
    }
    let mut targets = Vec::with_capacity(threads.len() + 1);
    if let Some(parent) = root.parent_target() {
        targets.push(parent);
    }
    targets.extend(threads.into_values());
    Ok(targets)
}

pub(crate) fn parse_or_migrate_checkpoint(
    bytes: &[u8],
    corpus_id: &str,
    guild_id: &str,
    parent_channel_id: &str,
) -> Result<Checkpoint, BackfillContractError> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| BackfillContractError::InvalidCheckpoint)?;
    let object = value
        .as_object()
        .ok_or(BackfillContractError::InvalidCheckpoint)?;
    let checkpoint = if object.contains_key("version") {
        if object.contains_key("channel_id") || object.contains_key("after_message_id") {
            return Err(BackfillContractError::InvalidCheckpoint);
        }
        let checkpoint: Checkpoint =
            serde_json::from_value(value).map_err(|_| BackfillContractError::InvalidCheckpoint)?;
        if checkpoint.version != 2 {
            return Err(BackfillContractError::InvalidCheckpoint);
        }
        checkpoint
    } else {
        const V1_KEYS: [&str; 4] = ["after_message_id", "channel_id", "corpus_id", "updated_at"];
        let actual: BTreeSet<_> = object.keys().map(String::as_str).collect();
        let expected: BTreeSet<_> = V1_KEYS.into_iter().collect();
        if actual != expected {
            return Err(BackfillContractError::InvalidCheckpoint);
        }
        let corpus_id = string_field(object, "corpus_id")?;
        let channel_id = string_field(object, "channel_id")?;
        Checkpoint {
            version: 2,
            corpus_id: corpus_id.to_owned(),
            guild_id: guild_id.to_owned(),
            parent_channel_id: channel_id.to_owned(),
            streams: BTreeMap::from([(
                channel_id.to_owned(),
                StreamCheckpoint {
                    after_message_id: Some(string_field(object, "after_message_id")?.to_owned()),
                },
            )]),
            updated_at: string_field(object, "updated_at")?.to_owned(),
        }
    };
    if checkpoint.corpus_id != corpus_id
        || checkpoint.guild_id != guild_id
        || checkpoint.parent_channel_id != parent_channel_id
    {
        return Err(BackfillContractError::InvalidCheckpoint);
    }
    for (stream_id, stream) in &checkpoint.streams {
        parse_snowflake(stream_id).map_err(|_| BackfillContractError::InvalidCheckpoint)?;
        if let Some(after_message_id) = stream.after_message_id.as_deref() {
            parse_snowflake(after_message_id)
                .map_err(|_| BackfillContractError::InvalidCheckpoint)?;
        }
    }
    Ok(checkpoint)
}

fn parse_snowflake(value: &str) -> Result<u64, BackfillContractError> {
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value != 0)
        .ok_or(BackfillContractError::MalformedSnowflake)
}

fn string_field<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
) -> Result<&'a str, BackfillContractError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .ok_or(BackfillContractError::InvalidCheckpoint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    fn root() -> CaptureRoot {
        CaptureRoot {
            corpus_id: "fixture".to_owned(),
            guild_id: "10".to_owned(),
            parent_channel_id: "100".to_owned(),
            kind: RootKind::Text,
        }
    }

    fn candidate(channel_id: &str, kind: ThreadKind) -> ThreadCandidate {
        ThreadCandidate {
            channel_id: channel_id.to_owned(),
            guild_id: "10".to_owned(),
            parent_id: "100".to_owned(),
            kind,
        }
    }

    fn page(route: DiscoveryRoute, candidates: Vec<ThreadCandidate>) -> DiscoveryPage {
        DiscoveryPage {
            route,
            candidates,
            has_more: false,
            before: None,
        }
    }

    fn discovery_fixture() -> Vec<DiscoveryPage> {
        vec![
            page(
                DiscoveryRoute::ActiveSnapshotA,
                vec![candidate("300", ThreadKind::Private)],
            ),
            page(
                DiscoveryRoute::PublicArchived,
                vec![candidate("200", ThreadKind::Public)],
            ),
            page(DiscoveryRoute::PrivateArchived, Vec::new()),
            page(DiscoveryRoute::JoinedPrivateArchived, Vec::new()),
            page(
                DiscoveryRoute::ActiveSnapshotB,
                vec![candidate("400", ThreadKind::Public)],
            ),
        ]
    }

    #[test]
    fn default_backfill_captures_parent_and_principal_visible_threads() {
        let targets = discover_capture_targets(&root(), &discovery_fixture()).unwrap();
        assert_eq!(
            targets
                .iter()
                .map(CaptureTarget::channel_id)
                .collect::<Vec<_>>(),
            vec!["100", "200", "300", "400"]
        );
    }

    #[test]
    fn wrong_parent_candidate_is_rejected_before_child_message_fetch() {
        let mut foreign = candidate("999", ThreadKind::Public);
        foreign.parent_id = "101".to_owned();
        let pages = vec![page(DiscoveryRoute::PublicArchived, vec![foreign])];
        let error = discover_capture_targets(&root(), &pages).unwrap_err();
        assert_eq!(error, BackfillContractError::ForeignThread);
    }

    #[test]
    fn exact_root_thread_route_matrix_is_enforced() {
        let roots = [
            RootKind::Text,
            RootKind::Announcement,
            RootKind::Forum,
            RootKind::Media,
        ];
        let thread_kinds = [
            ThreadKind::Announcement,
            ThreadKind::Public,
            ThreadKind::Private,
        ];
        let routes = [
            DiscoveryRoute::ActiveSnapshotA,
            DiscoveryRoute::PublicArchived,
            DiscoveryRoute::PrivateArchived,
            DiscoveryRoute::JoinedPrivateArchived,
            DiscoveryRoute::ActiveSnapshotB,
        ];
        let allowed = [
            (
                RootKind::Text,
                ThreadKind::Public,
                DiscoveryRoute::ActiveSnapshotA,
            ),
            (
                RootKind::Text,
                ThreadKind::Public,
                DiscoveryRoute::PublicArchived,
            ),
            (
                RootKind::Text,
                ThreadKind::Public,
                DiscoveryRoute::ActiveSnapshotB,
            ),
            (
                RootKind::Text,
                ThreadKind::Private,
                DiscoveryRoute::ActiveSnapshotA,
            ),
            (
                RootKind::Text,
                ThreadKind::Private,
                DiscoveryRoute::PrivateArchived,
            ),
            (
                RootKind::Text,
                ThreadKind::Private,
                DiscoveryRoute::JoinedPrivateArchived,
            ),
            (
                RootKind::Text,
                ThreadKind::Private,
                DiscoveryRoute::ActiveSnapshotB,
            ),
            (
                RootKind::Announcement,
                ThreadKind::Announcement,
                DiscoveryRoute::ActiveSnapshotA,
            ),
            (
                RootKind::Announcement,
                ThreadKind::Announcement,
                DiscoveryRoute::PublicArchived,
            ),
            (
                RootKind::Announcement,
                ThreadKind::Announcement,
                DiscoveryRoute::ActiveSnapshotB,
            ),
            (
                RootKind::Forum,
                ThreadKind::Public,
                DiscoveryRoute::ActiveSnapshotA,
            ),
            (
                RootKind::Forum,
                ThreadKind::Public,
                DiscoveryRoute::PublicArchived,
            ),
            (
                RootKind::Forum,
                ThreadKind::Public,
                DiscoveryRoute::ActiveSnapshotB,
            ),
            (
                RootKind::Media,
                ThreadKind::Public,
                DiscoveryRoute::ActiveSnapshotA,
            ),
            (
                RootKind::Media,
                ThreadKind::Public,
                DiscoveryRoute::PublicArchived,
            ),
            (
                RootKind::Media,
                ThreadKind::Public,
                DiscoveryRoute::ActiveSnapshotB,
            ),
        ];

        for root_kind in roots {
            for thread_kind in thread_kinds {
                for route in routes {
                    let mut capture_root = root();
                    capture_root.kind = root_kind;
                    let actual = discover_capture_targets(
                        &capture_root,
                        &[page(route, vec![candidate("200", thread_kind)])],
                    );
                    let expected = allowed.contains(&(root_kind, thread_kind, route));
                    assert_eq!(
                        actual.is_ok(),
                        expected,
                        "root={root_kind:?} thread={thread_kind:?} route={route:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn exact_v1_checkpoint_fixture_migrates_to_parent_only_v2_shape() {
        let v1 = br#"{"corpus_id":"fixture","channel_id":"100","after_message_id":"123","updated_at":"2026-07-21T00:00:00Z"}"#;
        let actual = parse_or_migrate_checkpoint(v1, "fixture", "10", "100").unwrap();
        let expected: Checkpoint = serde_json::from_value(serde_json::json!({
            "version": 2,
            "corpus_id": "fixture",
            "guild_id": "10",
            "parent_channel_id": "100",
            "streams": {"100": {"after_message_id": "123"}},
            "updated_at": "2026-07-21T00:00:00Z"
        }))
        .unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn unknown_mixed_foreign_and_corrupt_checkpoints_fail_closed() {
        let fixtures: [&[u8]; 8] = [
            br#"{"version":3,"corpus_id":"fixture","guild_id":"10","parent_channel_id":"100","streams":{},"updated_at":"2026-07-21T00:00:00Z"}"#,
            br#"{"version":2,"corpus_id":"fixture","guild_id":"10","parent_channel_id":"100","channel_id":"100","after_message_id":"1","streams":{},"updated_at":"2026-07-21T00:00:00Z"}"#,
            br#"{"version":2,"corpus_id":"foreign","guild_id":"10","parent_channel_id":"100","streams":{},"updated_at":"2026-07-21T00:00:00Z"}"#,
            br#"{"version":2,"corpus_id":"fixture""#,
            br#"{"version":2,"corpus_id":"fixture","guild_id":"10","parent_channel_id":"100","streams":{"0":{"after_message_id":null}},"updated_at":"2026-07-21T00:00:00Z"}"#,
            br#"{"version":2,"corpus_id":"fixture","guild_id":"10","parent_channel_id":"100","streams":{"not-a-snowflake":{"after_message_id":null}},"updated_at":"2026-07-21T00:00:00Z"}"#,
            br#"{"version":2,"corpus_id":"fixture","guild_id":"10","parent_channel_id":"100","streams":{"100":{"after_message_id":"0"}},"updated_at":"2026-07-21T00:00:00Z"}"#,
            br#"{"version":2,"corpus_id":"fixture","guild_id":"10","parent_channel_id":"100","streams":{"100":{"after_message_id":"not-a-snowflake"}},"updated_at":"2026-07-21T00:00:00Z"}"#,
        ];

        for fixture in fixtures {
            assert_eq!(
                parse_or_migrate_checkpoint(fixture, "fixture", "10", "100").unwrap_err(),
                BackfillContractError::InvalidCheckpoint
            );
        }
    }

    #[test]
    fn discovery_permutations_produce_the_same_numeric_target_plan() {
        let first = discovery_fixture();
        let mut second = first.clone();
        second.reverse();
        for page in &mut second {
            page.candidates.reverse();
        }

        let first_plan = discover_capture_targets(&root(), &first).unwrap();
        let second_plan = discover_capture_targets(&root(), &second).unwrap();
        let oracle: BTreeSet<u64> = [100, 200, 300, 400].into_iter().collect();
        let first_ids: Vec<u64> = first_plan
            .iter()
            .map(|target| target.channel_id.parse().unwrap())
            .collect();

        assert_eq!(first_plan, second_plan);
        assert_eq!(first_ids, oracle.into_iter().collect::<Vec<_>>());
    }

    #[test]
    fn page_bounds_rejects_malformed_ids_and_orders_snowflakes() {
        let page = [
            serde_json::json!({"id":"20"}),
            serde_json::json!({"id":"10"}),
        ];
        assert_eq!(
            message_page_bounds(&page).unwrap(),
            ("10".to_owned(), "20".to_owned())
        );
        assert!(message_page_bounds(&[serde_json::json!({"id":"not-a-snowflake"})]).is_err());
        assert!(message_page_bounds(&[serde_json::json!({})]).is_err());
    }

    #[test]
    fn pagination_cursor_requires_strict_backward_progress_from_fresh_state() {
        let mut seen = HashSet::new();
        assert_eq!(
            advance_message_cursor(None, &mut seen, "10".to_owned()).unwrap(),
            "10"
        );
        assert!(advance_message_cursor(Some("10"), &mut HashSet::new(), "10".to_owned()).is_err());
        assert!(advance_message_cursor(Some("10"), &mut HashSet::new(), "11".to_owned()).is_err());
        assert_eq!(
            advance_message_cursor(Some("10"), &mut HashSet::new(), "9".to_owned()).unwrap(),
            "9"
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        #[test]
        fn prop_gaie_archive_page_bounds_match_numeric_min_max(ids in prop::collection::vec(1_u64..u64::MAX, 1..100)) {
            let page: Vec<_> = ids.iter().map(|id| serde_json::json!({"id":id.to_string()})).collect();
            let expected_min = ids.iter().min().unwrap().to_string();
            let expected_max = ids.iter().max().unwrap().to_string();
            prop_assert_eq!(message_page_bounds(&page).unwrap(), (expected_min, expected_max));
        }

        #[test]
        fn prop_gaie_archive_cursor_acceptance_matches_dedup_model(candidates in prop::collection::vec(1_u64..10_000, 0..100)) {
            let mut production_seen = HashSet::new();
            let mut accepted_prefix = Vec::new();
            let mut current: Option<String> = None;
            for candidate in candidates {
                let candidate = candidate.to_string();
                let expected = current
                    .as_deref()
                    .is_none_or(|current| candidate.parse::<u64>().unwrap() < current.parse::<u64>().unwrap())
                    && !accepted_prefix.iter().any(|accepted| accepted == &candidate);
                let actual = advance_message_cursor(current.as_deref(), &mut production_seen, candidate.clone()).is_ok();
                prop_assert_eq!(actual, expected);
                if actual {
                    accepted_prefix.push(candidate.clone());
                    current = Some(candidate);
                }
            }
        }
    }
}
