use dione_provider_delivery::{
    TeamsAdmissionOutcome, TeamsDelivery, TeamsDeliveryError, TeamsEventSink, TeamsResidentEvent,
};
use dione_teams::{
    AdmissionPolicy, ChannelPolicy,
    edge::{ClientSecretTokenProvider, TeamsEdge},
    runtime::TeamsResidentBridge,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
};
struct Sink;
impl TeamsEventSink for Sink {
    fn deliver<'a>(
        &'a self,
        _: TeamsResidentEvent,
    ) -> Pin<Box<dyn Future<Output = Result<TeamsAdmissionOutcome, TeamsDeliveryError>> + Send + 'a>>
    {
        Box::pin(async { Ok(TeamsAdmissionOutcome::Committed) })
    }
}
fn main() {
    let app_id: dione_teams_identities::ApplicationId = "app".parse().unwrap();
    let tenant_id = "tenant".parse().unwrap();
    let client = reqwest::Client::new();
    let provider = ClientSecretTokenProvider::new(
        client.clone(),
        &tenant_id,
        app_id.clone(),
        secrecy::SecretString::from("synthetic".to_owned()),
    )
    .unwrap();
    let policy = AdmissionPolicy {
        app_id,
        tenant_id,
        allowed_service_hosts: BTreeSet::from(["connector.example".to_owned()]),
        allowed_conversations: BTreeSet::from(["conversation".parse().unwrap()]),
        channels: BTreeMap::from([(
            "msteams".to_owned(),
            ChannelPolicy {
                requires_key_endorsement: true,
            },
        )]),
    };
    let bridge = TeamsResidentBridge::new(
        TeamsEdge::new(client, provider, policy).unwrap(),
        TeamsDelivery::new(Sink),
    );
    assert_eq!(
        dione_teams::mcp_tools(Some(std::sync::Arc::new(bridge)))
            .unwrap()
            .len(),
        1
    );
    assert!(dione_teams::mcp_tools(None).unwrap().is_empty());
}
