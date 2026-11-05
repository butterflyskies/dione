use dione_discord_presence::PresenceSnapshot;
fn erase(snapshot: &mut PresenceSnapshot) {
    snapshot.desired = None;
}
fn main() {}
