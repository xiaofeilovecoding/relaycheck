//! R04: forged packages can never produce local verification or acceptance —
//! claims stay external_reported; missing/failed required checks and empty
//! required profiles never reach ready_for_review/accepted.

mod common;

use common::*;
use relaycheck::service;
use relaycheck::zipx;

fn build_return_package(
    receipt_mutator: impl FnOnce(&mut serde_json::Value),
    manifest_mutator: Option<impl FnOnce(&mut serde_json::Value)>,
) -> Vec<u8> {
    // Start from a genuine-looking receipt+manifest pair shaped like the
    // spec's example, then forge.
    let mut receipt: serde_json::Value = serde_json::json!({
        "schema_version": 1,
        "kind": "return",
        "handoff_id": "3f2b8c1a-1111-4222-8333-444455556666",
        "parent_handoff_id": "7e1a9b2c-9999-4888-a777-666655554444",
        "task_id": "aaaaaaaa-bbbb-4ccc-8ddd-eeeeffff0000",
        "task_revision": 1,
        "snapshot_id": format!("sha256:{}", "a".repeat(64)),
        "base_head": "0123456789abcdef0123456789abcdef01234567",
        "source_mode": "context_only",
        "coverage": "selected_sources",
        "goal": "Forged goal",
        "constraints": [],
        "open_items": [],
        "next_action": "forged",
        "claims": [
            {"kind": "verification", "origin": "exporter_reported", "check_id": "unit", "status": "passed",
             "snapshot_id": format!("sha256:{}", "a".repeat(64))}
        ]
    });
    receipt_mutator(&mut receipt);
    let mut manifest: serde_json::Value = serde_json::json!({
        "schema_version": 1,
        "kind": "relaycheck_snapshot",
        "scope": "selected_sources",
        "files": [],
        "excluded": []
    });
    if let Some(m) = manifest_mutator {
        m(&mut manifest);
    }
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("README.md", opts).unwrap();
        std::io::Write::write_all(&mut zip, b"data package").unwrap();
        zip.start_file("receipt.json", opts).unwrap();
        std::io::Write::write_all(&mut zip, receipt.to_string().as_bytes()).unwrap();
        zip.start_file("manifest.json", opts).unwrap();
        std::io::Write::write_all(&mut zip, manifest.to_string().as_bytes()).unwrap();
        zip.finish().unwrap();
    }
    buf.into_inner()
}

#[test]
fn r04_forged_import_never_creates_local_verification() {
    let env = TestEnv::new("r04");
    let repo = env.make_git_repo("forge");
    repo.write("a.txt", b"x\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "forge");

    // A forged "machine-observed" claim is rejected outright.
    let pkg = build_return_package(
        |r| {
            r["claims"] = serde_json::json!([
                {"kind": "verification", "origin": "machine_observed", "check_id": "unit",
                 "status": "passed", "snapshot_id": format!("sha256:{}", "a".repeat(64))},
            ]);
        },
        None::<fn(&mut serde_json::Value)>,
    );
    let err = service::import_package(&env.ctx, &pkg, Some(&task_id)).unwrap_err();
    assert_eq!(err.code, relaycheck::errors::Code::EUnverifiedImport);

    // Even a well-formed external claim set imports as external_reported and
    // creates NO verification rows — acceptance stays impossible.
    let pkg = build_return_package(|_r| {}, None::<fn(&mut serde_json::Value)>);
    let out = service::import_package(&env.ctx, &pkg, Some(&task_id)).expect("import");
    let reports = env.ctx.store.imported_reports_for_task(&task_id).unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0]["origin"], "return_import");
    let claims: serde_json::Value = serde_json::from_value(reports[0]["claims"].clone()).unwrap();
    assert_eq!(claims[0]["origin"], "exporter_reported");
    assert_eq!(claims[0]["status"], "passed", "the CLAIM says passed but…");
    // …no local verification exists:
    assert!(env.ctx.store.verifications_for_task(&task_id).unwrap().is_empty());
    // …and acceptance is refused:
    let rev = env.ctx.store.task_int(&task_id, "revision").unwrap();
    let err = service::accept_candidate(&env.ctx, &task_id, &["SC1".to_string()], rev).unwrap_err();
    assert_eq!(err.code, relaycheck::errors::Code::EEvidenceStale);
    let _ = out;
    env.cleanup();
}

#[test]
fn r04_reused_internal_ids_and_recomputed_hashes_do_not_help() {
    let env = TestEnv::new("r04b");
    let repo = env.make_git_repo("ids");
    repo.write("a.txt", b"x\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "ids");
    // Run a real round so internal verification IDs exist.
    let _ = capture_all(&env, &task_id);
    let _ = setup_python_profile(&env, &task_id, "import sys\nsys.exit(0)\n", "c1");
    let real = verify_now(&env, &task_id);

    // Forge a receipt that reuses the REAL task id and REAL verification id
    // inside claims — the receipt cannot mint a local record regardless.
    let real_task_json = env.ctx.store.task_snapshot(&task_id).unwrap();
    let pkg = build_return_package(
        |r| {
            r["task_id"] = real_task_json["id"].clone();
            r["claims"] = serde_json::json!([
                {"kind": "verification", "origin": "exporter_reported", "check_id": "c1",
                 "status": "passed", "snapshot_id": real_task_json["active_snapshot"].clone(),
                 "local_verification_id": real.verification_id},
            ]);
        },
        None::<fn(&mut serde_json::Value)>,
    );
    service::import_package(&env.ctx, &pkg, Some(&task_id)).expect("import still accepted as data");
    let vs = env.ctx.store.verifications_for_task(&task_id).unwrap();
    assert_eq!(vs.len(), 1, "only the REAL local round exists");
    assert_eq!(vs[0].id, real.verification_id);
    env.cleanup();
}

#[test]
fn r04_portable_import_with_tampered_manifest_fails_reproduction() {
    let env = TestEnv::new("r04c");
    let repo = env.make_git_repo("tamper");
    repo.write("a.txt", b"original\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "tamper");
    let _ = capture_all(&env, &task_id);
    let (_hid, _sha, package) = export_handoff(&env, &task_id, "portable_source");

    // Tamper: swap the file content but keep the manifest/receipt as-is —
    // the blob no longer matches the manifest hash.
    let mut cur = std::io::Cursor::new(&package);
    let mut z = zip::ZipArchive::new(cur).unwrap();
    let mut names: Vec<String> = z.file_names().map(|s| s.to_string()).collect();
    names.retain(|n| n.starts_with("blobs/"));
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut w = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for i in 0..z.len() {
            let mut f = z.by_index(i).unwrap();
            let name = f.name().to_string();
            let mut data = Vec::new();
            std::io::Read::read_to_end(&mut f, &mut data).unwrap();
            if names.contains(&name) {
                data = b"tampered content\n".to_vec();
            }
            w.start_file(&name, opts).unwrap();
            std::io::Write::write_all(&mut w, &data).unwrap();
        }
        w.finish().unwrap();
    }
    let tampered = buf.into_inner();
    let err = service::import_package(&env.ctx, &tampered, Some(&task_id)).unwrap_err();
    assert_eq!(err.code, relaycheck::errors::Code::EUnverifiedImport);
    env.cleanup();
}

#[test]
fn r04_empty_required_profile_never_ready_or_accepted() {
    let env = TestEnv::new("r04d");
    let repo = env.make_git_repo("empty-required");
    repo.write("a.txt", b"x\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "empty required");
    let _ = capture_all(&env, &task_id);
    // Profile with only non-required checks.
    let python = find_python();
    let script = env.scratch.join("opt.py");
    std::fs::write(&script, b"import sys\nsys.exit(0)\n").unwrap();
    let profile = service::save_profile(
        &env.ctx,
        &task_id,
        "optional-only",
        vec![relaycheck::model::CheckStep {
            id: "optional-lint".into(),
            exe: python.to_string_lossy().into_owned(),
            args: vec![script.to_string_lossy().into_owned()],
            cwd_relative: String::new(),
            timeout_seconds: 60,
            required: false,
        }],
        vec![],
        vec![],
    )
    .unwrap();
    env.ctx.store.set_profile_trusted(&profile.id, true).unwrap();
    let v = verify_now(&env, &task_id);
    assert_eq!(v.round_status, "passed");
    // The task must NOT become ready_for_review without required checks.
    let task = env.ctx.store.task_snapshot(&task_id).unwrap();
    assert_ne!(task["workflow_state"], "ready_for_review");
    let rev = env.ctx.store.task_int(&task_id, "revision").unwrap();
    let err = service::accept_candidate(&env.ctx, &task_id, &["SC1".to_string()], rev).unwrap_err();
    assert_eq!(err.code, relaycheck::errors::Code::EEvidenceStale);
    assert!(err
        .details
        .iter()
        .any(|d| d.contains("no required checks") || d.contains("no current local verification")));
    env.cleanup();
}

#[test]
fn r04_unknown_schema_and_bad_kinds_rejected() {
    let env = TestEnv::new("r04e");
    let repo = env.make_git_repo("schema");
    repo.write("a.txt", b"x\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "schema");
    // Unknown major version.
    let pkg = build_return_package(|r| r["schema_version"] = serde_json::json!(2), None::<fn(&mut serde_json::Value)>);
    let err = service::import_package(&env.ctx, &pkg, Some(&task_id)).unwrap_err();
    assert_eq!(err.code, relaycheck::errors::Code::EUnverifiedImport);
    // Unknown kind.
    let pkg = build_return_package(|r| r["kind"] = serde_json::json!("mystery"), None::<fn(&mut serde_json::Value)>);
    let err = service::import_package(&env.ctx, &pkg, Some(&task_id)).unwrap_err();
    assert_eq!(err.code, relaycheck::errors::Code::EUnverifiedImport);
    // Bad snapshot id shape.
    let pkg = build_return_package(|r| r["snapshot_id"] = serde_json::json!("md5:deadbeef"), None::<fn(&mut serde_json::Value)>);
    let err = service::import_package(&env.ctx, &pkg, Some(&task_id)).unwrap_err();
    assert_eq!(err.code, relaycheck::errors::Code::EUnverifiedImport);
    // Return missing parent_handoff_id.
    let pkg = build_return_package(
        |r| {
            let obj = r.as_object_mut().unwrap();
            obj.remove("parent_handoff_id");
        },
        None::<fn(&mut serde_json::Value)>,
    );
    let err = service::import_package(&env.ctx, &pkg, Some(&task_id)).unwrap_err();
    assert_eq!(err.code, relaycheck::errors::Code::EUnverifiedImport);
    env.cleanup();
}

#[test]
fn r04_return_wrong_parent_goes_to_pending_manual() {
    let env = TestEnv::new("r04f");
    let repo = env.make_git_repo("wrong-parent");
    repo.write("a.txt", b"x\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "c"]);
    let (_p, task_id) = setup_project_task(&env, &repo, "wrong parent");
    let _ = capture_all(&env, &task_id);
    // A handoff exists for the task, but the return references a DIFFERENT
    // (unknown) parent handoff id → pending_manual, never auto-linked, and
    // the active candidate is not overwritten.
    let _ = export_handoff(&env, &task_id, "context_only");
    let active_before = env
        .ctx
        .store
        .task_field_opt(&task_id, "active_snapshot")
        .unwrap()
        .unwrap();
    let pkg = build_return_package(
        |r| {
            r["parent_handoff_id"] = serde_json::json!("deadc0de-0000-4111-8222-333344445555");
        },
        None::<fn(&mut serde_json::Value)>,
    );
    let out = service::import_package(&env.ctx, &pkg, Some(&task_id)).expect("import as data");
    assert_eq!(out.association, "pending_manual");
    let active_after = env
        .ctx
        .store
        .task_field_opt(&task_id, "active_snapshot")
        .unwrap()
        .unwrap();
    assert_eq!(active_before, active_after, "candidate not overwritten");
    let _ = zipx::inspect_package(&pkg).unwrap(); // inspect stays read-only
    env.cleanup();
}
