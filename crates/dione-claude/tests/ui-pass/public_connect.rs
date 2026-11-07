fn main() {
    let connection = dione_claude::connect(
        dione_harness::ProtocolOutput::new(tokio::io::sink()),
        &["actual-composition-catalog"],
    );
    assert_eq!(connection.attention_policy(), dione_harness::AttentionPolicy::Direct);
}
