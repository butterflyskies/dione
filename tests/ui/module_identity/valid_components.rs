use dione::core::{ModuleInstanceId, ModuleSlotId, ProviderId};

fn main() {
    let provider = ProviderId::new("discord").unwrap();
    let slot = ModuleSlotId::new("primary").unwrap();
    let _module = ModuleInstanceId::new(provider, slot);
}
