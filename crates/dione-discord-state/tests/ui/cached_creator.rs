use dione_discord_state::SharedState;
fn main() {
    SharedState::new().cached_webhook_creator(40);
}
