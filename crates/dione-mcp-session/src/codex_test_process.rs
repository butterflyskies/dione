use crate::teams_test_process::{AdmissionResult, FixtureProcess, ReplyResult};
use camino::Utf8PathBuf;
use dione_harness::{
    AttentionPolicy, HarnessConnection, HarnessDelivery, HarnessError, HarnessProtocol,
    ProtocolOutput,
};
use dione_provider_delivery::{
    ReplyHandle, TeamsAdmissionOutcome, TeamsDelivery, TeamsDeliveryError, TeamsDeliveryErrorKind,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    process::{Child, Command},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[derive(Clone)]
struct Proxy {
    client: reqwest::Client,
    url: String,
    tools: Vec<Value>,
    operation: Arc<AtomicU64>,
}
impl Proxy {
    async fn command(&self, request: Value) -> Value {
        let command = match request["command"].as_str() {
            Some("tools") => "tools",
            Some("teams_setup") => "teams_setup",
            Some("teams_begin") => "teams_begin",
            Some("teams_wait") => "teams_wait",
            Some("teams_reached") => "teams_reached",
            Some("teams_release") => "teams_release",
            Some("teams_cancel") => "teams_cancel",
            Some("teams_drain") => "teams_drain",
            Some("teams_restart") => "teams_restart",
            Some("shutdown") => "shutdown",
            _ => "other",
        };
        self.client
            .post(&self.url)
            .json(&request)
            .send()
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "Codex fixture {command} send failed; timed_out={}",
                    error.is_timeout()
                )
            })
            .error_for_status()
            .unwrap_or_else(|_| panic!("Codex fixture {command} returned an error status"))
            .json()
            .await
            .unwrap_or_else(|_| panic!("Codex fixture {command} response decoding failed"))
    }
}
impl HarnessDelivery for Proxy {
    fn attention_policy(&self) -> AttentionPolicy {
        AttentionPolicy::SafeTurn
    }
    fn deliver<'a>(
        &'a self,
        value: &'a dione_messages::delivery::Delivery,
    ) -> BoxFuture<'a, Result<(), HarnessError>> {
        Box::pin(async move {
            let delivery = crate::codex_test_wire::encode(value);
            let source = delivery["messages"][0]["source"].clone();
            let reply = self
                .command(json!({"command":"deliver","delivery":delivery}))
                .await;
            match reply["kind"].as_str().unwrap() {
                "ok" => Ok(()),
                "completion_unknown" => {
                    assert_eq!(
                        reply["source"], source,
                        "completion attestation belongs to actual first observed source"
                    );
                    Err(HarnessError::completion_unknown(std::io::Error::other(
                        "remote Codex owner observed completion unknown",
                    )))
                }
                other => panic!("unexpected actual owner delivery result {other}"),
            }
        })
    }
    fn invalidate_attention(
        &self,
        _: dione_contracts::MessageRef,
    ) -> BoxFuture<'_, Result<(), HarnessError>> {
        Box::pin(async { panic!("prototype forwards binding protocol only") })
    }
}
impl HarnessProtocol for Proxy {
    fn capabilities(&self) -> Value {
        json!({"tools":{}})
    }
    fn tools(&self) -> Vec<Value> {
        self.tools.clone()
    }
    fn resident_control(&self) -> bool {
        false
    }
    fn permission_relay(&self) -> bool {
        false
    }
    fn call<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
    ) -> BoxFuture<'a, Option<Result<Value, HarnessError>>> {
        Box::pin(async move {
            if !self.tools.iter().any(|tool| tool["name"] == name) {
                return None;
            }
            let reply = self
                .command(json!({"command":"call","name":name,"args":args}))
                .await;
            Some(if let Some(error) = reply["error"].as_str() {
                Err(HarnessError::source(std::io::Error::other(
                    error.to_owned(),
                )))
            } else {
                Ok(reply["ok"].clone())
            })
        })
    }
}

pub(crate) struct OwnerProcess {
    child: ChildGuard,
    directory: tempfile::TempDir,
    proxy: Proxy,
}
impl OwnerProcess {
    pub(crate) async fn start() -> Self {
        let directory = tempfile::TempDir::new().unwrap();
        let path = Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
        let mut child = ChildGuard(
            Command::new(std::env::var_os("DIONE_CODEX_FIXTURE_EXECUTABLE").expect(
                "verification setup supplies exact current Codex owner fixture compiler artifact",
            ))
            .args([
                "--exact",
                "boundary_fixture::private_owner_fixture_service",
                "--ignored",
                "--nocapture",
            ])
            .env("DIONE_CODEX_BOUNDARY_STATE", path.as_str())
            .spawn()
            .unwrap(),
        );
        let address = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Ok(address) = std::fs::read_to_string(path.join("ready")) {
                    break address;
                }
                assert!(
                    child.0.try_wait().unwrap().is_none(),
                    "owner fixture exited before readiness"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let mut proxy = Proxy {
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(120))
                .build()
                .unwrap(),
            url: format!("http://{address}/"),
            tools: vec![],
            operation: Arc::new(AtomicU64::new(1)),
        };
        proxy.tools =
            serde_json::from_value(proxy.command(json!({"command":"tools"})).await).unwrap();
        Self {
            child,
            directory,
            proxy,
        }
    }
    pub(crate) async fn command(&self, request: Value) -> Value {
        self.proxy.command(request).await
    }

    pub(crate) fn connection(&self) -> HarnessConnection {
        HarnessConnection::from_adapter(self.proxy.clone(), ProtocolOutput::new(tokio::io::sink()))
    }
    pub(crate) async fn server(&self) -> crate::mcp::server::DioneServer {
        let path = Utf8PathBuf::from_path_buf(self.directory.path().to_owned()).unwrap();
        let connection = self.connection();
        let (notification_tx, _notification_rx) = tokio::sync::mpsc::channel(1);
        crate::mcp::server::DioneServer::new(
            crate::TEST_APPLICATION_VERSION,
            dione_discord_state::new_state(),
            std::sync::Arc::new(tokio::sync::Mutex::new(
                dione_access_requests::AccessQueue::load(&path),
            )),
            std::sync::Arc::new(serenity::http::Http::new("fake")),
            path.clone(),
            notification_tx,
            dione_tracing_channel::TraceLevelController::noop(),
            connection,
            std::sync::Arc::new(dione_outbound_consent::consent::ConsentGate::new(&path)),
            std::sync::Arc::new(dione_discord_runtime::IngressLedger::new()),
            dione_contracts::ModuleInstanceId::new(
                dione_contracts::ProviderId::new("discord").unwrap(),
                dione_contracts::ModuleSlotId::new("primary").unwrap(),
            ),
            None,
        )
        .await
        .unwrap()
    }
    pub(crate) async fn shutdown(&mut self) {
        self.command(json!({"command":"shutdown"})).await;
        let status = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(status) = self.child.0.try_wait().unwrap() {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(status.success(), "owner fixture completed normally");
    }
}

struct TeamsProxy(Proxy);
impl dione_provider_delivery::TeamsEventSink for TeamsProxy {
    fn deliver<'a>(
        &'a self,
        event: dione_provider_delivery::TeamsResidentEvent,
    ) -> BoxFuture<
        'a,
        Result<
            dione_provider_delivery::TeamsAdmissionOutcome,
            dione_provider_delivery::TeamsDeliveryError,
        >,
    > {
        Box::pin(async move {
            let operation_id = self.0.operation.fetch_add(1, Ordering::SeqCst);
            self.0.command(json!({"command":"teams_begin","operation_id":operation_id,"event":{
                "provider_key":event.provider_key,"content":event.content,"message_id":event.message_id,
                "conversation_id":event.conversation_id,"channel_id":event.channel_id,"user_id":event.user_id,
                "bot_id":event.bot_id,"tenant_id":event.tenant_id,"reply_handle":event.reply_handle,"timestamp":event.timestamp
            }})).await;
            loop {
                let result = self
                    .0
                    .command(json!({"command":"teams_wait","operation_id":operation_id}))
                    .await;
                if result["kind"] != "pending" {
                    assert_eq!(result["operation_id"], operation_id);
                }
                match result["kind"].as_str().unwrap() {
                    "pending" => tokio::time::sleep(Duration::from_millis(10)).await,
                    "committed" => return Ok(TeamsAdmissionOutcome::Committed),
                    "duplicate" => return Ok(TeamsAdmissionOutcome::Duplicate),
                    "owner_join_panic" => {
                        assert_eq!(result["operation_id"], operation_id);
                        return Ok(TeamsAdmissionOutcome::CompletionUnknown(
                            TeamsDeliveryError {
                                kind: TeamsDeliveryErrorKind::Storage(Box::new(
                                    crate::teams_test_process::RemoteOwnerPanic { operation_id },
                                )),
                            },
                        ));
                    }
                    "route_unavailable" | "cancelled" => {
                        return Err(TeamsDeliveryError {
                            kind: TeamsDeliveryErrorKind::RouteUnavailable(Box::new(
                                std::io::Error::other(
                                    "owner admission caller explicitly cancelled",
                                ),
                            )),
                        });
                    }
                    other => panic!("unexpected owner operation result {other}"),
                }
            }
        })
    }
}

#[tokio::test]
async fn owned_admission_completion_retains_native_authority_after_queue_restart() {
    for lose_response in [false, true] {
        let mut owner = OwnerProcess::start().await;
        let proxy = owner.proxy.clone();
        proxy
            .command(json!({"command":"teams_setup","lose_response":lose_response}))
            .await;
        let mut fixture = FixtureProcess::start(
            TeamsDelivery::new(TeamsProxy(proxy.clone())),
            "owned/admission",
        )
        .await;
        fixture.begin(1, &fixture.activity("owned/admission")).await;
        proxy.command(json!({"command":"teams_reached"})).await;
        if lose_response {
            proxy.command(json!({"command":"teams_release"})).await;
            assert!(matches!(
                fixture.wait(1).await,
                AdmissionResult::CompletionUnknown
            ));
            assert_eq!(fixture.owner_join_panic_count(), 1);
            assert_eq!(fixture.owner_operation_id(), 1);
            assert_eq!(fixture.join_panic_count(), 0);
        } else {
            fixture.cancel(1).await;
            proxy
                .command(json!({"command":"teams_cancel","operation_id":1}))
                .await;
            proxy.command(json!({"command":"teams_release"})).await;
        }
        proxy.command(json!({"command":"teams_drain"})).await;
        fixture.close().await;
        let recovered = proxy.command(json!({"command":"teams_restart"})).await;
        assert_eq!(recovered["message_id"], "owned/admission");
        assert_eq!(recovered["genuine_panics"], u64::from(lose_response));
        fixture
            .reopen(TeamsDelivery::new(TeamsProxy(proxy.clone())))
            .await;
        let handle = ReplyHandle::parse(recovered["handle"].as_str().unwrap()).unwrap();
        let before = fixture.snapshot().await;
        assert!(matches!(
            fixture
                .reply(&handle, "reply after owned admission restart")
                .await,
            ReplyResult::Sent { .. }
        ));
        let after = fixture.snapshot().await;
        assert_eq!(after.replies.len(), before.replies.len() + 1);
        assert_eq!(after.token_requests, before.token_requests + 1);
        assert!(matches!(
            fixture.reply(&handle, "must not send twice").await,
            ReplyResult::UnknownReplyHandle
        ));
        let repeated = fixture.snapshot().await;
        assert_eq!(repeated.replies.len(), before.replies.len() + 1);
        assert_eq!(repeated.token_requests, before.token_requests + 1);
        fixture.shutdown().await;
        owner.shutdown().await;
    }
}
