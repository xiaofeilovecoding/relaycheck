//! RC10 regression: the handoff's current verified projection and receipt
//! claims only include rounds valid for the COMPLETE current identity.
//! History is listed explicitly as not-valid, never silently dropped.

mod common;

use common::*;
use relaycheck::service;

#[test]
fn rc10_export_after_profile_change_lists_old_round_as_historical() {
    let env = TestEnv::new("rc10");
    let repo = env.make_git_repo("projection");
    repo.write("a.txt", b"x\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "verified projection");
    let _ = capture_all(&env, &task_id);

    // Old profile passes (round 1).
    let _ = setup_python_profile(&env, &task_id, "import sys\nsys.exit(0)\n", "old-check");
    verify_now(&env, &task_id);

    // Save a NEW (unverified) required profile — revision bumps, acceptance
    // becomes impossible for the old round.
    let python = find_python();
    let script2 = env.scratch.join("new-check.py");
    std::fs::write(&script2, b"import sys\nsys.exit(0)\n").unwrap();
    let rev = env.ctx.store.task_int(&task_id, "revision").unwrap();
    service::save_profile(
        &env.ctx,
        &task_id,
        "new-profile",
        vec![relaycheck::model::CheckStep {
            id: "new-check".into(),
            exe: python.to_string_lossy().into_owned(),
            args: vec![script2.to_string_lossy().into_owned()],
            cwd_relative: String::new(),
            timeout_seconds: 60,
            required: true,
        }],
        vec![],
        vec![],
        rev,
    )
    .unwrap();
    let rev_after = env.ctx.store.task_int(&task_id, "revision").unwrap();
    assert_eq!(rev_after, rev + 1);
    let acc = service::acceptance_check(&env.ctx, &task_id).unwrap();
    assert!(!acc.ok, "old round must not satisfy the new profile");

    // Export at the CURRENT revision: the old round is history, not current.
    let (_hid, _sha, package) = export_handoff(&env, &task_id, "context_only");
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&package)).unwrap();
    let md = std::io::read_to_string(zip.by_name("handoff.md").unwrap()).unwrap();
    // Current verified section must NOT claim the old check.
    let verified_section = md
        .split("## Verified\n")
        .nth(1)
        .unwrap()
        .split("\n### Historical")
        .next()
        .unwrap()
        .to_string();
    assert!(
        !verified_section.contains("old-check"),
        "current verified claims the stale round: {verified_section}"
    );
    assert!(verified_section.contains("no local verification valid for the current candidate"));
    // History is kept and explicitly marked.
    assert!(md.contains("### Historical (NOT valid for the current candidate)"));
    assert!(md.contains("old-check"));
    // Receipt claims: no opaque passed for the stale round.
    let receipt_str = std::io::read_to_string(zip.by_name("receipt.json").unwrap()).unwrap();
    let receipt: serde_json::Value = serde_json::from_str(&receipt_str).unwrap();
    let passed_claims: Vec<&serde_json::Value> = receipt["claims"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["status"] == "passed" && c["check_id"] == "old-check")
        .collect();
    assert!(passed_claims.is_empty(), "stale claim in receipt: {receipt_str}");
    env.cleanup();
}

#[test]
fn rc10_export_after_env_epoch_change_marks_old_round_historical() {
    let env = TestEnv::new("rc10b");
    let repo = env.make_git_repo("env-proj");
    repo.write("a.txt", b"x\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "env projection");
    let _ = capture_all(&env, &task_id);
    let _ = setup_python_profile(&env, &task_id, "import sys\nsys.exit(0)\n", "c1");
    verify_now(&env, &task_id);

    env.ctx.store.bump_environment_epoch();
    let (_hid, _sha, package) = export_handoff(&env, &task_id, "context_only");
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&package)).unwrap();
    let md = std::io::read_to_string(zip.by_name("handoff.md").unwrap()).unwrap();
    let verified_section = md
        .split("## Verified\n")
        .nth(1)
        .unwrap()
        .split("\n### Historical")
        .next()
        .unwrap()
        .to_string();
    assert!(
        !verified_section.contains("c1"),
        "stale-epoch round in current verified: {verified_section}"
    );
    assert!(md.contains("Historical (NOT valid"));
    let receipt_str = std::io::read_to_string(zip.by_name("receipt.json").unwrap()).unwrap();
    assert!(!receipt_str.contains("\"check_id\":\"c1\"") && !receipt_str.contains("\"check_id\": \"c1\""));
    env.cleanup();
}

#[test]
fn rc10_current_pass_still_projects_and_aba() {
    let env = TestEnv::new("rc10c");
    let repo = env.make_git_repo("aba-proj");
    repo.write("a.txt", b"x\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "aba projection");
    let a = capture_all(&env, &task_id).expect("capture");
    let _ = setup_python_profile(&env, &task_id, "import sys\nsys.exit(0)\n", "c1");
    verify_now(&env, &task_id);

    // Switch to B and back to A: the old round is historical; a fresh round
    // on A (current epoch) projects again.
    repo.write("a.txt", b"B\n");
    capture_all(&env, &task_id).expect("capture B");
    repo.write("a.txt", b"x\n");
    capture_all(&env, &task_id).expect("capture A again");
    verify_now(&env, &task_id);
    let (_hid, _sha, package) = export_handoff(&env, &task_id, "context_only");
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&package)).unwrap();
    let md = std::io::read_to_string(zip.by_name("handoff.md").unwrap()).unwrap();
    let verified_section = md
        .split("## Verified\n")
        .nth(1)
        .unwrap()
        .split("\n### Historical")
        .next()
        .unwrap()
        .to_string();
    assert!(verified_section.contains("c1"), "fresh round must project: {verified_section}");
    let _ = a;
    env.cleanup();
}
