//! R11 (ADOPTION R01/R02/R03): offline `verify-handoff` — compare a handoff
//! package's listed files with an explicitly designated Git checkout by real
//! bytes, size and SHA-256. Covers clean match (context-only and
//! portable-source), one-byte drift, missing files, CRLF byte drift, moved
//! HEAD with matching content, extra files never widening the conclusion,
//! damaged packages and link refusal, non-UTF-8 content verified as bytes,
//! unsafe manifest paths, and the CLI exit contract (0 match / 1 drift /
//! 2 bad args / 3 cannot verify safely). Zero model calls, all synthetic.

mod common;

use common::{export_handoff, setup_project_task, TestEnv};
use relaycheck::errors::Code;
use relaycheck::service::CaptureRequest;
use relaycheck::verify_handoff;
use std::path::{Path, PathBuf};

const ISOLATED_HOME_REL: &str = "verify-home";

fn isolated_home(env: &TestEnv) -> PathBuf {
    let dir = env.scratch.join(ISOLATED_HOME_REL);
    std::fs::create_dir_all(dir.join("hooks-empty")).unwrap();
    dir
}

/// Builds a repo with committed text, CRLF and binary files, captures the
/// candidate and exports a package through the REAL generation path.
struct Fixture {
    env: TestEnv,
    repo: common::Repo,
    context_pkg: Vec<u8>,
    portable_pkg: Vec<u8>,
}

fn build_fixture(tag: &str) -> Fixture {
    let env = TestEnv::new(tag);
    let repo = env.make_git_repo("checkout");
    repo.write("docs/plan.md", b"step one\nstep two\n".as_slice());
    repo.write("docs/crlf.txt", b"line one\r\nline two\r\n".as_slice());
    repo.write("assets/logo.bin", [0x00, 0xff, 0x10, 0x7f, 0xfe].as_slice());
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "base"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "verify-handoff fixture");

    let op_id = format!("cap-{tag}");
    let _ = env.ctx.store.insert_operation(&op_id, "capture", Some(task_id.as_str()), None, "");
    let cancel = env.ctx.ops.begin(&op_id, "capture").unwrap();
    let cap = relaycheck::service::run_capture(
        &env.ctx,
        &CaptureRequest {
            task_id: task_id.clone(),
            untracked_selected: vec![],
            extra_excludes: vec![],
            attest_stopped: true,
            expected_revision: env.ctx.store.task_int(&task_id, "revision").unwrap(),
        },
        &op_id,
    )
    .expect("capture");
    env.ctx.ops.finish(&op_id);
    let _ = cancel;

    let (_id1, _sha1, context_pkg) = export_handoff(&env, &task_id, "context_only");
    let (_id2, _sha2, portable_pkg) = export_handoff(&env, &task_id, "portable_source");
    let _ = cap;
    Fixture { env, repo, context_pkg, portable_pkg }
}

fn verify_ok(pkg: &[u8], repo_root: &Path, home: &Path) -> verify_handoff::VerifyOutcome {
    verify_handoff::verify(pkg, repo_root.to_string_lossy().as_ref(), home)
        .expect("verification should complete")
}

fn verify_blocked(pkg: &[u8], repo_root: &Path, home: &Path) -> relaycheck::errors::RelayError {
    verify_handoff::verify(pkg, repo_root.to_string_lossy().as_ref(), home)
        .expect_err("verification must refuse")
}

#[test]
fn r11_clean_match_context_and_portable() {
    let fx = build_fixture("r11a");
    let home = isolated_home(&fx.env);

    for pkg in [&fx.context_pkg, &fx.portable_pkg] {
        let out = verify_ok(pkg, &fx.repo.root, &home);
        assert_eq!(out.result, "match", "clean checkout matches");
        assert_eq!(out.counts.matched, 3, "three listed files matched");
        assert_eq!(out.counts.changed + out.counts.missing + out.counts.unreadable, 0);
        assert_eq!(out.base_head_match, Some(true), "HEAD matches the declared base");
        // files sorted by path; statuses only carry relative names
        let paths: Vec<&str> = out.files.iter().map(|f| f.path.as_str()).collect();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted, "file list sorted by path");
        assert!(paths.contains(&"assets/logo.bin"), "non-UTF-8 content verified by bytes");
        // non-UTF-8 bytes really matched via sha256
        let bin = out.files.iter().find(|f| f.path == "assets/logo.bin").unwrap();
        assert_eq!(bin.status, "matched");
        // sender claims stay separate and labeled exporter_reported
        for c in &out.receipt_claims {
            assert_eq!(c.origin, "exporter_reported");
        }
    }

    // Read-only guarantee: the checkout is untouched and nothing was unpacked.
    let status = fx.repo.git(&["status", "--porcelain"]);
    assert!(status.trim().is_empty(), "verify leaves the checkout untouched");
    assert!(!fx.repo.root.join("blobs").exists(), "no blobs unpacked into the repo");
    let _ = fx.env.cleanup();
}

#[test]
fn r11_one_byte_drift_and_missing_and_crlf() {
    let fx = build_fixture("r11b");
    let home = isolated_home(&fx.env);

    // one byte changed in a tracked file
    fx.repo.write("docs/plan.md", b"step ONE\nstep two\n".as_slice());
    let out = verify_ok(&fx.context_pkg, &fx.repo.root, &home);
    assert_eq!(out.result, "drift");
    assert_eq!(out.counts.changed, 1);
    let plan = out.files.iter().find(|f| f.path == "docs/plan.md").unwrap();
    assert_eq!(plan.status, "changed");

    // CRLF byte drift with identical length and displayed text
    let fx2 = build_fixture("r11b2");
    fx2.repo.write("docs/crlf.txt", b"line one\nline two\n".as_slice());
    let out2 = verify_ok(&fx2.context_pkg, &fx2.repo.root, &home);
    assert_eq!(out2.result, "drift", "LF vs CRLF is a byte-level drift");
    assert_eq!(out2.counts.changed, 1);
    assert_eq!(
        out2.files.iter().find(|f| f.path == "docs/crlf.txt").unwrap().status,
        "changed"
    );

    // missing file
    let fx3 = build_fixture("r11b3");
    std::fs::remove_file(fx3.repo.root.join("assets/logo.bin")).unwrap();
    let out3 = verify_ok(&fx3.context_pkg, &fx3.repo.root, &home);
    assert_eq!(out3.result, "drift");
    assert_eq!(out3.counts.missing, 1);
    assert_eq!(
        out3.files.iter().find(|f| f.path == "assets/logo.bin").unwrap().status,
        "missing"
    );
    let _ = fx.env.cleanup();
    let _ = fx2.env.cleanup();
    let _ = fx3.env.cleanup();
}

#[test]
fn r11_head_moves_but_content_matches() {
    let fx = build_fixture("r11c");
    let home = isolated_home(&fx.env);
    // move HEAD (new commit touching nothing listed) — content still matches
    fx.repo.write("OTHER.txt", b"unrelated\n".as_slice());
    fx.repo.git(&["add", "."]);
    fx.repo.git(&["commit", "-qm", "unrelated"]);
    let out = verify_ok(&fx.context_pkg, &fx.repo.root, &home);
    assert_eq!(out.result, "match", "content match is independent of HEAD");
    assert_eq!(out.base_head_match, Some(false), "HEAD move reported independently");
    assert!(
        out.unverified.iter().any(|u| u.contains("ancestry")),
        "git ancestry explicitly unverified"
    );
    let _ = fx.env.cleanup();
}

#[test]
fn r11_extra_files_never_widen_the_conclusion() {
    let fx = build_fixture("r11d");
    let home = isolated_home(&fx.env);
    // extra modified file NOT in the manifest must not turn match into drift
    fx.repo.write("extra.txt", b"extra content\n".as_slice());
    let out = verify_ok(&fx.context_pkg, &fx.repo.root, &home);
    assert_eq!(out.result, "match", "worktree scope proves only listed files");
    assert_eq!(out.counts.matched, 3);
    assert!(
        out.unverified.iter().any(|u| u.contains("not listed")),
        "extra files explicitly unverified"
    );
    let _ = fx.env.cleanup();
}

#[test]
fn r11_damaged_package_is_blocked_not_drift() {
    let fx = build_fixture("r11e");
    let home = isolated_home(&fx.env);
    let mut damaged = fx.context_pkg.clone();
    let mid = damaged.len() / 2;
    damaged[mid] ^= 0xff;
    let err = verify_blocked(&damaged, &fx.repo.root, &home);
    assert_eq!(err.code, Code::EUnverifiedImport, "cannot verify safely");
    let _ = fx.env.cleanup();
}

#[test]
fn r11_symlink_entry_is_refused() {
    let env = TestEnv::new("r11f");
    // a package carrying a symlink entry is refused by the shared validation
    let mut raw = Vec::new();
    {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut raw));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .unix_permissions(0o120777); // S_IFLNK
        w.add_symlink("link", "target/evil.txt", opts).unwrap();
        w.finish().unwrap();
    }
    let err = verify_handoff::inspect(&raw).unwrap_err();
    assert_eq!(err.code, Code::EUnverifiedImport, "symlink entries refused");
    let _ = env.cleanup();
}

#[test]
fn r11_unsafe_manifest_paths_are_blocked() {
    let fx = build_fixture("r11g");
    let home = isolated_home(&fx.env);
    let mk = |manifest_mutator: &dyn Fn(&mut serde_json::Value)| -> Vec<u8> {
        let receipt = serde_json::json!({
            "schema_version": 1, "kind": "handoff",
            "handoff_id": "3f2b8c1a-1111-4222-8333-444455556666",
            "task_id": "aaaaaaaa-bbbb-4ccc-8ddd-eeeeffff0000",
            "task_revision": 1,
            "snapshot_id": format!("sha256:{}", "a".repeat(64)),
            "base_head": "",
            "source_mode": "context_only",
            "coverage": "selected_sources",
            "goal": "unsafe path probe", "constraints": [], "open_items": [],
            "next_action": "n", "claims": []
        });
        let mut manifest = serde_json::json!({
            "schema_version": 1, "kind": "relaycheck_snapshot",
            "scope": "selected_sources", "files": [], "excluded": []
        });
        manifest_mutator(&mut manifest);
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            w.start_file("README.md", opts).unwrap();
            std::io::Write::write_all(&mut w, b"pkg").unwrap();
            w.start_file("receipt.json", opts).unwrap();
            std::io::Write::write_all(&mut w, receipt.to_string().as_bytes()).unwrap();
            w.start_file("manifest.json", opts).unwrap();
            std::io::Write::write_all(&mut w, manifest.to_string().as_bytes()).unwrap();
            w.finish().unwrap();
        }
        buf.into_inner()
    };
    let file_entry = |path: &str| {
        serde_json::json!([{"path": path, "mode": "100644", "size": 1, "sha256": format!("{}", "b".repeat(64))}])
    };
    for bad in ["../escape.txt", ".git/config", "docs/plan.md:stream", "CON.txt"] {
        let pkg = mk(&|m: &mut serde_json::Value| m["files"] = file_entry(bad));
        let err = verify_blocked(&pkg, &fx.repo.root, &home);
        assert_eq!(err.code, Code::EUnverifiedImport, "unsafe manifest path refused: {bad}");
    }
    // case-insensitive collision inside the manifest
    let colliding = serde_json::json!([
        {"path": "docs/plan.md", "mode": "100644", "size": 1, "sha256": format!("{}", "b".repeat(64))},
        {"path": "DOCS/PLAN.MD", "mode": "100644", "size": 1, "sha256": format!("{}", "c".repeat(64))}
    ]);
    let pkg = mk(&|m: &mut serde_json::Value| m["files"] = colliding.clone());
    let err = verify_blocked(&pkg, &fx.repo.root, &home);
    assert_eq!(err.code, Code::EUnverifiedImport, "case-colliding manifest refused");
    let _ = fx.env.cleanup();
}

#[test]
fn r11_reparse_point_refusal() {
    // root replaced by a reparse point → blocked; listed path replaced by a
    // reparse point → blocked (never read through the link)
    let fx = build_fixture("r11h");
    let home = isolated_home(&fx.env);
    let link_root = fx.env.scratch.join("reparse-root");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&fx.repo.root, &link_root).expect("symlink");
    #[cfg(windows)]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link_root)
        .arg(&fx.repo.root)
        .output()
        .expect("mklink spawn");
    if link_root.exists() {
        let err = verify_blocked(&fx.context_pkg, &link_root, &home);
        assert_eq!(err.code, Code::EUnverifiedImport, "reparse root refused");
    }

    // listed path replaced by a junction/symlink pointing elsewhere
    let target_dir = fx.env.scratch.join("elsewhere");
    std::fs::create_dir_all(&target_dir).unwrap();
    let listed = fx.repo.root.join("docs/plan.md");
    std::fs::remove_file(&listed).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target_dir, &listed).expect("symlink");
    #[cfg(windows)]
    {
        let ok = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&listed)
            .arg(&target_dir)
            .output()
            .expect("mklink spawn")
            .status
            .success();
        if !ok {
            // without junction privileges the refusal path is covered elsewhere
            return;
        }
    }
    let err = verify_blocked(&fx.context_pkg, &fx.repo.root, &home);
    assert_eq!(err.code, Code::EUnverifiedImport, "listed path must not be read through a link");
    let _ = fx.env.cleanup();
}

#[test]
fn r11_cli_exit_contract() {
    let fx = build_fixture("r11i");
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_relaycheck"));
    let home = isolated_home(&fx.env);
    let pkg_path = fx.env.scratch.join("cli-pkg.zip");
    std::fs::write(&pkg_path, &fx.context_pkg).unwrap();

    let run = |args: &[&std::path::Path]| -> (i32, String) {
        let out = std::process::Command::new(&exe)
            .args(args)
            .env("TEMP", home.parent().unwrap_or(&fx.env.scratch))
            .output()
            .expect("spawn relaycheck");
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let repo = fx.repo.root.clone();
    let repo_arg = repo.clone();
    let pkg = pkg_path.clone();

    // match → exit 0 + JSON
    let (code, out) = run(&[std::path::Path::new("verify-handoff"), &pkg, std::path::Path::new("--repo"), &repo_arg, std::path::Path::new("--json")]);
    assert_eq!(code, 0, "match exits 0: {out}");
    let v: serde_json::Value = serde_json::from_str(&out).expect("JSON result");
    assert_eq!(v["result"], "match");
    assert!(v["schema_version"].as_i64().unwrap() >= 1);
    assert!(v["snapshot_id"].as_str().unwrap().starts_with("sha256:"));
    assert!(v["counts"]["matched"].as_u64().unwrap() == 3);

    // drift → exit 1
    fx.repo.write("docs/plan.md", b"step one\nstep TWO\n".as_slice());
    let repo_arg = repo.clone();
    let (code, out) = run(&[std::path::Path::new("verify-handoff"), &pkg, std::path::Path::new("--repo"), &repo_arg]);
    assert_eq!(code, 1, "drift exits 1");
    assert!(out.contains("drift"));

    // damaged package → exit 3
    let mut damaged = fx.context_pkg.clone();
    let mid = damaged.len() / 3;
    damaged[mid] ^= 0x5a;
    let damaged_path = fx.env.scratch.join("damaged.zip");
    std::fs::write(&damaged_path, &damaged).unwrap();
    let repo_arg = repo.clone();
    let (code, out) = run(&[std::path::Path::new("verify-handoff"), &damaged_path, std::path::Path::new("--repo"), &repo_arg, std::path::Path::new("--json")]);
    assert_eq!(code, 3, "cannot verify safely exits 3");
    let v: serde_json::Value = serde_json::from_str(&out).expect("JSON error");
    assert_eq!(v["error"]["code"], "E_UNVERIFIED_IMPORT");

    // missing args → exit 2
    let (code, _) = run(&[std::path::Path::new("verify-handoff")]);
    assert_eq!(code, 2, "missing args exit 2");
    let missing_pkg = fx.env.scratch.join("no-such.zip");
    let repo_arg = repo.clone();
    let (code, _) = run(&[std::path::Path::new("verify-handoff"), &missing_pkg, std::path::Path::new("--repo"), &repo_arg]);
    assert_eq!(code, 2, "missing package file exits 2");
    // relative repo path → bad args (2)
    let (code, _) = run(&[std::path::Path::new("verify-handoff"), &pkg, std::path::Path::new("--repo"), std::path::Path::new("relative/checkout")]);
    assert_eq!(code, 2, "relative checkout exits 2");

    let _ = fx.env.cleanup();
}

// ---- R01/R02 rework: content identity, bounded read, output hygiene ----

/// Rebuilds a zip with a mutated receipt (everything else byte-identical),
/// keeping the manifest as-is so the identity check can be exercised.
fn rebuild_with_receipt(original: &[u8], mutate: impl FnOnce(&mut serde_json::Value)) -> Vec<u8> {
    let mut receipt: serde_json::Value = {
        let cur = std::io::Cursor::new(original);
        let mut z = zip::ZipArchive::new(cur).unwrap();
        let mut f = z.by_name("receipt.json").unwrap();
        let mut raw = Vec::new();
        std::io::Read::read_to_end(&mut f, &mut raw).unwrap();
        serde_json::from_slice(&raw).unwrap()
    };
    mutate(&mut receipt);
    let manifest_raw: Vec<u8> = {
        let cur = std::io::Cursor::new(original);
        let mut z = zip::ZipArchive::new(cur).unwrap();
        let mut f = z.by_name("manifest.json").unwrap();
        let mut raw = Vec::new();
        std::io::Read::read_to_end(&mut f, &mut raw).unwrap();
        raw
    };
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut w = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        w.start_file("README.md", opts).unwrap();
        std::io::Write::write_all(&mut w, b"pkg").unwrap();
        w.start_file("receipt.json", opts).unwrap();
        std::io::Write::write_all(&mut w, receipt.to_string().as_bytes()).unwrap();
        w.start_file("manifest.json", opts).unwrap();
        std::io::Write::write_all(&mut w, &manifest_raw).unwrap();
        w.finish().unwrap();
    }
    buf.into_inner()
}

#[test]
fn r11_snapshot_identity_mismatch_is_blocked_never_match() {
    let fx = build_fixture("r11j");
    let home = isolated_home(&fx.env);
    let rehashed = rebuild_with_receipt(&fx.context_pkg, |r| {
        r["snapshot_id"] = serde_json::json!(format!("sha256:{}", "9".repeat(64)));
    });
    let err = verify_blocked(&rehashed, &fx.repo.root, &home);
    assert_eq!(err.code, Code::EUnverifiedImport);
    assert!(
        err.message.to_lowercase().contains("snapshot id"),
        "identity refusal names the mismatch: {}",
        err.message
    );
    // the untouched package still verifies as a match
    let ok = verify_ok(&fx.context_pkg, &fx.repo.root, &home);
    assert_eq!(ok.result, "match");
    let _ = fx.env.cleanup();
}

#[test]
fn r11_unknown_scope_and_duplicate_paths_blocked() {
    let fx = build_fixture("r11k");
    let home = isolated_home(&fx.env);
    // rebuild the whole package with a mutated manifest and a matching id so
    // only the scope/path checks can fire
    let rebuild_manifest = |scope: &str, files: serde_json::Value| -> Vec<u8> {
        let cur = std::io::Cursor::new(&fx.context_pkg);
        let mut z = zip::ZipArchive::new(cur).unwrap();
        let mut receipt_raw = Vec::new();
        std::io::Read::read_to_end(
            &mut z.by_name("receipt.json").unwrap(),
            &mut receipt_raw,
        )
        .unwrap();
        let mut manifest_raw = Vec::new();
        std::io::Read::read_to_end(
            &mut z.by_name("manifest.json").unwrap(),
            &mut manifest_raw,
        )
        .unwrap();
        let mut manifest: serde_json::Value = serde_json::from_slice(&manifest_raw).unwrap();
        manifest["scope"] = serde_json::json!(scope);
        manifest["files"] = files;
        let manifest_sha = relaycheck::canonical::canonical_sha256(&manifest).unwrap();
        let mut receipt: serde_json::Value = serde_json::from_slice(&receipt_raw).unwrap();
        receipt["snapshot_id"] = serde_json::json!(format!("sha256:{manifest_sha}"));
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            w.start_file("README.md", opts).unwrap();
            std::io::Write::write_all(&mut w, b"pkg").unwrap();
            w.start_file("receipt.json", opts).unwrap();
            std::io::Write::write_all(&mut w, receipt.to_string().as_bytes()).unwrap();
            w.start_file("manifest.json", opts).unwrap();
            std::io::Write::write_all(&mut w, manifest.to_string().as_bytes()).unwrap();
            w.finish().unwrap();
        }
        buf.into_inner()
    };
    let one_file = serde_json::json!([
        {"path": "docs/plan.md", "mode": "100644", "size": 1, "sha256": format!("{}", "b".repeat(64))}
    ]);
    let unknown_scope = rebuild_manifest("custom-scope", one_file);
    let err = verify_blocked(&unknown_scope, &fx.repo.root, &home);
    assert!(err.message.contains("scope"), "unknown scope refused: {}", err.message);

    let duplicated = serde_json::json!([
        {"path": "docs/plan.md", "mode": "100644", "size": 1, "sha256": format!("{}", "b".repeat(64))},
        {"path": "docs/plan.md", "mode": "100644", "size": 1, "sha256": format!("{}", "c".repeat(64))}
    ]);
    let dup = rebuild_manifest("selected_sources", duplicated);
    let err = verify_blocked(&dup, &fx.repo.root, &home);
    assert!(err.message.contains("twice"), "duplicate path refused: {}", err.message);
    let _ = fx.env.cleanup();
}

#[test]
fn r11_sender_claim_ids_render_as_safe_identifiers() {
    let fx = build_fixture("r11l");
    let home = isolated_home(&fx.env);
    let good = rebuild_with_receipt(&fx.context_pkg, |r| {
        r["claims"] = serde_json::json!([
            {"kind": "verification", "origin": "exporter_reported",
             "check_id": "unit pass with spaces", "status": "passed",
             "snapshot_id": format!("sha256:{}", "a".repeat(64))}
        ]);
    });
    let outcome = verify_ok(&good, &fx.repo.root, &home);
    assert_eq!(outcome.receipt_claims.len(), 1);
    assert_eq!(
        outcome.receipt_claims[0].check_id, "(unprintable-id)",
        "arbitrary sender text never reaches the output"
    );
    assert_eq!(outcome.receipt_claims[0].origin, "exporter_reported");
    let _ = fx.env.cleanup();
}

#[test]
fn r11_bounded_read_input_type_and_no_path_echo() {
    let fx = build_fixture("r11m");
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_relaycheck"));
    let repo_arg = fx.repo.root.clone();
    // oversized package input: refused by metadata before any full read
    let big = fx.env.scratch.join("big.zip");
    {
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(relaycheck::verify_handoff::max_package_bytes() as u64 + 1)
            .unwrap();
    }
    let out = std::process::Command::new(&exe)
        .args(["verify-handoff", big.to_str().unwrap(), "--repo", repo_arg.to_str().unwrap()])
        .output()
        .expect("spawn");
    assert_eq!(out.status.code(), Some(3), "oversized input exits 3");
    // a directory as package input is a bad argument (2), not a crash
    let dir_input = std::process::Command::new(&exe)
        .args(["verify-handoff", fx.env.scratch.to_str().unwrap(), "--repo", repo_arg.to_str().unwrap()])
        .output()
        .expect("spawn");
    assert_eq!(dir_input.status.code(), Some(2), "non-file package input exits 2");
    // package validation errors never echo the input paths
    let mut damaged = fx.context_pkg.clone();
    let mid = damaged.len() / 2;
    damaged[mid] ^= 0xa5;
    let damaged_path = fx.env.scratch.join("damaged2.zip");
    std::fs::write(&damaged_path, &damaged).unwrap();
    let out = std::process::Command::new(&exe)
        .args([
            "verify-handoff",
            damaged_path.to_str().unwrap(),
            "--repo",
            repo_arg.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("spawn");
    assert_eq!(out.status.code(), Some(3));
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        !stdout.contains(fx.env.scratch.to_str().unwrap()),
        "no absolute path echoed: {stdout}"
    );
    assert!(!stdout.contains(damaged_path.to_str().unwrap()), "no package path echoed");
    let _ = fx.env.cleanup();
}
