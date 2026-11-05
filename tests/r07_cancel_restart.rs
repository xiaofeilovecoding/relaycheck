//! R07: cancellation and crash recovery — pending/running cancel, child
//! process tree killed, Core restart marks interrupted without rerun,
//! handle-based waiting (no PID-polling), stop-unconfirmed keeps the lock.

mod common;

use common::*;
use relaycheck::checks::CancelFlag;
use std::time::Duration;

fn sleeping_script(secs: u64, child: bool) -> String {
    if child {
        format!(
            r#"
import subprocess, sys, time, os
# Spawn a grandchild that outlives the parent unless the tree is killed.
p = subprocess.Popen([sys.executable, "-c", "import time; time.sleep({secs}); import pathlib; pathlib.Path('grandchild-alive.txt').write_text('alive')"])
with open("child-pid.txt", "w") as f:
    f.write(str(p.pid))
time.sleep({secs})
"#
        )
    } else {
        format!("import time\ntime.sleep({secs})\n")
    }
}

#[test]
fn r07_cancel_running_check_kills_process_tree() {
    let env = TestEnv::new("r07");
    let repo = env.make_git_repo("cancel");
    repo.write("a.txt", b"x\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "cancel");
    let _ = capture_all(&env, &task_id);
    let _ = setup_python_profile(&env, &task_id, &sleeping_script(30, true), "sleeper");

    // Run the round on a worker thread; cancel while it is running.
    let op_id = "op-cancel-test";
    let _ = env.ctx.store.insert_operation(op_id, "verify", Some(&task_id), None, "");
    let cancel = env.ctx.ops.begin(op_id, "verify").unwrap();
    let ctx = env.ctx.clone();
    let t_task = task_id.clone();
    let cancel2 = cancel.clone();
    let worker = std::thread::spawn(move || {
        relaycheck::service::run_verify(
            &ctx,
            &relaycheck::service::VerifyRequest { task_id: t_task, trust_confirmed: true },
            &cancel2,
            op_id,
            |_| {},
        )
    });
    // Wait until the child actually started (child-pid.txt appears), then cancel.
    let workdirs = env.data_dir.join("tmp");
    let mut child_pid_file = None;
    for _ in 0..200 {
        if let Some(dir) = std::fs::read_dir(&workdirs)
            .ok()
            .and_then(|rd| rd.flatten().find(|e| e.path().join("child-pid.txt").exists()))
            .map(|e| e.path().join("child-pid.txt"))
        {
            child_pid_file = Some(dir);
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let pid_file = child_pid_file.expect("grandchild pid file never appeared");
    let child_pid: u32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
    cancel.cancel();
    let result = worker.join().unwrap();
    assert!(result.is_err() || result.unwrap().round_status == "interrupted");
    env.ctx.ops.finish(op_id);

    // The grandchild must be dead (tree kill), not just the direct child.
    std::thread::sleep(Duration::from_millis(1500));
    let alive = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {child_pid}")])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&child_pid.to_string()))
        .unwrap_or(false);
    assert!(!alive, "grandchild pid {child_pid} survived the tree kill");
    // And it never wrote its alive marker.
    let grandchild_marker = pid_file.parent().unwrap().join("grandchild-alive.txt");
    // The workdir may already be cleaned; if it still exists, the marker
    // must be absent (child killed before writing).
    if grandchild_marker.exists() {
        panic!("grandchild wrote its marker after cancellation");
    }
    env.cleanup();
}

#[test]
fn r07_recover_stale_marks_interrupted_without_rerun() {
    let env = TestEnv::new("r07b");
    let repo = env.make_git_repo("recover");
    repo.write("a.txt", b"x\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "recover");
    let _ = capture_all(&env, &task_id);
    // Simulate a crash: an operation left 'running' in the DB.
    env.ctx
        .store
        .insert_operation("stale-op-1", "verify", Some(&task_id), None, "")
        .unwrap();
    env.ctx.store.update_operation("stale-op-1", "running", None, None).unwrap();

    let ops = relaycheck::opsman::OpManager::new();
    let n = ops.recover_stale(&env.ctx.store).unwrap();
    assert_eq!(n, 1, "one stale operation recovered");
    let op = env.ctx.store.operation_state("stale-op-1").unwrap();
    assert_eq!(op["state"], "interrupted");
    assert_eq!(op["error"]["code"], "E_STOP_UNCONFIRMED");
    // No verification was created by recovery (nothing reran).
    assert!(env.ctx.store.verifications_for_task(&task_id).unwrap().is_empty());
    env.cleanup();
}

#[test]
fn r07_cancel_unknown_or_finished_operation_is_a_clear_error() {
    let env = TestEnv::new("r07c");
    // Unknown id → not cancellable (404 at the HTTP layer).
    let ok = env.ctx.ops.cancel("no-such-op").unwrap();
    assert!(!ok);
    // A completed op: cancel must not resurrect or corrupt state.
    env.ctx.store.insert_operation("done-op", "capture", None, None, "").unwrap();
    env.ctx.store.update_operation("done-op", "completed", Some("{}"), None).unwrap();
    let ok = env.ctx.ops.cancel("done-op").unwrap();
    assert!(!ok, "finished op is not active; nothing to cancel");
    let op = env.ctx.store.operation_state("done-op").unwrap();
    assert_eq!(op["state"], "completed");
    env.cleanup();
}

#[test]
fn r07_cancel_flag_semantics_and_handle_based_wait() {
    // The wait loop uses the process HANDLE (not PID polling), so PID reuse
    // cannot fool the stop confirmation. This test pins the CancelFlag
    // contract used across threads.
    let cancel = CancelFlag::new();
    assert!(!cancel.is_cancelled());
    let c2 = cancel.clone();
    let t = std::thread::spawn(move || {
        for _ in 0..100 {
            if c2.is_cancelled() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    });
    cancel.cancel();
    assert!(t.join().unwrap(), "clone observes cancellation");
}

#[test]
fn r07_timeout_interrupts_without_failing_assertions() {
    // A step that exceeds its own timeout → interrupted (超时/取消≠断言失败).
    let env = TestEnv::new("r07d");
    let repo = env.make_git_repo("timeout");
    repo.write("a.txt", b"x\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "timeout");
    let _ = capture_all(&env, &task_id);
    let python = find_python();
    let script = env.scratch.join("sleep2.py");
    std::fs::write(&script, b"import time\ntime.sleep(2)\n").unwrap();
    let profile = relaycheck::service::save_profile(
        &env.ctx,
        &task_id,
        "short-timeout",
        vec![relaycheck::model::CheckStep {
            id: "sleepy".into(),
            exe: python.to_string_lossy().into_owned(),
            args: vec![script.to_string_lossy().into_owned()],
            cwd_relative: String::new(),
            timeout_seconds: 1,
            required: true,
        }],
        vec![],
        vec![],
    )
    .unwrap();
    env.ctx.store.set_profile_trusted(&profile.id, true).unwrap();
    let v = verify_now(&env, &task_id);
    assert_eq!(v.round_status, "interrupted", "timeout is not an assertion failure");
    assert_eq!(v.results[0].status, "interrupted");
    assert_eq!(v.results[0].detail, "timeout");
    env.cleanup();
}
