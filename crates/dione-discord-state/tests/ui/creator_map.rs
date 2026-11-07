use dione_discord_state::SharedState;
fn main() {
    SharedState::new().webhook_creators.clear();
}
