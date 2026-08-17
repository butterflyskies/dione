#[path = "../../../src/discord/verified_action.rs"]
mod verified_action;

use verified_action::AdmissionFacts;

fn main() {
    let _ = AdmissionFacts {
        binding: loop {},
        provider: loop {},
        provenance: loop {},
        policy_generation: loop {},
        policy_fingerprint: loop {},
        admitted_at: loop {},
    };
}
