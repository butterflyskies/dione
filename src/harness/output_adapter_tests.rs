use super::test_support;
use dione_harness::ProtocolOutput;
use tokio::io::AsyncReadExt;

#[tokio::test(start_paused = true)]
async fn adapters_distinguish_partial_write_timeout_from_unattempted_poison_failure() {
    for adapter in [
        crate::harness::claude_with_output,
        crate::harness::oh_my_pi_with_output,
    ] {
        let (writer, mut reader) = tokio::io::duplex(1);
        let connection = adapter(ProtocolOutput::new(writer));
        let error = connection
            .deliver(&test_support::text("hello"))
            .await
            .unwrap_err();
        assert!(error.completion_is_unknown());
        let mut first_byte = [0; 1];
        reader.read_exact(&mut first_byte).await.unwrap();
        assert_eq!(first_byte, [b'{']);
        let error = connection
            .deliver(&test_support::text("never written"))
            .await
            .unwrap_err();
        assert!(!error.completion_is_unknown());
    }
}
