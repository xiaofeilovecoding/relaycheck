//! RC02 regression: the second instance must not touch the first instance's
//! database state — instance lock BEFORE any recovery/DB write.

mod common;

use common::*;

/// Holds the instance lock the way the first instance would.
fn hold_lock(data_dir: &std::path::Path) -> std::fs::File {
    std::fs::create_dir_all(data_dir).unwrap();
    let p = data_dir.join("instance.lock");
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .share_mode(0)
            .open(&p)
            .expect("first instance lock")
    }
    #[cfg(not(windows))]
    {
        std::fs::OpenOptions::new().create(true).write(true).open(&p).unwrap()
    }
}

#[test]
fn rc02_second_instance_rejected_and_first_db_untouched() {
    let env = TestEnv::new("rc02");
    let repo = env.make_git_repo("lock");
    repo.write("a.txt", b"x\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "lock order");
    let _ = capture_all(&env, &task_id).expect("capture");

    // The FIRST instance's running operation.
    env.ctx.store.insert_operation("op-running-first", "verify", Some(&task_id), None, "").unwrap();
    env.ctx.store.update_operation("op-running-first", "running", None, None).unwrap();

    // First instance holds the lock (as it would while serving).
    let _guard = hold_lock(&env.data_dir);

    // A second launcher on the SAME data dir must fail fast with E_LIMIT.
    let exe = std::env::var("RC_TEST_EXE")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_relaycheck").to_string());
    let out = std::process::Command::new(&exe)
        .args(["serve", "--data-dir", env.data_dir.to_string_lossy().as_ref()])
        .output()
        .expect("spawn second instance");
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(
        !out.status.success() && text.contains("E_LIMIT"),
        "second instance must fail with E_LIMIT, rc={:?} out={text}",
        out.status.code()
    );

    // The first instance's operation state is UNCHANGED.
    let op = env.ctx.store.operation_state("op-running-first").unwrap();
    assert_eq!(op["state"], "running",
        "second instance must not mark the first instance's op interrupted");

    // And the failed launcher must not have rewritten the port file.
    let port_file = env.data_dir.join("port");
    let before = std::fs::read_to_string(&port_file).ok();
    let _ = std::process::Command::new(&exe)
        .args(["serve", "--data-dir", env.data_dir.to_string_lossy().as_ref()])
        .output()
        .expect("second attempt");
    assert_eq!(std::fs::read_to_string(&port_file).ok(), before,
        "failed second instance must not rewrite the port file");

    // Recovery by the LEGITIMATE owner still works afterwards.
    let ops = relaycheck::opsman::OpManager::new();
    assert_eq!(ops.recover_stale(&env.ctx.store).unwrap(), 1);
    env.cleanup();
}

#[test]
fn rc02_lock_taken_before_store_open() {
    // Source-contract pin: serve_inner must take the lock before opening
    // the store or running recovery.
    let src = std::fs::read_to_string("src/main.rs").unwrap();
    let lock_pos = src.find("instance.lock").expect("lock code present");
    let open_pos = src.find("Store::open").expect("store open present");
    assert!(lock_pos < open_pos,
        "instance lock must precede Store::open (lock at {lock_pos}, open at {open_pos})");
    let recover_pos = src.find("recover_stale").expect("recovery present");
    assert!(lock_pos < recover_pos, "instance lock must precede recovery");
}
