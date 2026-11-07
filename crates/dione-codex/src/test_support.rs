use dione_attention::delivery::{
    AttentionDeliveryGuard, AttentionDeliveryReceipt, AttentionGuardFailure,
};
use dione_harness::HarnessConnection;
use futures_util::{SinkExt, StreamExt, future::BoxFuture};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{
    net::UnixListener,
    sync::{Notify, mpsc},
    task::JoinHandle,
    time::timeout,
};
use tokio_tungstenite::{accept_async, tungstenite::Message};
#[derive(Debug)]
pub(crate) struct Guard;
impl AttentionDeliveryGuard for Guard {
    fn check(
        &self,
        _: &dione_attention::delivery::AttentionDescriptor<'_>,
    ) -> Result<(), AttentionGuardFailure> {
        Ok(())
    }
    fn receipt<'a>(
        self: Arc<Self>,
        _: &'a dione_attention::types::RecordId,
        _: AttentionDeliveryReceipt,
    ) -> BoxFuture<'a, Result<(), dione_attention::delivery::AttentionReceiptError>> {
        Box::pin(async { Ok(()) })
    }
}

pub(crate) fn peer(
    listener: UnixListener,
    barrier: Arc<Notify>,
) -> (JoinHandle<()>, mpsc::UnboundedReceiver<Value>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            while let Some(Ok(message)) = socket.next().await {
                let Message::Text(text) = message else {
                    continue;
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                let Some(id) = request.get("id") else {
                    continue;
                };
                if request["method"] == "initialize" {
                    tx.send(request.clone()).unwrap();
                    barrier.notified().await;
                }
                let result = if request["method"] == "thread/read" {
                    json!({"thread":{"status":{"type":"idle"},"turns":[]}})
                } else {
                    json!({})
                };
                if socket
                    .send(Message::Text(json!({"id":id,"result":result}).to_string()))
                    .await
                    .is_err()
                {
                    break;
                }
                if request["method"] == "turn/start" {
                    tx.send(request).unwrap();
                }
            }
        }
    });
    (task, rx)
}

pub(crate) async fn receive(rx: &mut mpsc::UnboundedReceiver<Value>) -> Value {
    timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("session must deliver through its worker")
        .expect("peer remains open")
}

pub(crate) async fn wait_for_worker(connection: &HarnessConnection) {
    timeout(Duration::from_secs(5), async {
        loop {
            let status = connection
                .protocol()
                .call("event_queue_status", &json!({}))
                .await
                .unwrap()
                .unwrap();
            if !status["primary_consumer"].is_null() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("worker registers its durable consumer");
}

pub(crate) fn text(text: &str) -> dione_messages::delivery::Delivery {
    dione_messages::delivery::Delivery::control(
        dione_messages::events::ControlEvent::Trace {
            level: dione_messages::events::TraceLevel::INFO,
            target: "fixture".to_owned(),
            message: text.to_owned(),
            fields: Vec::new(),
        },
        dione_messages::delivery::PresentationContext::new(
            None,
            dione_messages::delivery::EvidenceVisibility::Enabled,
        ),
    )
}
