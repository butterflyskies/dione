//! Authorized Discord message mutations and thread creation.

use crate::{
    config::LoadedConfig,
    discord::messaging::{TargetError, TargetOperation, TargetPolicy, verify_message_target},
    gate::OutboundGate,
    ingress_ledger::IngressLedger,
    state::State,
};
use serenity::{
    builder::CreateThread,
    http::Http,
    model::{
        channel::ChannelType,
        id::{ChannelId, MessageId},
    },
};
use std::sync::Arc;
use thiserror::Error;

pub(crate) struct ManagementCtx {
    pub(crate) http: Arc<Http>,
    pub(crate) state: State,
    pub(crate) config: Arc<LoadedConfig>,
    pub(crate) ingress_ledger: Arc<IngressLedger>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum MessageAction {
    Pin,
    Unpin,
    Delete,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct MessageCommand {
    pub(crate) channel: ChannelId,
    pub(crate) message: MessageId,
    pub(crate) action: MessageAction,
}

#[derive(Debug)]
pub(crate) struct CreatedThread {
    pub(crate) id: ChannelId,
    pub(crate) name: String,
}

#[derive(Debug, Error)]
#[error("management operation failed in channel {channel}")]
pub(crate) struct ManagementError {
    pub(crate) channel: ChannelId,
    #[source]
    pub(crate) kind: ManagementErrorKind,
}

#[derive(Debug, Error)]
pub(crate) enum ManagementErrorKind {
    #[error("channel not in allowlist")]
    OutboundDenied,
    #[error(transparent)]
    Target(TargetError),
    #[error(transparent)]
    Discord(Box<serenity::Error>),
}

async fn check_outbound(ctx: &ManagementCtx, channel: ChannelId) -> Result<(), ManagementError> {
    let state = ctx.state.read().await;
    if !OutboundGate::check_channel_with_threads(
        &ctx.config,
        channel.get(),
        &state.dm_channel_ids,
        &state.thread_parents,
    ) {
        return Err(ManagementError {
            channel,
            kind: ManagementErrorKind::OutboundDenied,
        });
    }
    Ok(())
}

async fn verify_target(
    ctx: &ManagementCtx,
    channel: ChannelId,
    message: MessageId,
    operation: TargetOperation,
) -> Result<(), ManagementError> {
    let own_send = ctx.state.read().await.is_own_send(message.get());
    verify_message_target(
        &ctx.ingress_ledger,
        &ctx.http,
        &ctx.config,
        message,
        channel,
        operation,
        TargetPolicy::IngressOrOwnSend(own_send),
    )
    .await
    .map_err(|cause| ManagementError {
        channel,
        kind: ManagementErrorKind::Target(cause),
    })
}

pub(crate) async fn mutate_message(
    ctx: &ManagementCtx,
    command: MessageCommand,
) -> Result<(), ManagementError> {
    let MessageCommand {
        channel,
        message,
        action,
    } = command;
    check_outbound(ctx, channel).await?;
    let operation = match action {
        MessageAction::Pin => TargetOperation::PinMessage,
        MessageAction::Unpin => TargetOperation::UnpinMessage,
        MessageAction::Delete => TargetOperation::DeleteMessage,
    };
    verify_target(ctx, channel, message, operation).await?;
    let result = match action {
        MessageAction::Pin => ctx.http.pin_message(channel, message, None).await,
        MessageAction::Unpin => ctx.http.unpin_message(channel, message, None).await,
        MessageAction::Delete => ctx.http.delete_message(channel, message, None).await,
    };
    result.map_err(|cause| ManagementError {
        channel,
        kind: ManagementErrorKind::Discord(Box::new(cause)),
    })
}

pub(crate) async fn create_thread(
    ctx: &ManagementCtx,
    channel: ChannelId,
    message: Option<MessageId>,
    name: &str,
) -> Result<CreatedThread, ManagementError> {
    check_outbound(ctx, channel).await?;
    if let Some(message) = message {
        verify_target(ctx, channel, message, TargetOperation::CreateThread).await?;
    }
    let builder = CreateThread::new(name).kind(ChannelType::PublicThread);
    let result = match message {
        Some(message) => {
            ctx.http
                .create_thread_from_message(channel, message, &builder, None)
                .await
        }
        None => ctx.http.create_thread(channel, &builder, None).await,
    };
    let thread = result.map_err(|cause| ManagementError {
        channel,
        kind: ManagementErrorKind::Discord(Box::new(cause)),
    })?;
    ctx.state
        .write()
        .await
        .record_thread_parent(thread.id.get(), Some(channel.get()));
    Ok(CreatedThread {
        id: thread.id,
        name: thread.name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{ChannelConfig, Config, LoadedConfig},
        discord::management_wire,
        state::new_state,
    };
    use serde_json::{Value, json};
    use serenity::model::id::{ChannelId, UserId};
    use std::{error::Error as _, sync::Arc, time::Duration};
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        time::timeout,
    };

    fn config_with_channel(channel_id: u64) -> LoadedConfig {
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: channel_id.to_string(),
            ..Default::default()
        });
        LoadedConfig::from_raw(raw)
    }

    fn ctx(config: LoadedConfig) -> ManagementCtx {
        ManagementCtx {
            http: Arc::new(serenity::http::Http::new("fake")),
            state: new_state(),
            config: Arc::new(config),
            ingress_ledger: Arc::new(crate::ingress_ledger::IngressLedger::new()),
        }
    }

    #[tokio::test]
    async fn configured_channel_is_allowed() {
        let ctx = ctx(config_with_channel(100));
        assert!(check_outbound(&ctx, ChannelId::new(100)).await.is_ok());
    }

    #[tokio::test]
    async fn unknown_channel_is_denied() {
        let ctx = ctx(config_with_channel(100));
        assert!(check_outbound(&ctx, ChannelId::new(999)).await.is_err());
    }

    #[tokio::test]
    async fn thread_denied_when_parent_not_in_config() {
        let parent = 100u64;
        let thread = 200u64;
        let ctx = ctx(config_with_channel(999));
        {
            let mut state = ctx.state.write().await;
            state.record_thread_parent(thread, Some(parent));
        }
        assert!(
            check_outbound(&ctx, ChannelId::new(thread)).await.is_err(),
            "thread whose parent is absent from config must be denied"
        );
    }
    #[tokio::test]
    async fn management_ops_require_ingress_for_old_non_own_targets() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake Discord");
        let address = listener.local_addr().expect("fake Discord address");
        let mut ctx = ctx(config_with_channel(42));
        ctx.http = Arc::new(
            serenity::http::HttpBuilder::new("fake")
                .proxy(format!("http://{address}"))
                .ratelimiter_disabled(true)
                .build(),
        );
        let ch = ChannelId::new(42);
        let target = MessageId::new(9001);
        let cases = [
            (
                "delete_message",
                management_wire::mutation(
                    mutate_message(
                        &ctx,
                        MessageCommand {
                            channel: ch,
                            message: target,
                            action: MessageAction::Delete,
                        },
                    )
                    .await,
                ),
            ),
            (
                "pin_message",
                management_wire::mutation(
                    mutate_message(
                        &ctx,
                        MessageCommand {
                            channel: ch,
                            message: target,
                            action: MessageAction::Pin,
                        },
                    )
                    .await,
                ),
            ),
            (
                "unpin_message",
                management_wire::mutation(
                    mutate_message(
                        &ctx,
                        MessageCommand {
                            channel: ch,
                            message: target,
                            action: MessageAction::Unpin,
                        },
                    )
                    .await,
                ),
            ),
            (
                "create_thread",
                management_wire::thread(create_thread(&ctx, ch, Some(target), "t").await),
            ),
        ];
        for (op, result) in cases {
            assert_eq!(result["reason"], "ingress_required", "{op}: {result}");
        }
        assert!(
            timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err(),
            "no management operation may fetch or mutate an unledgered target"
        );
    }
    #[derive(Debug, PartialEq)]
    struct Request {
        line: String,
        body: Value,
    }

    async fn serve_fixture(
        listener: TcpListener,
        responses: Vec<(&'static str, Value)>,
    ) -> Vec<Request> {
        timeout(Duration::from_secs(5), async move {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = tokio::io::BufReader::new(stream);
                let mut line = String::new();
                assert_ne!(
                    reader.read_line(&mut line).await.unwrap(),
                    0,
                    "request line ended at EOF"
                );
                let mut length = 0;
                loop {
                    let mut header = String::new();
                    assert_ne!(
                        reader.read_line(&mut header).await.unwrap(),
                        0,
                        "request headers ended at EOF"
                    );
                    if header == "\r\n" {
                        break;
                    }
                    if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:")
                    {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
                let mut bytes = vec![0; length];
                reader.read_exact(&mut bytes).await.unwrap();
                requests.push(Request {
                    line: line.trim_end().to_owned(),
                    body: if bytes.is_empty() {
                        Value::Null
                    } else {
                        serde_json::from_slice(&bytes).unwrap()
                    },
                });
                let body = if status == "204 No Content" {
                    String::new()
                } else {
                    body.to_string()
                };
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
            requests
        })
        .await
        .expect("fake Discord exchange deadline")
    }

    async fn fixture(
        responses: Vec<(&'static str, Value)>,
    ) -> (ManagementCtx, tokio::task::JoinHandle<Vec<Request>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_fixture(listener, responses));
        let mut ctx = ctx(config_with_channel(42));
        ctx.http = Arc::new(
            serenity::http::HttpBuilder::new("fake")
                .proxy(format!("http://{address}"))
                .ratelimiter_disabled(true)
                .build(),
        );
        (ctx, server)
    }

    #[tokio::test]
    async fn management_fixture_rejects_eof_during_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_fixture(
            listener,
            vec![("204 No Content", Value::Null)],
        ));
        let mut peer = TcpStream::connect(address).await.unwrap();
        peer.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n")
            .await
            .unwrap();
        peer.shutdown().await.unwrap();
        let error = timeout(Duration::from_secs(10), server)
            .await
            .expect("fixture did not terminate after header EOF")
            .unwrap_err();
        let panic = error.into_panic().downcast::<String>().unwrap();
        assert!(panic.contains("request headers ended at EOF"), "{panic}");
    }

    #[tokio::test]
    async fn management_fixture_times_out_stalled_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_fixture(
            listener,
            vec![("204 No Content", Value::Null)],
        ));
        let mut peer = TcpStream::connect(address).await.unwrap();
        peer.write_all(b"POST / HTTP/1.1\r\nContent-Length: 2\r\n\r\n{")
            .await
            .unwrap();
        let error = timeout(Duration::from_secs(10), server)
            .await
            .expect("fixture outlived its exchange deadline")
            .unwrap_err();
        drop(peer);
        let panic = error.into_panic().downcast::<String>().unwrap();
        assert_eq!(*panic, "fake Discord exchange deadline: Elapsed(())");
    }

    fn thread_response() -> Value {
        json!({"id":"200", "guild_id":"7", "type":11, "name":"Discord returned name", "parent_id":"999", "position":0, "permission_overwrites":[], "nsfw":false})
    }

    #[tokio::test]
    async fn management_contract_mutations_accept_ingress_and_own_sends() {
        for own_send in [false, true] {
            let (ctx, server) = fixture(vec![("204 No Content", Value::Null); 3]).await;
            let channel = ChannelId::new(42);
            let message = MessageId::new(9001);
            if own_send {
                ctx.state.write().await.note_sent(9001);
            } else {
                ctx.ingress_ledger
                    .note_admitted(message, channel, UserId::new(7), "hello");
            }
            assert_eq!(
                management_wire::mutation(
                    mutate_message(
                        &ctx,
                        MessageCommand {
                            channel,
                            message,
                            action: MessageAction::Pin
                        }
                    )
                    .await
                ),
                json!({"ok":true})
            );
            assert_eq!(
                management_wire::mutation(
                    mutate_message(
                        &ctx,
                        MessageCommand {
                            channel,
                            message,
                            action: MessageAction::Unpin
                        }
                    )
                    .await
                ),
                json!({"ok":true})
            );
            assert_eq!(
                management_wire::mutation(
                    mutate_message(
                        &ctx,
                        MessageCommand {
                            channel,
                            message,
                            action: MessageAction::Delete
                        }
                    )
                    .await
                ),
                json!({"ok":true})
            );
            assert_eq!(
                server.await.unwrap(),
                vec![
                    Request {
                        line: "PUT /api/v10/channels/42/pins/9001 HTTP/1.1".into(),
                        body: Value::Null
                    },
                    Request {
                        line: "DELETE /api/v10/channels/42/pins/9001 HTTP/1.1".into(),
                        body: Value::Null
                    },
                    Request {
                        line: "DELETE /api/v10/channels/42/messages/9001 HTTP/1.1".into(),
                        body: Value::Null
                    },
                ]
            );
        }
    }

    #[tokio::test]
    async fn management_contract_denial_precedes_target_checks_and_http() {
        let (ctx, server) = fixture(vec![("204 No Content", Value::Null)]).await;
        let channel = ChannelId::new(999);
        let message = MessageId::new(9001);
        for result in [
            management_wire::mutation(
                mutate_message(
                    &ctx,
                    MessageCommand {
                        channel,
                        message,
                        action: MessageAction::Pin,
                    },
                )
                .await,
            ),
            management_wire::mutation(
                mutate_message(
                    &ctx,
                    MessageCommand {
                        channel,
                        message,
                        action: MessageAction::Unpin,
                    },
                )
                .await,
            ),
            management_wire::mutation(
                mutate_message(
                    &ctx,
                    MessageCommand {
                        channel,
                        message,
                        action: MessageAction::Delete,
                    },
                )
                .await,
            ),
            management_wire::thread(create_thread(&ctx, channel, Some(message), "ignored").await),
            management_wire::thread(create_thread(&ctx, channel, None, "ignored").await),
        ] {
            assert_eq!(result, json!({"error":"channel not in allowlist"}));
        }
        let channel = ChannelId::new(42);
        for result in [
            management_wire::mutation(
                mutate_message(
                    &ctx,
                    MessageCommand {
                        channel,
                        message,
                        action: MessageAction::Pin,
                    },
                )
                .await,
            ),
            management_wire::mutation(
                mutate_message(
                    &ctx,
                    MessageCommand {
                        channel,
                        message,
                        action: MessageAction::Unpin,
                    },
                )
                .await,
            ),
            management_wire::mutation(
                mutate_message(
                    &ctx,
                    MessageCommand {
                        channel,
                        message,
                        action: MessageAction::Delete,
                    },
                )
                .await,
            ),
            management_wire::thread(create_thread(&ctx, channel, Some(message), "ignored").await),
        ] {
            assert_eq!(
                result,
                json!({"error":"message target requires an ingress receipt or own send", "reason":"ingress_required"})
            );
        }
        ctx.state.write().await.note_sent(9001);
        assert_eq!(
            management_wire::mutation(
                mutate_message(
                    &ctx,
                    MessageCommand {
                        channel,
                        message,
                        action: MessageAction::Pin
                    }
                )
                .await
            ),
            json!({"ok":true})
        );
        assert_eq!(
            server.await.unwrap(),
            vec![Request {
                line: "PUT /api/v10/channels/42/pins/9001 HTTP/1.1".into(),
                body: Value::Null
            }]
        );
    }

    #[tokio::test]
    async fn management_contract_thread_creation_registers_requested_parent_on_success() {
        for (message, name, own_send, path) in [
            (
                None,
                "",
                false,
                "POST /api/v10/channels/42/threads HTTP/1.1",
            ),
            (
                Some(MessageId::new(9001)),
                "  thread\nname  ",
                false,
                "POST /api/v10/channels/42/messages/9001/threads HTTP/1.1",
            ),
            (
                Some(MessageId::new(9001)),
                "own thread",
                true,
                "POST /api/v10/channels/42/messages/9001/threads HTTP/1.1",
            ),
        ] {
            let (ctx, server) = fixture(vec![
                ("200 OK", thread_response()),
                ("204 No Content", Value::Null),
            ])
            .await;
            let channel = ChannelId::new(42);
            if own_send {
                ctx.state.write().await.note_sent(9001);
            } else {
                ctx.ingress_ledger.note_admitted(
                    MessageId::new(9001),
                    channel,
                    UserId::new(7),
                    "hello",
                );
            }
            assert_eq!(
                management_wire::thread(create_thread(&ctx, channel, message, name).await),
                json!({"ok":true,"thread_id":"200","name":"Discord returned name"})
            );
            assert_eq!(
                ctx.state.read().await.thread_parents.get(&200),
                Some(&Some(42))
            );
            ctx.state.write().await.note_sent(9002);
            assert_eq!(
                management_wire::mutation(
                    mutate_message(
                        &ctx,
                        MessageCommand {
                            channel: ChannelId::new(200),
                            message: MessageId::new(9002),
                            action: MessageAction::Pin
                        }
                    )
                    .await
                ),
                json!({"ok":true})
            );
            assert_eq!(
                server.await.unwrap(),
                vec![
                    Request {
                        line: path.into(),
                        body: json!({"name":name,"type":11})
                    },
                    Request {
                        line: "PUT /api/v10/channels/200/pins/9002 HTTP/1.1".into(),
                        body: Value::Null
                    },
                ]
            );
        }
    }

    #[tokio::test]
    async fn management_contract_http_errors_do_not_register_threads() {
        let (ctx, server) = fixture(vec![
            (
                "403 Forbidden",
                json!({"code":50013,"message":"Missing Permissions"})
            );
            5
        ])
        .await;
        let channel = ChannelId::new(42);
        let message = MessageId::new(9001);
        ctx.state.write().await.note_sent(9001);
        for result in [
            management_wire::mutation(
                mutate_message(
                    &ctx,
                    MessageCommand {
                        channel,
                        message,
                        action: MessageAction::Pin,
                    },
                )
                .await,
            ),
            management_wire::mutation(
                mutate_message(
                    &ctx,
                    MessageCommand {
                        channel,
                        message,
                        action: MessageAction::Unpin,
                    },
                )
                .await,
            ),
            management_wire::mutation(
                mutate_message(
                    &ctx,
                    MessageCommand {
                        channel,
                        message,
                        action: MessageAction::Delete,
                    },
                )
                .await,
            ),
            management_wire::thread(create_thread(&ctx, channel, Some(message), "failed").await),
            management_wire::thread(create_thread(&ctx, channel, None, "failed").await),
        ] {
            assert_eq!(result, json!({"error":"Missing Permissions"}));
        }
        assert!(ctx.state.read().await.thread_parents.is_empty());
        assert!(ctx.state.read().await.is_own_send(9001));
        assert_eq!(
            management_wire::mutation(
                mutate_message(
                    &ctx,
                    MessageCommand {
                        channel: ChannelId::new(200),
                        message,
                        action: MessageAction::Pin
                    }
                )
                .await
            ),
            json!({"error":"channel not in allowlist"})
        );
        assert_eq!(
            server.await.unwrap(),
            vec![
                Request {
                    line: "PUT /api/v10/channels/42/pins/9001 HTTP/1.1".into(),
                    body: Value::Null
                },
                Request {
                    line: "DELETE /api/v10/channels/42/pins/9001 HTTP/1.1".into(),
                    body: Value::Null
                },
                Request {
                    line: "DELETE /api/v10/channels/42/messages/9001 HTTP/1.1".into(),
                    body: Value::Null
                },
                Request {
                    line: "POST /api/v10/channels/42/messages/9001/threads HTTP/1.1".into(),
                    body: json!({"name":"failed","type":11})
                },
                Request {
                    line: "POST /api/v10/channels/42/threads HTTP/1.1".into(),
                    body: json!({"name":"failed","type":11})
                },
            ]
        );
    }
    #[tokio::test]
    async fn management_contract_mismatch_blocks_even_recorded_own_send() {
        let (ctx, server) = fixture(vec![("204 No Content", Value::Null)]).await;
        let channel = ChannelId::new(42);
        let message = MessageId::new(9001);
        ctx.state.write().await.note_sent(9001);
        ctx.ingress_ledger
            .note_admitted(message, ChannelId::new(99), UserId::new(7), "hello");
        for (result, operation, error) in [
            (
                management_wire::mutation(
                    mutate_message(
                        &ctx,
                        MessageCommand {
                            channel,
                            message,
                            action: MessageAction::Pin,
                        },
                    )
                    .await,
                ),
                "pin_message",
                "message 9001 was admitted in channel 99, not claimed channel 42; pin_message blocked",
            ),
            (
                management_wire::mutation(
                    mutate_message(
                        &ctx,
                        MessageCommand {
                            channel,
                            message,
                            action: MessageAction::Unpin,
                        },
                    )
                    .await,
                ),
                "unpin_message",
                "message 9001 was admitted in channel 99, not claimed channel 42; unpin_message blocked",
            ),
            (
                management_wire::mutation(
                    mutate_message(
                        &ctx,
                        MessageCommand {
                            channel,
                            message,
                            action: MessageAction::Delete,
                        },
                    )
                    .await,
                ),
                "delete_message",
                "message 9001 was admitted in channel 99, not claimed channel 42; delete_message blocked",
            ),
            (
                management_wire::thread(
                    create_thread(&ctx, channel, Some(message), "ignored").await,
                ),
                "create_thread",
                "message 9001 was admitted in channel 99, not claimed channel 42; create_thread blocked",
            ),
        ] {
            assert_eq!(
                result,
                json!({"error":error,"reason":"channel_mismatch","blocked":true,"message_id":"9001","admitted_channel_id":"99","claimed_channel_id":"42","operation":operation})
            );
        }
        ctx.state.write().await.note_sent(9002);
        assert_eq!(
            management_wire::mutation(
                mutate_message(
                    &ctx,
                    MessageCommand {
                        channel,
                        message: MessageId::new(9002),
                        action: MessageAction::Pin
                    }
                )
                .await
            ),
            json!({"ok":true})
        );
        assert_eq!(
            server.await.unwrap(),
            vec![Request {
                line: "PUT /api/v10/channels/42/pins/9002 HTTP/1.1".into(),
                body: Value::Null
            }]
        );
    }
    #[tokio::test]
    async fn management_typed_errors_retain_channel_category_and_source() {
        let (ctx, server) = fixture(vec![(
            "403 Forbidden",
            json!({"code":50013,"message":"Missing Permissions"}),
        )])
        .await;
        let command = MessageCommand {
            channel: ChannelId::new(42),
            message: MessageId::new(9001),
            action: MessageAction::Pin,
        };
        let error = mutate_message(&ctx, command).await.unwrap_err();
        assert_eq!(error.channel, ChannelId::new(42));
        assert!(matches!(
            error.kind,
            ManagementErrorKind::Target(TargetError::IngressRequired)
        ));
        assert_eq!(
            error.to_string(),
            "management operation failed in channel 42"
        );
        assert!(
            error
                .source()
                .unwrap()
                .downcast_ref::<ManagementErrorKind>()
                .is_some()
        );
        assert_eq!(
            error.source().unwrap().to_string(),
            "message target requires an ingress receipt or own send"
        );
        ctx.state.write().await.note_sent(9001);
        let error = mutate_message(&ctx, command).await.unwrap_err();
        assert_eq!(error.channel, ChannelId::new(42));
        let ManagementErrorKind::Discord(cause) = &error.kind else {
            panic!("expected Discord error, got {error:?}");
        };
        let serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response)) =
            cause.as_ref()
        else {
            panic!("expected retained Discord response, got {error:?}");
        };
        assert_eq!(response.status_code.as_u16(), 403);
        assert_eq!(response.error.code, 50013);
        assert_eq!(error.source().unwrap().to_string(), "Missing Permissions");
        assert_eq!(
            management_wire::mutation(Err(error)),
            json!({"error":"Missing Permissions"})
        );
        assert_eq!(
            server.await.unwrap(),
            vec![Request {
                line: "PUT /api/v10/channels/42/pins/9001 HTTP/1.1".into(),
                body: Value::Null
            }]
        );
    }
    #[tokio::test]
    async fn registered_management_preserves_http_and_reloads_authority() {
        let (ctx, peer) = fixture(vec![
            ("204 No Content", Value::Null),
            ("204 No Content", Value::Null),
            ("204 No Content", Value::Null),
            ("200 OK", thread_response()),
            ("200 OK", thread_response()),
        ])
        .await;
        let dir = tempfile::TempDir::new().unwrap();
        let path = camino::Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
        ctx.state.write().await.note_sent(9001);
        let server = crate::mcp::server::DioneServer::new(
            ctx.state.clone(),
            Arc::new(tokio::sync::Mutex::new(crate::queue::AccessQueue::load(
                &path,
            ))),
            ctx.http.clone(),
            path.clone(),
            tokio::sync::mpsc::channel(1).0,
            crate::tracing_channel::TraceLevelController::noop(),
            crate::harness::HarnessConnection::claude(),
            Arc::new(crate::no_rly::consent::ConsentGate::new(&path)),
            ctx.ingress_ledger.clone(),
            crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            ),
            None,
        )
        .await
        .unwrap();
        for name in [
            "pin_message",
            "unpin_message",
            "delete_message",
            "create_thread",
        ] {
            let invalid = crate::mcp::dispatch::call_tool(
                &server,
                name,
                json!({"channel_id":"bad", "message_id":"9001", "name":"thread"}),
            )
            .await
            .unwrap_err();
            assert_eq!(invalid, "invalid channel_id: not a valid u64");
            let denied = crate::mcp::dispatch::call_tool(
                &server,
                name,
                json!({"channel_id":"42", "message_id":"9001", "name":"thread"}),
            )
            .await
            .unwrap();
            assert_eq!(
                denied,
                json!({"content":[{"type":"text","text":"{\"error\":\"channel not in allowlist\"}"}],"isError":true})
            );
        }
        std::fs::write(path.join("config.toml"), "[[channels]]\nid = \"42\"\n").unwrap();
        assert!(crate::config::reload_config(&path).await.1.is_none());
        for name in ["pin_message", "unpin_message", "delete_message"] {
            let result = crate::mcp::dispatch::call_tool(
                &server,
                name,
                json!({"channel_id":42,"message_id":"9001","extra":true}),
            )
            .await
            .unwrap();
            assert_eq!(
                result,
                json!({"content":[{"type":"text","text":"{\"ok\":true}"}]})
            );
        }
        for message in [json!({}), json!("9001")] {
            let result = crate::mcp::dispatch::call_tool(
                &server,
                "create_thread",
                json!({"channel_id":"42","message_id":message,"name":"","extra":true}),
            )
            .await
            .unwrap();
            let wire: Value =
                serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
            assert_eq!(
                wire,
                json!({"ok":true,"thread_id":"200","name":"Discord returned name"})
            );
        }
        assert_eq!(
            ctx.state.read().await.thread_parents.get(&200),
            Some(&Some(42))
        );
        std::fs::write(path.join("config.toml"), "").unwrap();
        assert!(crate::config::reload_config(&path).await.1.is_none());
        let denied = crate::mcp::dispatch::call_tool(
            &server,
            "pin_message",
            json!({"channel_id":42,"message_id":9001}),
        )
        .await
        .unwrap();
        assert_eq!(
            denied,
            json!({"content":[{"type":"text","text":"{\"error\":\"channel not in allowlist\"}"}],"isError":true})
        );
        assert_eq!(
            peer.await.unwrap(),
            vec![
                Request {
                    line: "PUT /api/v10/channels/42/pins/9001 HTTP/1.1".into(),
                    body: Value::Null
                },
                Request {
                    line: "DELETE /api/v10/channels/42/pins/9001 HTTP/1.1".into(),
                    body: Value::Null
                },
                Request {
                    line: "DELETE /api/v10/channels/42/messages/9001 HTTP/1.1".into(),
                    body: Value::Null
                },
                Request {
                    line: "POST /api/v10/channels/42/threads HTTP/1.1".into(),
                    body: json!({"name":"","type":11})
                },
                Request {
                    line: "POST /api/v10/channels/42/messages/9001/threads HTTP/1.1".into(),
                    body: json!({"name":"","type":11})
                },
            ]
        );
    }
}
