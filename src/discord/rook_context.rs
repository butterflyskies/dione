//! Recipient context through Discord's existing authorized query boundary.

use std::sync::Arc;

use futures_util::future::BoxFuture;
use serenity::{
    http::HttpError,
    model::id::{ChannelId, MessageId},
};

use super::{
    messaging::{MessagingCtx, MessagingError, MessagingErrorKind, queries},
    rook_observation::native_supplement,
};
use crate::{
    core::{ModuleInstanceId, ModuleSlotId, ProviderId},
    rook::{
        ContextCapabilities, ContextDirection, ContextError, ContextErrorKind, ContextObservation,
        ContextPage, ContextProvider, ContextQuery, ContextResponse, ConversationKey,
    },
};

/// Discord context reader bound to one configured module slot.
///
/// The trusted embedding supplies a factory that loads current configuration
/// for every request. Recipient authorization belongs to the Rook service;
/// this adapter additionally enforces Dione's current channel policy.
pub struct DiscordContextProvider {
    source: ModuleInstanceId,
    context: Arc<dyn Fn() -> MessagingCtx + Send + Sync>,
}

impl DiscordContextProvider {
    /// Binds the Discord module slot and a current-configuration factory.
    ///
    /// # Errors
    /// Returns `InvalidReference` for an empty module slot.
    pub fn new(
        slot: &str,
        context: Arc<dyn Fn() -> MessagingCtx + Send + Sync>,
    ) -> Result<Self, ContextError> {
        let provider = ProviderId::new("discord")
            .map_err(|_| ContextError::new(ContextErrorKind::InvalidReference))?;
        let slot = ModuleSlotId::new(slot)
            .map_err(|_| ContextError::new(ContextErrorKind::InvalidReference))?;
        Ok(Self {
            source: ModuleInstanceId::new(provider, slot),
            context,
        })
    }

    fn channel(&self, room: &ConversationKey) -> Result<ChannelId, ContextError> {
        if !room.matches_source(&self.source) {
            return Err(ContextError::new(ContextErrorKind::Unsupported));
        }
        room.numeric()
            .map(|id| ChannelId::new(id.get()))
            .ok_or_else(|| ContextError::new(ContextErrorKind::InvalidReference))
    }

    fn project(
        &self,
        message: &queries::MessageView,
        thread_parent: Option<ChannelId>,
    ) -> Result<ContextObservation, ContextError> {
        let common = message
            .context(&self.source)
            .map_err(|error| ContextError::with_cause(ContextErrorKind::Provider, error))?;
        let native = native_supplement(&common, message.native(), thread_parent)
            .map_err(|error| ContextError::with_cause(ContextErrorKind::Provider, error))?;
        ContextObservation::capture(&common)
            .with_native_supplement(native)
            .map_err(|error| ContextError::with_cause(ContextErrorKind::InvalidObservation, error))
    }
}

impl ContextProvider for DiscordContextProvider {
    fn capabilities(&self) -> ContextCapabilities {
        ContextCapabilities {
            exact: true,
            history: true,
            max_page_size: 100,
        }
    }

    fn read<'a>(
        &'a self,
        request: &'a ContextQuery,
    ) -> BoxFuture<'a, Result<ContextResponse, ContextError>> {
        Box::pin(async move {
            request.validate()?;
            let channel = self.channel(request.room())?;
            let ctx = (self.context)();
            let response = match request {
                ContextQuery::Exact(target) => {
                    let id = target
                        .numeric()
                        .ok_or_else(|| ContextError::new(ContextErrorKind::InvalidReference))?;
                    let result = queries::get_message(&ctx, channel, MessageId::new(id.get()))
                        .await
                        .map_err(map_error)?;
                    Ok::<_, ContextError>(ContextResponse::exact(
                        self.project(result.message(), thread_parent(&ctx, channel).await)?,
                    ))
                }
                ContextQuery::History {
                    direction, limit, ..
                } => {
                    let anchor = match direction {
                        ContextDirection::Recent => None,
                        ContextDirection::Before(key) | ContextDirection::After(key) => {
                            Some(MessageId::new(
                                key.numeric()
                                    .ok_or_else(|| {
                                        ContextError::new(ContextErrorKind::InvalidReference)
                                    })?
                                    .get(),
                            ))
                        }
                    };
                    let (before, after) = match direction {
                        ContextDirection::Recent => (None, None),
                        ContextDirection::Before(_) => (anchor, None),
                        ContextDirection::After(_) => (None, anchor),
                    };
                    let result = queries::fetch_messages(&ctx, channel, before, after, limit.get())
                        .await
                        .map_err(map_error)?;
                    let parent = thread_parent(&ctx, channel).await;
                    let messages = result
                        .messages()
                        .iter()
                        .map(|message| self.project(message, parent))
                        .collect::<Result<Vec<_>, _>>()?;
                    let has_more = messages.len() == usize::from(limit.get());
                    Ok(ContextResponse::history(ContextPage::new(
                        request.clone(),
                        messages,
                        has_more,
                    )?))
                }
            }?;
            super::messaging::check_outbound(&(self.context)(), channel)
                .await
                .map_err(map_error)?;
            Ok(response)
        })
    }
}

async fn thread_parent(ctx: &MessagingCtx, channel: ChannelId) -> Option<ChannelId> {
    ctx.state
        .read()
        .await
        .thread_parents
        .get(&channel.get())
        .copied()
        .flatten()
        .and_then(std::num::NonZeroU64::new)
        .map(|id| ChannelId::new(id.get()))
}

fn map_error(error: MessagingError) -> ContextError {
    let kind = match &error.kind {
        MessagingErrorKind::OutboundDenied(_) => Some(ContextErrorKind::AccessDenied),
        MessagingErrorKind::Observation(_) => Some(ContextErrorKind::InvalidResponse),
        MessagingErrorKind::Discord(cause) => match cause.as_ref() {
            serenity::Error::Http(HttpError::UnsuccessfulRequest(response)) => {
                match response.status_code.as_u16() {
                    401 | 403 => Some(ContextErrorKind::AccessDenied),
                    404 => Some(ContextErrorKind::NotFound),
                    429 | 500..=599 => Some(ContextErrorKind::AccessUnavailable),
                    _ => None,
                }
            }
            serenity::Error::Http(HttpError::Request(cause)) if cause.is_timeout() => {
                Some(ContextErrorKind::Timeout)
            }
            serenity::Error::Http(HttpError::Request(_)) => {
                Some(ContextErrorKind::AccessUnavailable)
            }
            _ => None,
        },
        _ => None,
    };
    match kind {
        Some(kind) => ContextError::with_cause(kind, error),
        None => ContextError::with_cause(ContextErrorKind::Provider, error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{ChannelConfig, Config, LoadedConfig},
        core::{ConversationId, ConversationRef, MessageRef},
        rook::{MessageKey, PageLimit},
    };
    use serde_json::{Value, json};
    use std::sync::Mutex;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
    };

    struct Fixture {
        provider: DiscordContextProvider,
        config: Arc<Mutex<LoadedConfig>>,
        state: crate::state::State,
        paths: Arc<Mutex<Vec<String>>>,
        server: tokio::task::JoinHandle<()>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.abort();
        }
    }
    impl Fixture {
        async fn new(status: &'static str, body: Value) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let paths = Arc::new(Mutex::new(Vec::new()));
            let seen = paths.clone();
            let server = tokio::spawn(async move {
                loop {
                    let (socket, _) = listener.accept().await.unwrap();
                    let mut reader = BufReader::new(socket);
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    seen.lock().unwrap().push(line);
                    loop {
                        let mut header = String::new();
                        if reader.read_line(&mut header).await.unwrap() == 0 || header == "\r\n" {
                            break;
                        }
                    }
                    let body = body.to_string();
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    reader
                        .get_mut()
                        .write_all(response.as_bytes())
                        .await
                        .unwrap();
                }
            });
            let http = Arc::new(
                serenity::http::HttpBuilder::new("fixture")
                    .proxy(format!("http://{address}"))
                    .ratelimiter_disabled(true)
                    .build(),
            );
            let mut raw = Config::default();
            raw.channels.push(ChannelConfig {
                id: "10".into(),
                ..Default::default()
            });
            let config = Arc::new(Mutex::new(LoadedConfig::from_raw(raw)));
            let current = config.clone();
            let state = crate::state::new_state();
            let shared_state = state.clone();
            let provider = DiscordContextProvider::new(
                "primary",
                Arc::new(move || {
                    MessagingCtx::new(
                        http.clone(),
                        shared_state.clone(),
                        Arc::new(current.lock().unwrap().clone()),
                        "/tmp".into(),
                        Arc::new(crate::no_rly::consent::ConsentGate::new(
                            camino::Utf8Path::new("/tmp"),
                        )),
                        Arc::new(crate::ingress_ledger::IngressLedger::new()),
                    )
                }),
            )
            .unwrap();
            Self {
                provider,
                config,
                state,
                paths,
                server,
            }
        }
        fn message(&self, id: u64) -> MessageKey {
            MessageKey::from(&MessageRef::new(
                ConversationRef::new(
                    self.provider.source.clone(),
                    ConversationId::numeric(std::num::NonZeroU64::new(10).unwrap()),
                ),
                crate::core::MessageId::numeric(std::num::NonZeroU64::new(id).unwrap()),
            ))
        }
    }

    fn message() -> Value {
        json!({"id":"30", "channel_id":"10", "guild_id":"40", "type":19,
            "author":{"id":"20", "username":"sender", "discriminator":"0000"},
            "content":"original body", "timestamp":"2026-10-13T12:00:00Z", "edited_timestamp":"2026-10-13T12:00:01Z",
            "tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],"embeds":[],"pinned":false,
            "message_reference":{"type":0,"channel_id":"10","message_id":"29","guild_id":"40"},
            "attachments":[{"id":"100","filename":"sample.txt","size":3,"url":"https://example.invalid/file","proxy_url":"https://example.invalid/proxy"}]})
    }

    #[tokio::test]
    async fn exact_read_preserves_native_metadata_and_current_policy() {
        let fixture = Fixture::new("200 OK", message()).await;
        fixture
            .state
            .write()
            .await
            .thread_parents
            .insert(10, Some(5));
        let request = ContextQuery::Exact(fixture.message(30));
        let ContextResponse::Exact(observed) = fixture.provider.read(&request).await.unwrap()
        else {
            panic!("exact response")
        };
        assert_eq!(observed.content(), "original body");
        let serialized = serde_json::to_value(&observed).unwrap();
        let native: Vec<u8> =
            serde_json::from_value(serialized["native"]["bytes"].clone()).unwrap();
        let native: Value = serde_json::from_slice(&native).unwrap();
        assert_eq!(native["author_id"], "20");
        assert_eq!(native["thread_parent"], "5");
        assert_eq!(native["reference"]["message_id"], "29");
        assert_eq!(native["attachments"][0]["filename"], "sample.txt");
        assert!(!native["edited_at"].is_null());
        *fixture.config.lock().unwrap() = LoadedConfig::from_raw(Config::default());
        assert!(matches!(
            fixture.provider.read(&request).await.unwrap_err().kind,
            ContextErrorKind::AccessDenied
        ));
        assert_eq!(fixture.paths.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn history_passes_exact_before_and_after_anchors() {
        let fixture = Fixture::new("200 OK", json!([message()])).await;
        for direction in [
            ContextDirection::Before(fixture.message(31)),
            ContextDirection::After(fixture.message(29)),
        ] {
            let request = ContextQuery::History {
                room: fixture.message(30).conversation().clone(),
                direction,
                limit: PageLimit::new(1).unwrap(),
            };
            let ContextResponse::History(page) = fixture.provider.read(&request).await.unwrap()
            else {
                panic!("history response")
            };
            assert_eq!(page.query(), &request);
            assert!(page.has_more_hint());
            assert_eq!(page.oldest(), Some(&fixture.message(30)));
        }
        let paths = fixture.paths.lock().unwrap();
        assert!(paths[0].contains("before=31"));
        assert!(paths[1].contains("after=29"));
    }

    #[tokio::test]
    async fn policy_revoked_during_read_prevents_disclosure() {
        let mut fixture = Fixture::new("200 OK", message()).await;
        let factory = fixture.provider.context.clone();
        let config = fixture.config.clone();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        fixture.provider.context = Arc::new(move || {
            if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0 {
                *config.lock().unwrap() = LoadedConfig::from_raw(Config::default());
            }
            factory()
        });
        assert!(matches!(
            fixture
                .provider
                .read(&ContextQuery::Exact(fixture.message(30)))
                .await
                .unwrap_err()
                .kind,
            ContextErrorKind::AccessDenied
        ));
        assert_eq!(fixture.paths.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn invalid_channel_and_missing_message_are_distinct() {
        let mut wrong = message();
        wrong["channel_id"] = json!("11");
        let fixture = Fixture::new("200 OK", wrong).await;
        let invalid = fixture
            .provider
            .read(&ContextQuery::Exact(fixture.message(30)))
            .await
            .unwrap_err();
        assert!(matches!(invalid.kind, ContextErrorKind::InvalidResponse));
        let cause = std::error::Error::source(&invalid)
            .unwrap()
            .downcast_ref::<MessagingError>()
            .unwrap();
        assert!(matches!(cause.kind, MessagingErrorKind::Observation(_)));
        let missing = Fixture::new(
            "404 Not Found",
            json!({"code":10008,"message":"Unknown Message"}),
        )
        .await;
        let not_found = missing
            .provider
            .read(&ContextQuery::Exact(missing.message(30)))
            .await
            .unwrap_err();
        assert!(matches!(not_found.kind, ContextErrorKind::NotFound));
        let cause = std::error::Error::source(&not_found)
            .unwrap()
            .downcast_ref::<MessagingError>()
            .unwrap();
        let MessagingErrorKind::Discord(cause) = &cause.kind else {
            panic!("Discord cause")
        };
        let serenity::Error::Http(HttpError::UnsuccessfulRequest(response)) = cause.as_ref() else {
            panic!("HTTP cause")
        };
        assert_eq!(response.status_code.as_u16(), 404);
        let denied = Fixture::new(
            "403 Forbidden",
            json!({"code":50001,"message":"Missing Access"}),
        )
        .await;
        assert!(matches!(
            denied
                .provider
                .read(&ContextQuery::Exact(denied.message(30)))
                .await
                .unwrap_err()
                .kind,
            ContextErrorKind::AccessDenied
        ));
    }

    #[tokio::test]
    async fn unsupported_source_and_opaque_identity_do_not_contact_discord() {
        let fixture = Fixture::new("200 OK", message()).await;
        let other_source = ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("other").unwrap(),
        );
        let other = MessageKey::from(&MessageRef::new(
            ConversationRef::new(
                other_source,
                ConversationId::numeric(std::num::NonZeroU64::new(10).unwrap()),
            ),
            crate::core::MessageId::numeric(std::num::NonZeroU64::new(30).unwrap()),
        ));
        assert!(matches!(
            fixture
                .provider
                .read(&ContextQuery::Exact(other))
                .await
                .unwrap_err()
                .kind,
            ContextErrorKind::Unsupported
        ));
        let opaque = MessageKey::from(&MessageRef::new(
            ConversationRef::new(
                fixture.provider.source.clone(),
                ConversationId::numeric(std::num::NonZeroU64::new(10).unwrap()),
            ),
            crate::core::MessageId::opaque("30").unwrap(),
        ));
        assert!(matches!(
            fixture
                .provider
                .read(&ContextQuery::Exact(opaque))
                .await
                .unwrap_err()
                .kind,
            ContextErrorKind::InvalidReference
        ));
        assert!(fixture.paths.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn timeout_is_a_visible_typed_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_millis(20))
            .build()
            .unwrap();
        let (accepted, response) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(
                listener.accept(),
                client.get(format!("http://{address}/fixture")).send()
            )
        })
        .await
        .unwrap();
        let (_socket, _) = accepted.unwrap();
        let error = response.unwrap_err();
        assert!(error.is_timeout());
        let mapped = map_error(
            MessagingErrorKind::Discord(Box::new(serenity::Error::Http(HttpError::Request(error))))
                .into(),
        );
        assert!(matches!(mapped.kind, ContextErrorKind::Timeout));
    }
}
