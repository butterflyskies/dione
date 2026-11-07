use dione_teams::{edge::ClientSecretTokenProvider,runtime::TeamsResidentBridge};
fn forbidden(bridge:TeamsResidentBridge<ClientSecretTokenProvider>) { bridge.poison_pending_for_test(); }
fn main() {}
