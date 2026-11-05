use dione_discord_presence::{ActivityType, OnlineStatus, PresenceActivity, PresenceRequest, wire_shared_presence};
fn main() {
    let request = PresenceRequest::new(OnlineStatus::Online, Some(PresenceActivity::new(ActivityType::Playing, String::new())));
    assert_eq!(request.status(), OnlineStatus::Online);
    let (_gateway, _direct) = wire_shared_presence();
}
