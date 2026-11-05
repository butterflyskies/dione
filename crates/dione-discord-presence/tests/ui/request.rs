use dione_discord_presence::{OnlineStatus, PresenceRequest};
fn main() {
    let mut request = PresenceRequest::new(OnlineStatus::Online, None);
    request.status = OnlineStatus::Idle;
}
