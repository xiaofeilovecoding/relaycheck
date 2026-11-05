//! R03: version binding — A's passing result can never accept B; A→B→A,
//! goal/profile/environment changes and stale If-Match cannot revive
//! invalidated results.

mod common;

use common::*;
use relaycheck::service;

const PASS: &str = "import sys\nsys.exit(0)\n";

#[test]
fn r03_a_check_cannot_accept_b() {
    let env = TestEnv::new("r03");
    let repo = env.make_git_repo("versions");
    repo.write("f.txt", b"A\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "a"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "version binding");
    let _a = capture_all(&env, &task_id);
    let _ = setup_python_profile(&env, &task_id, PASS, "c1");
    verify_now(&env, &task_id);

    repo.write("f.txt", b"B\n");
    let _b = capture_all(&env, &task_id);
    let rev = env.ctx.store.task_int(&task_id, "revision").unwrap();
    let err = service::accept_candidate(&env.ctx, &task_id, &["SC1".to_string()], rev).unwrap_err();
    assert_eq!(err.code, relaycheck::errors::Code::EEvidenceStale);
    assert!(err.details.iter().any(|d| d.contains("no current local verification")));
    env.cleanup();
}

#[test]
fn r03_a_b_a_switch_cannot_revive_old_result() {
    let env = TestEnv::new("r03b");
    let repo = env.make_git_repo("aba");
    repo.write("f.txt", b"A\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "a"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "aba");
    let a = capture_all(&env, &task_id).expect("capture");
    let _ = setup_python_profile(&env, &task_id, PASS, "c1");
    verify_now(&env, &task_id); // passed on A

    // Switch to B.
    repo.write("f.txt", b"B\n");
    capture_all(&env, &task_id);
    // Switch back to A (same snapshot content — capture dedupes to the same id).
    repo.write("f.txt", b"A\n");
    let a2 = capture_all(&env, &task_id).expect("capture");
    assert_eq!(a.snapshot_id, a2.snapshot_id, "content-identical capture reuses the snapshot id");
    // But the candidate epoch moved: A's old round must not gate acceptance.
    let rev = env.ctx.store.task_int(&task_id, "revision").unwrap();
    let err = service::accept_candidate(&env.ctx, &task_id, &["SC1".to_string()], rev).unwrap_err();
    assert_eq!(err.code, relaycheck::errors::Code::EEvidenceStale);
    // Re-running on A (fresh round at the current epoch) unblocks.
    verify_now(&env, &task_id);
    service::accept_candidate(&env.ctx, &task_id, &["SC1".to_string()], rev).expect("accept after fresh round");
    env.cleanup();
}

#[test]
fn r03_goal_edit_bumps_revision_and_invalidates() {
    let env = TestEnv::new("r03c");
    let repo = env.make_git_repo("goal-edit");
    repo.write("f.txt", b"A\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "a"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "goal edit");
    let _ = capture_all(&env, &task_id);
    let _ = setup_python_profile(&env, &task_id, PASS, "c1");
    verify_now(&env, &task_id);

    // Edit the goal → revision bumps → the old verification cannot accept.
    let old_rev = env.ctx.store.task_int(&task_id, "revision").unwrap();
    let new_rev = service::update_task(
        &env.ctx, &task_id, old_rev,
        Some("goal edited: now cover cancellation too".into()), None, None,
    )
    .unwrap();
    assert_eq!(new_rev, old_rev + 1);
    let err = service::accept_candidate(&env.ctx, &task_id, &["SC1".to_string()], new_rev).unwrap_err();
    assert_eq!(err.code, relaycheck::errors::Code::EEvidenceStale);

    // A stale If-Match (old revision) is a 409 E_REVISION_CONFLICT.
    let err2 = service::update_task(
        &env.ctx, &task_id, old_rev,
        Some("stale if-match edit".into()), None, None,
    )
    .unwrap_err();
    assert_eq!(err2.code, relaycheck::errors::Code::ERevisionConflict);

    // Criteria edit also bumps.
    let rev3 = env.ctx.store.task_int(&task_id, "revision").unwrap();
    let rev4 = service::update_task(
        &env.ctx, &task_id, rev3, None, None,
        Some(vec![relaycheck::model::Criterion {
            id: "SC2".into(),
            text: "extra criterion".into(),
            required: true,
        }]),
    )
    .unwrap();
    assert_eq!(rev4, rev3 + 1);
    env.cleanup();
}

#[test]
fn r03_profile_change_invalidates_old_rounds() {
    let env = TestEnv::new("r03d");
    let repo = env.make_git_repo("profile-change");
    repo.write("f.txt", b"A\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "a"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "profile change");
    let _ = capture_all(&env, &task_id);
    let _p1 = setup_python_profile(&env, &task_id, PASS, "c1");
    verify_now(&env, &task_id);
    let rev_before = env.ctx.store.task_int(&task_id, "revision").unwrap();

    // Replace the profile with a different one → revision bump; the round
    // bound to the old profile hash cannot accept.
    let _p2 = setup_python_profile(&env, &task_id, "print('different')\nimport sys\nsys.exit(0)\n", "c1-new");
    let rev_after = env.ctx.store.task_int(&task_id, "revision").unwrap();
    assert_eq!(rev_after, rev_before + 1);
    let err = service::accept_candidate(&env.ctx, &task_id, &["SC1".to_string()], rev_after).unwrap_err();
    assert_eq!(err.code, relaycheck::errors::Code::EEvidenceStale);

    // A fresh round under the new profile unblocks.
    verify_now(&env, &task_id);
    service::accept_candidate(&env.ctx, &task_id, &["SC1".to_string()], rev_after).unwrap();
    env.cleanup();
}

#[test]
fn r03_environment_epoch_bump_invalidates_acceptance() {
    let env = TestEnv::new("r03e");
    let repo = env.make_git_repo("env-epoch");
    repo.write("f.txt", b"A\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "a"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "env epoch");
    let _ = capture_all(&env, &task_id);
    let _ = setup_python_profile(&env, &task_id, PASS, "c1");
    verify_now(&env, &task_id);

    // The operator explicitly marks the check environment changed.
    let old_epoch = env.ctx.store.environment_epoch();
    let new_epoch = env.ctx.store.bump_environment_epoch();
    assert_eq!(new_epoch, old_epoch + 1);
    let rev = env.ctx.store.task_int(&task_id, "revision").unwrap();
    let err = service::accept_candidate(&env.ctx, &task_id, &["SC1".to_string()], rev).unwrap_err();
    assert_eq!(err.code, relaycheck::errors::Code::EEvidenceStale);
    verify_now(&env, &task_id);
    service::accept_candidate(&env.ctx, &task_id, &["SC1".to_string()], rev).unwrap();
    env.cleanup();
}

#[test]
fn r03_acceptance_is_cas_single_shot() {
    let env = TestEnv::new("r03f");
    let repo = env.make_git_repo("cas-accept");
    repo.write("f.txt", b"A\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "a"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "cas");
    let _ = capture_all(&env, &task_id);
    let _ = setup_python_profile(&env, &task_id, PASS, "c1");
    verify_now(&env, &task_id);
    let rev = env.ctx.store.task_int(&task_id, "revision").unwrap();
    service::accept_candidate(&env.ctx, &task_id, &["SC1".to_string()], rev).unwrap();
    // Second accept for the same key → E_STATE (already accepted).
    let err = service::accept_candidate(&env.ctx, &task_id, &["SC1".to_string()], rev).unwrap_err();
    assert_eq!(err.code, relaycheck::errors::Code::EState);
    // Acceptance requires the required criterion acknowledgement.
    repo.write("f.txt", b"B\n");
    capture_all(&env, &task_id);
    verify_now(&env, &task_id);
    let rev = env.ctx.store.task_int(&task_id, "revision").unwrap();
    let err = service::accept_candidate(&env.ctx, &task_id, &[], rev).unwrap_err();
    assert_eq!(err.code, relaycheck::errors::Code::EValidation);
    assert!(err.message.contains("SC1"));
    env.cleanup();
}

#[test]
fn r03_untracked_must_be_selected_explicitly() {
    let env = TestEnv::new("r03g");
    let repo = env.make_git_repo("untracked");
    repo.write("tracked.txt", b"t\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    repo.write("secret_draft.txt", b"untracked\n");
    let (_p, task_id) = setup_project_task(&env, &repo, "untracked");
    let cap = capture_all(&env, &task_id).expect("capture");
    assert_eq!(cap.file_count, 1, "untracked file NOT auto-included");
    let files = env.ctx.store.snapshot_files(&cap.snapshot_id).unwrap();
    assert!(files.iter().all(|f| f.path != "secret_draft.txt"));
    env.cleanup();
}
