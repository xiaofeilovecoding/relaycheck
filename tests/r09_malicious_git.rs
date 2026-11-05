//! R09: malicious git configuration must never execute — fsmonitor, hooks,
//! clean/smudge filters and prompt helpers are all pinned off; corrupted
//! blob store bars checks and acceptance.

mod common;

use common::*;
use relaycheck::service;

/// Writes a marker via cmd whenever executed. If git ever runs our pinned
/// config values, hooks, or filters, the marker file appears and the test
/// fails.
fn marker_writer_fixture(repo: &Repo) -> std::path::PathBuf {
    let marker_cmd = repo.root.join(".rc-marker.cmd");
    std::fs::write(
        &marker_cmd,
        "@echo executed > .rc-marker-executed.txt\n".as_bytes(),
    )
    .unwrap();
    marker_cmd
}

#[test]
fn r09_malicious_git_config_and_hooks_never_execute() {
    let env = TestEnv::new("r09");
    let repo = env.make_git_repo("malicious");
    repo.write("a.txt", b"content\n");
    repo.write("evil.bin", b"\x00\x01\x02binary\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "base"]);

    marker_writer_fixture(&repo);
    let marker = repo.root.join(".rc-marker-executed.txt");

    // Malicious local config: fsmonitor + hooksPath + filters pointing at
    // the marker script. None of these may run during attach/capture.
    let config = repo.root.join(".git").join("config");
    let mut cfg = std::fs::read_to_string(&config).unwrap();
    cfg.push_str(&format!(
        "[core]\n\tfsmonitor = {}\n\thooksPath = {}\n\taskpass = {}\n\teditor = {}\n[filter \"rc-evil\"]\n\tclean = {}\n\tsmudge = {}\n\trequired = false\n",
        marker_cmd_display(&repo, ".rc-marker.cmd"),
        marker_cmd_display(&repo, "."),
        marker_cmd_display(&repo, ".rc-marker.cmd"),
        marker_cmd_display(&repo, ".rc-marker.cmd"),
        marker_cmd_display(&repo, ".rc-marker.cmd"),
        marker_cmd_display(&repo, ".rc-marker.cmd"),
    ));
    std::fs::write(&config, cfg).unwrap();

    // Malicious hooks in the default hooks path too.
    let hooks = repo.root.join(".git").join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    for h in ["pre-commit", "post-commit", "pre-merge-commit", "reference-transaction", "post-index-change"] {
        let p = hooks.join(format!("{h}.cmd"));
        std::fs::write(&p, "@echo executed > ../.rc-marker-executed.txt\n").unwrap();
        let p2 = hooks.join(h);
        std::fs::write(&p2, "#!/bin/sh\necho executed > ../.rc-marker-executed.txt\n").unwrap();
    }

    // A repo-level .gitattributes trying to install the filter on our files.
    repo.write(".gitattributes", b"*.txt filter=rc-evil\n*.bin filter=rc-evil\n");

    // The product operations: attach, task, capture.
    let (_p, task_id) = setup_project_task(&env, &repo, "malicious config");
    let _ = capture_all(&env, &task_id).expect("capture proceeds with isolated git");

    // The marker script was NEVER executed.
    assert!(!marker.exists(), "marker executed: malicious config/hook/filter ran!");

    // Repo state untouched (except the test's own fixture files, which were
    // added BEFORE the fingerprint).
    env.cleanup();
}

fn marker_cmd_display(repo: &Repo, rel: &str) -> String {
    // Use forward-slash-free absolute path in config values.
    let p = repo.root.join(rel);
    p.to_string_lossy().replace('\\', "/")
}

#[test]
fn r09_corrupt_blob_bars_checks_and_acceptance() {
    let env = TestEnv::new("r09b");
    let repo = env.make_git_repo("corrupt");
    repo.write("a.txt", b"valuable\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "corrupt");
    let cap = capture_all(&env, &task_id).expect("capture");
    let files = env.ctx.store.snapshot_files(&cap.snapshot_id).unwrap();

    // A passing round exists first; corruption then bars future rounds.
    let profile0 = service::save_profile(
        &env.ctx,
        &task_id,
        "pre",
        vec![relaycheck::model::CheckStep {
            id: "c0".into(),
            exe: find_python().to_string_lossy().into_owned(),
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
    env.ctx.store.set_profile_trusted(&profile0.id, true).unwrap();
    verify_now(&env, &task_id);

    // Corrupt the blob on disk.
    let sha = &files[0].sha256;
    let blob_path = env
        .data_dir
        .join("blobs")
        .join(&sha[..2])
        .join(sha);
    std::fs::write(&blob_path, b"TAMPERED\n").unwrap();

    // Reading the blob through the store fails with E_CORRUPT.
    let err = env.ctx.blobs.get(sha).unwrap_err();
    assert_eq!(err.code, relaycheck::errors::Code::ECorrupt);

    // Checks refuse to run on an incomplete store (same profile, so the
    // profile hash and revision are unchanged from the passing round).
    let op_id = "op-corrupt";
    let _ = env.ctx.store.insert_operation(op_id, "verify", Some(&task_id), None, "");
    let cancel = env.ctx.ops.begin(op_id, "verify").unwrap();
    let result = service::run_verify(
        &env.ctx,
        &{
            let (pid, phash, rev) = profile_identity(&env, &task_id);
            service::VerifyRequest {
                task_id: task_id.clone(),
                trust_confirmed: true,
                profile_id: pid,
                profile_hash: phash,
                if_match: rev,
            }
        },
        &cancel,
        op_id,
        |_| {},
    );
    env.ctx.ops.finish(op_id);
    let err = result.unwrap_err();
    assert_eq!(err.code, relaycheck::errors::Code::ECorrupt);

    // Acceptance check also fails closed.
    let check = service::acceptance_check(&env.ctx, &task_id).unwrap();
    assert!(!check.ok);
    assert!(check.missing.iter().any(|m| m.contains("blobs incomplete")));
    env.cleanup();
}

#[test]
fn r09_attach_rejects_unc_device_and_missing_paths() {
    let env = TestEnv::new("r09c");
    for bad in [
        r"\\server\share",
        r"\\.\pipe\x",
        "relative/path",
    ] {
        let err = relaycheck::gitx::identify(bad, &env.ctx.isolated_home).unwrap_err();
        assert_eq!(
            err.code,
            relaycheck::errors::Code::EPathBoundary,
            "path {bad} must be rejected with E_PATH_BOUNDARY, got {err:?}"
        );
    }
    let err = relaycheck::gitx::identify(
        &env.scratch.join("does-not-exist").to_string_lossy(),
        &env.ctx.isolated_home,
    )
    .unwrap_err();
    assert_eq!(err.code, relaycheck::errors::Code::EPathBoundary);
    env.cleanup();
}

#[test]
fn r09_submodule_capture_rejected_with_actionable_message() {
    let env = TestEnv::new("r09d");
    let repo = env.make_git_repo("submod");
    repo.write("a.txt", b"x\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "base"]);
    // Stage a gitlink entry without a real submodule checkout.
    let idx = repo.git(&["ls-files", "--stage"]);
    assert!(idx.contains("100644"), "fixture sanity: a.txt staged as 100644");
    // Create a gitlink manually: add an empty dir and use update-index.
    std::fs::create_dir_all(repo.root.join("embedded")).unwrap();
    repo.git(&["update-index", "--add", "--cacheinfo", "160000,1234567890abcdef1234567890abcdef12345678,embedded"]);
    repo.git(&["commit", "-qm", "with-gitlink"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "submodule");
    let err = capture_all(&env, &task_id).unwrap_err();
    assert!(
        err.message.contains("submodule"),
        "actionable submodule error, got: {}", err.message
    );
    env.cleanup();
}

#[test]
fn r09_lfs_dependency_rejected_without_remote() {
    let env = TestEnv::new("r09e");
    let repo = env.make_git_repo("lfs");
    repo.write("a.txt", b"x\n");
    repo.write(".gitattributes", b"*.bin filter=lfs\n");
    repo.write("model.bin", b"\x00\x01fake lfs pointer content\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "lfs");
    let err = capture_all(&env, &task_id).unwrap_err();
    assert!(
        err.message.contains("Git LFS"),
        "LFS capture refused with actionable message, got: {}", err.message
    );
    env.cleanup();
}

#[test]
fn r09_clean_filter_via_gitattributes_never_executes() {
    // Real hazard found while building this suite: `git status --porcelain`
    // re-hashes modified worktree files, which EXECUTES the clean filter
    // declared in .gitattributes (a "read-only" command running arbitrary
    // code). The product derives dirty-ness from index hashes instead and
    // never runs status — this test pins that contract.
    let env = TestEnv::new("r09f");
    let repo = env.make_git_repo("clean-filter");
    repo.write("a.txt", b"original
");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "base"]);
    let marker_cmd = repo.root.join(".rc-marker.cmd");
    std::fs::write(&marker_cmd, "@echo executed > .rc-marker-executed.txt
").unwrap();
    let marker = repo.root.join(".rc-marker-executed.txt");
    // Local config installs the filter; .gitattributes activates it.
    let config = repo.root.join(".git").join("config");
    let mut cfg = std::fs::read_to_string(&config).unwrap();
    let root_s = repo.root.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
    cfg.push_str(&format!(
        "[filter \"rc-evil\"]
	clean = {root_s}/.rc-marker.cmd
	required = false
"
    ));
    std::fs::write(&config, cfg).unwrap();
    repo.write(".gitattributes", b"*.txt filter=rc-evil
");
    // The file is MODIFIED relative to the index — exactly the state that
    // makes git status run clean filters.
    repo.write("a.txt", b"modified
");

    let (_p, task_id) = setup_project_task(&env, &repo, "clean filter hazard");
    let _ = capture_all(&env, &task_id).expect("capture proceeds");
    // Dirty detection came from hash comparison; the filter never ran.
    assert!(!marker.exists(), "clean filter executed during capture");
    env.cleanup();
}
