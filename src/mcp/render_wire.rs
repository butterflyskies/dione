//! MCP projections of rendered files and Discord rendering outcomes.

use crate::{
    discord::{
        messaging_wire,
        rendering::{RenderToChannelError, RenderToChannelErrorKind},
    },
    rendering::{
        FileRenderError, FileRenderErrorKind, PersistedImage, RenderError, RenderErrorKind,
    },
};
use serde_json::{Value, json};
use serenity::model::id::MessageId;

pub(super) fn file(result: Result<PersistedImage, FileRenderError>) -> Value {
    match result {
        Ok(image) => {
            json!({"ok": true, "path": image.path().to_string_lossy(), "size_bytes": image.size_bytes()})
        }
        Err(error) => match error.kind {
            FileRenderErrorKind::Render(error) => render_error(error),
            FileRenderErrorKind::Create(error) => {
                json!({"error": format!("failed to create temp file: {error}")})
            }
            FileRenderErrorKind::Write { source, .. } => {
                json!({"error": format!("failed to write PNG: {source}")})
            }
            FileRenderErrorKind::Persist(error) => {
                json!({"error": format!("failed to persist temp file: {error}")})
            }
        },
    }
}

pub(super) fn channel(result: Result<MessageId, RenderToChannelError>) -> Value {
    match result {
        Ok(message_id) => messaging_wire::message(Ok(message_id)),
        Err(error) => match error.kind {
            RenderToChannelErrorKind::Render(error) => render_error(error),
            RenderToChannelErrorKind::Messaging(error) => messaging_wire::message(Err(*error)),
        },
    }
}

fn render_error(error: RenderError) -> Value {
    let message = match error.kind {
        RenderErrorKind::IfTypstUnsupported => "\\iftypst is not supported".to_string(),
        RenderErrorKind::Parse(error) => format!("LaTeX parse error: {error}"),
        RenderErrorKind::UnsafeTypstOutput => "expression produces unsafe typst output".to_string(),
        RenderErrorKind::Compile(error) => format!("typst compile error: {error}"),
        RenderErrorKind::NoPages => "typst produced no pages".to_string(),
        RenderErrorKind::Encode(error) => format!("PNG encode error: {error}"),
    };
    json!({"error": message})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Config, LoadedConfig},
        discord::{messaging::MessagingCtx, rendering::render_to_channel},
        pre_send::HookName,
        state::new_state,
    };
    use camino::Utf8PathBuf;
    use serenity::model::id::ChannelId;
    use std::{io, path::PathBuf, sync::Arc};

    #[test]
    fn render_error_projectors_preserve_legacy_text() {
        for (kind, expected) in [
            (
                RenderErrorKind::Parse("invalid token".into()),
                "LaTeX parse error: invalid token",
            ),
            (
                RenderErrorKind::UnsafeTypstOutput,
                "expression produces unsafe typst output",
            ),
            (RenderErrorKind::NoPages, "typst produced no pages"),
            (
                RenderErrorKind::Encode(png::EncodingError::IoError(io::Error::other(
                    "encoder fixture",
                ))),
                "PNG encode error: encoder fixture",
            ),
        ] {
            assert_eq!(
                render_error(RenderError { kind }),
                json!({"error": expected})
            );
        }
        assert_eq!(
            file(Err(FileRenderError {
                kind: FileRenderErrorKind::Create(io::Error::other("create fixture"))
            })),
            json!({"error": "failed to create temp file: create fixture"})
        );
        assert_eq!(
            file(Err(FileRenderError {
                kind: FileRenderErrorKind::Write {
                    path: PathBuf::from("private-path"),
                    source: io::Error::other("write fixture")
                }
            })),
            json!({"error": "failed to write PNG: write fixture"})
        );
    }

    #[test]
    fn persistence_error_projection_retains_display_and_cleanup() {
        let temp_path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
        let path = temp_path.to_path_buf();
        let error = tempfile::PathPersistError {
            error: io::Error::other("persist fixture"),
            path: temp_path,
        };
        assert!(path.exists());
        assert_eq!(
            file(Err(FileRenderError {
                kind: FileRenderErrorKind::Persist(error)
            })),
            json!({"error": "failed to persist temp file: failed to persist temporary file path: persist fixture"})
        );
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn captionless_render_to_channel_rejects_hook_overrides_before_rendering() {
        let ctx = MessagingCtx::new(
            Arc::new(serenity::http::Http::new("fake")),
            new_state(),
            Arc::new(LoadedConfig::from_raw(Config::default())),
            Utf8PathBuf::from("/tmp"),
            Arc::new(crate::no_rly::consent::ConsentGate::new(
                camino::Utf8Path::new("/tmp"),
            )),
            Arc::new(crate::ingress_ledger::IngressLedger::new()),
        );
        let hook = HookName::parse("tier-1").unwrap();

        let result = channel(
            render_to_channel(
                &ctx,
                ChannelId::new(42),
                "this is deliberately not latex",
                None,
                &[hook],
            )
            .await,
        );

        assert_eq!(
            result["error"],
            "no_rly_hooks cannot be used when no caption is sent"
        );
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    use crate::{
        config::{ChannelConfig, Config, LoadedConfig},
        discord::{messaging::MessagingCtx, rendering::render_to_channel},
        ingress_ledger::IngressLedger,
        no_rly::consent::ConsentGate,
        pre_send::{
            AuditTrail, ConstructFeedback, HookContext, HookDecision, HookName, HookOutput,
            PipelineMode, PreSendHook, PreSendPipeline,
        },
        rendering::render_file,
        state::new_state,
    };
    use camino::Utf8PathBuf;
    use serenity::model::id::ChannelId;
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        time::timeout,
    };

    async fn fixture(allowed: bool) -> (MessagingCtx, TcpListener, tempfile::TempDir) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = serenity::http::HttpBuilder::new("fake")
            .proxy(format!("http://{}", listener.local_addr().unwrap()))
            .ratelimiter_disabled(true)
            .build();
        let dir = tempfile::tempdir().unwrap();
        let state_dir = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let mut config = Config::default();
        if allowed {
            config.channels.push(ChannelConfig {
                id: "42".into(),
                ..Default::default()
            });
        }
        let ctx = MessagingCtx::new(
            Arc::new(http),
            new_state(),
            Arc::new(LoadedConfig::from_raw(config)),
            state_dir.clone(),
            Arc::new(ConsentGate::new(&state_dir)),
            Arc::new(IngressLedger::new()),
        );
        (ctx, listener, dir)
    }

    async fn assert_no_request(listener: &TcpListener) {
        assert!(
            timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    }

    async fn upload_exchange(
        listener: TcpListener,
        status: &'static str,
        body: Value,
    ) -> Vec<Vec<u8>> {
        timeout(Duration::from_secs(30), async {
            let mut requests = Vec::new();
            for (status, body) in [("403 Forbidden", json!({"code":50013,"message":"Missing Permissions"}).to_string()), (status, body.to_string())] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0; 8192];
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert_ne!(read, 0, "request ended before its declared body");
                    request.extend_from_slice(&chunk[..read]);
                    if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let length = headers.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(|value| value.trim().parse::<usize>().unwrap())).unwrap_or_default();
                        if request.len() >= end + 4 + length { break; }
                    }
                }
                let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                stream.write_all(response.as_bytes()).await.unwrap();
                requests.push(request);
            }
            requests
        }).await.expect("bounded Discord exchange")
    }

    fn sent_message() -> Value {
        json!({"id":"9001","type":0,"channel_id":"42","author":{"id":"77","username":"fixture","discriminator":"0","bot":true},"content":"","timestamp":"2026-10-10T12:00:00Z","edited_timestamp":null,"tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],"attachments":[],"embeds":[],"pinned":false,"flags":0,"components":[]})
    }

    struct CaptionHook {
        decision: HookDecision,
        surfaces: Arc<Mutex<Vec<String>>>,
    }

    impl PreSendHook for CaptionHook {
        fn name(&self) -> HookName {
            HookName::parse("capture-caption").unwrap()
        }
        fn execute(&self, context: &HookContext) -> HookOutput {
            self.surfaces
                .lock()
                .unwrap()
                .push(context.metadata("outbound_surface").unwrap().to_owned());
            HookOutput::new(
                self.decision.clone(),
                ConstructFeedback::default(),
                AuditTrail::default(),
            )
        }
    }

    fn with_hook(
        ctx: MessagingCtx,
        decision: HookDecision,
    ) -> (MessagingCtx, Arc<Mutex<Vec<String>>>) {
        let surfaces = Arc::new(Mutex::new(Vec::new()));
        let hook = CaptionHook {
            decision,
            surfaces: Arc::clone(&surfaces),
        };
        let pipeline = PreSendPipeline::new(vec![Box::new(hook)])
            .unwrap()
            .with_mode(PipelineMode::Enforce);
        (ctx.with_pre_send_pipeline(Arc::new(pipeline)), surfaces)
    }

    #[tokio::test]
    async fn rendering_errors_retain_requested_channel_and_cause() {
        for allowed in [false, true] {
            let (ctx, listener, _dir) = fixture(allowed).await;
            let error = render_to_channel(&ctx, ChannelId::new(42), r"\iftypst", None, &[])
                .await
                .unwrap_err();
            assert_eq!(error.channel_id, ChannelId::new(42));
            if allowed {
                assert!(matches!(
                    error.kind,
                    RenderToChannelErrorKind::Render(RenderError {
                        kind: RenderErrorKind::IfTypstUnsupported
                    })
                ));
            } else {
                assert!(matches!(error.kind, RenderToChannelErrorKind::Messaging(_)));
            }
            assert_no_request(&listener).await;
        }
    }

    #[tokio::test]
    async fn render_contract_preflight_precedence_and_no_http() {
        for allowed in [false, true] {
            let (ctx, listener, _dir) = fixture(allowed).await;
            assert_eq!(
                channel(
                    render_to_channel(
                        &ctx,
                        ChannelId::new(42),
                        r"\iftypst",
                        None,
                        &[HookName::parse("tier-1").unwrap()]
                    )
                    .await
                ),
                json!({"error":"no_rly_hooks cannot be used when no caption is sent"})
            );
            let expected = if allowed {
                json!({"error":"\\iftypst is not supported"})
            } else {
                json!({"error":"channel 42 is not a permitted outbound target"})
            };
            assert_eq!(
                channel(render_to_channel(&ctx, ChannelId::new(42), r"\iftypst", None, &[]).await),
                expected
            );
            assert!(!ctx.state.read().await.is_own_send(9001));
            assert_no_request(&listener).await;
        }
    }

    #[tokio::test]
    async fn render_contract_caption_hook_halts_before_transport() {
        let (ctx, listener, _dir) = fixture(true).await;
        let (ctx, surfaces) = with_hook(
            ctx,
            HookDecision::Halt {
                reason: "render caption halted".into(),
            },
        );
        assert_eq!(
            channel(render_to_channel(&ctx, ChannelId::new(42), "x", Some("caption"), &[]).await),
            json!({"error":"render caption halted","pre_send_feedback":[]})
        );
        assert_eq!(*surfaces.lock().unwrap(), ["render-latex-caption"]);
        assert!(!ctx.state.read().await.is_own_send(9001));
        assert_no_request(&listener).await;
    }

    #[tokio::test]
    async fn render_contract_multipart_upload_and_sent_state() {
        for caption in [None, Some("original caption"), Some("")] {
            let (ctx, listener, _dir) = fixture(true).await;
            let (ctx, surfaces) = with_hook(ctx, HookDecision::Continue);
            let exchange = tokio::spawn(upload_exchange(listener, "200 OK", sent_message()));
            let response = timeout(Duration::from_secs(30), async {
                channel(render_to_channel(&ctx, ChannelId::new(42), "x", caption, &[]).await)
            })
            .await
            .unwrap();
            assert_eq!(response, json!({"ok":true,"message_id":"9001"}));
            assert!(ctx.state.read().await.is_own_send(9001));
            let requests = exchange.await.unwrap();
            assert!(requests[0].starts_with(b"POST /api/v10/channels/42/typing HTTP/1.1\r\n"));
            assert!(requests[1].starts_with(b"POST /api/v10/channels/42/messages HTTP/1.1\r\n"));
            let upload = &requests[1];
            let text = String::from_utf8_lossy(upload);
            assert!(text.contains("filename=\"math.png\""), "{text}");
            assert!(upload.windows(8).any(|bytes| bytes == b"\x89PNG\r\n\x1a\n"));
            let payload_start = text.find("name=\"payload_json\"").unwrap();
            let payload = text[payload_start..]
                .split_once("\r\n\r\n")
                .unwrap()
                .1
                .split("\r\n")
                .next()
                .unwrap();
            let payload: Value = serde_json::from_str(payload).unwrap();
            match caption {
                Some(caption) => {
                    assert_eq!(payload["content"], caption);
                    assert_eq!(*surfaces.lock().unwrap(), ["render-latex-caption"]);
                }
                None => {
                    assert!(payload.get("content").is_none());
                    assert!(surfaces.lock().unwrap().is_empty());
                }
            }
        }
    }

    #[tokio::test]
    async fn render_contract_empty_caption_accepts_registered_override() {
        let (ctx, listener, _dir) = fixture(true).await;
        let (ctx, surfaces) = with_hook(
            ctx,
            HookDecision::Halt {
                reason: "must be bypassed".into(),
            },
        );
        let exchange = tokio::spawn(upload_exchange(listener, "200 OK", sent_message()));
        let hooks = [HookName::parse("capture-caption").unwrap()];
        let response = timeout(Duration::from_secs(30), async {
            channel(render_to_channel(&ctx, ChannelId::new(42), "x", Some(""), &hooks).await)
        })
        .await
        .unwrap();
        assert_eq!(response, json!({"ok":true,"message_id":"9001"}));
        assert!(surfaces.lock().unwrap().is_empty());
        assert_eq!(exchange.await.unwrap().len(), 2);
    }
    #[tokio::test]
    async fn render_contract_failed_upload_does_not_record_send() {
        let (ctx, listener, _dir) = fixture(true).await;
        let exchange = tokio::spawn(upload_exchange(
            listener,
            "403 Forbidden",
            json!({"code":50013,"message":"Missing Permissions"}),
        ));
        let response = timeout(Duration::from_secs(30), async {
            channel(render_to_channel(&ctx, ChannelId::new(42), "x", None, &[]).await)
        })
        .await
        .unwrap();
        assert_eq!(
            response,
            json!({"error":"failed to send: Missing Permissions"})
        );
        assert!(!ctx.state.read().await.is_own_send(9001));
        assert_eq!(exchange.await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn render_contract_persisted_png_survives_return() {
        assert_eq!(
            file(render_file(r"\iftypst")),
            json!({"error":"\\iftypst is not supported"})
        );
        let response = file(render_file("x"));
        let path = std::path::PathBuf::from(response["path"].as_str().unwrap());
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(path.is_absolute());
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("dione-latex-") && name.ends_with(".png"));
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(
            response,
            json!({"ok":true,"path":path.to_string_lossy(),"size_bytes":bytes.len()})
        );
    }
}
