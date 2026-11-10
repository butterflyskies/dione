//! Root consumer fixtures enter the same concrete gateway event boundary as the SDK.
use dione_discord_runtime::{GatewayHandler, GatewayServices, IngressLedger};
use std::sync::Arc;

pub(crate) fn module() -> dione_contracts::ModuleInstanceId {
    dione_contracts::ModuleInstanceId::new(
        dione_contracts::ProviderId::new("discord").unwrap(),
        dione_contracts::ModuleSlotId::new("fixture").unwrap(),
    )
}

pub(crate) fn handler(
    ledger: Arc<IngressLedger>,
    state: dione_discord_state::State,
    directory: camino::Utf8PathBuf,
) -> (
    GatewayHandler,
    tokio::sync::mpsc::Receiver<dione_discord_events::NotificationEvent>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    let (presence, _) = dione_discord_presence::wire_shared_presence();
    let queue = Arc::new(tokio::sync::Mutex::new(
        dione_access_requests::AccessQueue::load(&directory),
    ));
    (
        GatewayHandler::new(GatewayServices::new(
            state,
            queue,
            tx,
            directory,
            ledger,
            presence,
            module(),
        )),
        rx,
    )
}
