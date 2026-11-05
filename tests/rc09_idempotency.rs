//! RC09 regression: idempotency is durable (DB-backed), covers the FULL
//! canonical request (open_items/constraints changes are conflicts, not
//! replays), and survives a service restart.

mod common;

use common::http::*;
use common::*;

#[test]
fn rc09_handoff_open_items_change_conflicts_not_replays() {
    let env = TestEnv::new("rc09");
    let http = start_http(&env);
    let (_repo, task_id) = common::http::setup_capture_ready(&http, &env);

    let rev = task_revision(&http, &task_id);
    let body1 = serde_json::json!({
        "source_mode": "context_only",
        "next_action": "do X",
        "open_items": [{"id": "C1", "text": "first"}],
        "notes_constraints": [],
        "edited_fields": ["next_action"],
    });
    let r1 = http
        .post_full(&format!("/v1/tasks/{task_id}/handoffs"), body1.clone(), Some("k-handoff"), Some(rev))
        .unwrap();
    let v1: serde_json::Value = r1.into_json().unwrap();
    let id1 = v1["result"]["handoff_id"].as_str().unwrap().to_string();

    // Same key, ONLY open_items changed → 409 conflict (never the old package).
    let body2 = serde_json::json!({
        "source_mode": "context_only",
        "next_action": "do X",
        "open_items": [{"id": "C1", "text": "CHANGED"}],
        "notes_constraints": [],
        "edited_fields": ["next_action"],
    });
    let r2 = http.post_full(
        &format!("/v1/tasks/{task_id}/handoffs"),
        body2,
        Some("k-handoff"),
        Some(rev),
    );
    match r2 {
        Err(ureq::Error::Status(409, _)) => {}
        other => panic!("expected 409 for changed body, got {other:?}"),
    }
    // The stored handoff is still the FIRST one.
    let detail = task_detail(&http, &task_id);
    assert_eq!(detail["handoffs"].as_array().map(|a| a.len()), Some(1), "detail: {detail}");
    assert_eq!(detail["handoffs"][0]["id"].as_str().unwrap(), id1);

    // Same key + same body → durable replay with the SAME handoff id.
    let r3 = http
        .post_full(&format!("/v1/tasks/{task_id}/handoffs"), body1, Some("k-handoff"), Some(rev))
        .unwrap();
    let v3: serde_json::Value = r3.into_json().unwrap();
    assert_eq!(v3["replayed"], true);
    assert_eq!(v3["result"]["handoff_id"].as_str().unwrap(), id1);
    env.cleanup();
}

#[test]
fn rc09_idempotency_survives_restart() {
    let env = TestEnv::new("rc09b");
    let http = start_http(&env);
    let (_repo, task_id) = common::http::setup_capture_ready(&http, &env);
    let rev = task_revision(&http, &task_id);
    let body = serde_json::json!({
        "source_mode": "context_only",
        "next_action": "continue",
        "open_items": [],
        "notes_constraints": [],
        "edited_fields": ["next_action"],
    });
    let r1 = http
        .post_full(&format!("/v1/tasks/{task_id}/handoffs"), body.clone(), Some("k-restart"), Some(rev))
        .unwrap();
    let id1: String = r1.into_json::<serde_json::Value>().unwrap()["result"]["handoff_id"]
        .as_str()
        .unwrap()
        .to_string();

    // "Restart": a NEW server state over the SAME database (new bootstrap,
    // new memory) — the DB-backed replay still returns the original op.
    let http2 = start_http(&env);
    let r2 = http2.post_full(
        &format!("/v1/tasks/{task_id}/handoffs"),
        body,
        Some("k-restart"),
        Some(rev),
    );
    match r2 {
        Ok(resp) => {
            let v: serde_json::Value = resp.into_json().unwrap();
            let got = v["result"]["handoff_id"].as_str().unwrap_or("");
            assert_eq!(got, id1, "restart must not mint a second handoff for the same key");
            // Only ONE handoff exists.
            let detail = task_detail(&http2, &task_id);
            assert_eq!(detail["handoffs"].as_array().unwrap().len(), 1);
        }
        Err(ureq::Error::Status(409, _)) => {
            // A conflict is also acceptable ONLY if no second handoff exists.
            let detail = task_detail(&http2, &task_id);
            assert_eq!(detail["handoffs"].as_array().unwrap().len(), 1);
        }
        other => panic!("unexpected: {other:?}"),
    }
    env.cleanup();
}

#[test]
fn rc09_capture_replay_returns_same_operation() {
    let env = TestEnv::new("rc09c");
    let http = start_http(&env);
    let (repo, task_id) = common::http::setup_capture_ready(&http, &env);
    let rev = task_revision(&http, &task_id);
    let body = serde_json::json!({"attest_stopped": true, "untracked_selected": [], "extra_excludes": []});
    let r1 = http
        .post_full(&format!("/v1/tasks/{task_id}/capture"), body.clone(), Some("k-cap"), Some(rev))
        .unwrap();
    let op1: String = r1.into_json::<serde_json::Value>().unwrap()["operation"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    http.wait_op(&op1);
    // Same key + body → same operation id (durable, even after completion).
    let r2 = http
        .post_full(&format!("/v1/tasks/{task_id}/capture"), body, Some("k-cap"), Some(rev))
        .unwrap();
    let v2: serde_json::Value = r2.into_json().unwrap();
    let op2 = v2["operation"]["id"].as_str().unwrap().to_string();
    assert_eq!(op1, op2);
    assert_eq!(v2["operation"]["state"], "completed");
    // And no second snapshot was captured: modifying the repo + different key
    // produces a different snapshot, but the replay itself changed nothing.
    let snaps_before = task_detail(&http, &task_id)["snapshots"].as_array().unwrap().len();
    repo.write("a.txt", b"changed\n");
    let rev2 = task_revision(&http, &task_id);
    let r3 = http
        .post_full(
            &format!("/v1/tasks/{task_id}/capture"),
            serde_json::json!({"attest_stopped": true, "untracked_selected": [], "extra_excludes": []}),
            Some("k-cap-2"),
            Some(rev2),
        )
        .unwrap();
    http.wait_op(r3.into_json::<serde_json::Value>().unwrap()["operation"]["id"].as_str().unwrap());
    let snaps_after = task_detail(&http, &task_id)["snapshots"].as_array().unwrap().len();
    assert_eq!(snaps_before + 1, snaps_after);
    env.cleanup();
}
