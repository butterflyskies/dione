use dione_discord_presence::{ActivityType, PresenceActivity};
fn main() {
    let mut activity = PresenceActivity::new(ActivityType::Playing, String::new());
    activity.name = "replacement".to_owned();
}
