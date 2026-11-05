//! RC03 regression: imported packages are secret-scanned entry-by-entry
//! before anything persists. A canary in the receipt, handoff text or
//! packaged source blocks the whole import; the task API never returns the
//! canary text.

mod common;

use common::*;
use relaycheck::service;

fn zip_bytes(entries: Vec<(&str, Vec<u8>)>) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut w = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in entries {
            w.start_file(name, opts).unwrap();
            std::io::Write::write_all(&mut w, &data).unwrap();
        }
        w.finish().unwrap();
    }
    buf.into_inner()
}

fn receipt_json(goal: &str) -> String {
    serde_json::json!({
        "schema_version": 1,
        "kind": "handoff",
        "handoff_id": "11111111-2222-4333-8444-555566667777",
        "task_id": "aaaaaaaa-bbbb-4ccc-8ddd-eeeeffff0000",
        "task_revision": 1,
        "snapshot_id": format!("sha256:{}", "a".repeat(64)),
        "base_head": "",
        "source_mode": "context_only",
        "coverage": "selected_sources",
        "goal": goal,
        "constraints": [],
        "open_items": [],
        "next_action": "n",
        "claims": []
    })
    .to_string()
}

fn manifest_json() -> String {
    serde_json::json!({
        "schema_version": 1,
        "kind": "relaycheck_snapshot",
        "scope": "selected_sources",
        "files": [],
        "excluded": []
    })
    .to_string()
}

const CANARY: &str = "RC_CANARY_LIVE_TOKEN_0123456789abcdef";

#[test]
fn rc03_canary_in_receipt_blocks_import_and_never_reaches_task() {
    let env = TestEnv::new("rc03");
    let repo = env.make_git_repo("canary");
    repo.write("a.txt", b"x\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "import canary");
    let rev = env.ctx.store.task_int(&task_id, "revision").unwrap();

    // The canary rides in the receipt's goal (a visible, persisted field).
    let pkg = zip_bytes(vec![
        ("receipt.json", receipt_json(&format!("goal {CANARY}")).into_bytes()),
        ("manifest.json", manifest_json().into_bytes()),
        ("handoff.md", format!("goal {CANARY}").into_bytes()),
    ]);
    let err = service::import_package(&env.ctx, &pkg, Some(&task_id)).unwrap_err();
    assert_eq!(err.code, relaycheck::errors::Code::ESecretDetected);
    // Error details name the entries, never the secret itself.
    let rendered = format!("{:?}", err.details);
    assert!(!rendered.contains(CANARY), "error echoes the secret: {rendered}");

    // Nothing persisted: no imported report for the task, no visible canary.
    assert!(env.ctx.store.imported_reports_for_task(&task_id).unwrap().is_empty());
    let detail = relaycheck::http::build_task_detail(&server_state(&env), &task_id).unwrap();
    let rendered = detail.to_string();
    assert!(!rendered.contains(CANARY), "task API returned the canary");
    let _ = rev;
    // Quarantine cleaned up.
    let tmp = env.ctx.tmp_root.clone();
    let leftovers: Vec<_> = std::fs::read_dir(&tmp)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("import-"))
        .collect();
    assert!(leftovers.is_empty(), "quarantine not cleaned: {:?}", leftovers.iter().map(|e| e.file_name()).collect::<Vec<_>>());
    env.cleanup();
}

#[test]
fn rc03_canary_in_portable_source_blocks_import() {
    let env = TestEnv::new("rc03b");
    let repo = env.make_git_repo("canary-src");
    repo.write("a.txt", b"x\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "import canary src");

    let sha = crate_sha(b"key = AKIAIOSFODNN7EXAMPLE\n");
    let pkg = zip_bytes(vec![
        ("receipt.json", receipt_json("clean goal").into_bytes()),
        ("manifest.json", serde_json::json!({
            "schema_version": 1, "kind": "relaycheck_snapshot", "scope": "selected_sources",
            "files": [{"path": "a.txt", "mode": "100644", "size": sha.1, "sha256": sha.0}],
            "excluded": []
        }).to_string().into_bytes()),
        (format!("blobs/{}", sha.0).as_str(), b"key = AKIAIOSFODNN7EXAMPLE\n".to_vec()),
    ]);
    let err = service::import_package(&env.ctx, &pkg, Some(&task_id)).unwrap_err();
    assert_eq!(err.code, relaycheck::errors::Code::ESecretDetected);
    // The blob never entered the store from this import path.
    assert!(env.ctx.store.imported_reports_for_task(&task_id).unwrap().is_empty());
    env.cleanup();
}

#[test]
fn rc03_clean_package_still_imports() {
    let env = TestEnv::new("rc03c");
    let pkg = zip_bytes(vec![
        ("receipt.json", receipt_json("honest goal").into_bytes()),
        ("manifest.json", manifest_json().into_bytes()),
        ("handoff.md", b"# Handoff\n\n## Goal\nhonest goal\n".to_vec()),
    ]);
    let out = service::import_package(&env.ctx, &pkg, None).expect("clean import");
    assert_eq!(out.association, "pending_manual");
    env.cleanup();
}

fn crate_sha(data: &[u8]) -> (String, i64) {
    (relaycheck::ids::sha256_hex(data), data.len() as i64)
}
