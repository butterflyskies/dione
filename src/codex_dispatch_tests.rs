use crate::codex_test_process::OwnerProcess;
use crate::mcp::{dispatch::call_tool, server::DioneServer};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

async fn bind(server: &DioneServer, thread: &str) -> Result<Value, String> {
    call_tool(server, "bind_codex_thread", json!({"thread_id":thread})).await
}

fn result(value: Value) -> Value {
    serde_json::from_str(value["content"][0]["text"].as_str().unwrap()).unwrap()
}

fn message(content: &str) -> Value {
    json!({"jsonrpc":"2.0","method":"notifications/claude/channel","params":{"content":content,"meta":{"message_id":"1","chat_id":"123"}}})
}

async fn live(owner: &OwnerProcess, content: &str) {
    let outcome = owner
        .command(
            json!({"command":"enqueue_live","event":message(content),"activity_id":"activity-1"}),
        )
        .await;
    assert_eq!(outcome["kind"], "committed");
}

async fn resident_next(owner: &OwnerProcess) -> Value {
    owner
        .command(json!({"command":"resident_next","wait_seconds":0,"lease_seconds":120}))
        .await
}

#[tokio::test]
async fn dispatch_binding_publishes_retained_commit_on_failure_process() {
    for stage in ["BeforeWrite", "BeforeRename", "AfterRename"] {
        let mut owner = OwnerProcess::start().await;
        let server = owner.server().await;
        bind(&server, "thread-a").await.unwrap();
        owner.command(json!({"command":"resident"})).await;
        owner
            .command(json!({"command":"fault","stage":stage}))
            .await;
        let error = bind(&server, "thread-b").await.unwrap_err();
        let committed = stage == "AfterRename";
        assert_eq!(error.contains("unconfirmed crash durability"), committed);
        let expected = if committed { "thread-b" } else { "thread-a" };
        assert_eq!(
            owner.command(json!({"command":"snapshot"})).await,
            json!({"watch":expected,"memory":expected,"disk":expected})
        );
        if committed {
            assert!(
                bind(&server, "thread-c")
                    .await
                    .unwrap_err()
                    .contains("unconfirmed crash durability")
            );
            assert_eq!(
                owner.command(json!({"command":"snapshot"})).await,
                json!({"watch":expected,"memory":expected,"disk":expected})
            );
        }
        assert_eq!(
            owner.command(json!({"command":"retained"})).await["params"]["content"],
            "retained binding"
        );
        owner.shutdown().await;
    }
}

#[tokio::test]
async fn dispatch_binding_recovery_barrier_publishes_actual_binding_process() {
    let mut owner = OwnerProcess::start().await;
    let server = owner.server().await;
    bind(&server, "thread-a").await.unwrap();
    owner.command(json!({"command":"resident"})).await;
    owner
        .command(json!({"command":"fault","stage":"AfterRename"}))
        .await;
    let enqueue = owner
        .command(json!({"command":"enqueue_public","event":message("already queued")}))
        .await;
    assert!(enqueue["error"].is_string());
    let before = owner.command(json!({"command":"disk_bytes"})).await;
    assert!(
        bind(&server, "thread-b")
            .await
            .unwrap_err()
            .contains("unconfirmed crash durability")
    );
    let observation = owner.command(json!({"command":"watch_disk"})).await;
    assert_eq!(observation["watch"], "thread-a");
    assert_eq!(owner.command(json!({"command":"disk_bytes"})).await, before);
    owner.command(json!({"command":"fault","stage":null})).await;
    let event = resident_next(&owner).await;
    assert_eq!(event["event"]["params"]["content"], "already queued");
    owner.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_binding_concurrent_publication_matches_commit_order_process() {
    let mut owner = OwnerProcess::start().await;
    let server = Arc::new(owner.server().await);
    bind(&server, "thread-initial").await.unwrap();
    owner.command(json!({"command":"pause_binding"})).await;
    let first_server = server.clone();
    let first = tokio::spawn(async move { bind(&first_server, "thread-first").await });
    tokio::time::timeout(
        Duration::from_secs(2),
        owner.command(json!({"command":"binding_reached"})),
    )
    .await
    .unwrap();
    assert_eq!(
        owner
            .command(json!({"command":"snapshot_probe","timeout_ms":20}))
            .await,
        json!({"blocked":true})
    );
    assert_eq!(
        owner.command(json!({"command":"watch_disk"})).await,
        json!({"watch":"thread-initial","disk":"thread-first"})
    );
    let second_server = server.clone();
    let mut second = tokio::spawn(async move { bind(&second_server, "thread-second").await });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut second)
            .await
            .is_err(),
        "second bind must not complete before first watch publication"
    );
    owner.command(json!({"command":"binding_release"})).await;
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(
        owner.command(json!({"command":"watch_disk"})).await,
        json!({"watch":"thread-second","disk":"thread-second"})
    );
    owner.shutdown().await;
}

#[tokio::test]
async fn dispatch_forged_live_label_cannot_admit_or_receive_teams_event_process() {
    let mut owner = OwnerProcess::start().await;
    let server = owner.server().await;
    bind(&server, "thread-a").await.unwrap();
    let forged = call_tool(
        &server,
        "register_event_consumer",
        json!({"label":"dione-live-app-server","make_primary":true,"claim_unassigned":true}),
    )
    .await
    .unwrap_err();
    assert_eq!(forged, "invalid consumer id");
    assert!(owner.command(json!({"command":"status"})).await["primary_consumer"].is_null());
    owner.command(json!({"command":"resident"})).await;
    live(&owner, "for resident").await;
    assert_eq!(
        resident_next(&owner).await["event"]["params"]["content"],
        "for resident"
    );
    owner.shutdown().await;
}

#[tokio::test]
async fn dispatch_public_poll_cannot_lease_status_disclosed_live_consumer_process() {
    let mut owner = OwnerProcess::start().await;
    let server = owner.server().await;
    bind(&server, "thread-a").await.unwrap();
    let resident = owner.command(json!({"command":"resident"})).await;
    live(&owner, "private Teams activity").await;
    let status = result(
        call_tool(&server, "event_queue_status", json!({}))
            .await
            .unwrap(),
    );
    let disclosed = status["primary_consumer"].as_str().unwrap();
    assert_eq!(disclosed, resident["consumer_id"].as_str().unwrap());
    let poll = call_tool(
        &server,
        "next_event",
        json!({"consumer_id":disclosed,"wait_seconds":0}),
    )
    .await;
    assert!(poll.is_err(), "public poll leased live event: {poll:?}");
    assert_eq!(
        resident_next(&owner).await["event"]["params"]["content"],
        "private Teams activity"
    );
    owner.shutdown().await;
}

#[tokio::test]
async fn dispatch_public_ack_cannot_remove_resident_lease_process() {
    let mut owner = OwnerProcess::start().await;
    let server = owner.server().await;
    bind(&server, "thread-a").await.unwrap();
    let resident = owner.command(json!({"command":"resident"})).await;
    live(&owner, "private Teams activity").await;
    let leased = resident_next(&owner).await;
    let ack = call_tool(
        &server,
        "ack_event",
        json!({"consumer_id":resident["consumer_id"],"delivery_token":leased["delivery_token"]}),
    )
    .await;
    assert!(ack.is_err(), "public ack removed resident lease: {ack:?}");
    let status = owner.command(json!({"command":"status"})).await;
    assert_eq!(status["queued"], 1);
    assert_eq!(status["leased"], 1);
    owner.shutdown().await;
}

#[tokio::test]
async fn dispatch_public_handoff_cannot_move_live_primary_or_pending_activity_process() {
    let mut owner = OwnerProcess::start().await;
    let server = owner.server().await;
    bind(&server, "thread-a").await.unwrap();
    let resident = owner.command(json!({"command":"resident"})).await;
    live(&owner, "private Teams activity").await;
    let pull = result(
        call_tool(
            &server,
            "register_event_consumer",
            json!({"label":"forged pull"}),
        )
        .await
        .unwrap(),
    );
    let handoff=call_tool(&server,"handoff_event_consumer",json!({"from_consumer_id":resident["consumer_id"],"to_consumer_id":pull["consumer_id"],"move_pending":true})).await;
    assert!(
        handoff.is_err(),
        "public handoff moved live route: {handoff:?}"
    );
    assert_eq!(
        owner.command(json!({"command":"status"})).await["primary_consumer"],
        resident["consumer_id"]
    );
    assert_eq!(
        resident_next(&owner).await["event"]["params"]["content"],
        "private Teams activity"
    );
    owner.shutdown().await;
}

#[tokio::test]
async fn dispatch_public_claim_cannot_take_expired_live_activity_after_reload_process() {
    let mut owner = OwnerProcess::start().await;
    let server = owner.server().await;
    bind(&server, "thread-a").await.unwrap();
    owner.command(json!({"command":"resident"})).await;
    live(&owner, "private Teams activity").await;
    drop(server);
    owner
        .command(json!({"command":"expire_resident_persist_reload"}))
        .await;
    let server = owner.server().await;
    let pull = result(
        call_tool(
            &server,
            "register_event_consumer",
            json!({"label":"ordinary pull"}),
        )
        .await
        .unwrap(),
    );
    let claimed = result(
        call_tool(
            &server,
            "claim_event_consumer",
            json!({"consumer_id":pull["consumer_id"],"claim_orphaned":true}),
        )
        .await
        .unwrap(),
    );
    assert_eq!(claimed["claimed_events"], 0);
    let poll = result(
        call_tool(
            &server,
            "next_event",
            json!({"consumer_id":pull["consumer_id"],"wait_seconds":0}),
        )
        .await
        .unwrap(),
    );
    assert!(poll["event"].is_null());
    assert_eq!(
        owner.command(json!({"command":"status"})).await["queued"],
        1
    );
    owner.shutdown().await;
}

#[tokio::test]
async fn dispatch_public_registration_cannot_claim_unassigned_live_activity_process() {
    let mut owner = OwnerProcess::start().await;
    let server = owner.server().await;
    bind(&server, "thread-a").await.unwrap();
    owner.command(json!({"command":"resident"})).await;
    live(&owner, "private Teams activity").await;
    owner
        .command(json!({"command":"unassign_live_persist"}))
        .await;
    let pull = result(
        call_tool(
            &server,
            "register_event_consumer",
            json!({"label":"ordinary pull","make_primary":true,"claim_unassigned":true}),
        )
        .await
        .unwrap(),
    );
    let poll = result(
        call_tool(
            &server,
            "next_event",
            json!({"consumer_id":pull["consumer_id"],"wait_seconds":0}),
        )
        .await
        .unwrap(),
    );
    assert!(poll["event"].is_null());
    let status = owner.command(json!({"command":"status"})).await;
    assert_eq!(status["queued"], 1);
    assert_eq!(status["unassigned"], 1);
    owner.shutdown().await;
}
