use dione_discord_state::SharedState;
fn main() {
    SharedState::new().webhook_creator_order.clear();
}
