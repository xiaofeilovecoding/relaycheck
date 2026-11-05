//! RC01 regression: run approval binds to the profile the user actually
//! reviewed (id + content hash) and to the task revision they saw. A stale
//! or mismatched approval must never execute anything and never mark a
//! profile trusted.

mod common;

use common::*;
use relaycheck::service;

fn profile_body(name: &str, script: &str, env: &TestEnv, check_id: &str) -> (Vec<relaycheck::model::CheckStep>, String) {
    let python = find_python();
    let script_path = env.scratch.join(format!("script-{name}.py"));
    std::fs::write(&script_path, script).unwrap();
    (
        vec![relaycheck::model::CheckStep {
            id: check_id.to_string(),
            exe: python.to_string_lossy().into_owned(),
            args: vec![script_path.to_string_lossy().into_owned()],
            cwd_relative: String::new(),
            timeout_seconds: 60,
            required: true,
        }],
        script_path.to_string_lossy().into_owned(),
    )
}

#[test]
fn rc01_stale_approval_cannot_execute_unreviewed_profile() {
    let env = TestEnv::new("rc01");
    let repo = env.make_git_repo("approval");
    repo.write("a.txt", b"x\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "approval binding");

    // Tab 1: save profile A (sees revision R1, save bumps to R2).
    let (checks_a, _path_a) = profile_body("a", "import sys\nsys.exit(0)\n", &env, "chk-a");
    let rev1 = env.ctx.store.task_int(&task_id, "revision").unwrap();
    let profile_a = service::save_profile(&env.ctx, &task_id, "profile-A", checks_a, vec![], vec![], rev1).unwrap();
    let rev2 = env.ctx.store.task_int(&task_id, "revision").unwrap();
    assert_eq!(rev2, rev1 + 1);

    // Tab 2: save profile B on the CURRENT revision (bumps to R3) — the task
    // now points at B.
    let (checks_b, _path_b) = profile_body("b", "print('UNREVIEWED_B')\nimport sys\nsys.exit(0)\n", &env, "chk-b");
    let profile_b = service::save_profile(&env.ctx, &task_id, "profile-B", checks_b, vec![], vec![], rev2).unwrap();
    let rev3 = env.ctx.store.task_int(&task_id, "revision").unwrap();
    assert_eq!(env.ctx.store.task_field_opt(&task_id, "profile_id").unwrap().unwrap(), profile_b.id);

    // Stale replay: the old tab sends its approval for revision R1 (or R2)
    // with profile A — nothing may run and NOTHING may become trusted.
    for stale_rev in [rev1, rev2] {
        let op_id = "stale-op";
        let _ = env.ctx.store.insert_operation(op_id, "verify", Some(&task_id), None, "");
        let cancel = env.ctx.ops.begin(op_id, "verify").unwrap();
        let result = service::run_verify(
            &env.ctx,
            &service::VerifyRequest {
                task_id: task_id.clone(),
                trust_confirmed: true,
                profile_id: profile_a.id.clone(),
                profile_hash: profile_a.hash.clone(),
                if_match: stale_rev,
            },
            &cancel,
            op_id,
            |_| {},
        );
        env.ctx.ops.finish(op_id);
        let err = result.unwrap_err();
        assert_eq!(err.code, relaycheck::errors::Code::ERevisionConflict, "stale rev {stale_rev}");
    }
    assert!(!env.ctx.store.profile_trusted(&profile_a.id).unwrap(), "A must stay untrusted");
    assert!(!env.ctx.store.profile_trusted(&profile_b.id).unwrap(), "B must stay untrusted after stale approvals");
    assert!(env.ctx.store.verifications_for_task(&task_id).unwrap().is_empty(), "no round ran");

    // Mismatched identity: current revision but profile A while the task
    // points at B → E_PROFILE_CHANGED, still nothing trusted.
    let op_id = "mismatch-op";
    let _ = env.ctx.store.insert_operation(op_id, "verify", Some(&task_id), None, "");
    let cancel = env.ctx.ops.begin(op_id, "verify").unwrap();
    let result = service::run_verify(
        &env.ctx,
        &service::VerifyRequest {
            task_id: task_id.clone(),
            trust_confirmed: true,
            profile_id: profile_a.id.clone(),
            profile_hash: profile_a.hash.clone(),
            if_match: rev3,
        },
        &cancel,
        op_id,
        |_| {},
    );
    env.ctx.ops.finish(op_id);
    assert_eq!(result.unwrap_err().code, relaycheck::errors::Code::EProfileChanged);
    assert!(!env.ctx.store.profile_trusted(&profile_a.id).unwrap());

    // Wrong hash for the CURRENT profile id → E_PROFILE_CHANGED.
    let op_id = "hash-op";
    let _ = env.ctx.store.insert_operation(op_id, "verify", Some(&task_id), None, "");
    let cancel = env.ctx.ops.begin(op_id, "verify").unwrap();
    let result = service::run_verify(
        &env.ctx,
        &service::VerifyRequest {
            task_id: task_id.clone(),
            trust_confirmed: true,
            profile_id: profile_b.id.clone(),
            profile_hash: "0".repeat(64),
            if_match: rev3,
        },
        &cancel,
        op_id,
        |_| {},
    );
    env.ctx.ops.finish(op_id);
    assert_eq!(result.unwrap_err().code, relaycheck::errors::Code::EProfileChanged);
    assert!(!env.ctx.store.profile_trusted(&profile_b.id).unwrap());

    // Capture a candidate, then the honest path: approval for B at the
    // CURRENT revision runs and binds.
    let _ = capture_all(&env, &task_id).expect("capture");
    let op_id = "good-op";
    let _ = env.ctx.store.insert_operation(op_id, "verify", Some(&task_id), None, "");
    let cancel = env.ctx.ops.begin(op_id, "verify").unwrap();
    let result = service::run_verify(
        &env.ctx,
        &service::VerifyRequest {
            task_id: task_id.clone(),
            trust_confirmed: true,
            profile_id: profile_b.id.clone(),
            profile_hash: profile_b.hash.clone(),
            if_match: rev3,
        },
        &cancel,
        op_id,
        |_| {},
    );
    env.ctx.ops.finish(op_id);
    let v = result.expect("identity-bound approval runs");
    assert_eq!(v.round_status, "passed");
    assert!(env.ctx.store.profile_trusted(&profile_b.id).unwrap());
    env.cleanup();
}

#[test]
fn rc01_save_profile_cas_two_tabs() {
    let env = TestEnv::new("rc01b");
    let repo = env.make_git_repo("save-cas");
    repo.write("a.txt", b"x\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "save cas");
    let (checks_a, _) = profile_body("a", "import sys\nsys.exit(0)\n", &env, "chk-a");
    let (checks_b, _) = profile_body("b", "import sys\nsys.exit(1)\n", &env, "chk-b");

    let rev = env.ctx.store.task_int(&task_id, "revision").unwrap();
    // Tab 1 saves on rev → ok, bumps.
    service::save_profile(&env.ctx, &task_id, "A", checks_a, vec![], vec![], rev).unwrap();
    // Tab 2 replays its save on the SAME stale rev → conflict, no silent overwrite.
    let err = service::save_profile(&env.ctx, &task_id, "B", checks_b, vec![], vec![], rev).unwrap_err();
    assert_eq!(err.code, relaycheck::errors::Code::ERevisionConflict);
    // The task still points at A.
    let pid = env.ctx.store.task_field_opt(&task_id, "profile_id").unwrap().unwrap();
    assert_eq!(env.ctx.store.profile_json(&pid).unwrap().name, "A");
    env.cleanup();
}
