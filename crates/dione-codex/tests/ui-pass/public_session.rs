fn main() {
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let path = camino::Utf8Path::from_path(directory.path()).unwrap();
        let module = dione_contracts::ModuleInstanceId::new(
            dione_contracts::ProviderId::new("discord").unwrap(),
            dione_contracts::ModuleSlotId::new("primary").unwrap(),
        );
        let session = dione_codex::CodexSession::prepare(
            path,
            dione_codex::CodexSessionOptions::new(
                None,
                None,
                dione_codex::LegacyDiscordScope::new(module),
            ),
        )
        .await
        .unwrap();
        let connection = session.connection();
        let bound = connection
            .protocol()
            .call(
                "bind_codex_thread",
                &serde_json::json!({"thread_id":"public-thread"}),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bound["bound"], true);
        assert_eq!(session.admission().status().await.queued, 0);
        drop(dione_codex::teams_delivery(session.admission()));
    });
}
