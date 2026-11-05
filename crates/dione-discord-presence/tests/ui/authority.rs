use dione_discord_presence::SharedPresence;
fn reset(authority: &mut SharedPresence) {
    authority.effects = Default::default();
}
fn main() {}
