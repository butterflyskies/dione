async fn forge(queue: &dione_codex::CodexEventQueue) {
    let _ = queue.register_live_consumer().await;
}
fn main() {}
