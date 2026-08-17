#[path = "../../../src/discord/verified_action.rs"]
mod verified_action;

use verified_action::{Resolved, Unresolved};

fn main() {
    let _ = Unresolved { private: () };
    let _ = Resolved { state: loop {} };
}
