use dione_teams::{edge::ClientSecretTokenProvider,runtime::TeamsResidentBridge};
fn forbidden(bridge:TeamsResidentBridge<ClientSecretTokenProvider>) { let _ = bridge.pending; }
fn main() {}
