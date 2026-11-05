async fn forge(queue: &dione_codex::CodexEventQueue) {
    queue.test_set_persist_failure(None).await;
}
fn main() {}
