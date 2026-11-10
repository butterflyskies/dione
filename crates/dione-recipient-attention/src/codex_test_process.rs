use camino::Utf8PathBuf;
use dione_attention::delivery::OwnedAttentionDescriptor;
use dione_harness::{
    AttentionPolicy, HarnessConnection, HarnessDelivery, HarnessError, HarnessProtocol,
    ProtocolOutput,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    process::{Child, Command},
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
}
impl Proxy {
    async fn command(&self, request: Value) -> Value {
        self.client
            .post(&self.url)
            .json(&request)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
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
    _directory: tempfile::TempDir,
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
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            url: format!("http://{address}/"),
            tools: vec![],
        };
        proxy.tools =
            serde_json::from_value(proxy.command(json!({"command":"tools"})).await).unwrap();
        Self {
            child,
            _directory: directory,
            proxy,
        }
    }
    pub(crate) async fn command(&self, request: Value) -> Value {
        self.proxy.command(request).await
    }
    pub(crate) async fn decoded_attention(
        &self,
        payload: &Value,
        scope: &dione_contracts::ModuleInstanceId,
    ) -> OwnedAttentionDescriptor {
        let response=self.command(json!({"command":"decode_attention","payload":payload,"provider":scope.provider().as_str(),"slot":scope.slot().as_str()})).await;
        crate::codex_test_wire::decode_descriptor(&response)
    }
    pub(crate) fn connection(&self) -> HarnessConnection {
        HarnessConnection::from_adapter(self.proxy.clone(), ProtocolOutput::new(tokio::io::sink()))
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
