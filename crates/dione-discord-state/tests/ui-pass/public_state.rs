use dione_discord_state::{PendingPermission, SharedState, new_state, observe_webhook_creator};
use serenity::model::id::ChannelId;
fn main() {
    let shared = new_state();
    let _alias = shared.clone();
    let mut state = SharedState::new();
    state.record_dm_channel(1, 2);
    state.pending_permissions.insert(3, PendingPermission { request_id: "fixture".into(), channel_id: ChannelId::new(2), created_at: chrono::Utc::now() });
    let _observer = observe_webhook_creator;
}
