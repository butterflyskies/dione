use crate::gaie::archive::StoredOriginEvidence;
use crate::gaie::backfill::{
    DiscoveryPage, DiscoveryRoute, RootKind, ThreadCandidate, ThreadKind, discover_capture_targets,
};
use crate::gaie::origin::{
    DISCORD_COLLECTOR_VERSION, DISCORD_HTTP_ADAPTER_NAME, DISCORD_HTTP_ADAPTER_VERSION,
    project_discord_message, project_discord_reaction,
};
use crate::gaie::{
    Attachment, CaptureRoot, CaptureTarget, Event, EventKind, Ingest, Lineage, OriginAdapter,
    OriginEvidenceRef, Payload, Relations, Source,
};
use camino::{Utf8Path, Utf8PathBuf};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::net::IpAddr;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::time::{Duration, Instant};
use thiserror::Error;
use uuid::Uuid;

/// Summary returned by a one-shot backfill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackfillReport {
    pub new_messages: usize,
    pub skipped_messages: usize,
}

/// Stable archive context applied while converting one Discord message.
#[derive(Debug, Clone, Copy)]
pub struct MessageContext<'a> {
    pub corpus_id: &'a str,
    pub guild_id: &'a str,
    pub channel_id: &'a str,
    pub thread_id: Option<&'a str>,
    pub thread_parent_channel_id: Option<&'a str>,
    pub observed_at: &'a str,
}

#[derive(Debug)]
pub(crate) struct ObservedMessagePage {
    exact_bytes: Vec<u8>,
    parsed: Value,
}

impl ObservedMessagePage {
    pub(crate) fn is_empty(&self) -> bool {
        self.parsed.as_array().is_none_or(std::vec::Vec::is_empty)
    }

    pub(crate) fn exact_bytes(&self) -> &[u8] {
        &self.exact_bytes
    }

    pub(crate) fn into_parsed(self) -> Value {
        self.parsed
    }
}

/// Retained page evidence and the original array index of one message.
pub(crate) struct MessageOriginEvidence<'a> {
    pub(crate) page: &'a Value,
    pub(crate) message_index: usize,
    pub(crate) stored: &'a StoredOriginEvidence,
}

impl MessageOriginEvidence<'_> {
    fn reference(
        &self,
        selector: String,
        expected: &Value,
    ) -> Result<OriginEvidenceRef, DiscordArchiveError> {
        if self.page.pointer(&selector) != Some(expected) {
            return Err(DiscordArchiveError::Selector);
        }
        Ok(OriginEvidenceRef {
            adapter: OriginAdapter {
                name: DISCORD_HTTP_ADAPTER_NAME.to_owned(),
                version: DISCORD_HTTP_ADAPTER_VERSION.to_owned(),
            },
            sha256: self.stored.sha256.clone(),
            location: self.stored.location(),
            media_type: "application/json".to_owned(),
            selector,
            harness: None,
        })
    }

    fn message_reference(&self, message: &Value) -> Result<OriginEvidenceRef, DiscordArchiveError> {
        self.reference(format!("/{}", self.message_index), message)
    }

    fn reaction_reference(
        &self,
        reaction_index: usize,
        reaction: &Value,
    ) -> Result<OriginEvidenceRef, DiscordArchiveError> {
        self.reference(
            format!("/{}/reactions/{reaction_index}", self.message_index),
            reaction,
        )
    }
}

/// A lossless Discord HTTP client used only by explicit archive commands.
///
/// The current atom exposes raw JSON retrieval so a mock can prove allowlist
/// routing without coupling the daemon's lossy MCP rendering path to GAIE.
#[derive(Debug, Clone)]
pub struct DiscordArchiveClient {
    client: reqwest::Client,
    base_url: reqwest::Url,
}

impl Default for DiscordArchiveClient {
    fn default() -> Self {
        Self {
            client: archive_http_client(),
            base_url: reqwest::Url::parse("https://discord.com/api/v10/")
                .unwrap_or_else(|_| unreachable!("the static Discord API URL is valid")),
        }
    }
}

/// Errors from the lossless Discord archive HTTP boundary.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DiscordArchiveError {
    #[error("archive request failed")]
    Request(#[source] reqwest::Error),
    #[error("archive request exceeded its total retry deadline")]
    RequestTimeout,
    #[error("Discord returned a non-success archive response")]
    Status,
    #[error("archive response did not have the expected shape")]
    Shape,
    #[error("origin-evidence selector does not resolve to the normalized source object")]
    Selector,
    #[error("injected archive API origin must be loopback HTTP")]
    UnsafeBaseUrl,
    #[error("attachment URL is not an allowed Discord CDN or test origin")]
    UnsafeAttachmentUrl,
    #[error("attachment storage failed for `{path}`")]
    AttachmentIo {
        path: Utf8PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("stored attachment hash does not match its content-addressed filename")]
    AttachmentIntegrity,
    #[error("channel `{requested}` is outside the configured archive scope `{allowed}`")]
    OutsideScope { requested: String, allowed: String },
}

impl DiscordArchiveClient {
    /// Creates a client which does not retain the resolved Discord token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a client with an injected API origin for hermetic HTTP tests.
    pub fn with_base_url(base_url: reqwest::Url) -> Result<Self, DiscordArchiveError> {
        let loopback = base_url.host_str() == Some("localhost")
            || base_url
                .host_str()
                .and_then(|host| host.parse::<IpAddr>().ok())
                .is_some_and(|host| host.is_loopback());
        if base_url.scheme() != "http" || !loopback {
            return Err(DiscordArchiveError::UnsafeBaseUrl);
        }
        Ok(Self {
            client: archive_http_client(),
            base_url,
        })
    }

    /// Fetches one raw message page for a previously verified capture target.
    pub async fn raw_message_page(
        &self,
        token: &str,
        target: &CaptureTarget,
        before: Option<&str>,
        after: Option<&str>,
    ) -> Result<Vec<Value>, DiscordArchiveError> {
        let page = self
            .observed_message_page(token, target, before, after)
            .await?;
        page.into_parsed()
            .as_array()
            .cloned()
            .ok_or(DiscordArchiveError::Shape)
    }

    pub(crate) async fn observed_message_page(
        &self,
        token: &str,
        target: &CaptureTarget,
        before: Option<&str>,
        after: Option<&str>,
    ) -> Result<ObservedMessagePage, DiscordArchiveError> {
        if before.is_some() && after.is_some() {
            return Err(DiscordArchiveError::Shape);
        }
        let channel_id = target.channel_id();
        let url = self
            .base_url
            .join(&format!("channels/{channel_id}/messages"))
            .map_err(|_| DiscordArchiveError::Shape)?;
        let mut request = self
            .client
            .get(url)
            .header("Authorization", format!("Bot {token}"))
            .header("User-Agent", "GAIE/gaie-alpha-0.8.0")
            .query(&[("limit", "100")]);
        if let Some(before) = before {
            request = request.query(&[("before", before)]);
        }
        if let Some(after) = after {
            request = request.query(&[("after", after)]);
        }
        let response = self.send_with_retries(request).await?;
        if !response.status().is_success() {
            return Err(DiscordArchiveError::Status);
        }
        let bytes = response
            .bytes()
            .await
            .map_err(DiscordArchiveError::Request)?;
        let parsed: Value =
            serde_json::from_slice(&bytes).map_err(|_| DiscordArchiveError::Shape)?;
        if !parsed.is_array() {
            return Err(DiscordArchiveError::Shape);
        }
        Ok(ObservedMessagePage {
            exact_bytes: bytes.to_vec(),
            parsed,
        })
    }

    /// Validates the configured parent metadata and returns a typed capture root.
    pub async fn capture_root(
        &self,
        token: &str,
        corpus_id: &str,
        guild_id: &str,
        parent_channel_id: &str,
    ) -> Result<CaptureRoot, DiscordArchiveError> {
        parse_nonzero_snowflake(guild_id)?;
        parse_nonzero_snowflake(parent_channel_id)?;
        let url = self
            .base_url
            .join(&format!("channels/{parent_channel_id}"))
            .map_err(|_| DiscordArchiveError::Shape)?;
        let response = self
            .send_with_retries(self.authorized_get(token, url))
            .await?;
        if !response.status().is_success() {
            return Err(DiscordArchiveError::Status);
        }
        let value = Self::response_json(response).await?;
        let object = value.as_object().ok_or(DiscordArchiveError::Shape)?;
        if object.get("id").and_then(Value::as_str) != Some(parent_channel_id)
            || object.get("guild_id").and_then(Value::as_str) != Some(guild_id)
        {
            return Err(DiscordArchiveError::Shape);
        }
        let kind = match object.get("type").and_then(Value::as_u64) {
            Some(0) => RootKind::Text,
            Some(5) => RootKind::Announcement,
            Some(15) => RootKind::Forum,
            Some(16) => RootKind::Media,
            _ => return Err(DiscordArchiveError::Shape),
        };
        Ok(CaptureRoot {
            corpus_id: corpus_id.to_owned(),
            guild_id: guild_id.to_owned(),
            parent_channel_id: parent_channel_id.to_owned(),
            kind,
        })
    }

    /// Discovers and verifies the principal-visible capture targets for a root.
    pub async fn discover_targets(
        &self,
        token: &str,
        root: &CaptureRoot,
    ) -> Result<Vec<CaptureTarget>, DiscordArchiveError> {
        let mut pages = Vec::new();
        pages.push(
            self.active_threads(token, root, DiscoveryRoute::ActiveSnapshotA)
                .await?,
        );
        pages.extend(
            self.archived_threads(token, root, "public", DiscoveryRoute::PublicArchived)
                .await?,
        );
        if root.kind == RootKind::Text {
            match self
                .archived_threads(token, root, "private", DiscoveryRoute::PrivateArchived)
                .await
            {
                Ok(private) => pages.extend(private),
                Err(DiscordArchiveError::OutsideScope { .. }) => {
                    pages.extend(self.joined_private_threads(token, root).await?)
                }
                Err(error) => return Err(error),
            }
        }
        pages.push(
            self.active_threads(token, root, DiscoveryRoute::ActiveSnapshotB)
                .await?,
        );
        discover_capture_targets(root, &pages).map_err(|_| DiscordArchiveError::Shape)
    }

    fn authorized_get(&self, token: &str, url: reqwest::Url) -> reqwest::RequestBuilder {
        self.client
            .get(url)
            .header("Authorization", format!("Bot {token}"))
            .header("User-Agent", "GAIE/gaie-alpha-0.8.0")
    }

    async fn active_threads(
        &self,
        token: &str,
        root: &CaptureRoot,
        route: DiscoveryRoute,
    ) -> Result<DiscoveryPage, DiscordArchiveError> {
        let url = self
            .base_url
            .join(&format!("guilds/{}/threads/active", root.guild_id))
            .map_err(|_| DiscordArchiveError::Shape)?;
        let response = self
            .send_with_retries(self.authorized_get(token, url))
            .await?;
        if !response.status().is_success() {
            return Err(DiscordArchiveError::Status);
        }
        let value = Self::response_json(response).await?;
        Ok(DiscoveryPage {
            route,
            candidates: parse_thread_candidates(&value, root)?,
            has_more: false,
            before: None,
        })
    }

    async fn archived_threads(
        &self,
        token: &str,
        root: &CaptureRoot,
        visibility: &str,
        route: DiscoveryRoute,
    ) -> Result<Vec<DiscoveryPage>, DiscordArchiveError> {
        let mut before = None::<String>;
        let mut seen = std::collections::HashSet::new();
        let mut pages = Vec::new();
        loop {
            let url = self
                .base_url
                .join(&format!(
                    "channels/{}/threads/archived/{visibility}",
                    root.parent_channel_id
                ))
                .map_err(|_| DiscordArchiveError::Shape)?;
            let mut request = self.authorized_get(token, url);
            if let Some(cursor) = before.as_deref() {
                request = request.query(&[("before", cursor)]);
            }
            let response = self.send_with_retries(request).await?;
            if response.status() == reqwest::StatusCode::FORBIDDEN && visibility == "private" {
                return Err(DiscordArchiveError::OutsideScope {
                    requested: "private-all".to_owned(),
                    allowed: "joined-private".to_owned(),
                });
            }
            if !response.status().is_success() {
                return Err(DiscordArchiveError::Status);
            }
            let value = Self::response_json(response).await?;
            let has_more = value
                .get("has_more")
                .and_then(Value::as_bool)
                .ok_or(DiscordArchiveError::Shape)?;
            let candidates = parse_thread_candidates(&value, root)?;
            let next = candidates
                .last()
                .and_then(|candidate| {
                    value
                        .get("threads")
                        .and_then(Value::as_array)
                        .and_then(|threads| {
                            threads.iter().find(|thread| {
                                thread.get("id").and_then(Value::as_str)
                                    == Some(candidate.channel_id.as_str())
                            })
                        })
                })
                .and_then(|thread| thread.get("thread_metadata"))
                .and_then(|metadata| metadata.get("archive_timestamp"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            pages.push(DiscoveryPage {
                route,
                candidates,
                has_more,
                before: before.clone(),
            });
            if !has_more {
                break;
            }
            let next = next.ok_or(DiscordArchiveError::Shape)?;
            before = Some(advance_archive_timestamp_cursor(
                before.as_deref(),
                &mut seen,
                next,
            )?);
        }
        Ok(pages)
    }

    async fn joined_private_threads(
        &self,
        token: &str,
        root: &CaptureRoot,
    ) -> Result<Vec<DiscoveryPage>, DiscordArchiveError> {
        let mut before = None::<String>;
        let mut seen = std::collections::HashSet::new();
        let mut pages = Vec::new();
        loop {
            let url = self
                .base_url
                .join(&format!(
                    "channels/{}/users/@me/threads/archived/private",
                    root.parent_channel_id
                ))
                .map_err(|_| DiscordArchiveError::Shape)?;
            let mut request = self.authorized_get(token, url);
            if let Some(cursor) = before.as_deref() {
                request = request.query(&[("before", cursor)]);
            }
            let response = self.send_with_retries(request).await?;
            if !response.status().is_success() {
                return Err(DiscordArchiveError::Status);
            }
            let value = Self::response_json(response).await?;
            let has_more = value
                .get("has_more")
                .and_then(Value::as_bool)
                .ok_or(DiscordArchiveError::Shape)?;
            let candidates = parse_thread_candidates(&value, root)?;
            let next = candidates
                .last()
                .map(|candidate| candidate.channel_id.clone());
            pages.push(DiscoveryPage {
                route: DiscoveryRoute::JoinedPrivateArchived,
                candidates,
                has_more,
                before: before.clone(),
            });
            if !has_more {
                break;
            }
            let next = next.ok_or(DiscordArchiveError::Shape)?;
            before = Some(advance_archive_snowflake_cursor(
                before.as_deref(),
                &mut seen,
                next,
            )?);
        }
        Ok(pages)
    }

    /// Downloads, verifies, and durably stores one content-addressed attachment.
    pub async fn download_attachment(
        &self,
        url: &str,
        directory: &Utf8Path,
    ) -> Result<String, DiscordArchiveError> {
        let url = reqwest::Url::parse(url).map_err(|_| DiscordArchiveError::UnsafeAttachmentUrl)?;
        if !self.attachment_url_allowed(&url) {
            return Err(DiscordArchiveError::UnsafeAttachmentUrl);
        }
        reject_symlink_ancestors(directory)?;
        reject_symlink(directory)?;
        fs::create_dir_all(directory.as_std_path())
            .map_err(|source| attachment_io(directory, source))?;
        #[cfg(unix)]
        fs::set_permissions(directory.as_std_path(), fs::Permissions::from_mode(0o700))
            .map_err(|source| attachment_io(directory, source))?;
        let request = self
            .client
            .get(url.clone())
            .header("User-Agent", "GAIE/gaie-alpha-0.8.0");
        let response = self.send_with_retries(request).await?;
        if !response.status().is_success() {
            return Err(DiscordArchiveError::Status);
        }
        let bytes = response
            .bytes()
            .await
            .map_err(DiscordArchiveError::Request)?;
        let digest = hash_bytes(&bytes);
        let extension = safe_extension(url.path());
        let destination = directory.join(format!("{digest}{extension}"));
        reject_symlink(&destination)?;
        if destination.exists() {
            let stored = fs::read(destination.as_std_path())
                .map_err(|source| attachment_io(&destination, source))?;
            if hash_bytes(&stored) != digest {
                return Err(DiscordArchiveError::AttachmentIntegrity);
            }
            return Ok(digest);
        }
        let temporary = directory.join(format!(".download-{}", Uuid::new_v4().simple()));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        let mut file = options
            .open(temporary.as_std_path())
            .map_err(|source| attachment_io(&temporary, source))?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|source| attachment_io(&temporary, source))?;
        if hash_bytes(
            &fs::read(temporary.as_std_path())
                .map_err(|source| attachment_io(&temporary, source))?,
        ) != digest
        {
            let _ = fs::remove_file(temporary.as_std_path());
            return Err(DiscordArchiveError::AttachmentIntegrity);
        }
        fs::rename(temporary.as_std_path(), destination.as_std_path())
            .map_err(|source| attachment_io(&destination, source))?;
        File::open(directory.as_std_path())
            .and_then(|file| file.sync_all())
            .map_err(|source| attachment_io(directory, source))?;
        Ok(digest)
    }

    fn attachment_url_allowed(&self, url: &reqwest::Url) -> bool {
        let discord = url.scheme() == "https"
            && matches!(
                url.host_str(),
                Some("cdn.discordapp.com" | "media.discordapp.net")
            );
        let test_origin = self.base_url.scheme() == "http"
            && url.scheme() == "http"
            && url.host_str() == self.base_url.host_str()
            && url.port_or_known_default() == self.base_url.port_or_known_default();
        url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && (discord || test_origin)
    }

    async fn send_with_retries(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, DiscordArchiveError> {
        tokio::time::timeout(Duration::from_secs(90), async {
            for attempt in 0..5 {
                let retry = request.try_clone().ok_or(DiscordArchiveError::Shape)?;
                let started = Instant::now();
                let response = retry.send().await.map_err(DiscordArchiveError::Request)?;
                tracing::debug!(
                    http.status = %response.status(),
                    http.endpoint = %safe_http_endpoint(response.url()),
                    elapsed_ms = started.elapsed().as_millis(),
                    attempt,
                    "Discord archive HTTP request completed"
                );
                if response.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
                    return Ok(response);
                }
                if attempt == 4 {
                    return Ok(response);
                }
                let delay = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<f64>().ok())
                    .filter(|value| value.is_finite() && *value >= 0.0)
                    .unwrap_or(5.0)
                    .min(30.0);
                tokio::time::sleep(Duration::from_secs_f64(delay)).await;
            }
            Err(DiscordArchiveError::Shape)
        })
        .await
        .map_err(|_| DiscordArchiveError::RequestTimeout)?
    }

    async fn response_json(response: reqwest::Response) -> Result<Value, DiscordArchiveError> {
        let bytes = response
            .bytes()
            .await
            .map_err(DiscordArchiveError::Request)?;
        serde_json::from_slice(&bytes).map_err(|_| DiscordArchiveError::Shape)
    }
}

fn safe_http_endpoint(url: &reqwest::Url) -> String {
    format!("{}{}", url.origin().ascii_serialization(), url.path())
}

fn advance_archive_timestamp_cursor(
    current: Option<&str>,
    seen: &mut std::collections::HashSet<String>,
    next: String,
) -> Result<String, DiscordArchiveError> {
    let next_timestamp =
        chrono::DateTime::parse_from_rfc3339(&next).map_err(|_| DiscordArchiveError::Shape)?;
    if current
        .map(chrono::DateTime::parse_from_rfc3339)
        .transpose()
        .map_err(|_| DiscordArchiveError::Shape)?
        .is_some_and(|current| next_timestamp >= current)
        || !seen.insert(next.clone())
    {
        return Err(DiscordArchiveError::Shape);
    }
    Ok(next)
}

fn advance_archive_snowflake_cursor(
    current: Option<&str>,
    seen: &mut std::collections::HashSet<String>,
    next: String,
) -> Result<String, DiscordArchiveError> {
    let next_numeric = parse_nonzero_snowflake(&next)?;
    if current
        .map(parse_nonzero_snowflake)
        .transpose()?
        .is_some_and(|current| next_numeric >= current)
        || !seen.insert(next.clone())
    {
        return Err(DiscordArchiveError::Shape);
    }
    Ok(next)
}

fn archive_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| unreachable!("static archive HTTP client configuration is valid"))
}

fn parse_nonzero_snowflake(value: &str) -> Result<u64, DiscordArchiveError> {
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value != 0)
        .ok_or(DiscordArchiveError::Shape)
}

fn parse_thread_candidates(
    value: &Value,
    root: &CaptureRoot,
) -> Result<Vec<ThreadCandidate>, DiscordArchiveError> {
    value
        .get("threads")
        .and_then(Value::as_array)
        .ok_or(DiscordArchiveError::Shape)?
        .iter()
        .map(|thread| {
            let channel_id = thread
                .get("id")
                .and_then(Value::as_str)
                .ok_or(DiscordArchiveError::Shape)?;
            parse_nonzero_snowflake(channel_id)?;
            let guild_id = thread
                .get("guild_id")
                .and_then(Value::as_str)
                .ok_or(DiscordArchiveError::Shape)?;
            parse_nonzero_snowflake(guild_id)?;
            let parent_id = thread
                .get("parent_id")
                .and_then(Value::as_str)
                .ok_or(DiscordArchiveError::Shape)?;
            parse_nonzero_snowflake(parent_id)?;
            let kind = match thread.get("type").and_then(Value::as_u64) {
                Some(10) => ThreadKind::Announcement,
                Some(11) => ThreadKind::Public,
                Some(12) => ThreadKind::Private,
                _ => return Err(DiscordArchiveError::Shape),
            };
            if guild_id != root.guild_id && parent_id == root.parent_channel_id {
                return Err(DiscordArchiveError::Shape);
            }
            Ok(ThreadCandidate {
                channel_id: channel_id.to_owned(),
                guild_id: guild_id.to_owned(),
                parent_id: parent_id.to_owned(),
                kind,
            })
        })
        .collect()
}

/// Converts one lossless Discord message into the collector's create and
/// aggregate reaction-snapshot events. It intentionally makes no claim about
/// reactor attribution or live reaction deltas.
pub fn message_batch(
    message: &Value,
    context: MessageContext<'_>,
    first_sequence: u64,
    attachment_hashes: &std::collections::HashMap<String, String>,
) -> Result<Vec<Event>, DiscordArchiveError> {
    message_batch_with_origin(message, context, first_sequence, attachment_hashes, None)
}

pub(crate) fn message_batch_with_origin(
    message: &Value,
    context: MessageContext<'_>,
    first_sequence: u64,
    attachment_hashes: &std::collections::HashMap<String, String>,
    origin: Option<&MessageOriginEvidence<'_>>,
) -> Result<Vec<Event>, DiscordArchiveError> {
    let projected = project_discord_message(message).ok_or(DiscordArchiveError::Shape)?;
    let message_id = projected.message_id.clone();
    let message_origin = origin
        .map(|evidence| evidence.message_reference(message))
        .transpose()?;
    let embedded_thread_id = projected.embedded_thread_id.as_deref();
    let source = Source {
        platform: "discord".to_owned(),
        guild_id: context.guild_id.to_owned(),
        channel_id: context.channel_id.to_owned(),
        thread_id: embedded_thread_id.or(context.thread_id).map(str::to_owned),
        message_id: message_id.clone(),
        actor_id: projected.actor_id.clone(),
        created_at: projected.created_at.clone(),
        edited_at: projected.edited_at.clone(),
    };
    let attachments = projected
        .attachments
        .iter()
        .map(|attachment| Attachment {
            id: attachment.id.clone(),
            filename: attachment.filename.clone(),
            media_type: attachment.media_type.clone(),
            size: attachment.size,
            sha256: attachment_hashes.get(&attachment.id).cloned(),
            url: attachment.url.clone(),
        })
        .collect();
    let relations = Relations {
        reply_to_message_id: projected.reply_to_message_id.clone(),
        thread_parent_channel_id: context
            .thread_parent_channel_id
            .or_else(|| embedded_thread_id.map(|_| context.channel_id))
            .map(str::to_owned),
    };
    let lineage = Lineage {
        observed_version_ordinal: None,
        predecessor_event_id: None,
        history_status: projected.history_status.clone(),
    };
    let mut events = vec![Event {
        schema_version: "1".to_owned(),
        corpus_id: context.corpus_id.to_owned(),
        archive_seq: first_sequence,
        event_id: Uuid::new_v4().to_string(),
        event_kind: EventKind::MessageCreate,
        observed_at: context.observed_at.to_owned(),
        source: source.clone(),
        payload: Payload {
            content: projected.content.clone(),
            content_sha256: projected.content_sha256.clone(),
            attachments,
            emoji_id: None,
            count: None,
            normal_count: None,
            burst_count: None,
        },
        relations: relations.clone(),
        lineage,
        ingest: Ingest {
            collector_version: DISCORD_COLLECTOR_VERSION.to_owned(),
            raw_payload_sha256: projected.raw_payload_sha256,
        },
        origin_evidence: message_origin,
    }];
    for (offset, reaction) in message
        .get("reactions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let projected = project_discord_reaction(reaction);
        let reaction_origin = origin
            .map(|evidence| evidence.reaction_reference(offset, reaction))
            .transpose()?;
        events.push(Event {
            schema_version: "1".to_owned(),
            corpus_id: context.corpus_id.to_owned(),
            archive_seq: first_sequence + offset as u64 + 1,
            event_id: Uuid::new_v4().to_string(),
            event_kind: EventKind::ReactionSnapshot,
            observed_at: context.observed_at.to_owned(),
            source: Source {
                actor_id: None,
                created_at: None,
                edited_at: None,
                ..source.clone()
            },
            payload: Payload {
                content: Some(projected.emoji_name),
                content_sha256: Some(projected.key_sha256),
                attachments: vec![],
                emoji_id: projected.emoji_id,
                count: Some(projected.count),
                normal_count: Some(projected.normal_count),
                burst_count: Some(projected.burst_count),
            },
            relations: Relations {
                reply_to_message_id: None,
                thread_parent_channel_id: relations.thread_parent_channel_id.clone(),
            },
            lineage: Lineage {
                observed_version_ordinal: None,
                predecessor_event_id: None,
                history_status: "complete".to_owned(),
            },
            ingest: Ingest {
                collector_version: DISCORD_COLLECTOR_VERSION.to_owned(),
                raw_payload_sha256: projected.raw_payload_sha256,
            },
            origin_evidence: reaction_origin,
        });
    }
    Ok(events)
}

fn hash_bytes(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}
fn safe_extension(path: &str) -> String {
    path.rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, extension)| extension)
        .filter(|extension| {
            !extension.is_empty()
                && extension.len() <= 16
                && extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
        .map_or_else(String::new, |extension| format!(".{extension}"))
}
fn reject_symlink(path: &Utf8Path) -> Result<(), DiscordArchiveError> {
    match fs::symlink_metadata(path.as_std_path()) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(DiscordArchiveError::AttachmentIo {
                path: path.to_owned(),
                source: std::io::Error::other("symlink rejected"),
            })
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(attachment_io(path, source)),
    }
}
fn reject_symlink_ancestors(path: &Utf8Path) -> Result<(), DiscordArchiveError> {
    for ancestor in path.ancestors().skip(1) {
        match fs::symlink_metadata(ancestor.as_std_path()) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(attachment_io(
                    ancestor,
                    std::io::Error::other("symlink ancestor rejected"),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(attachment_io(ancestor, source)),
        }
    }
    Ok(())
}
fn attachment_io(path: &Utf8Path, source: std::io::Error) -> DiscordArchiveError {
    DiscordArchiveError::AttachmentIo {
        path: path.to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use test_case::test_case;
    use tokio::task::JoinHandle;

    fn http_json(status: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    async fn scripted_client(
        responses: Vec<Vec<u8>>,
    ) -> (
        DiscordArchiveClient,
        Arc<Mutex<Vec<String>>>,
        JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let server_calls = Arc::clone(&calls);
        let server = tokio::spawn(async move {
            for response in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0; 2048];
                loop {
                    let read = stream.read(&mut buffer).await.unwrap();
                    bytes.extend_from_slice(&buffer[..read]);
                    if read == 0 || bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8(bytes).unwrap();
                server_calls
                    .lock()
                    .unwrap()
                    .push(request.lines().next().unwrap().to_owned());
                stream.write_all(&response).await.unwrap();
            }
        });
        let client = DiscordArchiveClient::with_base_url(
            reqwest::Url::parse(&format!("http://{address}/")).unwrap(),
        )
        .unwrap();
        (client, calls, server)
    }

    fn empty_text_discovery() -> Vec<Vec<u8>> {
        vec![
            http_json(
                "200 OK",
                r#"{"id":"100","guild_id":"10","parent_id":null,"type":0}"#,
            ),
            http_json("200 OK", r#"{"threads":[]}"#),
            http_json("200 OK", r#"{"threads":[],"has_more":false}"#),
            http_json("200 OK", r#"{"threads":[],"has_more":false}"#),
            http_json("200 OK", r#"{"threads":[]}"#),
        ]
    }

    #[test]
    fn test_http_trace_endpoint_strips_credentials_query_and_fragment() {
        let url = reqwest::Url::parse(
            "https://user:password@cdn.discordapp.com/attachments/1/a.txt?ex=abc&sig=secret#frag",
        )
        .unwrap();
        let safe = safe_http_endpoint(&url);

        assert_eq!(safe, "https://cdn.discordapp.com/attachments/1/a.txt");
        assert!(!safe.contains("password"));
        assert!(!safe.contains("sig="));
        assert!(!safe.contains("frag"));
    }

    #[test]
    fn test_archive_cursors_require_typed_strict_backward_progress_from_fresh_state() {
        assert_eq!(
            advance_archive_timestamp_cursor(
                None,
                &mut std::collections::HashSet::new(),
                "2026-01-01T00:00:00Z".to_owned(),
            )
            .unwrap(),
            "2026-01-01T00:00:00Z"
        );
        assert!(
            advance_archive_timestamp_cursor(
                Some("2026-01-01T00:00:00Z"),
                &mut std::collections::HashSet::new(),
                "2027-01-01T00:00:00Z".to_owned(),
            )
            .is_err()
        );
        assert!(
            advance_archive_timestamp_cursor(
                None,
                &mut std::collections::HashSet::new(),
                "not-rfc3339".to_owned(),
            )
            .is_err()
        );
        assert_eq!(
            advance_archive_snowflake_cursor(
                None,
                &mut std::collections::HashSet::new(),
                "250".to_owned(),
            )
            .unwrap(),
            "250"
        );
        assert!(
            advance_archive_snowflake_cursor(
                Some("250"),
                &mut std::collections::HashSet::new(),
                "260".to_owned(),
            )
            .is_err()
        );
        assert!(
            advance_archive_snowflake_cursor(
                None,
                &mut std::collections::HashSet::new(),
                "not-a-snowflake".to_owned(),
            )
            .is_err()
        );
    }

    #[test_case("null"; "uncategorized root")]
    #[test_case("\"50\""; "category-owned root")]
    #[tokio::test]
    async fn test_capture_root_accepts_optional_category_parent(parent_id: &str) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let body = format!(r#"{{"id":"100","guild_id":"10","parent_id":{parent_id},"type":0}}"#);
        let response = http_json("200 OK", &body);
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let _ = stream.read(&mut request).await.unwrap();
            stream.write_all(&response).await.unwrap();
        });
        let client = DiscordArchiveClient::with_base_url(
            reqwest::Url::parse(&format!("http://{address}/")).unwrap(),
        )
        .unwrap();
        let root = client
            .capture_root("secret", "fixture", "10", "100")
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(root.parent_channel_id, "100");
        assert_eq!(root.kind, RootKind::Text);
    }

    #[tokio::test]
    async fn test_message_page_requires_verified_target_shape() {
        let target = CaptureTarget {
            channel_id: "2".to_owned(),
            thread_id: Some("2".to_owned()),
            thread_parent_channel_id: Some("1".to_owned()),
        };
        let error = DiscordArchiveClient::new()
            .raw_message_page("secret", &target, Some("1"), Some("2"))
            .await
            .unwrap_err();
        assert!(matches!(error, DiscordArchiveError::Shape));
    }

    #[test]
    fn test_message_batch_matches_oracle_attachment_and_reaction_shape() {
        let message = serde_json::json!({
            "id":"3", "content":"hello", "timestamp":"2026-01-01T00:00:00Z", "edited_timestamp":null,
            "author":{"id":"4"}, "attachments":[{"id":"5","filename":"a.txt","content_type":"text/plain","size":2,"url":"https://cdn.discordapp.com/a.txt"}],
            "reactions":[{"emoji":{"id":null,"name":"💜"},"count":2,"count_details":{"normal":2,"burst":0}}]
        });
        let events = message_batch(
            &message,
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
        )
        .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(
            serde_json::to_value(&events[0].payload.attachments[0]).unwrap(),
            serde_json::json!({
                "id":"5","filename":"a.txt","media_type":"text/plain","size":2,"sha256":null,"url":"https://cdn.discordapp.com/a.txt"
            })
        );
        assert_eq!(events[1].event_kind, EventKind::ReactionSnapshot);
        assert_eq!(events[1].payload.count, Some(2));
        assert_eq!(events[1].relations.reply_to_message_id, None);
        assert_eq!(
            events[1].ingest.raw_payload_sha256,
            "c679d9678790d608e061230a5e644112585c457dcf611de83053445caac71a0c"
        );
        assert!(events.iter().all(|event| event.origin_evidence.is_none()));
        assert!(
            serde_json::to_value(&events[0])
                .unwrap()
                .get("origin_evidence")
                .is_none()
        );
    }

    #[test]
    fn test_message_and_reaction_origin_selectors_resolve_without_harness_identity() {
        let message = serde_json::json!({
            "id":"3", "content":"hello", "author":{"id":"4"}, "attachments":[],
            "reactions":[{"emoji":{"id":null,"name":"💜"},"count":2}]
        });
        let page = serde_json::json!([{"id":"2"}, message.clone()]);
        let stored = StoredOriginEvidence {
            sha256: "a".repeat(64),
        };
        let origin = MessageOriginEvidence {
            page: &page,
            message_index: 1,
            stored: &stored,
        };

        let events = message_batch_with_origin(
            &message,
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

        let message_ref = events[0].origin_evidence.as_ref().unwrap();
        let reaction_ref = events[1].origin_evidence.as_ref().unwrap();
        assert_eq!(message_ref.selector, "/1");
        assert_eq!(reaction_ref.selector, "/1/reactions/0");
        assert_eq!(page.pointer(&message_ref.selector), Some(&message));
        assert_eq!(
            page.pointer(&reaction_ref.selector),
            message.pointer("/reactions/0")
        );
        assert_eq!(message_ref.adapter.name, DISCORD_HTTP_ADAPTER_NAME);
        assert_eq!(message_ref.adapter.version, DISCORD_HTTP_ADAPTER_VERSION);
        assert_eq!(message_ref.media_type, "application/json");
        assert!(message_ref.harness.is_none());
        assert!(
            serde_json::to_value(message_ref)
                .unwrap()
                .get("harness")
                .is_none()
        );
    }

    #[test]
    fn test_origin_selector_mismatch_fails_closed() {
        let message = serde_json::json!({"id":"3","attachments":[]});
        let wrong_page = serde_json::json!([{"id":"4","attachments":[]}]);
        let stored = StoredOriginEvidence {
            sha256: "a".repeat(64),
        };
        let origin = MessageOriginEvidence {
            page: &wrong_page,
            message_index: 0,
            stored: &stored,
        };

        let error = message_batch_with_origin(
            &message,
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
        .unwrap_err();
        assert!(matches!(error, DiscordArchiveError::Selector));
    }

    #[tokio::test]
    async fn test_message_page_retains_exact_successful_response_bytes() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let body = br#"[ { "z":null, "id":"3", "missing_is_absent": true } ]"#;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let response = http_json("200 OK", std::str::from_utf8(body).unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let _ = stream.read(&mut request).await.unwrap();
            stream.write_all(&response).await.unwrap();
        });
        let client = DiscordArchiveClient::with_base_url(
            reqwest::Url::parse(&format!("http://{address}/")).unwrap(),
        )
        .unwrap();
        let page = client
            .observed_message_page(
                "secret",
                &CaptureTarget {
                    channel_id: "1".to_owned(),
                    thread_id: None,
                    thread_parent_channel_id: None,
                },
                None,
                None,
            )
            .await
            .unwrap();
        server.await.unwrap();

        assert_eq!(page.exact_bytes(), body);
        assert_eq!(page.parsed.as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_embedded_thread_id_overrides_context() {
        let message = serde_json::json!({"id":"3","content":"x","author":{},"attachments":[],"thread":{"id":"99"}});
        let events = message_batch(
            &message,
            MessageContext {
                corpus_id: "fixture",
                guild_id: "1",
                channel_id: "2",
                thread_id: Some("88"),
                thread_parent_channel_id: None,
                observed_at: "2026-01-01T00:00:00Z",
            },
            1,
            &std::collections::HashMap::new(),
        )
        .unwrap();
        assert_eq!(events[0].source.thread_id.as_deref(), Some("99"));
        assert_eq!(
            events[0].relations.thread_parent_channel_id.as_deref(),
            Some("2")
        );
    }

    #[tokio::test]
    async fn test_attachment_download_is_content_addressed_and_verified() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nblob")
                .await
                .unwrap();
        });
        let base = reqwest::Url::parse(&format!("http://{address}/")).unwrap();
        let client = DiscordArchiveClient::with_base_url(base.clone()).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let directory = Utf8PathBuf::from_path_buf(temp.path().join("attachments")).unwrap();
        let digest = client
            .download_attachment(base.join("files/test.txt").unwrap().as_str(), &directory)
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(
            digest,
            "fa2c8cc4f28176bbeed4b736df569a34c79cd3723e9ec42f9674b4d46ac6b8b8"
        );
        assert_eq!(
            fs::read(directory.join(format!("{digest}.txt")).as_std_path()).unwrap(),
            b"blob"
        );
    }

    #[tokio::test]
    async fn test_gaie_archive_page_retries_429_with_retry_after() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for response in [
                b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice(),
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]".as_slice(),
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 1024];
                let _ = stream.read(&mut request).await.unwrap();
                stream.write_all(response).await.unwrap();
            }
        });
        let base = reqwest::Url::parse(&format!("http://{address}/")).unwrap();
        let client = DiscordArchiveClient::with_base_url(base).unwrap();
        assert!(
            client
                .raw_message_page(
                    "secret",
                    &CaptureTarget {
                        channel_id: "1".to_owned(),
                        thread_id: None,
                        thread_parent_channel_id: None,
                    },
                    None,
                    None,
                )
                .await
                .unwrap()
                .is_empty()
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn test_atom_1b_http_transcript_routes_cursors_and_verified_message_fetches() {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let server_calls = Arc::clone(&calls);
        let responses = vec![
            http_json(
                "200 OK",
                r#"{"id":"100","guild_id":"10","parent_id":null,"type":0}"#,
            ),
            http_json(
                "200 OK",
                r#"{"threads":[{"id":"300","guild_id":"10","parent_id":"100","type":11}]}"#,
            ),
            http_json(
                "200 OK",
                r#"{"threads":[{"id":"200","guild_id":"10","parent_id":"100","type":11,"thread_metadata":{"archive_timestamp":"2026-01-01T00:00:00Z"}}],"has_more":true}"#,
            ),
            http_json(
                "200 OK",
                r#"{"threads":[{"id":"220","guild_id":"10","parent_id":"100","type":11,"thread_metadata":{"archive_timestamp":"2025-01-01T00:00:00Z"}}],"has_more":false}"#,
            ),
            http_json("403 Forbidden", r#"{"message":"forbidden"}"#),
            http_json(
                "200 OK",
                r#"{"threads":[{"id":"250","guild_id":"10","parent_id":"100","type":12}],"has_more":true}"#,
            ),
            http_json(
                "200 OK",
                r#"{"threads":[{"id":"240","guild_id":"10","parent_id":"100","type":12}],"has_more":false}"#,
            ),
            http_json(
                "200 OK",
                r#"{"threads":[{"id":"400","guild_id":"10","parent_id":"100","type":11}]}"#,
            ),
            http_json("200 OK", "[]"),
            http_json("200 OK", "[]"),
            http_json("200 OK", "[]"),
            http_json("200 OK", "[]"),
            http_json("200 OK", "[]"),
            http_json("200 OK", "[]"),
            http_json("200 OK", "[]"),
        ];
        let server = tokio::spawn(async move {
            for response in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0; 2048];
                loop {
                    let read = stream.read(&mut buffer).await.unwrap();
                    bytes.extend_from_slice(&buffer[..read]);
                    if read == 0 || bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8(bytes).unwrap();
                server_calls
                    .lock()
                    .unwrap()
                    .push(request.lines().next().unwrap().to_owned());
                stream.write_all(&response).await.unwrap();
            }
        });

        let client = DiscordArchiveClient::with_base_url(
            reqwest::Url::parse(&format!("http://{address}/")).unwrap(),
        )
        .unwrap();
        let root = client
            .capture_root("secret", "fixture", "10", "100")
            .await
            .unwrap();
        let targets = client.discover_targets("secret", &root).await.unwrap();
        for target in &targets {
            assert!(
                client
                    .raw_message_page("secret", target, None, None)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        server.await.unwrap();

        assert_eq!(
            targets
                .iter()
                .map(CaptureTarget::channel_id)
                .collect::<Vec<_>>(),
            vec!["100", "200", "220", "240", "250", "300", "400"]
        );
        let calls = calls.lock().unwrap();
        assert_eq!(calls[0], "GET /channels/100 HTTP/1.1");
        assert_eq!(calls[1], "GET /guilds/10/threads/active HTTP/1.1");
        assert_eq!(
            calls[2],
            "GET /channels/100/threads/archived/public HTTP/1.1"
        );
        assert!(calls[3].contains("before=2026-01-01T00%3A00%3A00Z"));
        assert_eq!(
            calls[4],
            "GET /channels/100/threads/archived/private HTTP/1.1"
        );
        assert_eq!(
            calls[5],
            "GET /channels/100/users/@me/threads/archived/private HTTP/1.1"
        );
        assert!(calls[6].contains("before=250"));
        assert_eq!(calls[7], "GET /guilds/10/threads/active HTTP/1.1");
        assert_eq!(
            &calls[8..],
            [
                "GET /channels/100/messages?limit=100 HTTP/1.1",
                "GET /channels/200/messages?limit=100 HTTP/1.1",
                "GET /channels/220/messages?limit=100 HTTP/1.1",
                "GET /channels/240/messages?limit=100 HTTP/1.1",
                "GET /channels/250/messages?limit=100 HTTP/1.1",
                "GET /channels/300/messages?limit=100 HTTP/1.1",
                "GET /channels/400/messages?limit=100 HTTP/1.1",
            ]
        );
    }

    #[tokio::test]
    async fn test_incompatible_thread_type_fails_before_any_child_message_fetch() {
        let responses = vec![
            http_json(
                "200 OK",
                r#"{"id":"100","guild_id":"10","parent_id":null,"type":0}"#,
            ),
            http_json(
                "200 OK",
                r#"{"threads":[{"id":"200","guild_id":"10","parent_id":"100","type":10}]}"#,
            ),
            http_json("200 OK", r#"{"threads":[],"has_more":false}"#),
            http_json("200 OK", r#"{"threads":[],"has_more":false}"#),
            http_json("200 OK", r#"{"threads":[]}"#),
        ];
        let (client, calls, server) = scripted_client(responses).await;
        let root = client
            .capture_root("secret", "fixture", "10", "100")
            .await
            .unwrap();
        let error = client.discover_targets("secret", &root).await.unwrap_err();
        server.await.unwrap();

        assert!(matches!(error, DiscordArchiveError::Shape));
        assert_eq!(calls.lock().unwrap().len(), 5);
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .all(|call| !call.contains("/messages"))
        );
    }

    #[tokio::test]
    async fn test_v1_checkpoint_is_persisted_as_v2_once_on_remote_noop() {
        use crate::gaie::{Archive, ArchivePaths, BackfillOptions, CorpusId, run_backfill};

        let temp = tempfile::tempdir().unwrap();
        let directory = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(directory.clone(), &corpus).unwrap();
        Archive::open(paths.clone(), corpus.clone(), "2026-07-21T00:00:00Z").unwrap();
        let checkpoint_path = directory.join("fixture.checkpoint.json");
        let v1 = br#"{"corpus_id":"fixture","channel_id":"100","after_message_id":"123","updated_at":"2026-07-21T00:00:00Z"}"#;
        fs::write(checkpoint_path.as_std_path(), v1).unwrap();

        let mut first_responses = empty_text_discovery();
        first_responses.push(http_json("200 OK", "[]"));
        let (first_client, _, first_server) = scripted_client(first_responses).await;
        let first_added = run_backfill(
            &first_client,
            "secret",
            BackfillOptions::new("fixture", "10", "100", false),
            paths.clone(),
            corpus.clone(),
            "2026-07-21T00:00:00Z",
        )
        .await
        .unwrap();
        first_server.await.unwrap();
        let migrated = fs::read(checkpoint_path.as_std_path()).unwrap();
        let migrated_value: Value = serde_json::from_slice(&migrated).unwrap();

        assert_eq!(first_added, 0);
        assert_ne!(migrated, v1);
        assert_eq!(migrated_value["version"], 2);
        assert_eq!(migrated_value["streams"]["100"]["after_message_id"], "123");

        let mut second_responses = empty_text_discovery();
        second_responses.push(http_json("200 OK", "[]"));
        let (second_client, _, second_server) = scripted_client(second_responses).await;
        let second_added = run_backfill(
            &second_client,
            "secret",
            BackfillOptions::new("fixture", "10", "100", false),
            paths,
            corpus,
            "2026-07-21T00:00:00Z",
        )
        .await
        .unwrap();
        second_server.await.unwrap();

        assert_eq!(second_added, 0);
        assert_eq!(fs::read(checkpoint_path.as_std_path()).unwrap(), migrated);
    }

    #[tokio::test]
    async fn test_atom_1b_library_backfill_transcript_reaches_durable_archive() {
        use crate::gaie::{
            Archive, ArchivePaths, BackfillOptions, CorpusId, EventKind, run_backfill,
        };
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let page_body = r#"[{"id":"150","channel_id":"100","content":"receipt","author":{"id":"9"},"attachments":[],"reactions":[]}]"#;
        let responses = vec![
            http_json(
                "200 OK",
                r#"{"id":"100","guild_id":"10","parent_id":null,"type":0}"#,
            ),
            http_json("200 OK", r#"{"threads":[]}"#),
            http_json("200 OK", r#"{"threads":[],"has_more":false}"#),
            http_json("200 OK", r#"{"threads":[],"has_more":false}"#),
            http_json("200 OK", r#"{"threads":[]}"#),
            http_json("200 OK", page_body),
        ];
        let server = tokio::spawn(async move {
            for response in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0; 2048];
                loop {
                    let read = stream.read(&mut buffer).await.unwrap();
                    bytes.extend_from_slice(&buffer[..read]);
                    if read == 0 || bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                stream.write_all(&response).await.unwrap();
            }
        });
        let client = DiscordArchiveClient::with_base_url(
            reqwest::Url::parse(&format!("http://{address}/")).unwrap(),
        )
        .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let directory = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(directory.clone(), &corpus).unwrap();

        let added = run_backfill(
            &client,
            "secret",
            BackfillOptions::new("fixture", "10", "100", false),
            paths.clone(),
            corpus.clone(),
            "2026-07-21T00:00:00Z",
        )
        .await
        .unwrap();
        server.await.unwrap();
        let archive = Archive::open(paths, corpus, "2026-07-21T00:00:00Z").unwrap();
        let events = archive.read_committed().unwrap().events;
        let checkpoint = archive.load_checkpoint("10", "100").unwrap().unwrap();

        assert_eq!(added, 1);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_kind, EventKind::MessageCreate);
        assert_eq!(events[0].source.message_id, "150");
        let origin = events[0].origin_evidence.as_ref().unwrap();
        assert_eq!(origin.selector, "/0");
        assert!(origin.harness.is_none());
        assert_eq!(origin.sha256, hash_bytes(page_body.as_bytes()));
        assert_eq!(
            fs::read(directory.join(&origin.location).as_std_path()).unwrap(),
            page_body.as_bytes()
        );
        assert_eq!(
            checkpoint.streams["100"].after_message_id.as_deref(),
            Some("150")
        );
    }

    #[tokio::test]
    async fn test_atom_1b_multirun_backfill_preserves_alias_relations_and_noop_bytes() {
        use crate::gaie::{
            Archive, ArchivePaths, BackfillOptions, CorpusId, EventKind, run_backfill,
        };

        let temp = tempfile::tempdir().unwrap();
        let directory = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(directory.clone(), &corpus).unwrap();

        let run_one_responses = vec![
            http_json(
                "200 OK",
                r#"{"id":"100","guild_id":"10","parent_id":"50","type":0}"#,
            ),
            http_json(
                "200 OK",
                r#"{"threads":[{"id":"300","guild_id":"10","parent_id":"100","type":11}]}"#,
            ),
            http_json(
                "200 OK",
                r#"{"threads":[{"id":"200","guild_id":"10","parent_id":"100","type":11}],"has_more":false}"#,
            ),
            http_json("200 OK", r#"{"threads":[],"has_more":false}"#),
            http_json(
                "200 OK",
                r#"{"threads":[{"id":"300","guild_id":"10","parent_id":"100","type":11}]}"#,
            ),
            http_json(
                "200 OK",
                r#"[{"id":"200","channel_id":"100","content":"starter","author":{"id":"9"},"attachments":[],"reactions":[],"thread":{"id":"200"}}]"#,
            ),
            http_json(
                "200 OK",
                r#"[{"id":"201","channel_id":"200","content":"reply","author":{"id":"9"},"attachments":[],"reactions":[]},{"id":"200","channel_id":"200","content":"starter alias","author":{"id":"9"},"attachments":[],"reactions":[]}]"#,
            ),
            http_json(
                "200 OK",
                r#"[{"id":"301","channel_id":"300","content":"active child","author":{"id":"9"},"attachments":[],"reactions":[]}]"#,
            ),
        ];
        let (run_one_client, _, run_one_server) = scripted_client(run_one_responses).await;
        let first_added = run_backfill(
            &run_one_client,
            "secret",
            BackfillOptions::new("fixture", "10", "100", false),
            paths.clone(),
            corpus.clone(),
            "2026-07-21T00:00:00Z",
        )
        .await
        .unwrap();
        run_one_server.await.unwrap();

        let archive = Archive::open(paths.clone(), corpus.clone(), "2026-07-21T00:00:00Z").unwrap();
        let events = archive.read_committed().unwrap().events;
        let create_ids: Vec<_> = events
            .iter()
            .filter(|event| event.event_kind == EventKind::MessageCreate)
            .map(|event| event.source.message_id.as_str())
            .collect();
        let starter = events
            .iter()
            .find(|event| event.source.message_id == "200")
            .unwrap();
        assert_eq!(first_added, 3);
        assert_eq!(create_ids, ["200", "201", "301"]);
        assert_eq!(starter.source.thread_id.as_deref(), Some("200"));
        assert_eq!(
            starter.relations.thread_parent_channel_id.as_deref(),
            Some("100")
        );

        let mut stale = archive.load_checkpoint("10", "100").unwrap().unwrap();
        stale.streams.get_mut("300").unwrap().after_message_id = Some("300".to_owned());
        archive.save_checkpoint(&stale).unwrap();
        drop(archive);

        let run_two_responses = vec![
            http_json(
                "200 OK",
                r#"{"id":"100","guild_id":"10","parent_id":"50","type":0}"#,
            ),
            http_json(
                "200 OK",
                r#"{"threads":[{"id":"300","guild_id":"10","parent_id":"100","type":11},{"id":"400","guild_id":"10","parent_id":"100","type":11}]}"#,
            ),
            http_json(
                "200 OK",
                r#"{"threads":[{"id":"200","guild_id":"10","parent_id":"100","type":11}],"has_more":false}"#,
            ),
            http_json("200 OK", r#"{"threads":[],"has_more":false}"#),
            http_json(
                "200 OK",
                r#"{"threads":[{"id":"300","guild_id":"10","parent_id":"100","type":11},{"id":"400","guild_id":"10","parent_id":"100","type":11}]}"#,
            ),
            http_json("200 OK", "[]"),
            http_json("200 OK", "[]"),
            http_json("200 OK", "[]"),
            http_json(
                "200 OK",
                r#"[{"id":"401","channel_id":"400","content":"new child","author":{"id":"9"},"attachments":[],"reactions":[]}]"#,
            ),
        ];
        let (run_two_client, run_two_calls, run_two_server) =
            scripted_client(run_two_responses).await;
        let second_added = run_backfill(
            &run_two_client,
            "secret",
            BackfillOptions::new("fixture", "10", "100", false),
            paths.clone(),
            corpus.clone(),
            "2026-07-21T00:00:00Z",
        )
        .await
        .unwrap();
        run_two_server.await.unwrap();
        {
            let calls = run_two_calls.lock().unwrap();
            assert_eq!(second_added, 1);
            assert!(calls[5].contains("/channels/100/messages?limit=100&after=200"));
            assert!(calls[6].contains("/channels/200/messages?limit=100&after=201"));
            assert!(calls[7].contains("/channels/300/messages?limit=100&after=300"));
            assert_eq!(calls[8], "GET /channels/400/messages?limit=100 HTTP/1.1");
        }

        let archive = Archive::open(paths.clone(), corpus.clone(), "2026-07-21T00:00:00Z").unwrap();
        let checkpoint = archive.load_checkpoint("10", "100").unwrap().unwrap();
        assert_eq!(
            checkpoint.streams["300"].after_message_id.as_deref(),
            Some("301")
        );
        assert_eq!(
            checkpoint.streams["400"].after_message_id.as_deref(),
            Some("401")
        );
        assert_eq!(archive.read_committed().unwrap().events.len(), 4);
        drop(archive);

        let events_path = directory.join("fixture.ndjson");
        let checkpoint_path = directory.join("fixture.checkpoint.json");
        let events_before_noop = fs::read(events_path.as_std_path()).unwrap();
        let checkpoint_before_noop = fs::read(checkpoint_path.as_std_path()).unwrap();
        let mut run_three_responses = vec![
            http_json(
                "200 OK",
                r#"{"id":"100","guild_id":"10","parent_id":"50","type":0}"#,
            ),
            http_json(
                "200 OK",
                r#"{"threads":[{"id":"300","guild_id":"10","parent_id":"100","type":11},{"id":"400","guild_id":"10","parent_id":"100","type":11}]}"#,
            ),
            http_json(
                "200 OK",
                r#"{"threads":[{"id":"200","guild_id":"10","parent_id":"100","type":11}],"has_more":false}"#,
            ),
            http_json("200 OK", r#"{"threads":[],"has_more":false}"#),
            http_json(
                "200 OK",
                r#"{"threads":[{"id":"300","guild_id":"10","parent_id":"100","type":11},{"id":"400","guild_id":"10","parent_id":"100","type":11}]}"#,
            ),
        ];
        run_three_responses.extend((0..4).map(|_| http_json("200 OK", "[]")));
        let (run_three_client, _, run_three_server) = scripted_client(run_three_responses).await;
        let third_added = run_backfill(
            &run_three_client,
            "secret",
            BackfillOptions::new("fixture", "10", "100", false),
            paths,
            corpus,
            "2026-07-21T00:00:00Z",
        )
        .await
        .unwrap();
        run_three_server.await.unwrap();

        assert_eq!(third_added, 0);
        assert_eq!(
            fs::read(events_path.as_std_path()).unwrap(),
            events_before_noop
        );
        assert_eq!(
            fs::read(checkpoint_path.as_std_path()).unwrap(),
            checkpoint_before_noop
        );
    }

    #[tokio::test]
    async fn test_incremental_short_after_page_wrong_direction_fails_closed() {
        use crate::gaie::{
            Archive, ArchivePaths, BackfillOptions, BackfillRunError, Checkpoint, CorpusId,
            StreamCheckpoint, run_backfill,
        };
        use std::collections::BTreeMap;

        let temp = tempfile::tempdir().unwrap();
        let directory = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(directory, &corpus).unwrap();
        let archive = Archive::open(paths.clone(), corpus.clone(), "2026-07-21T00:00:00Z").unwrap();
        archive
            .save_checkpoint(&Checkpoint {
                version: 2,
                corpus_id: "fixture".to_owned(),
                guild_id: "10".to_owned(),
                parent_channel_id: "100".to_owned(),
                streams: BTreeMap::from([(
                    "100".to_owned(),
                    StreamCheckpoint {
                        after_message_id: Some("100".to_owned()),
                    },
                )]),
                updated_at: "2026-07-21T00:00:00Z".to_owned(),
            })
            .unwrap();
        drop(archive);

        let mut responses = empty_text_discovery();
        responses.push(http_json(
            "200 OK",
            r#"[{"id":"99","channel_id":"100","content":"wrong side","author":{"id":"9"},"attachments":[],"reactions":[]}]"#,
        ));
        let (client, calls, server) = scripted_client(responses).await;
        let error = run_backfill(
            &client,
            "secret",
            BackfillOptions::new("fixture", "10", "100", false),
            paths,
            corpus,
            "2026-07-21T00:00:00Z",
        )
        .await
        .unwrap_err();
        server.await.unwrap();

        assert!(matches!(error, BackfillRunError::InvalidMessage));
        assert!(calls.lock().unwrap()[5].contains("after=100"));
    }

    #[tokio::test]
    async fn test_fresh_backfill_short_before_page_wrong_direction_fails_closed() {
        use crate::gaie::{
            ArchivePaths, BackfillOptions, BackfillRunError, CorpusId, run_backfill,
        };

        let page = serde_json::to_string(
            &(101_u64..=200)
                .rev()
                .map(|id| {
                    serde_json::json!({
                        "id": id.to_string(),
                        "channel_id": "100",
                        "content": id.to_string(),
                        "author": {"id":"9"},
                        "attachments": [],
                        "reactions": []
                    })
                })
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let directory = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(directory, &corpus).unwrap();
        let mut responses = empty_text_discovery();
        responses.push(http_json("200 OK", &page));
        responses.push(http_json(
            "200 OK",
            r#"[{"id":"101","channel_id":"100","content":"repeated bound","author":{"id":"9"},"attachments":[],"reactions":[]}]"#,
        ));
        let (client, calls, server) = scripted_client(responses).await;
        let error = run_backfill(
            &client,
            "secret",
            BackfillOptions::new("fixture", "10", "100", false),
            paths,
            corpus,
            "2026-07-21T00:00:00Z",
        )
        .await
        .unwrap_err();
        server.await.unwrap();

        assert!(matches!(error, BackfillRunError::InvalidMessage));
        assert!(calls.lock().unwrap()[6].contains("before=101"));
    }

    #[tokio::test]
    async fn test_incremental_backfill_paginates_backward_over_150_new_messages_exactly_once() {
        use crate::gaie::{
            Archive, ArchivePaths, BackfillOptions, Checkpoint, CorpusId, StreamCheckpoint,
            run_backfill,
        };
        use std::collections::{BTreeMap, BTreeSet};
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let temp = tempfile::tempdir().unwrap();
        let directory = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(directory, &corpus).unwrap();
        {
            let archive =
                Archive::open(paths.clone(), corpus.clone(), "2026-07-21T00:00:00Z").unwrap();
            archive
                .save_checkpoint(&Checkpoint {
                    version: 2,
                    corpus_id: "fixture".to_owned(),
                    guild_id: "10".to_owned(),
                    parent_channel_id: "100".to_owned(),
                    streams: BTreeMap::from([(
                        "100".to_owned(),
                        StreamCheckpoint {
                            after_message_id: Some("100".to_owned()),
                        },
                    )]),
                    updated_at: "2026-07-21T00:00:00Z".to_owned(),
                })
                .unwrap();
        }

        let message_page = |ids: Vec<u64>| {
            serde_json::to_string(
                &ids.into_iter()
                    .map(|id| {
                        serde_json::json!({
                            "id":id.to_string(), "channel_id":"100", "content":id.to_string(),
                            "author":{"id":"9"}, "attachments":[], "reactions":[]
                        })
                    })
                    .collect::<Vec<_>>(),
            )
            .unwrap()
        };
        let first = message_page((151..=250).rev().collect());
        let second = message_page((101..=150).rev().collect());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let server_calls = Arc::clone(&calls);
        let responses = vec![
            http_json(
                "200 OK",
                r#"{"id":"100","guild_id":"10","parent_id":"50","type":0}"#,
            ),
            http_json("200 OK", r#"{"threads":[]}"#),
            http_json("200 OK", r#"{"threads":[],"has_more":false}"#),
            http_json("200 OK", r#"{"threads":[],"has_more":false}"#),
            http_json("200 OK", r#"{"threads":[]}"#),
            http_json("200 OK", &first),
            http_json("200 OK", &second),
        ];
        let server = tokio::spawn(async move {
            for response in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0; 2048];
                loop {
                    let read = stream.read(&mut buffer).await.unwrap();
                    bytes.extend_from_slice(&buffer[..read]);
                    if read == 0 || bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8(bytes).unwrap();
                server_calls
                    .lock()
                    .unwrap()
                    .push(request.lines().next().unwrap().to_owned());
                stream.write_all(&response).await.unwrap();
            }
        });
        let client = DiscordArchiveClient::with_base_url(
            reqwest::Url::parse(&format!("http://{address}/")).unwrap(),
        )
        .unwrap();
        let added = run_backfill(
            &client,
            "secret",
            BackfillOptions::new("fixture", "10", "100", false),
            paths.clone(),
            corpus.clone(),
            "2026-07-21T00:00:00Z",
        )
        .await
        .unwrap();
        server.await.unwrap();

        let archive = Archive::open(paths, corpus, "2026-07-21T00:00:00Z").unwrap();
        let events = archive.read_committed().unwrap().events;
        let ids: BTreeSet<_> = events
            .iter()
            .filter(|event| matches!(event.event_kind, EventKind::MessageCreate))
            .map(|event| event.source.message_id.parse::<u64>().unwrap())
            .collect();
        assert_eq!(added, 150);
        assert_eq!(ids, (101_u64..=250).collect::<BTreeSet<_>>());
        let calls = calls.lock().unwrap();
        assert!(calls[5].contains("after=100"));
        assert!(calls[6].contains("before=151"));
    }

    #[tokio::test]
    async fn test_run_backfill_repairs_stale_stream_cursor_from_durable_commit_when_remote_empty() {
        use crate::gaie::{
            Archive, ArchivePaths, BackfillOptions, Checkpoint, CorpusId, StreamCheckpoint,
            run_backfill,
        };
        use std::collections::BTreeMap;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let temp = tempfile::tempdir().unwrap();
        let directory = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let corpus = CorpusId::parse("fixture").unwrap();
        let paths = ArchivePaths::new(directory, &corpus).unwrap();
        {
            let mut archive =
                Archive::open(paths.clone(), corpus.clone(), "2026-07-21T00:00:00Z").unwrap();
            let message = serde_json::json!({
                "id":"160", "channel_id":"100", "content":"already committed",
                "author":{"id":"9"}, "attachments":[], "reactions":[]
            });
            let batch = message_batch(
                &message,
                MessageContext {
                    corpus_id: "fixture",
                    guild_id: "10",
                    channel_id: "100",
                    thread_id: None,
                    thread_parent_channel_id: None,
                    observed_at: "2026-07-21T00:00:00Z",
                },
                1,
                &std::collections::HashMap::new(),
            )
            .unwrap();
            archive
                .append_batch(&batch, "160", "2026-07-21T00:00:00Z")
                .unwrap();
            archive
                .save_checkpoint(&Checkpoint {
                    version: 2,
                    corpus_id: "fixture".to_owned(),
                    guild_id: "10".to_owned(),
                    parent_channel_id: "100".to_owned(),
                    streams: BTreeMap::from([(
                        "100".to_owned(),
                        StreamCheckpoint {
                            after_message_id: Some("150".to_owned()),
                        },
                    )]),
                    updated_at: "2026-07-21T00:00:00Z".to_owned(),
                })
                .unwrap();
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let responses = vec![
            http_json(
                "200 OK",
                r#"{"id":"100","guild_id":"10","parent_id":null,"type":0}"#,
            ),
            http_json("200 OK", r#"{"threads":[]}"#),
            http_json("200 OK", r#"{"threads":[],"has_more":false}"#),
            http_json("200 OK", r#"{"threads":[],"has_more":false}"#),
            http_json("200 OK", r#"{"threads":[]}"#),
            http_json("200 OK", "[]"),
        ];
        let server = tokio::spawn(async move {
            for response in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 2048];
                let _ = stream.read(&mut request).await.unwrap();
                stream.write_all(&response).await.unwrap();
            }
        });
        let client = DiscordArchiveClient::with_base_url(
            reqwest::Url::parse(&format!("http://{address}/")).unwrap(),
        )
        .unwrap();
        let added = run_backfill(
            &client,
            "secret",
            BackfillOptions::new("fixture", "10", "100", false),
            paths.clone(),
            corpus.clone(),
            "2026-07-21T00:00:00Z",
        )
        .await
        .unwrap();
        server.await.unwrap();
        let archive = Archive::open(paths, corpus, "2026-07-21T00:00:00Z").unwrap();
        let checkpoint = archive.load_checkpoint("10", "100").unwrap().unwrap();

        assert_eq!(added, 0);
        assert_eq!(archive.read_committed().unwrap().events.len(), 1);
        assert!(
            fs::read_dir(temp.path().join("origin-evidence"))
                .unwrap()
                .next()
                .is_none(),
            "terminal empty message pages must not create orphan evidence objects"
        );
        assert_eq!(
            checkpoint.streams["100"].after_message_id.as_deref(),
            Some("160")
        );
    }
}
