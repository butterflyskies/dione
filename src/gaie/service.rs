use crate::gaie::{Attachment, Event, EventKind, Ingest, Lineage, Payload, Relations, Source};
use camino::{Utf8Path, Utf8PathBuf};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::net::IpAddr;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::time::Duration;
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

    /// Fetches one raw message page for exactly the configured parent channel.
    pub async fn raw_message_page(
        &self,
        token: &str,
        allowed_parent: &str,
        channel_id: &str,
        before: Option<&str>,
        after: Option<&str>,
    ) -> Result<Vec<Value>, DiscordArchiveError> {
        if channel_id != allowed_parent {
            return Err(DiscordArchiveError::OutsideScope {
                requested: channel_id.to_owned(),
                allowed: allowed_parent.to_owned(),
            });
        }
        if before.is_some() && after.is_some() {
            return Err(DiscordArchiveError::Shape);
        }
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
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| DiscordArchiveError::Shape)?;
        value.as_array().cloned().ok_or(DiscordArchiveError::Shape)
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
                let response = retry.send().await.map_err(DiscordArchiveError::Request)?;
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
}

fn archive_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| unreachable!("static archive HTTP client configuration is valid"))
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
    let message_id = string_field(message, "id")?;
    let edited_at = optional_string(message, "edited_timestamp");
    let content = optional_string(message, "content");
    let raw_hash = value_hash(message, false);
    let embedded_thread_id = message.pointer("/thread/id").and_then(Value::as_str);
    let source = Source {
        platform: "discord".to_owned(),
        guild_id: context.guild_id.to_owned(),
        channel_id: context.channel_id.to_owned(),
        thread_id: embedded_thread_id.or(context.thread_id).map(str::to_owned),
        message_id: message_id.clone(),
        actor_id: message
            .pointer("/author/id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        created_at: optional_string(message, "timestamp"),
        edited_at: edited_at.clone(),
    };
    let attachments = message
        .get("attachments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|attachment| Attachment {
            id: attachment
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            filename: attachment
                .get("filename")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            media_type: attachment
                .get("content_type")
                .and_then(Value::as_str)
                .map(str::to_owned),
            size: attachment.get("size").and_then(Value::as_u64).unwrap_or(0),
            sha256: attachment
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| attachment_hashes.get(id))
                .cloned(),
            url: attachment
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        })
        .collect();
    let relations = Relations {
        reply_to_message_id: message
            .pointer("/message_reference/message_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        thread_parent_channel_id: context.thread_parent_channel_id.map(str::to_owned),
    };
    let lineage = Lineage {
        observed_version_ordinal: None,
        predecessor_event_id: None,
        history_status: if edited_at.is_some() {
            "unknown_prior"
        } else {
            "complete"
        }
        .to_owned(),
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
            content: content.clone(),
            content_sha256: content.as_deref().map(hash),
            attachments,
            emoji_id: None,
            count: None,
            normal_count: None,
            burst_count: None,
        },
        relations: relations.clone(),
        lineage,
        ingest: Ingest {
            collector_version: "gaie-alpha-0.8.0".to_owned(),
            raw_payload_sha256: raw_hash,
        },
    }];
    for (offset, reaction) in message
        .get("reactions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let emoji_name = reaction
            .pointer("/emoji/name")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let emoji_id = reaction
            .pointer("/emoji/id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let key = emoji_id
            .as_ref()
            .map_or_else(|| emoji_name.to_owned(), |id| format!("{id}:{emoji_name}"));
        let count = reaction.get("count").and_then(Value::as_u64).unwrap_or(0);
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
                content: Some(emoji_name.to_owned()),
                content_sha256: Some(hash(&key)),
                attachments: vec![],
                emoji_id,
                count: Some(count),
                normal_count: Some(
                    reaction
                        .pointer("/count_details/normal")
                        .and_then(Value::as_u64)
                        .unwrap_or(count),
                ),
                burst_count: Some(
                    reaction
                        .pointer("/count_details/burst")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                ),
            },
            relations: Relations {
                reply_to_message_id: None,
                thread_parent_channel_id: context.thread_parent_channel_id.map(str::to_owned),
            },
            lineage: Lineage {
                observed_version_ordinal: None,
                predecessor_event_id: None,
                history_status: "complete".to_owned(),
            },
            ingest: Ingest {
                collector_version: "gaie-alpha-0.8.0".to_owned(),
                raw_payload_sha256: value_hash(reaction, true),
            },
        });
    }
    Ok(events)
}

fn string_field(value: &Value, field: &str) -> Result<String, DiscordArchiveError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(DiscordArchiveError::Shape)
}
fn optional_string(value: &Value, field: &str) -> Option<String> {
    value.get(field).and_then(Value::as_str).map(str::to_owned)
}
fn hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
fn value_hash(value: &Value, ensure_ascii: bool) -> String {
    hash(&python_json(value, ensure_ascii))
}

fn python_json(value: &Value, ensure_ascii: bool) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(value) => if *value { "true" } else { "false" }.to_owned(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => json_string(value, ensure_ascii),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(|value| python_json(value, ensure_ascii))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) => {
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_by_key(|(key, _)| *key);
            format!(
                "{{{}}}",
                entries
                    .into_iter()
                    .map(|(key, value)| {
                        format!(
                            "{}: {}",
                            json_string(key, ensure_ascii),
                            python_json(value, ensure_ascii)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }
}

fn json_string(value: &str, ensure_ascii: bool) -> String {
    let encoded = serde_json::to_string(value).unwrap_or_default();
    if !ensure_ascii {
        return encoded;
    }
    let mut ascii = String::new();
    for character in encoded.chars() {
        if character.is_ascii() {
            ascii.push(character);
        } else {
            let code = character as u32;
            if code <= 0xffff {
                ascii.push_str(&format!("\\u{code:04x}"));
            } else {
                let adjusted = code - 0x1_0000;
                ascii.push_str(&format!(
                    "\\u{:04x}\\u{:04x}",
                    0xd800 + (adjusted >> 10),
                    0xdc00 + (adjusted & 0x3ff)
                ));
            }
        }
    }
    ascii
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

    #[tokio::test]
    async fn test_allowlist_rejects_unowned_channel_before_http() {
        let error = DiscordArchiveClient::new()
            .raw_message_page("secret", "1", "2", None, None)
            .await
            .unwrap_err();
        assert!(matches!(error, DiscordArchiveError::OutsideScope { .. }));
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
                .raw_message_page("secret", "1", "1", None, None)
                .await
                .unwrap()
                .is_empty()
        );
        server.await.unwrap();
    }
}
