//! Authenticated Teams ingress and private OMP controls share the assembled MCP route.
use super::*;
use crate::{
    harness::omp_session::OmpSession,
    teams::{edge::tests::ConnectorFixture, runtime::TeamsResidentBridge},
};
use axum::response::IntoResponse;
use sha2::{Digest as _, Sha256};
use std::{
    io::{Read as _, Write as _},
    path::PathBuf,
    process::Stdio,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader, DuplexStream},
    signal::unix::{SignalKind, signal},
};
use tokio_util::sync::CancellationToken;

async fn ingress(
    bridge: &Arc<TeamsResidentBridge<crate::teams::edge::ClientSecretTokenProvider>>,
    authorization: &str,
    body: &[u8],
) -> axum::http::StatusCode {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        authorization.parse().unwrap(),
    );
    crate::teams::runtime::messages(
        axum::extract::State(bridge.clone()),
        headers,
        axum::body::Bytes::copy_from_slice(body),
    )
    .await
    .into_response()
    .status()
}

async fn control(server: &DioneServer, name: &str, arguments: Value) -> Value {
    let response = handle_request_from(
        server,
        request(name, arguments),
        RequestOrigin::ResidentControl,
    )
    .await
    .unwrap();
    assert!(response.get("error").is_none(), "{response}");
    result_value(&response)
}
async fn event(reader: &mut BufReader<DuplexStream>) -> Value {
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    let frame: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(frame["method"], "notifications/dione/event");
    assert_eq!(frame["params"]["version"], 1);
    frame["params"].clone()
}

#[tokio::test]
async fn authenticated_teams_replays_after_restart_and_replies_through_registered_tool() {
    let fixture = ConnectorFixture::start().await;
    let directory = tempfile::TempDir::new().unwrap();
    let path = Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
    let session = OmpSession::open(&path).await.unwrap();
    let (writer, reader) = tokio::io::duplex(64 * 1024);
    let output = ProtocolOutput::new(writer);
    let harness =
        HarnessConnection::oh_my_pi_with_control_and_output(session.control(), output.clone());
    let bridge = Arc::new(
        TeamsResidentBridge::new_durable(
            fixture.edge.clone(),
            crate::composition::teams_admission::from_omp(session.admission()),
            &path,
        )
        .await
        .unwrap(),
    );
    let running = session.start(output, CancellationToken::new());
    let (_server_dir, assembled) = server(harness).await;
    let assembled = assembled
        .with_module_tools(crate::teams::mcp_tools(Some(bridge.clone())).unwrap())
        .unwrap();
    let mut reader = BufReader::new(reader);
    let authorization = fixture.authorization();
    let activity = fixture.activity("activity/id%2F?");
    assert_eq!(
        ingress(&bridge, &authorization, &activity).await,
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    handle_request(&assembled, json!({"method":"notifications/initialized"})).await;
    let binding = control(
        &assembled,
        "teams_bind_session",
        json!({"session_id":"saved-main-session"}),
    )
    .await;
    assert_eq!(binding["bound"], true);
    let mut denied: Value = serde_json::from_slice(&activity).unwrap();
    denied["conversation"]["id"] = json!("excluded/conversation");
    assert_eq!(
        ingress(
            &bridge,
            &authorization,
            &serde_json::to_vec(&denied).unwrap()
        )
        .await,
        axum::http::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        ingress(&bridge, "Bearer invalid", &activity).await,
        axum::http::StatusCode::UNAUTHORIZED
    );
    assert_eq!(fixture.token_requests(), 0);
    assert_eq!(
        ingress(&bridge, &authorization, &activity).await,
        axum::http::StatusCode::ACCEPTED
    );
    assert_eq!(
        ingress(&bridge, &authorization, &activity).await,
        axum::http::StatusCode::ACCEPTED
    );
    let first = event(&mut reader).await;
    assert_eq!(first["meta"]["provider"], "teams");
    assert_eq!(first["meta"]["message_id"], "activity/id%2F?");
    assert_eq!(first["meta"]["conversation_id"], "conversation/id");
    assert_eq!(first["meta"]["mcp_session_id"], "saved-main-session");
    assert_eq!(first["meta"]["binding_id"], binding["binding_id"]);
    assert_eq!(first["content"], "hello");
    let discord = crate::harness::test_support::delivery(
        crate::discord::events::NotificationEvent::Message(crate::discord::events::MessageEvent {
            chat_id: serenity::model::id::ChannelId::new(10),
            message_id: serenity::model::id::MessageId::new(20),
            user: "Discord author".into(),
            user_id: serenity::model::id::UserId::new(30),
            author_kind: Default::default(),
            content: "Discord still works".into(),
            targeting: crate::discord::events::MessageTargeting::DirectMessage,
            timestamp: crate::timestamp::Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: None,
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
            bells: None,
            bells_status: None,
        }),
    );
    assembled.harness.deliver(&discord).await.unwrap();
    let discord_frame = event(&mut reader).await;
    assert_eq!(discord_frame["meta"]["message_id"], "20");
    assert_ne!(discord_frame["meta"]["provider"], "teams");
    assert!(
        discord_frame["content"]
            .as_str()
            .unwrap()
            .contains("Discord still works")
    );
    let forged = handle_request(&assembled, request("teams_ack_event", json!({"binding_id":binding["binding_id"],"delivery_token":first["meta"]["delivery_token"]}))).await.unwrap();
    assert!(forged.get("error").is_some());
    assembled
        .harness
        .protocol()
        .stdio_disconnected()
        .await
        .unwrap();
    running.shutdown().await.unwrap();
    drop(assembled);
    drop(bridge);

    let session = OmpSession::open(&path)
        .await
        .expect("shutdown releases the inbox lock");
    let (writer, reader) = tokio::io::duplex(64 * 1024);
    let output = ProtocolOutput::new(writer);
    let harness =
        HarnessConnection::oh_my_pi_with_control_and_output(session.control(), output.clone());
    let bridge = Arc::new(
        TeamsResidentBridge::new_durable(
            fixture.edge.clone(),
            crate::composition::teams_admission::from_omp(session.admission()),
            &path,
        )
        .await
        .unwrap(),
    );
    let running = session.start(output, CancellationToken::new());
    let (_server_dir, assembled) = server(harness).await;
    let assembled = assembled
        .with_module_tools(crate::teams::mcp_tools(Some(bridge.clone())).unwrap())
        .unwrap();
    handle_request(&assembled, json!({"method":"notifications/initialized"})).await;
    let binding = control(
        &assembled,
        "teams_bind_session",
        json!({"session_id":"saved-main-session"}),
    )
    .await;
    let mut reader = BufReader::new(reader);
    let replay = event(&mut reader).await;
    assert_eq!(replay["meta"]["event_id"], first["meta"]["event_id"]);
    assert_eq!(
        replay["meta"]["reply_handle"],
        first["meta"]["reply_handle"]
    );
    assert_ne!(
        replay["meta"]["delivery_token"],
        first["meta"]["delivery_token"]
    );
    assert_ne!(replay["meta"]["binding_id"], first["meta"]["binding_id"]);
    let stale = handle_request_from(&assembled, request("teams_ack_event", json!({"binding_id":binding["binding_id"],"delivery_token":first["meta"]["delivery_token"]})), RequestOrigin::ResidentControl).await.unwrap();
    assert!(stale.get("error").is_some(), "{stale}");
    let acknowledged = control(&assembled, "teams_ack_event", json!({"binding_id":binding["binding_id"],"delivery_token":replay["meta"]["delivery_token"]})).await;
    assert_eq!(acknowledged["acknowledged"], true);
    let reply_request = request(
        "teams_reply",
        json!({"reply_handle":replay["meta"]["reply_handle"],"content":"reply through MCP"}),
    );
    let reply = handle_request(&assembled, reply_request.clone())
        .await
        .unwrap();
    assert!(reply.get("error").is_none(), "{reply}");
    assert_ne!(reply["result"]["isError"], true, "{reply}");
    let captured = fixture.replies();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0].0,
        "/v3/conversations/conversation%2Fid/activities/activity%2Fid%252F%3F"
    );
    assert_eq!(captured[0].1["replyToId"], "activity/id%2F?");
    assert_eq!(captured[0].1["text"], "reply through MCP");
    let repeated = handle_request(&assembled, reply_request).await.unwrap();
    assert!(repeated.get("error").is_some(), "{repeated}");
    assert_eq!(fixture.replies().len(), 1);
    assert_eq!(
        ingress(&bridge, &authorization, &activity).await,
        axum::http::StatusCode::ACCEPTED
    );
    let mut unexpected = String::new();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(1500),
            reader.read_line(&mut unexpected)
        )
        .await
        .is_err()
    );
    running.shutdown().await.unwrap();
}

const CHILD_STATE: &str = "DIONE_OMP_LIFECYCLE_FIXTURE_STATE";
const CHILD_TEST: &str =
    "mcp::server::registration_tests::teams_omp_tests::omp_lifecycle_child_entry";

// Retain only this test's private fixture on unwind; process custody stays with
// kill_on_drop and the external test supervisor, not this synchronous Drop.
struct LifecycleFixtureEvidence {
    directory: Option<tempfile::TempDir>,
    pass: usize,
    stage: &'static str,
    last_error: Option<String>,
    child_status: Option<String>,
}

impl Drop for LifecycleFixtureEvidence {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        let Some(directory) = self.directory.take() else {
            return;
        };
        let root = directory.keep();
        let socket = root.join("control/omp.sock");
        let stderr: Vec<_> = (0..2)
            .map(|pass| {
                let path = root.join(format!("child-{pass}.stderr"));
                let captured = match std::fs::read(&path) {
                    Ok(bytes) => json!({"bytes":bytes}),
                    Err(error) => json!({"read_error":error.to_string()}),
                };
                json!({"path":path,"captured":captured})
            })
            .collect();
        let record = json!({
            "fixture":"lifecycle_failure", "root":root, "pass":self.pass,
            "stage":self.stage, "socket":socket,
            "socket_bytes":socket.as_os_str().len(), "last_error":self.last_error,
            "child_status":self.child_status, "stderr":stderr,
            "capture_may_be_incomplete":true,
            "process_custody":"kill_on_drop and external supervisor; no reap claim from Drop"
        });
        // Keep both structured raw evidence and the private root even if writing
        // the extra receipt fails. Diagnostics must never panic during unwind.
        let mut stderr = std::io::stderr().lock();
        if let Err(error) = std::fs::write(root.join("lifecycle-failure.json"), record.to_string())
        {
            let _ = writeln!(stderr, "fixture evidence write failed: {error}");
        }
        let _ = writeln!(stderr, "{record}");
    }
}

#[test]
fn omp_lifecycle_child_entry() {
    let Some(directory) = std::env::var_os(CHILD_STATE) else {
        return;
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(async {
            let path = Utf8PathBuf::from_path_buf(directory.into()).unwrap();
            let session = OmpSession::open(&path).await.unwrap();
            let admission = session.admission();
            let output = ProtocolOutput::new(tokio::io::stdout());
            let harness = HarnessConnection::oh_my_pi_with_control_and_output(
                session.control(),
                output.clone(),
            );
            let cancel = CancellationToken::new();
            let running = session.start(output.clone(), cancel.clone());
            let (_server_dir, assembled) = server(harness).await;
            let assembled = Arc::new(assembled);
            let mut socket = tokio::spawn(run_resident_control(
                assembled.clone(),
                path.join("control/omp.sock"),
                cancel.clone(),
            ));
            let admit = tokio::spawn(async move {
                let make_event = || {
                    let tenant = "tenant".parse().unwrap();
                    let bot = "bot".parse().unwrap();
                    let conversation = "19:child/conversation".parse().unwrap();
                    let activity = "child/activity".parse().unwrap();
                    crate::teams::delivery::TeamsResidentEvent {
                        provider_key: crate::delivery::ProviderEventKey::teams(
                            tenant,
                            bot,
                            conversation,
                            activity,
                        ),
                        content: "retained across actual process exit".into(),
                        message_id: "child/activity".parse().unwrap(),
                        conversation_id: "19:child/conversation".parse().unwrap(),
                        channel_id: "msteams".into(),
                        user_id: "participant".parse().unwrap(),
                        bot_id: "bot".parse().unwrap(),
                        tenant_id: "tenant".parse().unwrap(),
                        reply_handle: ReplyHandle::generate(),
                        timestamp: crate::timestamp::Timestamp::now(),
                    }
                };
                tokio::time::timeout(Duration::from_secs(8), async {
                    loop {
                        match admission.enqueue(make_event()).await {
                            Ok(_) => break,
                            Err(error)
                                if matches!(
                                    error.kind,
                                    crate::harness::omp_session::OmpSessionErrorKind::Unavailable
                                ) =>
                            {
                                tokio::time::sleep(Duration::from_millis(10)).await
                            }
                            Err(error) => panic!("fixture admission failed: {error}"),
                        }
                    }
                })
                .await
                .unwrap();
            });
            let mut sigterm = signal(SignalKind::terminate()).unwrap();
            let socket_path = path.join("control/omp.sock");
            let ready = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match UnixStream::connect(&socket_path).await {
                        Ok(stream) => break Ok::<_, std::io::Error>(stream),
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::NotFound
                                    | std::io::ErrorKind::ConnectionRefused
                            ) =>
                        {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        Err(error) => break Err(error),
                    }
                }
            });
            tokio::select! {
                biased;
                result = &mut socket => {
                    eprintln!("{}", json!({"fixture":"startup_error", "stage":"control_task",
                        "socket":socket_path, "socket_bytes":socket_path.as_str().len(),
                        "error":format!("{result:?}")}));
                    panic!("control task exited before readiness");
                }
                result = ready => match result {
                    Ok(Ok(stream)) => drop(stream),
                    failure => {
                        eprintln!("{}", json!({"fixture":"startup_error", "stage":"control_connect",
                            "socket":socket_path, "socket_bytes":socket_path.as_str().len(),
                            "error":format!("{failure:?}")}));
                        panic!("control listener did not become ready within five seconds");
                    }
                }
            }
            output.write(&json!({"fixture":"ready"})).await.unwrap();
            let (_sender, receiver) = mpsc::channel(1);
            let serving = run(
                assembled,
                receiver,
                cancel.clone(),
                StdioExitPolicy::ShutdownProcess,
            );
            tokio::pin!(serving);
            eprintln!(
                "{}",
                json!({"fixture":"phase", "phase":"serving_drain_start"})
            );
            tokio::select! {
                result = &mut serving => result.unwrap(),
                _ = sigterm.recv() => {
                    eprintln!("{}", json!({"fixture":"phase", "phase":"signal_received"}));
                    cancel.cancel();
                    serving.await.unwrap();
                }
            }
            assert!(cancel.is_cancelled());
            eprintln!("{}", json!({"fixture":"phase", "phase":"serving_drained"}));
            eprintln!(
                "{}",
                json!({"fixture":"phase", "phase":"admission_drain_start"})
            );
            admit.await.unwrap();
            eprintln!(
                "{}",
                json!({"fixture":"phase", "phase":"admission_drained"})
            );
            eprintln!(
                "{}",
                json!({"fixture":"phase", "phase":"control_drain_start"})
            );
            socket.await.unwrap().unwrap();
            eprintln!("{}", json!({"fixture":"phase", "phase":"control_drained"}));
            eprintln!(
                "{}",
                json!({"fixture":"phase", "phase":"resident_drain_start"})
            );
            running.shutdown().await.unwrap();
            eprintln!("{}", json!({"fixture":"phase", "phase":"resident_drained"}));
        });
        eprintln!(
            "{}",
            json!({"fixture":"phase", "phase":"async_body_returned"})
        );
        // Only successful, fully drained async work can produce this marker.
        // Keep writes inside catch_unwind so I/O failure cannot hang runtime Drop.
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{}", json!({"fixture":"async_complete"})).unwrap();
        stdout.flush().unwrap();
    }));
    if result.is_err() {
        let _ = writeln!(
            std::io::stderr().lock(),
            "{}",
            json!({"fixture":"phase", "phase":"async_panic"})
        );
    }
    let _ = writeln!(
        std::io::stderr().lock(),
        "{}",
        json!({"fixture":"phase", "phase":"runtime_shutdown_background_start"})
    );
    // Match production teardown policy for uncancellable blocking stdin reads.
    // Successful product-owned drains were awaited above; panic stays a failure.
    runtime.shutdown_background();
    let _ = writeln!(
        std::io::stderr().lock(),
        "{}",
        json!({"fixture":"phase", "phase":"runtime_shutdown_background_returned"})
    );
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

async fn child_json(reader: &mut BufReader<tokio::process::ChildStdout>) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut line = String::new();
            assert_ne!(
                reader.read_line(&mut line).await.unwrap(),
                0,
                "child stdout ended before expected frame"
            );
            if let Ok(frame) = serde_json::from_str::<Value>(&line) {
                return frame;
            }
        }
    })
    .await
    .expect("child must produce a frame within ten seconds")
}

async fn executable_digest(path: PathBuf) -> sha2::digest::Output<Sha256> {
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(path).unwrap();
        let mut hash = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = file.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        hash.finalize()
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn actual_child_stdin_close_stops_socket_releases_lock_and_preserves_backlog() {
    let executable = std::env::current_exe().unwrap();
    let before = executable_digest(executable.clone()).await;
    let directory = tempfile::Builder::new()
        .prefix("dione-omp-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in("/tmp")
        .unwrap();
    let path = Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
    let mut evidence = LifecycleFixtureEvidence {
        directory: Some(directory),
        pass: 0,
        stage: "root_preflight",
        last_error: None,
        child_status: None,
    };
    let metadata = std::fs::symlink_metadata(&path).unwrap();
    assert!(metadata.is_dir() && !metadata.file_type().is_symlink());
    assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
    assert_eq!(metadata.permissions().mode() & 0o7777, 0o700);
    assert_eq!(std::fs::canonicalize(&path).unwrap(), path.as_std_path());
    let socket_path = path.join("control/omp.sock");
    std::os::unix::net::SocketAddr::from_pathname(socket_path.as_std_path())
        .expect("private fixture socket must fit the native Unix address");
    let mut first: Option<Value> = None;
    for pass in 0..2 {
        evidence.pass = pass;
        evidence.stage = "spawn";
        evidence.last_error = None;
        evidence.child_status = None;
        let stderr_path = path.join(format!("child-{pass}.stderr"));
        let mut child = tokio::process::Command::new(&executable)
            .args([
                "--exact",
                CHILD_TEST,
                "--nocapture",
                "--test-threads=1",
                "--quiet",
            ])
            .env(CHILD_STATE, &path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(std::fs::File::create(&stderr_path).unwrap()))
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        evidence.stage = "child_ready";
        assert_eq!(child_json(&mut stdout).await["fixture"], "ready");
        evidence.stage = "tools_list";
        stdin.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n{\"jsonrpc\":\"2.0\",\"id\":77,\"method\":\"tools/list\"}\n").await.unwrap();
        assert_eq!(child_json(&mut stdout).await["id"], 77);
        evidence.stage = "parent_control_connect";
        let mut stream = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match UnixStream::connect(&socket_path).await {
                    Ok(stream) => break stream,
                    Err(error) => {
                        evidence.last_error = Some(format!("{:?}: {error}", error.kind()));
                        if !matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                        ) {
                            panic!("control connect failed: {error}");
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
        })
        .await
        .unwrap();
        evidence.stage = "bind_and_event";
        stream
            .write_all(
                format!(
                    "{}\n",
                    request(
                        "teams_bind_session",
                        json!({"session_id":"actual-child-main"})
                    )
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut line = String::new();
        tokio::time::timeout(
            Duration::from_secs(5),
            BufReader::new(stream).read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        let bound: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(result_value(&bound)["bound"], true);
        let frame = child_json(&mut stdout).await;
        assert_eq!(frame["method"], "notifications/dione/event");
        assert_eq!(frame["params"]["meta"]["message_id"], "child/activity");
        if let Some(original) = &first {
            assert_eq!(
                frame["params"]["meta"]["event_id"],
                original["params"]["meta"]["event_id"]
            );
            assert_eq!(
                frame["params"]["meta"]["reply_handle"],
                original["params"]["meta"]["reply_handle"]
            );
            assert_ne!(
                frame["params"]["meta"]["delivery_token"],
                original["params"]["meta"]["delivery_token"]
            );
        } else {
            first = Some(frame);
        }
        evidence.stage = if pass == 1 { "sigterm" } else { "stdin_close" };
        let mut retained_stdin = Some(stdin);
        if pass == 1 {
            let pid = i32::try_from(child.id().unwrap()).unwrap();
            assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        } else {
            drop(retained_stdin.take());
        }
        let status = match tokio::time::timeout(Duration::from_secs(10), async {
            evidence.stage = "waiting_async_complete";
            assert_eq!(
                child_json(&mut stdout).await,
                json!({"fixture":"async_complete"})
            );
            evidence.stage = "waiting_process_exit";
            child.wait().await
        })
        .await
        {
            Ok(status) => status.unwrap(),
            Err(_) => {
                child.kill().await.unwrap();
                child.wait().await.unwrap();
                panic!(
                    "child did not exit after {}: {}",
                    if pass == 1 {
                        "SIGTERM with stdin retained"
                    } else {
                        "stdin close"
                    },
                    std::fs::read_to_string(stderr_path).unwrap()
                );
            }
        };
        evidence.child_status = Some(status.to_string());
        evidence.stage = "exit_socket_lock_checks";
        // The signal pass cannot borrow EOF as a second shutdown trigger.
        drop(retained_stdin);
        assert!(
            status.success(),
            "{}",
            std::fs::read_to_string(stderr_path).unwrap()
        );
        assert!(UnixStream::connect(&socket_path).await.is_err());
        drop(
            OmpSession::open(&path)
                .await
                .expect("terminated child released its lock"),
        );
    }
    evidence.stage = "executable_digest";
    assert_eq!(before, executable_digest(executable.clone()).await);
}
