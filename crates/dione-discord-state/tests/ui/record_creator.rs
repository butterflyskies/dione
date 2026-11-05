use dione_discord_state::SharedState;
fn main() {
    SharedState::new().record_webhook_creator(40, 200);
}
