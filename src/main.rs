//! RelayCheck entry point: `serve` (local web workspace) and the CLI verbs
//! (`attach`, `capture`, `verify`, `tasks`) that talk to the running service
//! over the Bearer-protected local channel. CLI exit contract (spec §5):
//! 0 ok / 1 check failure / 2 bad args / 3 blocked-conflict-unverified /
//! 4 internal / 5 stop unknown.

use relaycheck::errors::{Code, RelayError, RelayResult};
use relaycheck::opsman::OpManager;
use relaycheck::service::Ctx;
use relaycheck::store::Store;
use std::path::PathBuf;
use std::sync::Arc;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = run(&args);
    std::process::exit(code);
}

fn data_dir_default() -> PathBuf {
    std::env::var("LOCALAPPDATA")
        .map(|d| PathBuf::from(d).join("RelayCheck"))
        .unwrap_or_else(|_| PathBuf::from("relaycheck-data"))
}

fn parse_data_dir(args: &[String]) -> PathBuf {
    let mut dir = data_dir_default();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--data-dir" {
            if let Some(v) = args.get(i + 1) {
                dir = PathBuf::from(v);
            }
        }
        i += 1;
    }
    dir
}

fn print_usage() -> i32 {
    eprintln!(
        "RelayCheck {} — a local workspace for handing coding tasks between tools.\n\n\
         Usage:\n  relaycheck serve [--data-dir D] [--port P]\n      start the local web workspace (prints the one-time launcher URL)\n  \
         relaycheck attach <repo-path>\n      register a Git project; prints a one-time claim code for the browser\n  \
         relaycheck tasks\n      list registered projects and tasks (JSON)\n  \
         relaycheck capture --task <id> [--untracked a,b] [--exclude a,b] [--json]\n      capture a candidate snapshot (waits)\n  \
         relaycheck verify --task <id> [--json]\n      run the task's approved checks (waits)\n  \
         relaycheck verify-handoff <package.zip> --repo <checkout> [--json]\n      offline: compare the package's listed files with the checkout (match/drift/blocked)\n\n\
         Exit codes: 0 ok, 1 check failure, 2 bad args, 3 blocked/conflict, 4 internal, 5 stop unknown.\n\
         verify-handoff exits: 0 match, 1 drift, 2 bad args, 3 cannot verify safely.",
        env!("CARGO_PKG_VERSION")
    );
    2
}

fn run(args: &[String]) -> i32 {
    match args.first().map(|s| s.as_str()) {
        Some("serve") => cmd_serve(args),
        Some("attach") => cmd_attach(args),
        Some("tasks") => cmd_tasks(args),
        Some("capture") => cmd_remote_op(args, "capture"),
        Some("verify") => cmd_remote_op(args, "verify"),
        Some("verify-handoff") => cmd_verify_handoff(args),
        Some("--help") | Some("-h") | Some("help") | None => print_usage(),
        Some(other) => {
            eprintln!("unknown command: {other}");
            print_usage()
        }
    }
}

fn find_flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn find_flag_bool(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

// ---------------------------------------------------------------------------
// serve
// ---------------------------------------------------------------------------

fn cmd_serve(args: &[String]) -> i32 {
    let data_dir = parse_data_dir(args);
    let port_override = find_flag(args, "--port").and_then(|p| p.parse::<u16>().ok());
    match serve_inner(data_dir, port_override) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{}: {}", e.code.as_str(), e.message);
            e.code.cli_exit()
        }
    }
}

fn serve_inner(data_dir: PathBuf, port_override: Option<u16>) -> RelayResult<()> {
    // RC02: the single-instance lock MUST be taken before anything touches
    // the database — a second launcher opening the same data dir used to
    // mark the first instance's running operations interrupted even though
    // it then exited. Directory creation itself is side-effect-free.
    std::fs::create_dir_all(&data_dir)?;
    let lock_path = data_dir.join("instance.lock");
    let _lock: std::fs::File = {
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .share_mode(0)
                .open(&lock_path)
                .map_err(|_| {
                    RelayError::new(
                        Code::ELimit,
                        "RelayCheck is already running (single-instance lock held)",
                    )
                })?
        }
        #[cfg(not(windows))]
        {
            std::fs::OpenOptions::new().create(true).write(true).open(&lock_path)?
        }
    };

    let store = Arc::new(Store::open(&data_dir)?);
    let blobs = Arc::new(relaycheck::blobstore::BlobStore::new(&data_dir)?);
    let ops = Arc::new(OpManager::new());
    let recovered = ops.recover_stale(&store)?;
    if recovered > 0 {
        eprintln!("[relaycheck] marked {recovered} unfinished operation(s) interrupted after restart; nothing was rerun");
    }
    let ctx = Ctx {
        store: store.clone(),
        blobs,
        ops,
        tmp_root: data_dir.join("tmp"),
        isolated_home: data_dir.join("isolated-home"),
    };
    std::fs::create_dir_all(&ctx.tmp_root)?;
    std::fs::create_dir_all(&ctx.isolated_home)?;

    let cli_token = read_or_create_cli_token(&data_dir)?;
    let (tx, _rx) = tokio::sync::broadcast::channel::<i64>(256);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| RelayError::internal(format!("tokio runtime: {e}")))?;

    rt.block_on(async move {
        let addr = format!("127.0.0.1:{}", port_override.unwrap_or(0));
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| RelayError::internal(format!("bind {addr}: {e}")))?;
        let port = listener.local_addr().map_err(RelayError::from)?.port();

        let state = Arc::new(relaycheck::http::ServerState {
            ctx,
            port,
            cli_token,
            events_tx: tx,
            admission: std::sync::Mutex::new(()),
        });

        // One-time bootstrap token; the fragment never reaches server logs.
        let token = relaycheck::ids::new_token(24);
        let hash = relaycheck::ids::sha256_hex(token.as_bytes());
        state.ctx.store.insert_bootstrap_token(&hash)?;

        // Publish the port so the CLI can discover this instance.
        std::fs::write(data_dir.join("port"), port.to_string())?;

        println!("RelayCheck {} — local workspace", env!("CARGO_PKG_VERSION"));
        println!("Open this URL in your browser (one-time):");
        println!("  http://127.0.0.1:{}/#rcb={}", port, token);
        println!("Data directory: {}", data_dir.display());
        println!("Press Ctrl+C to stop. Nothing is sent to any coding tool automatically.");
        eprintln!("[relaycheck] serving on 127.0.0.1:{port}");

        let app = relaycheck::http::router(state.clone());
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
                eprintln!("[relaycheck] shutting down");
            })
            .await
            .map_err(|e| RelayError::internal(format!("server error: {e}")))?;
        Ok::<(), RelayError>(())
    })
}

fn read_or_create_cli_token(data_dir: &std::path::Path) -> RelayResult<String> {
    let path = data_dir.join("cli-token");
    if let Ok(tok) = std::fs::read_to_string(&path) {
        let t = tok.trim().to_string();
        if t.len() >= 32 {
            return Ok(t);
        }
    }
    let t = relaycheck::ids::new_token(32);
    std::fs::write(&path, &t)?;
    Ok(t)
}

// ---------------------------------------------------------------------------
// CLI verbs (talk to the running service over localhost + Bearer)
// ---------------------------------------------------------------------------

fn data_dir_from_env() -> PathBuf {
    // The CLI uses the same default data dir unless --data-dir is given.
    std::env::var("RELAYCHECK_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| data_dir_default())
}

struct CliHttp {
    base: String,
    token: String,
}

impl CliHttp {
    fn discover(args: &[String]) -> RelayResult<CliHttp> {
        let data_dir = parse_data_dir(args);
        let port_file = data_dir.join("port");
        let port = std::fs::read_to_string(&port_file)
            .ok()
            .and_then(|p| p.trim().parse::<u16>().ok())
            .ok_or_else(|| {
                RelayError::new(
                    Code::ENotFound,
                    "no running RelayCheck service found; start one with: relaycheck serve",
                )
            })?;
        let token = std::fs::read_to_string(data_dir.join("cli-token"))
            .map_err(|_| RelayError::new(Code::EUnauthorized, "CLI token file missing"))?
            .trim()
            .to_string();
        Ok(CliHttp { base: format!("http://127.0.0.1:{port}"), token })
    }

    fn post_json(&self, path: &str, body: &serde_json::Value) -> RelayResult<serde_json::Value> {
        let resp = ureq::post(&format!("{}{}", self.base, path))
            .set("Authorization", &format!("Bearer {}", self.token))
            .timeout(std::time::Duration::from_secs(120))
            .send_json(body)
            .map_err(|e| ureq_error(e))?;
        Ok(resp.into_json::<serde_json::Value>().map_err(|e| RelayError::internal(format!("bad JSON: {e}")))?)
    }

    fn get_json(&self, path: &str) -> RelayResult<serde_json::Value> {
        let resp = ureq::get(&format!("{}{}", self.base, path))
            .set("Authorization", &format!("Bearer {}", self.token))
            .timeout(std::time::Duration::from_secs(60))
            .call()
            .map_err(|e| ureq_error(e))?;
        Ok(resp.into_json::<serde_json::Value>().map_err(|e| RelayError::internal(format!("bad JSON: {e}")))?)
    }
}

fn ureq_error(e: ureq::Error) -> RelayError {
    match e {
        ureq::Error::Status(code, resp) => {
            let body: serde_json::Value = resp.into_json().unwrap_or(serde_json::json!({}));
            let code_str = body
                .pointer("/error/code")
                .and_then(|c| c.as_str())
                .unwrap_or("E_INTERNAL")
                .to_string();
            let msg = body
                .pointer("/error/message")
                .and_then(|m| m.as_str())
                .unwrap_or(&format!("HTTP {code}"))
                .to_string();
            let code_enum = match code_str.as_str() {
                "E_REVISION_CONFLICT" => Code::ERevisionConflict,
                "E_EVIDENCE_STALE" => Code::EEvidenceStale,
                "E_PROFILE_CHANGED" => Code::EProfileChanged,
                "E_UNVERIFIED_IMPORT" => Code::EUnverifiedImport,
                "E_CAPTURE_UNSTABLE" => Code::ECaptureUnstable,
                "E_PATH_BOUNDARY" => Code::EPathBoundary,
                "E_SECRET_DETECTED" => Code::ESecretDetected,
                "E_LIMIT" => Code::ELimit,
                "E_STOP_UNCONFIRMED" => Code::EStopUnconfirmed,
                "E_EVENT_GAP" => Code::EEventGap,
                "E_CORRUPT" => Code::ECorrupt,
                "E_PROFILE_UNTRUSTED" => Code::EProfileUntrusted,
                "E_MISSING_DEPENDENCY" => Code::EMissingDependency,
                "E_UNAUTHORIZED" => Code::EUnauthorized,
                "E_FORBIDDEN" => Code::EForbidden,
                "E_NOT_FOUND" => Code::ENotFound,
                "E_STATE" => Code::EState,
                "E_REQUEST_CONFLICT" => Code::ERequestConflict,
                _ => Code::EInternal,
            };
            RelayError::new(code_enum, msg)
        }
        other => RelayError::internal(format!("request failed: {other}")),
    }
}

fn wait_operation(cli: &CliHttp, op_id: &str) -> RelayResult<serde_json::Value> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30 * 60);
    loop {
        let op = cli.get_json(&format!("/v1/operations/{op_id}"))?;
        let state = op.get("state").and_then(|s| s.as_str()).unwrap_or("pending").to_string();
        match state.as_str() {
            "completed" => return Ok(op),
            "failed" => {
                let err = op.get("error").cloned().unwrap_or(serde_json::json!({}));
                let code = err.get("code").and_then(|c| c.as_str()).unwrap_or("E_INTERNAL");
                let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("operation failed");
                let code_enum = match code {
                    "E_REVISION_CONFLICT" => Code::ERevisionConflict,
                    "E_EVIDENCE_STALE" => Code::EEvidenceStale,
                    "E_PROFILE_CHANGED" => Code::EProfileChanged,
                    "E_UNVERIFIED_IMPORT" => Code::EUnverifiedImport,
                    "E_CAPTURE_UNSTABLE" => Code::ECaptureUnstable,
                    "E_PATH_BOUNDARY" => Code::EPathBoundary,
                    "E_SECRET_DETECTED" => Code::ESecretDetected,
                    "E_LIMIT" => Code::ELimit,
                    "E_STOP_UNCONFIRMED" => Code::EStopUnconfirmed,
                    "E_EVENT_GAP" => Code::EEventGap,
                    "E_CORRUPT" => Code::ECorrupt,
                    "E_PROFILE_UNTRUSTED" => Code::EProfileUntrusted,
                    "E_MISSING_DEPENDENCY" => Code::EMissingDependency,
                    _ => Code::EInternal,
                };
                return Err(RelayError::new(code_enum, msg));
            }
            "cancelled" | "interrupted" => {
                return Err(RelayError::new(Code::EStopUnconfirmed, format!("operation {state}")));
            }
            _ => {
                if std::time::Instant::now() > deadline {
                    return Err(RelayError::new(Code::ELimit, "timed out waiting for operation"));
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }
}

fn cmd_attach(args: &[String]) -> i32 {
    let Some(root) = args.iter().nth(1).filter(|a| !a.starts_with("--")).cloned() else {
        eprintln!("usage: relaycheck attach <repo-path>");
        return 2;
    };
    let cli = match CliHttp::discover(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{}: {}", e.code.as_str(), e.message);
            return e.code.cli_exit();
        }
    };
    match cli.post_json("/v1/cli/attach", &serde_json::json!({"root": root})) {
        Ok(v) => {
            let pid = v.pointer("/result/project_id").and_then(|x| x.as_str()).unwrap_or("?");
            let code = v.pointer("/result/claim_code").and_then(|x| x.as_str()).unwrap_or("?");
            println!("project registered: {pid}");
            println!("one-time claim code for the browser: {code}");
            0
        }
        Err(e) => {
            eprintln!("{}: {}", e.code.as_str(), e.message);
            e.code.cli_exit()
        }
    }
}

fn cmd_tasks(args: &[String]) -> i32 {
    let cli = match CliHttp::discover(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{}: {}", e.code.as_str(), e.message);
            return e.code.cli_exit();
        }
    };
    match cli.get_json("/v1/cli/tasks") {
        Ok(v) => {
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            0
        }
        Err(e) => {
            eprintln!("{}: {}", e.code.as_str(), e.message);
            e.code.cli_exit()
        }
    }
}

// ---------------------------------------------------------------------------
// verify-handoff (offline; no service, no unpack, no registration)
// ---------------------------------------------------------------------------

fn cmd_verify_handoff(args: &[String]) -> i32 {
    let Some(pkg_arg) = args.iter().nth(1).filter(|a| !a.starts_with("--")).cloned() else {
        eprintln!("usage: relaycheck verify-handoff <package.zip> --repo <checkout> [--json]");
        return 2;
    };
    let Some(repo_arg) = find_flag(args, "--repo") else {
        eprintln!("verify-handoff needs the checkout to compare against: --repo <checkout>");
        return 2;
    };
    let json_out = find_flag_bool(args, "--json");
    // Package input: read the named file only. Errors never echo the path.
    let pkg_path = std::path::PathBuf::from(&pkg_arg);
    let data = match std::fs::read(&pkg_path) {
        Ok(d) => d,
        Err(_) => {
            eprintln!("package file not found or unreadable");
            return 2;
        }
    };
    if data.len() as i64 > relaycheck::verify_handoff::max_package_bytes() {
        eprintln!("package exceeds the size budget; cannot verify safely");
        return 3;
    }
    // Checkout input: strict local absolute path, same boundary as attach.
    let repo_root = match relaycheck::pathsafe::validate_root(&repo_arg) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{}: {}", e.code.as_str(), e.message);
            return 2;
        }
    };
    // Isolated git home (empty hooks dir) outside any repo or data dir.
    let home_base = std::env::temp_dir().join("relaycheck-verify-handoff");
    let isolated_home = match relaycheck::gitx::isolated_home(&home_base) {
        Ok(p) => p,
        Err(_) => {
            eprintln!("could not prepare the isolated git home; cannot verify safely");
            return 3;
        }
    };
    match relaycheck::verify_handoff::verify(&data, &repo_root, &isolated_home) {
        Ok(outcome) => {
            let exit = match outcome.result.as_str() {
                "match" => 0,
                "drift" => 1,
                _ => 3,
            };
            if json_out {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&outcome).unwrap_or_else(|_| "{}".into())
                );
            } else {
                print!("{}", relaycheck::verify_handoff::render_summary(&outcome));
            }
            exit
        }
        Err(e) => {
            if json_out {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "error": {"code": e.code.as_str(), "message": e.message, "details": e.details}
                    }))
                    .unwrap_or_default()
                );
            } else {
                eprintln!("{}: {}", e.code.as_str(), e.message);
            }
            match e.code {
                Code::EPathBoundary | Code::EValidation | Code::ENotFound => 2,
                _ => 3,
            }
        }
    }
}

fn cmd_remote_op(args: &[String], kind: &str) -> i32 {
    let Some(task) = find_flag(args, "--task") else {
        eprintln!("usage: relaycheck {kind} --task <task-id>");
        return 2;
    };
    let json_out = find_flag_bool(args, "--json");
    let cli = match CliHttp::discover(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{}: {}", e.code.as_str(), e.message);
            return e.code.cli_exit();
        }
    };
    let result = (|| -> RelayResult<serde_json::Value> {
        let body = if kind == "capture" {
            let untracked: Vec<String> = find_flag(args, "--untracked")
                .map(|s| s.split(',').filter(|x| !x.is_empty()).map(|x| x.to_string()).collect())
                .unwrap_or_default();
            let exclude: Vec<String> = find_flag(args, "--exclude")
                .map(|s| s.split(',').filter(|x| !x.is_empty()).map(|x| x.to_string()).collect())
                .unwrap_or_default();
            serde_json::json!({"task_id": task, "untracked_selected": untracked, "extra_excludes": exclude, "attest_stopped": true})
        } else {
            serde_json::json!({"task_id": task, "trust_confirmed": true})
        };
        let path = if kind == "capture" { "/v1/cli/capture" } else { "/v1/cli/verify" };
        let resp = cli.post_json(path, &body)?;
        let op_id = resp
            .pointer("/operation/id")
            .and_then(|x| x.as_str())
            .ok_or_else(|| RelayError::internal("no operation id"))?
            .to_string();
        wait_operation(&cli, &op_id)
    })();
    match result {
        Ok(op) => {
            let result = op.get("result").cloned().unwrap_or(serde_json::json!({}));
            if json_out {
                println!("{}", serde_json::to_string_pretty(&result).unwrap_or_default());
            } else {
                match kind {
                    "capture" => {
                        let sid = result.get("snapshot_id").and_then(|x| x.as_str()).unwrap_or("?");
                        let files = result.get("file_count").and_then(|x| x.as_i64()).unwrap_or(0);
                        println!("captured snapshot {sid} ({files} files)");
                    }
                    "verify" => {
                        let status = result.get("round_status").and_then(|x| x.as_str()).unwrap_or("?");
                        println!("check round: {status}");
                        for r in result.get("results").and_then(|x| x.as_array()).unwrap_or(&Vec::new()) {
                            println!(
                                "  {}: {} (exit {:?})",
                                r.get("check_id").and_then(|x| x.as_str()).unwrap_or("?"),
                                r.get("status").and_then(|x| x.as_str()).unwrap_or("?"),
                                r.get("exit_code").and_then(|x| x.as_i64())
                            );
                        }
                    }
                    _ => {}
                }
            }
            // Exit 1 when a check round failed; blocked rounds exit 3 (spec §5).
            if kind == "verify" {
                let status = result.get("round_status").and_then(|x| x.as_str()).unwrap_or("");
                return match status {
                    "passed" => 0,
                    "failed" => 1,
                    "blocked" => 3,
                    _ => 5,
                };
            }
            0
        }
        Err(e) => {
            if json_out {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "error": {"code": e.code.as_str(), "message": e.message, "details": e.details}
                    }))
                    .unwrap_or_default()
                );
            } else {
                eprintln!("{}: {}", e.code.as_str(), e.message);
            }
            e.code.cli_exit()
        }
    }
}
