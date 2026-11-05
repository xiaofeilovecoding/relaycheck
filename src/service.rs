//! Service orchestration shared by the HTTP API and the CLI.
//!
//! Every mutating operation here is a plain function over (Store, BlobStore,
//! OpManager); the HTTP layer wraps them with sessions/idempotency and runs
//! the long ones on worker threads.

use crate::blobstore::BlobStore;
use crate::capture;
use crate::checks;
use crate::errors::{Code, RelayError, RelayResult};
use crate::gitx;
use crate::handoff;
use crate::ids::{new_token, new_uuid, now_ms, sha256_hex};
use crate::model::*;
use crate::opsman::OpManager;
use crate::store::Store;
use crate::zipx;
use std::path::Path;
use std::sync::Arc;

#[derive(Clone)]
pub struct Ctx {
    pub store: Arc<Store>,
    pub blobs: Arc<BlobStore>,
    pub ops: Arc<OpManager>,
    pub tmp_root: std::path::PathBuf,
    pub isolated_home: std::path::PathBuf,
}

/// Locates a Python interpreter for the demo's real check. Returns None
/// honestly when none exists (the demo then ships without a check profile).
pub fn find_python_exe() -> Option<std::path::PathBuf> {
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let p = dir.join(if cfg!(windows) { "python.exe" } else { "python3" });
            if p.is_file() {
                return Some(p);
            }
        }
    }
    let candidates = ["D:/dev/python/python.exe", "C:/Python314/python.exe", "C:/Python312/python.exe"];
    for c in candidates {
        let p = std::path::PathBuf::from(c);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

pub fn git_exe_path() -> RelayResult<std::path::PathBuf> {
    // Locate git from PATH; never bundled, always the user's tool.
    let candidates = if cfg!(windows) {
        vec!["git.exe", r"C:\Program Files\Git\cmd\git.exe", r"C:\Program Files\Git\bin\git.exe"]
    } else {
        vec!["git", "/usr/bin/git", "/usr/local/bin/git"]
    };
    for c in candidates {
        let p = std::path::PathBuf::from(c);
        if p.is_absolute() && p.exists() {
            return Ok(p);
        }
        if !p.is_absolute() {
            if let Ok(path_var) = std::env::var("PATH") {
                for dir in std::env::split_paths(&path_var) {
                    let full = dir.join(c);
                    if full.is_file() {
                        return Ok(full);
                    }
                }
            }
        }
    }
    Err(RelayError::new(
        Code::EMissingDependency,
        "git was not found; RelayCheck uses your existing Git installation",
    ))
}

// ---------------------------------------------------------------------------
// Projects
// ---------------------------------------------------------------------------

pub fn attach_project(ctx: &Ctx, root: &str, display_name: Option<&str>) -> RelayResult<(String, String)> {
    let identity = gitx::identify(root, &ctx.isolated_home)?;
    if let Some((existing, _)) = ctx.store.project_by_root(&identity.root) {
        // Idempotent attach: reuse the registration.
        let code = new_token(4);
        ctx.store.insert_register_claim(&code, &existing)?;
        return Ok((existing, code));
    }
    let id = new_uuid();
    let name = display_name
        .map(|s| s.to_string())
        .unwrap_or_else(|| Path::new(&identity.root)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| "project".into()));
    ctx.store.insert_project(&id, &name, &identity.root, &identity.common_dir, &identity.head_commit)?;
    let code = new_token(4);
    ctx.store.insert_register_claim(&code, &id)?;
    let _ = ctx.store.append_event(
        "project.registered",
        &serde_json::json!({ "project_id": id, "display_name": name }),
    );
    Ok((id, code))
}

pub fn claim_project(ctx: &Ctx, code: &str) -> RelayResult<String> {
    let project_id = ctx
        .store
        .consume_register_claim(code)?
        .ok_or_else(|| RelayError::new(Code::EValidation, "registration code is unknown or already used"))?;
    let _ = ctx.store.append_event(
        "project.claimed",
        &serde_json::json!({ "project_id": project_id }),
    );
    Ok(project_id)
}

// ---------------------------------------------------------------------------
// Demo ("试用样例"): a real local fixture repo + a real local check, marked
// Demo everywhere. No network, no model, no fabricated success.
// ---------------------------------------------------------------------------

pub fn create_demo(ctx: &Ctx) -> RelayResult<String> {
    let repo_dir = ctx
        .store
        .data_dir
        .join("demo")
        .join(format!("repo-{}", new_token(6)));
    std::fs::create_dir_all(&repo_dir)?;
    let run = |args: &[&str]| -> RelayResult<()> {
        let git = git_exe_path()?;
        let out = std::process::Command::new(git)
            .args(args)
            .current_dir(&repo_dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null_dev())
            .env("GIT_CONFIG_SYSTEM", null_dev())
            .env("GIT_AUTHOR_NAME", "RelayCheck Demo")
            .env("GIT_AUTHOR_EMAIL", "demo@local")
            .env("GIT_COMMITTER_NAME", "RelayCheck Demo")
            .env("GIT_COMMITTER_EMAIL", "demo@local")
            .output()
            .map_err(|e| RelayError::internal(format!("git spawn: {e}")))?;
        if !out.status.success() {
            return Err(RelayError::internal("demo git step failed"));
        }
        Ok(())
    };
    run(&["init", "-q"])?;
    run(&["config", "user.email", "demo@local"])?;
    run(&["config", "user.name", "RelayCheck Demo"])?;
    std::fs::create_dir_all(repo_dir.join("src"))?;
    std::fs::write(
        repo_dir.join("src").join("retry.py"),
        b"def retry(fn, times=3, cancelled=None):\n    if cancelled:\n        raise TimeoutError()\n    return fn()\n",
    )?;
    std::fs::create_dir_all(repo_dir.join("tests"))?;
    // RC08: the demo check must be a REAL behavioral assertion — it calls
    // the retry function and verifies the cancellation contract. Breaking
    // the source makes it fail; it is never an `exit 0` probe.
    std::fs::write(
        repo_dir.join("tests").join("test_retry.py"),
        b"import sys\nsys.path.insert(0, 'src')\nfrom retry import retry\n\nassert retry(lambda: 1) == 1\ntry:\n    retry(lambda: 1, cancelled=True)\n    print('FAIL: cancellation did not raise')\n    sys.exit(1)\nexcept TimeoutError:\n    pass\nprint('demo retry behaviour ok')\n",
    )?;
    run(&["add", "."])?;
    run(&["commit", "-qm", "demo base"])?;

    // Register + immediately claim (demo is created by the local service).
    let (project_id, code) = attach_project(ctx, &repo_dir.to_string_lossy(), Some("demo-retry-lab"))?;
    ctx.store.consume_register_claim(&code)?;

    let task_id = create_task(
        ctx,
        TaskDraft {
            project_id: project_id.clone(),
            goal: if cfg!(windows) {
                "演示：修复重试次数的边界（Demo / simulated relay）".into()
            } else {
                "Demo: fix the retry boundary".into()
            },
            constraints: vec![
                "保持公开函数签名不变".into(),
                "不自动应用、提交或推送".into(),
            ],
            criteria: vec![Criterion { id: "SC1".into(), text: "重试边界被测试覆盖".into(), required: true }],
        },
        true,
    )?;

    // RC08: the demo check runs the REAL behavioral test above with the
    // machine's Python. When no Python exists the demo honestly ships
    // WITHOUT a profile (the UI says so) — a smoke-only `exit 0` check is
    // never presented as retry verification.
    if let Some(python) = find_python_exe() {
        let demo_rev = ctx.store.task_int(&task_id, "revision")?;
        let test_script = repo_dir.join("tests").join("test_retry.py");
        let profile = save_profile(
            ctx,
            &task_id,
            "demo-checks",
            vec![CheckStep {
                id: "demo-retry-test".into(),
                exe: python.to_string_lossy().into_owned(),
                // -B: no bytecode cache writes — the check stays read-only
                // on the snapshot copy (a real run invalidated once through
                // __pycache__; see the RC08 gate evidence).
                args: vec!["-B".into(), test_script.to_string_lossy().into_owned()],
                cwd_relative: String::new(),
                timeout_seconds: 60,
                required: true,
            }],
            vec![],
            vec![],
            demo_rev,
        )?;
        ctx.store.set_profile_trusted(&profile.id, true)?;
        let _ = ctx.store.append_event(
            "demo.check_profile",
            &serde_json::json!({"kind": "real-assertion", "check": "demo-retry-test"}),
        );
    } else {
        let _ = ctx.store.append_event(
            "demo.check_profile",
            &serde_json::json!({"kind": "none", "reason": "no python found; demo ships without a check profile"}),
        );
    }
    // Demo attestation: the service created this empty repo itself; no
    // external tool could be writing it (documented in the UI copy).
    let cap_rev = ctx.store.task_int(&task_id, "revision")?;
    run_capture(
        ctx,
        &CaptureRequest {
            task_id,
            untracked_selected: vec![],
            extra_excludes: vec![],
            attest_stopped: true,
            expected_revision: cap_rev,
        },
        "demo-capture",
    )?;
    let _ = ctx.store.append_event("demo.created", &serde_json::json!({}));
    Ok(ctx.store.list_tasks()?.first().and_then(|t| t.get("id").and_then(|v| v.as_str())).map(|s| s.to_string()).unwrap_or_default())
}

fn null_dev() -> &'static str {
    if cfg!(windows) { "nul" } else { "/dev/null" }
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

pub fn create_task(ctx: &Ctx, draft: TaskDraft, is_demo: bool) -> RelayResult<String> {
    if draft.goal.trim().is_empty() {
        return Err(RelayError::validation("goal must not be empty"));
    }
    if draft.goal.len() > 4000 {
        return Err(RelayError::validation("goal must be at most 4000 characters"));
    }
    for c in &draft.constraints {
        if c.len() > 1000 {
            return Err(RelayError::validation("each constraint must be at most 1000 characters"));
        }
    }
    for c in &draft.criteria {
        if c.id.is_empty() || c.text.is_empty() {
            return Err(RelayError::validation("criteria need id and text"));
        }
    }
    // Project must be claimed (browser can only see claimed projects).
    ctx.store.project_root(&draft.project_id)?;
    let id = new_uuid();
    ctx.store.insert_task(&id, &draft.project_id, draft.goal.trim(), &draft.constraints, &draft.criteria, is_demo)?;
    let _ = ctx.store.append_event(
        "task.created",
        &serde_json::json!({ "task_id": id, "goal_len": draft.goal.len() }),
    );
    Ok(id)
}

pub fn update_task(
    ctx: &Ctx,
    task_id: &str,
    if_match: i64,
    goal: Option<String>,
    constraints: Option<Vec<String>>,
    criteria: Option<Vec<Criterion>>,
) -> RelayResult<i64> {
    let rev = ctx.store.task_int(task_id, "revision")?;
    if if_match != rev {
        return Err(RelayError::new(
            Code::ERevisionConflict,
            format!("If-Match revision {if_match} does not match current task revision {rev}"),
        ));
    }
    let mut sets: Vec<(&str, serde_json::Value)> = Vec::new();
    if let Some(g) = &goal {
        if g.trim().is_empty() || g.len() > 4000 {
            return Err(RelayError::validation("goal must be 1..=4000 characters"));
        }
        sets.push(("goal", serde_json::Value::String(g.trim().to_string())));
    }
    if let Some(cs) = &constraints {
        sets.push(("constraints_json", serde_json::to_value(cs)?));
    }
    if let Some(cr) = &criteria {
        sets.push(("criteria_json", serde_json::to_value(cr)?));
    }
    if sets.is_empty() {
        return Ok(rev);
    }
    ctx.store.update_task_fields(task_id, &sets)?;
    // Any edit of goal/constraints/criteria bumps the revision: old results
    // can never gate a new acceptance (spec §3).
    let new_rev = ctx.store.bump_task_revision(task_id)?;
    let _ = ctx.store.append_event(
        "task.updated",
        &serde_json::json!({ "task_id": task_id, "revision": new_rev }),
    );
    Ok(new_rev)
}

// ---------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------

pub struct CaptureRequest {
    pub task_id: String,
    pub untracked_selected: Vec<String>,
    pub extra_excludes: Vec<String>,
    pub attest_stopped: bool,
    /// RC01: capture may only activate a candidate from the revision the
    /// caller saw (CAS is checked at request time; re-checked here so the
    /// worker cannot race a profile/revision bump between request and run).
    pub expected_revision: i64,
}

#[derive(Debug)]
pub struct CaptureResult {
    pub snapshot_id: String,
    pub file_count: usize,
    pub total_size: i64,
    pub excluded_count: usize,
    pub is_return: bool,
}

/// Prepares the capture plan (scope preview for the UI).
pub fn plan_capture(ctx: &Ctx, task_id: &str, untracked_selected: &[String], extra_excludes: &[String]) -> RelayResult<capture::CapturePlan> {
    let project_id = ctx.store.task_field(task_id, "project_id")?;
    let root = ctx.store.project_root(&project_id)?;
    capture::plan_capture(&root, &ctx.isolated_home, untracked_selected, extra_excludes)
}

/// Runs the capture synchronously (caller holds the write slot).
pub fn run_capture(ctx: &Ctx, req: &CaptureRequest, op_id: &str) -> RelayResult<CaptureResult> {
    let task_id = &req.task_id;
    let rev = ctx.store.task_int(task_id, "revision")?;
    if rev != req.expected_revision {
        return Err(RelayError::new(
            Code::ERevisionConflict,
            format!(
                "capture was approved at revision {}, but the task is at revision {}",
                req.expected_revision, rev
            ),
        ));
    }
    if !req.attest_stopped {
        return Err(RelayError::validation(
            "confirm the original tool has stopped writing before capturing (user attestation required)",
        ));
    }
    let project_id = ctx.store.task_field(task_id, "project_id")?;
    let root = ctx.store.project_root(&project_id)?;

    // Scope plan.
    let plan = capture::plan_capture(&root, &ctx.isolated_home, &req.untracked_selected, &req.extra_excludes)?;
    if plan.included.is_empty() {
        return Err(RelayError::validation("capture selection is empty; select at least one file"));
    }

    // Secret canary: a hit blocks the capture and lists offenders.
    let offenders = capture::scan_plan_secrets(Path::new(&root), &plan)?;
    if !offenders.is_empty() {
        return Err(RelayError::new(
            Code::ESecretDetected,
            "capture blocked: high-confidence secret material found in selected files",
        )
        .with_details(offenders));
    }

    let parent = ctx.store.task_field_opt(task_id, "active_snapshot")?;
    let parent_opt = parent.as_deref();
    let is_return = ctx
        .store
        .task_field_opt(task_id, "parent_handoff")?
        .map(|v| !v.is_empty())
        .unwrap_or(false);

    let outcome = capture::capture_from_repo(
        &root,
        &plan,
        &ctx.blobs,
        parent_opt,
        "selected_sources",
    )?;
    // Dirty = the captured bytes differ from the index (HEAD's view) for any
    // tracked file, or untracked files were selected. Computed WITHOUT ever
    // running `git status` (clean-filter execution hazard, see gitx.rs).
    let captured: std::collections::HashMap<String, String> = ctx
        .store
        .snapshot_files(&outcome.snapshot_id)
        .map(|fs| fs.into_iter().map(|f| (f.path, f.sha256)).collect())
        .unwrap_or_default();
    let dirty = !req.untracked_selected.is_empty()
        || plan.included.iter().any(|p| {
            plan.index_shas
                .get(p)
                .map(|idx_sha| captured.get(p).map(|c| c != idx_sha).unwrap_or(true))
                .unwrap_or(false)
        });
    if ctx.store.snapshot_exists(&outcome.snapshot_id) {
        // Identical content: reuse the immutable snapshot.
    } else {
        let manifest: SnapshotManifest = serde_json::from_str(&outcome.manifest_json)?;
        let manifest_sha = sha256_hex(outcome.manifest_json.as_bytes());
        ctx.store.insert_snapshot(
            &outcome.snapshot_id,
            &project_id,
            &plan.base_head,
            parent_opt,
            dirty,
            &manifest,
            &outcome.manifest_json,
            &manifest_sha,
        )?;
    }

    // Activate the new candidate: candidate_epoch bumps so no stale
    // verification can gate this snapshot's acceptance.
    let prev_state = ctx.store.task_field(task_id, "workflow_state")?;
    let had_accepted = prev_state == WORKFLOW_ACCEPTED;
    let new_state = if had_accepted { WORKFLOW_CANDIDATE } else { WORKFLOW_CANDIDATE };
    ctx.store.update_task_fields(
        task_id,
        &[
            ("active_snapshot", serde_json::Value::String(outcome.snapshot_id.clone())),
            ("workflow_state", serde_json::Value::String(new_state.into())),
        ],
    )?;
    ctx.store.bump_candidate_epoch(task_id)?;
    let _ = ctx.store.append_event(
        "task.captured",
        &serde_json::json!({
            "task_id": task_id,
            "snapshot_id": outcome.snapshot_id,
            "files": outcome.file_count,
            "is_return": is_return,
        }),
    );
    let _ = op_id;
    Ok(CaptureResult {
        snapshot_id: outcome.snapshot_id,
        file_count: outcome.file_count,
        total_size: outcome.total_size,
        excluded_count: plan.excluded.len(),
        is_return,
    })
}

// ---------------------------------------------------------------------------
// Profiles + verification rounds
// ---------------------------------------------------------------------------

pub fn profile_hash_of(p: &CheckProfile) -> RelayResult<String> {
    let v = serde_json::json!({
        "name": p.name,
        "checks": p.checks,
        "prep": p.prep,
        "outputs": p.outputs,
    });
    Ok(crate::canonical::canonical_sha256(&v)?)
}

pub fn save_profile(
    ctx: &Ctx,
    task_id: &str,
    name: &str,
    checks: Vec<CheckStep>,
    prep: Vec<PrepStep>,
    outputs: Vec<String>,
    if_match: i64,
) -> RelayResult<CheckProfile> {
    // RC01: saving a profile is a task mutation — CAS on the revision the
    // user actually saw, so two tabs cannot silently replace each other's
    // profile (the loser gets E_REVISION_CONFLICT and re-reads).
    let rev = ctx.store.task_int(task_id, "revision")?;
    if rev != if_match {
        return Err(RelayError::new(
            Code::ERevisionConflict,
            format!("If-Match revision {if_match} does not match current task revision {rev}"),
        ));
    }
    // Validate against the current snapshot inputs when one exists.
    let active = ctx
        .store
        .task_field_opt(task_id, "active_snapshot")?
        .unwrap_or_default();
    let input_paths: Vec<String> = if active.is_empty() {
        Vec::new()
    } else {
        ctx.store.snapshot_files(&active)?.into_iter().map(|f| f.path).collect()
    };
    let candidate = CheckProfile {
        id: new_uuid(),
        name: name.to_string(),
        checks,
        prep,
        outputs,
        revision: 1,
        hash: String::new(),
    };
    checks::validate_profile(&candidate, &input_paths)?;
    let hash = profile_hash_of(&candidate)?;
    let profile = CheckProfile { hash, ..candidate };
    let project_id = ctx.store.task_field(task_id, "project_id")?;
    ctx.store.insert_profile(&profile, &project_id)?;
    ctx.store.update_task_fields(task_id, &[("profile_id", serde_json::Value::String(profile.id.clone()))])?;
    // Profile edits bump the task revision: old verifications cannot be
    // reused for a different profile (spec R03).
    let rev = ctx.store.bump_task_revision(task_id)?;
    let _ = ctx.store.append_event(
        "task.profile_saved",
        &serde_json::json!({ "task_id": task_id, "profile_id": profile.id, "revision": rev }),
    );
    Ok(profile)
}

pub struct VerifyRequest {
    pub task_id: String,
    pub trust_confirmed: bool,
    /// RC01: the profile the user actually reviewed and approved. The run is
    /// refused unless these match the task's CURRENT profile identity.
    pub profile_id: String,
    pub profile_hash: String,
    pub if_match: i64,
}

#[derive(Debug)]
pub struct VerifyResult {
    pub verification_id: String,
    pub round_status: String,
    pub results: Vec<CheckResult>,
}

/// Runs the check round synchronously. The caller must hold the write slot.
pub fn run_verify(ctx: &Ctx, req: &VerifyRequest, cancel: &checks::CancelFlag, op_id: &str, mut notify: impl FnMut(&str)) -> RelayResult<VerifyResult> {
    let task_id = &req.task_id;
    if !req.trust_confirmed {
        return Err(RelayError::new(
            Code::EProfileUntrusted,
            "checks run your local code; review the repository and command first, then approve",
        ));
    }
    // RC01: the approval binds to the exact profile identity the user
    // previewed and to the task revision they saw. Nothing is marked trusted
    // until every check passes; stale or mismatched approvals are refused
    // outright so an unreviewed profile can never execute.
    let rev = ctx.store.task_int(task_id, "revision")?;
    if rev != req.if_match {
        return Err(RelayError::new(
            Code::ERevisionConflict,
            format!(
                "approval was given for revision {}, but the task is at revision {}",
                req.if_match, rev
            ),
        ));
    }
    let current_profile_id = ctx
        .store
        .task_field_opt(task_id, "profile_id")?
        .unwrap_or_default();
    if current_profile_id.is_empty() {
        return Err(RelayError::state("save and approve a check profile first"));
    }
    if req.profile_id != current_profile_id {
        return Err(RelayError::new(
            Code::EProfileChanged,
            "approval references a different profile than the task's current one",
        )
        .with_details(vec![format!("approved_profile={}", req.profile_id)]));
    }
    let profile = ctx.store.profile_json(&current_profile_id)?;
    if profile.hash != req.profile_hash {
        return Err(RelayError::new(
            Code::EProfileChanged,
            "approved profile hash does not match the current profile content",
        )
        .with_details(vec![format!("approved_hash={}", &req.profile_hash[..12.min(req.profile_hash.len())])]));
    }
    let active = ctx
        .store
        .task_field_opt(task_id, "active_snapshot")?
        .unwrap_or_default();
    if active.is_empty() {
        return Err(RelayError::state("capture a candidate before running checks"));
    }
    // The explicit, identity-bound confirmation IS the approval act
    // (spec §5: 保存并明确批准才可执行).
    ctx.store.set_profile_trusted(&current_profile_id, true)?;
    let files: Vec<(String, String)> = ctx
        .store
        .snapshot_files(&active)?
        .into_iter()
        .map(|f| (f.path, f.sha256))
        .collect();
    // Blob integrity gate: corrupt store bars checks (spec §4).
    for (_, sha) in &files {
        if let Err(e) = ctx.blobs.verify(sha) {
            return Err(RelayError::new(
                Code::ECorrupt,
                "snapshot blobs are incomplete; checks and acceptance are disabled for this candidate",
            )
            .with_details(vec![format!("blob={} ({})", &sha[..12.min(sha.len())], e.message)]));
        }
    }
    let rev = ctx.store.task_int(task_id, "revision")?;
    let epoch = ctx.store.environment_epoch();
    let cepoch = ctx.store.task_int(task_id, "candidate_epoch")?;
    ctx.store.update_task_fields(task_id, &[("workflow_state", serde_json::Value::String(WORKFLOW_CHECKING.into()))])?;
    let _ = ctx.store.append_event("task.checking", &serde_json::json!({ "task_id": task_id }));

    let round = checks::run_round(
        task_id, &active, &profile, &current_profile_id, rev, epoch, cepoch, &files, &ctx.blobs,
        &ctx.tmp_root, cancel, notify,
    );
    let round = match round {
        Ok(r) => r,
        Err(e) => {
            ctx.store.update_task_fields(task_id, &[("workflow_state", serde_json::Value::String(WORKFLOW_CANDIDATE.into()))])?;
            ctx.ops.finish(op_id);
            return Err(e);
        }
    };
    ctx.store.insert_verification(&round.record, &current_profile_id)?;
    let new_state = match round.record.round_status.as_str() {
        "passed" => {
            let has_required = profile.checks.iter().any(|c| c.required);
            if has_required { WORKFLOW_READY_FOR_REVIEW } else { WORKFLOW_CANDIDATE }
        }
        "failed" | "blocked" | "invalidated" => WORKFLOW_REWORK,
        _ => WORKFLOW_CANDIDATE,
    };
    ctx.store.update_task_fields(
        task_id,
        &[("workflow_state", serde_json::Value::String(new_state.into()))],
    )?;
    let _ = ctx.store.append_event(
        "task.checked",
        &serde_json::json!({ "task_id": task_id, "round_status": round.record.round_status }),
    );
    ctx.ops.finish(op_id);
    Ok(VerifyResult {
        verification_id: round.record.id.clone(),
        round_status: round.record.round_status.clone(),
        results: round.record.results.clone(),
    })
}

// ---------------------------------------------------------------------------
// Handoff export
// ---------------------------------------------------------------------------

pub struct HandoffRequest {
    pub task_id: String,
    pub source_mode: String, // context_only | portable_source
    pub next_action: String,
    pub open_items: Vec<OpenItem>,
    pub notes_constraints: Vec<String>,
    pub edited_fields: Vec<String>,
}

/// Creates the immutable handoff record + package. Requires next_action
/// (drafts live in the UI, never as formal packages — spec §1.2).
pub fn create_handoff(ctx: &Ctx, req: &HandoffRequest) -> RelayResult<(String, String, String)> {
    if req.source_mode != "context_only" && req.source_mode != "portable_source" {
        return Err(RelayError::validation("source_mode must be context_only or portable_source"));
    }
    if req.next_action.trim().is_empty() {
        return Err(RelayError::validation(
            "next_action is required to export a formal handoff; save a draft in the UI instead",
        ));
    }
    let active = ctx
        .store
        .task_field_opt(&req.task_id, "active_snapshot")?
        .unwrap_or_default();
    if active.is_empty() {
        return Err(RelayError::state("capture a candidate before preparing a handoff"));
    }
    let rev = ctx.store.task_int(&req.task_id, "revision")?;
    let snap = ctx.store.snapshot_json(&active)?;
    let manifest: SnapshotManifest = serde_json::from_value(snap.get("manifest").cloned().unwrap_or_default())?;

    let goal = ctx.store.task_field(&req.task_id, "goal")?;
    let constraints_json = ctx.store.task_field(&req.task_id, "constraints_json")?;
    let mut constraints: Vec<String> = serde_json::from_str(&constraints_json)?;
    constraints.extend(req.notes_constraints.iter().cloned());

    // local_verified_summary: valid local verifications for THIS snapshot.
    let verified = ctx
        .store
        .verifications_for_task(&req.task_id)?
        .into_iter()
        .filter(|v| v.snapshot_id == active && v.round_status == "passed")
        .map(|v| {
            let ids: Vec<String> = v.results.iter().filter(|r| r.status == "passed").map(|r| r.check_id.clone()).collect();
            format!(
                "{} on {} (local, revision {}, epoch {})",
                ids.join(", "),
                crate::ids::short_hash(&active),
                v.task_revision,
                v.environment_epoch
            )
        })
        .collect();

    let changed: Vec<String> = Vec::new(); // filled for returns via diff page; A-side export has no parent yet

    let body = HandoffBody {
        goal: goal.clone(),
        constraints: constraints.clone(),
        changed,
        verified,
        open_items: req.open_items.clone(),
        next_action: req.next_action.trim().to_string(),
        evidence: vec![
            "receipt.json".into(),
            "manifest.json".into(),
            if req.source_mode == "portable_source" { "blobs/<sha256> (source files)".into() } else { "(context-only: no source files included)".into() },
        ],
    };
    handoff::scan_body_secrets(&body)?;

    let handoff_id = new_uuid();
    let meta = HandoffMeta {
        handoff_id: handoff_id.clone(),
        task_id: req.task_id.clone(),
        task_revision: rev,
        candidate_snapshot: active.clone(),
        parent_handoff: None,
        source_mode: req.source_mode.clone(),
        body: body.clone(),
        edited_fields: req.edited_fields.clone(),
        created_at_ms: now_ms(),
    };
    let md = handoff::render_handoff_md(&body, &meta)?;

    let claims: Vec<Claim> = ctx
        .store
        .verifications_for_task(&req.task_id)?
        .into_iter()
        .filter(|v| v.snapshot_id == active)
        .flat_map(|v| {
            let ids: Vec<String> = v.results.iter().filter(|r| r.status == "passed").map(|r| r.check_id.clone()).collect();
            ids.into_iter().map(|cid| zipx::claim(&cid, "passed", &active))
        })
        .collect();

    let receipt = Receipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        kind: "handoff".into(),
        handoff_id: handoff_id.clone(),
        parent_handoff_id: None,
        task_id: req.task_id.clone(),
        task_revision: rev,
        snapshot_id: active.clone(),
        base_head: snap.get("base_head").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        source_mode: req.source_mode.clone(),
        coverage: "selected_sources".into(),
        goal: goal.clone(),
        constraints,
        open_items: req.open_items.clone(),
        next_action: req.next_action.trim().to_string(),
        claims,
    };

    let package = zipx::build_package(&zipx::ExportInput {
        receipt: &receipt,
        handoff_md: &md,
        manifest: &manifest,
        blobs: if req.source_mode == "portable_source" { Some(&ctx.blobs) } else { None },
    })?;
    let package_sha = sha256_hex(&package);
    let rel_dir = ctx.store.data_dir.join("packages");
    std::fs::create_dir_all(&rel_dir)?;
    let package_path = rel_dir.join(format!("{handoff_id}.zip"));
    std::fs::write(&package_path, &package)?;

    ctx.store.insert_handoff(&meta, &receipt, &package_sha, &package_path.to_string_lossy().as_ref())?;
    // Task now awaits a return through this handoff.
    ctx.store.update_task_fields(
        &req.task_id,
        &[("parent_handoff", serde_json::Value::String(handoff_id.clone()))],
    )?;
    let _ = ctx.store.append_event(
        "task.handoff_exported",
        &serde_json::json!({ "task_id": req.task_id, "handoff_id": handoff_id, "source_mode": req.source_mode }),
    );
    Ok((handoff_id, package_sha, md))
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ImportOutcome {
    pub imported_report_id: String,
    pub association: String, // linked | pending_manual
    pub snapshot_id: Option<String>,
    pub receipt: Receipt,
}

/// Imports a package (return B or handoff). Everything lands in quarantine;
/// claims are stored as external_reported and never create local
/// verification or acceptance records.
pub fn import_package(ctx: &Ctx, data: &[u8], task_id: Option<&str>) -> RelayResult<ImportOutcome> {
    let pack = zipx::unpack_package(data, &ctx.tmp_root)?;

    // Source snapshot creation (portable_source only): build the manifest via
    // the same capture pipeline over the quarantine directory.
    let mut snapshot_id = None;
    if pack.receipt.source_mode == "portable_source" {
        // Extract blobs into the blob store first.
        for (sha, rel) in &pack.source_blobs {
            let p = pack.quarantine_dir.join(rel);
            let data = std::fs::read(&p)?;
            ctx.blobs.put_bytes(&data, sha)?;
        }
        let files: Vec<ManifestFile> = pack.manifest.files.clone();
        let plan = capture::CapturePlan {
            included: files.iter().map(|f| f.path.clone()).collect(),
            excluded: pack.manifest.excluded.clone(),
            base_head: pack.receipt.base_head.clone(),
            dirty: false,
            index_shas: Default::default(),
        };
        // Recompute the manifest canonically; it must equal the packaged one.
        let outcome = capture::capture_from_paths(&pack.quarantine_dir, &plan, &ctx.blobs, None, "selected_sources")?;
        // The recomputed manifest must hash-match the receipt's snapshot —
        // otherwise the package content is not what it claims.
        if outcome.snapshot_id != pack.receipt.snapshot_id {
            return Err(RelayError::new(
                Code::EUnverifiedImport,
                "imported files do not reproduce the receipt's snapshot hash",
            ));
        }
        if !ctx.store.snapshot_exists(&outcome.snapshot_id) {
            let manifest: SnapshotManifest = serde_json::from_str(&outcome.manifest_json)?;
            let manifest_sha = sha256_hex(outcome.manifest_json.as_bytes());
            if let Some(pid) = task_id {
                let project_id = ctx.store.task_field(pid, "project_id")?;
                ctx.store.insert_snapshot(
                    &outcome.snapshot_id, &project_id, &pack.receipt.base_head, None, false,
                    &manifest, &outcome.manifest_json, &manifest_sha,
                )?;
            }
        }
        snapshot_id = Some(outcome.snapshot_id);
    }

    // Association: a return must reference a handoff we actually exported.
    let mut association = "pending_manual".to_string();
    let mut linked_task = task_id.map(|s| s.to_string());
    if pack.receipt.kind == "return" {
        let Some(parent) = &pack.receipt.parent_handoff_id else {
            return Err(RelayError::new(
                Code::EUnverifiedImport,
                "return package is missing parent_handoff_id",
            ));
        };
        // The parent handoff must exist and be a formal (non-draft) export.
        match ctx.store.handoff_json(parent) {
            Ok(h) => {
                // An explicit task binding from the caller wins; otherwise
                // the parent handoff's task is the link target.
                if linked_task.is_none() {
                    linked_task = h.get("task_id").and_then(|v| v.as_str()).map(|s| s.to_string());
                }
                // Target task revision must match what we exported (spec §6).
                if let Some(tid) = &linked_task {
                    let cur_rev = ctx.store.task_int(tid, "revision")?;
                    if pack.receipt.task_revision > cur_rev {
                        association = "pending_manual".to_string();
                    } else {
                        association = "linked".to_string();
                    }
                }
            }
            Err(_) => {
                // Unknown parent: association stays pending_manual, but a
                // caller-specified task binding is preserved so the report
                // is visible (read-only) inside that task.
                association = "pending_manual".to_string();
            }
        }
    }

    let report_id = new_uuid();
    ctx.store.insert_imported_report(
        &report_id,
        linked_task.as_deref(),
        pack.receipt.parent_handoff_id.as_deref(),
        &serde_json::to_string(&pack.receipt)?,
        &serde_json::to_string(&pack.receipt.claims)?,
        if pack.receipt.kind == "return" { "return_import" } else { "handoff_import" },
        &association,
        "",
    )?;
    // Quarantine cleanup happens after the snapshot pipeline copied blobs.
    std::fs::remove_dir_all(&pack.quarantine_dir).ok();
    let _ = ctx.store.append_event(
        "task.imported",
        &serde_json::json!({
            "imported_report_id": report_id,
            "association": association,
            "kind": pack.receipt.kind,
            "source_mode": pack.receipt.source_mode,
        }),
    );
    Ok(ImportOutcome {
        imported_report_id: report_id,
        association,
        snapshot_id,
        receipt: pack.receipt,
    })
}

/// Confirms an imported snapshot as the new active candidate (the UI's
/// "use as new candidate" CAS step, spec §6). Refuses to overwrite silently.
pub fn confirm_candidate(ctx: &Ctx, task_id: &str, snapshot_id: &str, if_match: i64) -> RelayResult<()> {
    let rev = ctx.store.task_int(task_id, "revision")?;
    if rev != if_match {
        return Err(RelayError::new(Code::ERevisionConflict, "stale revision; re-read the task"));
    }
    if !ctx.store.snapshot_exists(snapshot_id) {
        return Err(RelayError::not_found("snapshot not found"));
    }
    ctx.store.update_task_fields(
        task_id,
        &[
            ("active_snapshot", serde_json::Value::String(snapshot_id.to_string())),
            ("workflow_state", serde_json::Value::String(WORKFLOW_CANDIDATE.into())),
        ],
    )?;
    ctx.store.bump_candidate_epoch(task_id)?;
    let _ = ctx.store.append_event(
        "task.candidate_confirmed",
        &serde_json::json!({ "task_id": task_id, "snapshot_id": snapshot_id }),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Diff
// ---------------------------------------------------------------------------

pub struct DiffFile {
    pub path: String,
    pub kind: String,
    pub plus: usize,
    pub minus: usize,
    pub binary: bool,
    pub old_size: Option<i64>,
    pub new_size: Option<i64>,
    pub unified: Vec<String>,
    pub truncated: bool,
}

/// A→B change list. The baseline MUST be the parent handoff's snapshot when
/// one is linked; otherwise the explicit `from` snapshot. Missing baseline →
/// read-only error, never an automatic substitute (spec §1.2, R20).
pub fn diff_snapshots(ctx: &Ctx, from: &str, to: &str, fill_details: bool) -> RelayResult<Vec<DiffFile>> {
    let files_a = ctx.store.snapshot_files(from)?;
    let files_b = ctx.store.snapshot_files(to)?;
    let mut entries = crate::diffx::compare(&files_a, &files_b);
    let mut out = Vec::new();
    for e in entries.iter_mut() {
        if fill_details && e.kind == crate::diffx::ChangeKind::Modified {
            let old_sha = e.old_sha.clone().unwrap_or_default();
            let new_sha = e.new_sha.clone().unwrap_or_default();
            if ctx.blobs.exists(&old_sha) && ctx.blobs.exists(&new_sha) {
                let old = ctx.blobs.get(&old_sha)?;
                let new = ctx.blobs.get(&new_sha)?;
                crate::diffx::fill_text_detail(e, &old, &new);
            }
        }
        let binary = e.plus == 0 && e.minus == 0 && e.unified.is_empty() && e.kind == crate::diffx::ChangeKind::Modified;
        out.push(DiffFile {
            path: e.path.clone(),
            kind: match e.kind {
                crate::diffx::ChangeKind::Added => "added".into(),
                crate::diffx::ChangeKind::Modified => "modified".into(),
                crate::diffx::ChangeKind::Deleted => "deleted".into(),
            },
            plus: e.plus,
            minus: e.minus,
            binary,
            old_size: e.old_size,
            new_size: e.new_size,
            unified: e.unified.clone(),
            truncated: e.truncated,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Acceptance
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct AcceptCheck {
    pub ok: bool,
    pub missing: Vec<String>,
    pub profile_hash: String,
    pub revision: i64,
    pub snapshot_id: String,
}

/// Computes acceptance eligibility (UI shows the concrete missing items).
pub fn acceptance_check(ctx: &Ctx, task_id: &str) -> RelayResult<AcceptCheck> {
    let mut missing = Vec::new();
    let active = ctx
        .store
        .task_field_opt(task_id, "active_snapshot")?
        .unwrap_or_default();
    let rev = ctx.store.task_int(task_id, "revision")?;
    let epoch = ctx.store.environment_epoch();
    if active.is_empty() {
        missing.push("no active candidate".into());
        return Ok(AcceptCheck { ok: false, missing, profile_hash: String::new(), revision: rev, snapshot_id: active });
    }
    let profile_id = ctx
        .store
        .task_field_opt(task_id, "profile_id")?
        .unwrap_or_default();
    if profile_id.is_empty() {
        missing.push("no check profile saved".into());
        return Ok(AcceptCheck { ok: false, missing, profile_hash: String::new(), revision: rev, snapshot_id: active });
    }
    let profile = ctx.store.profile_json(&profile_id)?;
    if !profile.checks.iter().any(|c| c.required) {
        missing.push("profile defines no required checks; acceptance is disabled by contract".into());
    }
    let Some(v) = ctx.store.latest_valid_verification(task_id, &active)? else {
        missing.push("no current local verification for this candidate (checks must pass on this exact version)".into());
        return Ok(AcceptCheck { ok: false, missing, profile_hash: profile.hash, revision: rev, snapshot_id: active });
    };
    // Every required check must be passed in that round.
    for step in profile.checks.iter().filter(|c| c.required) {
        let r = v.results.iter().find(|r| r.check_id == step.id);
        match r.map(|r| r.status.as_str()) {
            Some("passed") => {}
            other => missing.push(format!(
                "required check '{}' is {:?}",
                step.id,
                other.unwrap_or("not run")
            )),
        }
    }
    // Corrupt blobs bar acceptance (content-verified, not just present).
    for f in ctx.store.snapshot_files(&active)? {
        if ctx.blobs.verify(&f.sha256).is_err() {
            missing.push("snapshot blobs incomplete or corrupted".into());
            break;
        }
    }
    Ok(AcceptCheck { ok: missing.is_empty(), missing, profile_hash: profile.hash, revision: rev, snapshot_id: active })
}

pub fn accept_candidate(ctx: &Ctx, task_id: &str, criteria_ack: &[String], if_match: i64) -> RelayResult<String> {
    let rev = ctx.store.task_int(task_id, "revision")?;
    if rev != if_match {
        return Err(RelayError::new(Code::ERevisionConflict, "stale revision; re-read the task"));
    }
    // Required criteria must be acknowledged by the user.
    let criteria_json = ctx.store.task_field(task_id, "criteria_json")?;
    let criteria: Vec<Criterion> = serde_json::from_str(&criteria_json)?;
    for c in criteria.iter().filter(|c| c.required) {
        if !criteria_ack.contains(&c.id) {
            return Err(RelayError::validation(format!(
                "required success condition '{}' must be acknowledged before acceptance",
                c.id
            )));
        }
    }
    let check = acceptance_check(ctx, task_id)?;
    if !check.ok {
        return Err(RelayError::new(
            Code::EEvidenceStale,
            "acceptance is blocked until every required check passes on the current candidate",
        )
        .with_details(check.missing.clone()));
    }
    if ctx.ops.any_active() {
        return Err(RelayError::new(Code::ELimit, "wait for the active operation to finish before accepting"));
    }
    let epoch = ctx.store.environment_epoch();
    if ctx.store.acceptance_exists(task_id, check.revision, &check.snapshot_id, &check.profile_hash, epoch)? {
        return Err(RelayError::state("this candidate is already accepted for the current revision"));
    }
    let id = new_uuid();
    let evidence: Vec<String> = ctx
        .store
        .verifications_for_task(task_id)?
        .into_iter()
        .filter(|v| v.snapshot_id == check.snapshot_id && v.round_status == "passed")
        .map(|v| v.id)
        .collect();
    let ok = ctx.store.insert_acceptance(
        &id,
        task_id,
        check.revision,
        &check.snapshot_id,
        &check.profile_hash,
        epoch,
        &serde_json::to_string(&evidence)?,
    )?;
    if !ok {
        return Err(RelayError::new(
            Code::ERevisionConflict,
            "the task changed while accepting; re-read and retry",
        ));
    }
    ctx.store.update_task_fields(task_id, &[("parent_handoff", serde_json::Value::Null)])?;
    let _ = ctx.store.append_event(
        "task.accepted",
        &serde_json::json!({ "task_id": task_id, "snapshot_id": check.snapshot_id, "revision": check.revision }),
    );
    Ok(id)
}
