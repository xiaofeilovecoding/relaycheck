//! R05: check semantics — PASS text is not a verdict; missing deps are
//! blocked; input writes invalidate; approved outputs are allowed; log
//! truncation never changes the verdict.

mod common;

use common::*;
use relaycheck::service;

const PYTHON: &str = r#"
import sys
sys.stdout.write("PASS everything is fine\n" * 3)
sys.exit(1)
"#;

#[test]
fn r05_pass_text_with_failed_exit_is_failed() {
    let env = TestEnv::new("r05");
    let repo = env.make_git_repo("pass-text");
    repo.write("a.txt", b"x\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "pass text");
    let _ = capture_all(&env, &task_id);
    let _ = setup_python_profile(&env, &task_id, PYTHON, "liar");
    let v = verify_now(&env, &task_id);
    assert_eq!(v.round_status, "failed");
    let r = &v.results[0];
    assert_eq!(r.status, "failed");
    assert_eq!(r.exit_code, Some(1));
    // The printed PASS text stays visible as DATA (redacted summary), but
    // the verdict is failure.
    assert!(r.summary.contains("PASS"), "output kept as data");
    assert_eq!(env.ctx.store.task_field(&task_id, "workflow_state").unwrap(), "rework");
    env.cleanup();
}

#[test]
fn r05_missing_dependency_is_blocked_not_failed() {
    let env = TestEnv::new("r05b");
    let repo = env.make_git_repo("missing-dep");
    repo.write("a.txt", b"x\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "missing dep");
    let _ = capture_all(&env, &task_id);
    // A plausible-but-absent exe path.
    let ghost = env.scratch.join("tools").join("ghost-runner.exe");
    let profile = service::save_profile(
        &env.ctx,
        &task_id,
        "ghost",
        vec![relaycheck::model::CheckStep {
            id: "ghost-check".into(),
            exe: ghost.to_string_lossy().into_owned(),
            args: vec![],
            cwd_relative: String::new(),
            timeout_seconds: 30,
            required: true,
        }],
        vec![],
        vec![],
        env.ctx.store.task_int(&task_id, "revision").unwrap(),
    )
    .unwrap();
    env.ctx.store.set_profile_trusted(&profile.id, true).unwrap();
    let v = verify_now(&env, &task_id);
    assert_eq!(v.round_status, "blocked");
    assert_eq!(v.results[0].status, "blocked");
    assert!(v.results[0].detail.contains("missing dependency"));
    assert_eq!(env.ctx.store.task_field(&task_id, "workflow_state").unwrap(), "rework");
    env.cleanup();
}

#[test]
fn r05_writing_inputs_invalidates_despite_exit_zero() {
    let env = TestEnv::new("r05c");
    let repo = env.make_git_repo("writes-inputs");
    repo.write("src/code.py", b"value = 1\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "writes inputs");
    let _ = capture_all(&env, &task_id);
    // The check "passes" (exit 0) but rewrites an input file in its workdir.
    let script = r#"
with open("src/code.py", "w") as f:
    f.write("value = 2\n")
import sys
sys.exit(0)
"#;
    let _ = setup_python_profile(&env, &task_id, script, "mutator");
    let v = verify_now(&env, &task_id);
    assert_eq!(v.round_status, "invalidated");
    assert!(v.results.iter().any(|r| r.status == "invalidated" && r.detail.contains("input files modified")));
    // The ORIGINAL repo file is untouched (checks ran on the copy).
    assert_eq!(repo.read("src/code.py"), b"value = 1\n");
    assert_eq!(env.ctx.store.task_field(&task_id, "workflow_state").unwrap(), "rework");
    env.cleanup();
}

#[test]
fn r05_approved_outputs_allowed_unapproved_writes_invalidated() {
    let env = TestEnv::new("r05d");
    let repo = env.make_git_repo("outputs");
    repo.write("src/code.py", b"value = 1\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "outputs");
    let _ = capture_all(&env, &task_id);
    let python = find_python();

    // (a) write only inside approved outputs → passed.
    let ok_script = r#"
import os
os.makedirs("build", exist_ok=True)
with open("build/report.txt", "w") as f:
    f.write("all good\n")
import sys
sys.exit(0)
"#;
    let script_path = env.scratch.join("ok_outputs.py");
    std::fs::write(&script_path, ok_script).unwrap();
    let profile = service::save_profile(
        &env.ctx,
        &task_id,
        "with-outputs",
        vec![relaycheck::model::CheckStep {
            id: "reporter".into(),
            exe: python.to_string_lossy().into_owned(),
            args: vec![script_path.to_string_lossy().into_owned()],
            cwd_relative: String::new(),
            timeout_seconds: 60,
            required: true,
        }],
        vec![],
        vec!["build".into()],
        env.ctx.store.task_int(&task_id, "revision").unwrap(),
    )
    .unwrap();
    env.ctx.store.set_profile_trusted(&profile.id, true).unwrap();
    let v = verify_now(&env, &task_id);
    assert_eq!(v.round_status, "passed", "results: {:?}", v.results);

    // (b) write outside outputs (and not an input) → invalidated.
    let bad_script = r#"
with open("stray_output.txt", "w") as f:
    f.write("unauthorized\n")
import sys
sys.exit(0)
"#;
    let bad_path = env.scratch.join("bad_writes.py");
    std::fs::write(&bad_path, bad_script).unwrap();
    let profile2 = service::save_profile(
        &env.ctx,
        &task_id,
        "no-outputs",
        vec![relaycheck::model::CheckStep {
            id: "stray-writer".into(),
            exe: python.to_string_lossy().into_owned(),
            args: vec![bad_path.to_string_lossy().into_owned()],
            cwd_relative: String::new(),
            timeout_seconds: 60,
            required: true,
        }],
        vec![],
        vec![],
        env.ctx.store.task_int(&task_id, "revision").unwrap(),
    )
    .unwrap();
    env.ctx.store.set_profile_trusted(&profile2.id, true).unwrap();
    let v2 = verify_now(&env, &task_id);
    assert_eq!(v2.round_status, "invalidated");
    assert!(v2.results.iter().any(|r| r.detail.contains("writes outside approved outputs")));
    env.cleanup();
}

#[test]
fn r05_output_truncation_marks_flag_and_keeps_verdict() {
    let env = TestEnv::new("r05e");
    let repo = env.make_git_repo("truncation");
    repo.write("a.txt", b"x\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "truncation");
    let _ = capture_all(&env, &task_id);
    // ~2 MiB of output then exit 0 → truncated flag, verdict passed.
    let script = format!(
        "import sys\nsys.stdout.write('x' * {})\nsys.stdout.flush()\nsys.exit(0)\n",
        2 * 1024 * 1024
    );
    let _ = setup_python_profile(&env, &task_id, &script, "chatty");
    let v = verify_now(&env, &task_id);
    assert_eq!(v.round_status, "passed", "truncation must not change the verdict");
    let r = &v.results[0];
    assert!(r.truncated, "truncation flag set");
    assert!(r.summary.len() <= 256 * 1024 + 4096, "summary bounded");
    // Same tool failing → truncation also does not flip failure.
    let script2 = format!(
        "import sys\nsys.stdout.write('x' * {})\nsys.stdout.flush()\nsys.exit(3)\n",
        2 * 1024 * 1024
    );
    let _ = setup_python_profile(&env, &task_id, &script2, "chatty-fail");
    let v2 = verify_now(&env, &task_id);
    assert_eq!(v2.round_status, "failed");
    assert!(v2.results[0].truncated);
    env.cleanup();
}

#[test]
fn r05_env_allowlist_strips_secrets() {
    let env = TestEnv::new("r05f");
    let repo = env.make_git_repo("env");
    repo.write("a.txt", b"x\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "env");
    let _ = capture_all(&env, &task_id);
    // The child prints a secret-shaped env var's PRESENCE (not its value).
    let script = r#"
import os, sys
names = set(k.upper() for k in os.environ)
print("HAS_API_KEY_VAR=", "API_KEY" in names or "MY_API_TOKEN" in names or "AWS_SECRET" in names)
print("HAS_PATH=", "PATH" in names)
sys.exit(0)
"#;
    let _ = setup_python_profile(&env, &task_id, script, "env-probe");
    let v = verify_now(&env, &task_id);
    assert_eq!(v.round_status, "passed");
    let summary = &v.results[0].summary;
    assert!(summary.contains("HAS_PATH= True"), "allowlisted vars forwarded: {summary}");
    assert!(summary.contains("HAS_API_KEY_VAR= False"), "secret-named vars stripped: {summary}");
    env.cleanup();
}

#[test]
fn r05_profile_validation_rejects_scripts_and_relative_exe() {
    let env = TestEnv::new("r05g");
    let repo = env.make_git_repo("profile-v");
    repo.write("a.txt", b"x\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "profile validation");
    let _ = capture_all(&env, &task_id);
    // .cmd exe refused (spec: 拒绝直接当exe执行.cmd/.bat).
    let err = service::save_profile(
        &env.ctx,
        &task_id,
        "cmd",
        vec![relaycheck::model::CheckStep {
            id: "c".into(),
            exe: r"C:\Windows\System32\cmd-script.cmd".into(),
            args: vec![],
            cwd_relative: String::new(),
            timeout_seconds: 30,
            required: true,
        }],
        vec![],
        vec![],
        env.ctx.store.task_int(&task_id, "revision").unwrap(),
    )
    .unwrap_err();
    assert!(err.message.contains("refusing to execute"), "msg: {}", err.message);
    // Relative exe refused.
    let err = service::save_profile(
        &env.ctx,
        &task_id,
        "rel",
        vec![relaycheck::model::CheckStep {
            id: "c".into(),
            exe: "python.exe".into(),
            args: vec![],
            cwd_relative: String::new(),
            timeout_seconds: 30,
            required: true,
        }],
        vec![],
        vec![],
        env.ctx.store.task_int(&task_id, "revision").unwrap(),
    )
    .unwrap_err();
    assert!(err.message.contains("absolute path"), "msg: {}", err.message);
    // outputs covering source refused.
    let python = find_python();
    let err = service::save_profile(
        &env.ctx,
        &task_id,
        "covers-src",
        vec![relaycheck::model::CheckStep {
            id: "c".into(),
            exe: python.to_string_lossy().into_owned(),
            args: vec![],
            cwd_relative: String::new(),
            timeout_seconds: 30,
            required: true,
        }],
        vec![],
        vec!["a.txt".into()],
        env.ctx.store.task_int(&task_id, "revision").unwrap(),
    )
    .unwrap_err();
    assert!(err.message.contains("must not cover source"), "msg: {}", err.message);
    env.cleanup();
}

#[test]
fn r05_secret_across_flushed_chunks_is_redacted() {
    // R11: a canary split across separately flushed writes must still be
    // redacted from the stored summary (we scan the complete bounded buffer,
    // so chunk boundaries cannot hide a match).
    let env = TestEnv::new("r05h");
    let repo = env.make_git_repo("chunks");
    repo.write("a.txt", b"x
");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "cross-chunk");
    let _ = capture_all(&env, &task_id);
    let script = r#"
import sys, time
sys.stdout.write("token=RC_CANARY_LI")
sys.stdout.flush()
time.sleep(0.2)
sys.stdout.write("VE_TOKEN_0123456789abcdef\n")
sys.stdout.flush()
sys.exit(0)
"#;
    let _ = setup_python_profile(&env, &task_id, script, "leaker");
    let v = verify_now(&env, &task_id);
    assert_eq!(v.round_status, "passed", "the check itself exits 0; results: {:?}", v.results);
    let summary = &v.results[0].summary;
    assert!(!summary.contains("RC_CANARY"), "secret leaked in summary: {summary}");
    assert!(v.results[0].redactions >= 1, "redaction counted");
    env.cleanup();
}
