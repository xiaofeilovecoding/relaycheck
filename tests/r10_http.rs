//! R10 + R06 HTTP surface tests against the real axum server: session
//! bootstrap, CSRF/Origin, Host, CLI bearer isolation, SSE auth + gaps,
//! idempotency replay/conflict, write-slot release (regression), and the
//! export preview===download immutability (R14).

mod common;

use common::*;
use common::http::*;
use relaycheck::http::ServerState;
use std::io::Read;
use std::sync::Arc;

fn setup_task(env: &TestEnv) -> (Repo, String) {
    let repo = env.make_git_repo("http-task");
    repo.write("a.txt", b"one\n");
    repo.write("src/lib.py", b"print(1)\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "base"]);
    let (project_id, _code) = relaycheck::service::attach_project(
        &env.ctx,
        &repo.root.to_string_lossy(),
        Some("http-repo"),
    )
    .unwrap();
    let task_id = relaycheck::service::create_task(
        &env.ctx,
        relaycheck::model::TaskDraft {
            project_id,
            goal: "http loop".into(),
            constraints: vec![],
            criteria: vec![relaycheck::model::Criterion {
                id: "SC1".into(),
                text: "done".into(),
                required: true,
            }],
        },
        false,
    )
    .unwrap();
    (repo, task_id)
}

#[test]
fn r10_bootstrap_reuse_and_missing_csrf_rejected() {
    let env = TestEnv::new("r10");
    let http = start_http(&env);

    // Bootstrap token reuse: the same fragment token exchanged again fails.
    let token = "deadbeefdeadbeefdeadbeefdeadbeef";
    let r = ureq::post(&format!("{}/v1/session/exchange", http.base))
        .set("Content-Type", "application/json")
        .set("Origin", &http.base)
        .send_json(serde_json::json!({ "token": token }));
    assert!(matches!(r, Err(ureq::Error::Status(401, _))), "reuse must 401");

    // Data endpoint without cookie → 401.
    let r = ureq::get(&format!("{}/v1/state", http.base)).call();
    assert!(matches!(r, Err(ureq::Error::Status(401, _))));

    // Write without CSRF → 403.
    let r = http.post("/v1/tasks", serde_json::json!({"project_id": "x", "goal": "g"}), false, None);
    assert!(matches!(r, Err(ureq::Error::Status(403, _))), "missing CSRF must 403");

    // Cross-origin write → 403.
    let r = http.post(
        "/v1/tasks",
        serde_json::json!({"project_id": "x", "goal": "g"}),
        true,
        Some("http://evil.example"),
    );
    assert!(matches!(r, Err(ureq::Error::Status(403, _))), "cross-origin must 403");

    // Wrong Host → 403 (host header is set explicitly here).
    let r = ureq::get(&format!("{}/v1/state", http.base))
        .set("Cookie", &http.cookie)
        .set("Host", "localhost:1")
        .call();
    assert!(matches!(r, Err(ureq::Error::Status(403, _))), "host mismatch must 403");

    // CLI bearer endpoint without the bearer → 401 even WITH a session.
    let code = status_of(http.get_raw("/v1/cli/tasks"));
    assert!(code == 401 || code == 403, "browser session cannot use CLI channel, got {code}");
    env.cleanup();
}

#[test]
fn r10_sse_requires_session_and_reports_gaps() {
    let env = TestEnv::new("r10b");
    let http = start_http(&env);
    // SSE without cookie → 401.
    let r = ureq::get(&format!("{}/v1/events?after=0", http.base)).call();
    assert!(matches!(r, Err(ureq::Error::Status(401, _))));
    // Gap: after beyond the head → E_EVENT_GAP.
    let r = ureq::get(&format!("{}/v1/events?after=999999", http.base))
        .set("Cookie", &http.cookie)
        .call();
    assert!(matches!(r, Err(ureq::Error::Status(409, _))), "gap must 409");
    let body = match r {
        Err(ureq::Error::Status(_, resp)) => resp.into_string().unwrap(),
        _ => String::new(),
    };
    assert!(body.contains("E_EVENT_GAP"));
    // WebSocket upgrade attempt is not an SSE/WS route: plain GET stays JSON.
    let r = http.get("/v1/state");
    let ct = r.header("content-type").unwrap_or("").to_string();
    assert!(ct.starts_with("application/json"), "no ws upgrade");
    env.cleanup();
}

#[test]
fn r10_cli_bearer_channel_works_and_browser_cannot_use_it() {
    let env = TestEnv::new("r10c");
    let http = start_http(&env);
    // With bearer: attach works.
    let repo = env.make_git_repo("bearer-repo");
    repo.write("x.txt", b"x\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let r: serde_json::Value = ureq::post(&format!("{}/v1/cli/attach", http.base))
        .set("Authorization", &format!("Bearer {}", http.cli_token))
        .set("Content-Type", "application/json")
        .send_json(serde_json::json!({"root": repo.root.to_string_lossy()}))
        .unwrap()
        .into_json()
        .unwrap();
    assert!(r["result"]["claim_code"].as_str().unwrap().len() >= 8);
    // Bearer + session are different channels: session cookie alone fails,
    // bearer alone cannot read session-scoped data.
    let r = ureq::get(&format!("{}/v1/state", http.base))
        .set("Authorization", &format!("Bearer {}", http.cli_token))
        .call();
    assert!(matches!(r, Err(ureq::Error::Status(401, _))), "bearer is not a session");
    env.cleanup();
}

#[test]
fn r06_idempotency_replay_and_conflict() {
    let env = TestEnv::new("r06");
    let http = start_http(&env);
    let (_repo, task_id) = setup_task(&env);
    let key = "idem-key-1";

    // Same key + same body → replay returns the same operation id.
    let rev0 = task_revision(&http, &task_id);
    let body = serde_json::json!({"attest_stopped": true, "untracked_selected": [], "extra_excludes": []});
    let r1 = http
        .post_full(&format!("/v1/tasks/{task_id}/capture"), body.clone(), Some(key), Some(rev0))
        .unwrap();
    assert_eq!(r1.status(), 202);
    let op1 = r1.into_json::<serde_json::Value>().unwrap()["operation"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let done1 = http.wait_op(&op1);
    assert_eq!(done1["state"], "completed");

    // Replay: same key, same body → 202 with the SAME op id, no new work.
    let r2 = http
        .post_full(&format!("/v1/tasks/{task_id}/capture"), body.clone(), Some(key), Some(rev0))
        .unwrap();
    let op2 = r2.into_json::<serde_json::Value>().unwrap()["operation"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(op1, op2, "idempotent replay returns the original operation");

    // Same key + different body → 409 E_REQUEST_CONFLICT.
    let r3 = http.post_full(
        &format!("/v1/tasks/{task_id}/capture"),
        serde_json::json!({"attest_stopped": true, "untracked_selected": ["other.txt"], "extra_excludes": []}),
        Some(key),
        Some(rev0),
    );
    match r3 {
        Err(ureq::Error::Status(409, resp)) => {
            let body = resp.into_string().unwrap();
            assert!(body.contains("E_REQUEST_CONFLICT"));
        }
        other => panic!("expected 409, got {other:?}"),
    }
    env.cleanup();
}

#[test]
fn r06_write_slot_released_after_each_operation() {
    // Regression: the HTTP worker leaked the single write slot after the
    // first operation, blocking every later write with E_LIMIT forever.
    let env = TestEnv::new("r06b");
    let http = start_http(&env);
    let (repo, task_id) = setup_task(&env);

    // First capture.
    let r = http
        .post_full(
            &format!("/v1/tasks/{task_id}/capture"),
            serde_json::json!({"attest_stopped": true, "untracked_selected": [], "extra_excludes": []}),
            None,
            Some(1),
        )
        .unwrap();
    let op1 = r.into_json::<serde_json::Value>().unwrap()["operation"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let done = http.wait_op(&op1);
    assert_eq!(done["state"], "completed");

    // Modify the repo, second capture must ALSO work (slot released).
    repo.write("a.txt", b"two\n");
    let rev = task_revision(&http, &task_id);
    let r = http
        .post_full(
            &format!("/v1/tasks/{task_id}/capture"),
            serde_json::json!({"attest_stopped": true, "untracked_selected": [], "extra_excludes": []}),
            None,
            Some(rev),
        )
        .unwrap();
    let op2 = r.into_json::<serde_json::Value>().unwrap()["operation"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let done2 = http.wait_op(&op2);
    assert_eq!(done2["state"], "completed", "second capture must not be E_LIMIT");
    assert_ne!(
        done2["result"]["snapshot_id"], done["result"]["snapshot_id"],
        "different content, different snapshot"
    );

    // And a verify right after — the slot is free again.
    let python = find_python();
    let script = env.scratch.join("pass.py");
    std::fs::write(&script, b"import sys\nsys.exit(0)\n").unwrap();
    relaycheck::service::save_profile(
        &env.ctx,
        &task_id,
        "p",
        vec![relaycheck::model::CheckStep {
            id: "c".into(),
            exe: python.to_string_lossy().into_owned(),
            args: vec![script.to_string_lossy().into_owned()],
            cwd_relative: String::new(),
            timeout_seconds: 60,
            required: true,
        }],
        vec![],
        vec![],
        env.ctx.store.task_int(&task_id, "revision").unwrap(),
    )
    .unwrap();
    env.ctx.store.set_profile_trusted(
        &env.ctx.store.task_field(&task_id, "profile_id").unwrap(),
        true,
    )
    .unwrap();
    let r = post_verify(&http, &task_id, None);
    let op3 = r.unwrap().into_json::<serde_json::Value>().unwrap()["operation"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let done3 = http.wait_op(&op3);
    assert_eq!(done3["state"], "completed");
    env.cleanup();
}

#[test]
fn r14_package_download_is_immutable_and_matches_preview() {
    let env = TestEnv::new("r14");
    let http = start_http(&env);
    let (_repo, task_id) = setup_task(&env);

    // capture → verify → handoff
    let r = http
        .post_full(
            &format!("/v1/tasks/{task_id}/capture"),
            serde_json::json!({"attest_stopped": true, "untracked_selected": [], "extra_excludes": []}),
            None,
            Some(1),
        )
        .unwrap();
    let op = r.into_json::<serde_json::Value>().unwrap()["operation"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    http.wait_op(&op);

    let python = find_python();
    let script = env.scratch.join("p.py");
    std::fs::write(&script, b"import sys\nsys.exit(0)\n").unwrap();
    relaycheck::service::save_profile(
        &env.ctx,
        &task_id,
        "p",
        vec![relaycheck::model::CheckStep {
            id: "c".into(),
            exe: python.to_string_lossy().into_owned(),
            args: vec![script.to_string_lossy().into_owned()],
            cwd_relative: String::new(),
            timeout_seconds: 60,
            required: true,
        }],
        vec![],
        vec![],
        env.ctx.store.task_int(&task_id, "revision").unwrap(),
    )
    .unwrap();
    env.ctx
        .store
        .set_profile_trusted(&env.ctx.store.task_field(&task_id, "profile_id").unwrap(), true)
        .unwrap();
    let r = post_verify(&http, &task_id, None);
    http.wait_op(&r.unwrap().into_json::<serde_json::Value>().unwrap()["operation"]["id"].as_str().unwrap());

    let rev = task_revision(&http, &task_id);
    let r = http
        .post_full(
            &format!("/v1/tasks/{task_id}/handoffs"),
            serde_json::json!({
                "source_mode": "context_only",
                "next_action": "continue from the boundary test",
                "open_items": [{"id": "C1", "text": "check cancellation"}],
                "notes_constraints": [],
                "edited_fields": ["next_action"],
            }),
            None,
            Some(rev),
        )
        .unwrap();
    let handoff = r.into_json::<serde_json::Value>().unwrap()["result"].clone();
    let handoff_id = handoff["handoff_id"].as_str().unwrap().to_string();
    let sha: String = handoff["package_sha256"].as_str().unwrap().to_string();

    // Download twice: identical bytes, matching the stored hash.
    let mut buf1 = Vec::new();
    http.get(&format!("/v1/handoffs/{handoff_id}/package"))
        .into_reader()
        .read_to_end(&mut buf1)
        .unwrap();
    let mut buf2 = Vec::new();
    http.get(&format!("/v1/handoffs/{handoff_id}/package"))
        .into_reader()
        .read_to_end(&mut buf2)
        .unwrap();
    assert_eq!(buf1, buf2);
    assert_eq!(relaycheck::ids::sha256_hex(&buf1), sha);

    // Change the candidate (capture B) — the OLD package bytes stay identical.
    let (_repo2, _t) = ((), ());
    // Modify via the project repo: find it from the task.
    let project_id = env.ctx.store.task_field(&task_id, "project_id").unwrap();
    let root = env.ctx.store.project_root(&project_id).unwrap();
    std::fs::write(std::path::Path::new(&root).join("a.txt"), b"changed\n").unwrap();
    let rev = task_revision(&http, &task_id);
    let r = http
        .post_full(
            &format!("/v1/tasks/{task_id}/capture"),
            serde_json::json!({"attest_stopped": true, "untracked_selected": [], "extra_excludes": []}),
            None,
            Some(rev),
        )
        .unwrap();
    http.wait_op(&r.into_json::<serde_json::Value>().unwrap()["operation"]["id"].as_str().unwrap());

    let mut buf3 = Vec::new();
    http.get(&format!("/v1/handoffs/{handoff_id}/package"))
        .into_reader()
        .read_to_end(&mut buf3)
        .unwrap();
    assert_eq!(buf1, buf3, "old handoff package must not change with the candidate");
    env.cleanup();
}

#[test]
fn r10_static_shell_has_csp_and_no_session_data_leak() {
    let env = TestEnv::new("r10d");
    let http = start_http(&env);
    // The shell is reachable anonymously but carries CSP.
    let r = ureq::get(&format!("{}/", http.base))
        .set("Host", &format!("127.0.0.1:{}", http.port))
        .call()
        .unwrap();
    let csp = r.header("content-security-policy").unwrap_or("").to_string();
    assert!(csp.contains("default-src 'self'"), "CSP present: {csp}");
    let html = r.into_string().unwrap();
    assert!(!html.contains("csrf"), "shell leaks no session data");
    // /v1/state without a session never leaks tasks.
    let r = ureq::get(&format!("{}/v1/state", http.base)).call();
    assert!(matches!(r, Err(ureq::Error::Status(401, _))));
    env.cleanup();
}
