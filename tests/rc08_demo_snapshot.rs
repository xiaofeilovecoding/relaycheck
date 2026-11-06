//! RC08 regression: the demo check entry resolves inside the SNAPSHOT copy.
//! Original-repo changes (source or test) never affect an already-captured
//! snapshot's verification; a genuinely broken new candidate fails.

mod common;

use common::*;
use relaycheck::service;

#[test]
fn rc08_demo_check_reads_snapshot_not_original_repo() {
    let env = TestEnv::new("rc08");
    let task_id = service::create_demo(&env.ctx).unwrap();
    let repo = std::fs::read_dir(env.data_dir.join("demo"))
        .unwrap().next().unwrap().unwrap().path();
    let script = repo.join("tests/test_retry.py");
    let source = repo.join("src/retry.py");
    let original_test = std::fs::read(&script).unwrap();
    let original_source = std::fs::read(&source).unwrap();
    assert_eq!(verify_now(&env, &task_id).round_status, "passed");

    std::fs::write(&script, b"raise RuntimeError('live test must not run')\n").unwrap();
    std::fs::write(&source, b"def retry(fn, times=3, cancelled=None): return fn()\n").unwrap();
    assert_eq!(verify_now(&env, &task_id).round_status, "passed", "frozen snapshot changed");

    std::fs::write(&script, original_test).unwrap();
    capture_all(&env, &task_id).expect("capture broken source");
    assert_eq!(verify_now(&env, &task_id).round_status, "failed");

    std::fs::write(&source, original_source).unwrap();
    capture_all(&env, &task_id).expect("capture restored source");
    assert_eq!(verify_now(&env, &task_id).round_status, "passed");
    env.cleanup();
}

#[test]
fn rc08_demo_profile_uses_relative_entry() {
    // The shipped demo profile must use a RELATIVE test entry (resolves in
    // the snapshot copy), never an absolute path into the original repo.
    let env = TestEnv::new("rc08b");
    let repo = env.make_git_repo("demo");
    repo.write("src/retry.py", b"def retry(fn, times=3, cancelled=None):\n    if cancelled:\n        raise TimeoutError()\n    return fn()\n");
    repo.write("tests/test_retry.py", b"import sys\nsys.path.insert(0, 'src')\nfrom retry import retry\nassert retry(lambda: 1) == 1\ntry:\n    retry(lambda: 1, cancelled=True)\n    print('FAIL: no raise')\n    sys.exit(1)\nexcept TimeoutError:\n    pass\nprint('ok')\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "demo base"]);
    let task_id = service::create_demo(&env.ctx).unwrap();
    let pid = env.ctx.store.task_field_opt(&task_id, "profile_id").unwrap().unwrap_or_default();
    assert!(!pid.is_empty(), "Python is required for the real demo check");
    let prof = env.ctx.store.profile_json(&pid).unwrap();
    let arg = &prof.checks[0].args[prof.checks[0].args.len() - 1];
    assert!(!arg.contains("demo-repo") && !arg.contains(':'), "absolute test entry: {arg}");
    assert_eq!(arg, "tests/test_retry.py");
    // And the demo task itself verifies green from the snapshot copy.
    let v = verify_now(&env, &task_id);
    assert_eq!(v.round_status, "passed", "demo real assertion must pass on the snapshot: {:?}", v.results);
    env.cleanup();
}
