//! Shared integration-test fixtures: hermetic data dirs, real git repos,
//! real python check processes. Tests run the production service code paths
//! (store + capture + checks + handoff + import), not mocks.

#![allow(dead_code)]

use relaycheck::blobstore::BlobStore;
use relaycheck::opsman::OpManager;
use relaycheck::service::Ctx;
use relaycheck::store::Store;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn test_root() -> PathBuf {
    std::env::var("RELAYCHECK_TEST_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("D:/dev/relaycheck-test-data"))
}

pub fn unique_tag(tag: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    format!("{tag}-{pid}-{nanos}")
}

pub struct TestEnv {
    pub data_dir: PathBuf,
    pub ctx: Ctx,
    pub scratch: PathBuf,
}

impl TestEnv {
    pub fn new(tag: &str) -> TestEnv {
        let scratch = test_root().join(unique_tag(tag));
        let data_dir = scratch.join("data");
        std::fs::create_dir_all(&data_dir).expect("create data dir");
        let store = Arc::new(Store::open(&data_dir).expect("store"));
        let blobs = Arc::new(BlobStore::new(&data_dir).expect("blobs"));
        let ops = Arc::new(OpManager::new());
        let tmp_root = data_dir.join("tmp");
        let isolated_home = data_dir.join("isolated-home");
        std::fs::create_dir_all(&tmp_root).unwrap();
        std::fs::create_dir_all(&isolated_home).unwrap();
        TestEnv {
            data_dir,
            ctx: Ctx { store, blobs, ops, tmp_root, isolated_home },
            scratch,
        }
    }

    pub fn make_git_repo(&self, name: &str) -> Repo {
        let root = self.scratch.join(name);
        std::fs::create_dir_all(&root).unwrap();
        let repo = Repo { root };
        repo.git(&["init", "-q"]);
        repo.git(&["config", "user.email", "test@example.com"]);
        repo.git(&["config", "user.name", "Test"]);
        // Isolation knobs that would otherwise leak the user's config.
        repo.git(&["config", "core.autocrlf", "false"]);
        repo
    }

    pub fn cleanup(self) {
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

pub struct Repo {
    pub root: PathBuf,
}

impl Repo {
    /// Runs git with fixed identity config. Panics on failure (tests).
    pub fn git(&self, args: &[&str]) -> String {
        let out = std::process::Command::new(git_exe())
            .args(args)
            .current_dir(&self.root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_CONFIG_SYSTEM", null_device())
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .expect("git spawn");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    pub fn try_git(&self, args: &[&str]) -> bool {
        std::process::Command::new(git_exe())
            .args(args)
            .current_dir(&self.root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_CONFIG_SYSTEM", null_device())
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    pub fn write(&self, rel: &str, contents: &[u8]) -> PathBuf {
        let p = self.root.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, contents).unwrap();
        p
    }

    pub fn read(&self, rel: &str) -> Vec<u8> {
        std::fs::read(self.root.join(rel)).unwrap()
    }

    pub fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"]).trim().to_string()
    }

    /// Records (head, index-dirty, worktree-dirty, status text) so tests can
    /// prove the product never rewrote repo state.
    pub fn state_fingerprint(&self) -> (String, String) {
        let head = self.head();
        let status = self.git(&["status", "--porcelain"]);
        (head, status)
    }
}

pub fn git_exe() -> PathBuf {
    let candidates = [
        "D:/dev/Git/cmd/git.exe",
        "C:/Program Files/Git/cmd/git.exe",
        "C:/Program Files/Git/bin/git.exe",
    ];
    for c in candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return p;
        }
    }
    PathBuf::from("git")
}

pub fn null_device() -> &'static str {
    if cfg!(windows) {
        "nul"
    } else {
        "/dev/null"
    }
}

/// Locates a real python.exe for check-profile processes. Skips the test
/// (not a silent pass) when no python is available.
pub fn find_python() -> PathBuf {
    let candidates = [
        "D:/dev/python/python.exe",
        "C:/Python314/python.exe",
        "C:/Python312/python.exe",
    ];
    for c in candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return p;
        }
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let full = dir.join("python.exe");
            if full.is_file() {
                return full;
            }
        }
    }
    panic!("no python.exe found for check-profile tests");
}

/// Attaches a repo and creates a task, returning (project_id, task_id).
pub fn setup_project_task(env: &TestEnv, repo: &Repo, goal: &str) -> (String, String) {
    let (project_id, _code) = relaycheck::service::attach_project(
        &env.ctx,
        &repo.root.to_string_lossy(),
        Some("test-repo"),
    )
    .expect("attach");
    let task_id = relaycheck::service::create_task(
        &env.ctx,
        relaycheck::model::TaskDraft {
            project_id: project_id.clone(),
            goal: goal.to_string(),
            constraints: vec!["keep the signature".into()],
            criteria: vec![relaycheck::model::Criterion {
                id: "SC1".into(),
                text: "retry boundary covered".into(),
                required: true,
            }],
        },
        false,
    )
    .expect("create task");
    (project_id, task_id)
}

/// Saves + trusts a python check profile for the task.
pub fn setup_python_profile(
    env: &TestEnv,
    task_id: &str,
    script: &str,
    check_id: &str,
) -> String {
    let python = find_python();
    let script_path = env.scratch.join(format!("script-{check_id}.py"));
    std::fs::write(&script_path, script).unwrap();
    let profile = relaycheck::service::save_profile(
        &env.ctx,
        task_id,
        "py-checks",
        vec![relaycheck::model::CheckStep {
            id: check_id.to_string(),
            exe: python.to_string_lossy().into_owned(),
            args: vec![script_path.to_string_lossy().into_owned()],
            cwd_relative: String::new(),
            timeout_seconds: 60,
            required: true,
        }],
        vec![],
        vec![],
    )
    .expect("save profile");
    env.ctx.store.set_profile_trusted(&profile.id, true).unwrap();
    profile.id
}

/// Captures with default scope (all tracked files).
pub fn capture_all(env: &TestEnv, task_id: &str) -> relaycheck::service::CaptureResult {
    let op_id = "test-capture-op";
    let _ = env.ctx.store.insert_operation(op_id, "capture", Some(task_id), None, "");
    let cancel = env.ctx.ops.begin(op_id, "capture").expect("begin capture");
    let result = relaycheck::service::run_capture(
        &env.ctx,
        &relaycheck::service::CaptureRequest {
            task_id: task_id.to_string(),
            untracked_selected: vec![],
            extra_excludes: vec![],
            attest_stopped: true,
        },
        op_id,
    );
    env.ctx.ops.finish(op_id);
    let _ = cancel;
    result.expect("capture")
}

/// Runs a verify round to completion.
pub fn verify_now(env: &TestEnv, task_id: &str) -> relaycheck::service::VerifyResult {
    let op_id = "test-verify-op";
    let _ = env.ctx.store.insert_operation(op_id, "verify", Some(task_id), None, "");
    let cancel = env.ctx.ops.begin(op_id, "verify").expect("begin verify");
    let result = relaycheck::service::run_verify(
        &env.ctx,
        &relaycheck::service::VerifyRequest { task_id: task_id.to_string(), trust_confirmed: true },
        &cancel,
        op_id,
        |_| {},
    );
    env.ctx.ops.finish(op_id);
    result.expect("verify")
}

/// Builds a ServerState over the test env (for detail-view / HTTP tests).
pub fn server_state(
    env: &TestEnv,
) -> std::sync::Arc<relaycheck::http::ServerState> {
    let (tx, _rx) = tokio::sync::broadcast::channel::<i64>(64);
    std::sync::Arc::new(relaycheck::http::ServerState {
        ctx: Ctx {
            store: env.ctx.store.clone(),
            blobs: env.ctx.blobs.clone(),
            ops: env.ctx.ops.clone(),
            tmp_root: env.ctx.tmp_root.clone(),
            isolated_home: env.ctx.isolated_home.clone(),
        },
        port: 0,
        cli_token: "test-cli-token-0123456789abcdef".into(),
        events_tx: tx,
        idem: std::sync::Mutex::new(std::collections::HashMap::new()),
    })
}

/// Formal handoff export with a next action.
pub fn export_handoff(
    env: &TestEnv,
    task_id: &str,
    source_mode: &str,
) -> (String, String, Vec<u8>) {
    let (handoff_id, sha, _md) = relaycheck::service::create_handoff(
        &env.ctx,
        &relaycheck::service::HandoffRequest {
            task_id: task_id.to_string(),
            source_mode: source_mode.to_string(),
            next_action: "Read the boundary test and reproduce C2".into(),
            open_items: vec![relaycheck::model::OpenItem {
                id: "C2".into(),
                text: "cancellation not checked".into(),
            }],
            notes_constraints: vec![],
            edited_fields: vec![],
        },
    )
    .expect("handoff");
    let path = env.ctx.store.handoff_package_path(&handoff_id).unwrap();
    let bytes = std::fs::read(path).unwrap();
    assert_eq!(relaycheck::ids::sha256_hex(&bytes), sha);
    (handoff_id, sha, bytes)
}
