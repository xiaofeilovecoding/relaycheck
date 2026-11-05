//! Local HTTP surface (spec §7): 127.0.0.1 only, strict Host, no CORS, no
//! WebSocket, one-time bootstrap → HttpOnly/SameSite=Strict session cookie,
//! CSRF + Origin on writes, Idempotency-Key + If-Match CAS, SSE with seq
//! replay, and a Bearer-protected CLI channel. The control plane never
//! exposes exec/read-file/path-picking endpoints.

use crate::errors::{Code, RelayError, RelayResult};
use crate::model::{CheckStep, Criterion, OpenItem, PrepStep, TaskDraft};
use crate::service::{self, Ctx};
use crate::store::Store;
use axum::body::Bytes;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

pub const SESSION_IDLE_MS: i64 = 15 * 60 * 1000;
pub const SESSION_ABS_MS: i64 = 8 * 60 * 60 * 1000;
pub const BOOTSTRAP_MAX_AGE_MS: i64 = 10 * 60 * 1000;
pub const COOKIE_NAME: &str = "rc_session";

pub struct ServerState {
    pub ctx: Ctx,
    pub port: u16,
    pub cli_token: String,
    pub events_tx: broadcast::Sender<i64>,
    /// In-memory idempotent-request response cache (key → (digest, response json))
    pub idem: std::sync::Mutex<HashMap<String, (String, serde_json::Value)>>,
}

pub type S = State<Arc<ServerState>>;

// ---------------------------------------------------------------------------
// Error envelope
// ---------------------------------------------------------------------------

impl IntoResponse for RelayError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = json!({
            "error": {
                "code": self.code.as_str(),
                "message": self.message,
                "details": self.details,
            }
        });
        (status, axum::Json(body)).into_response()
    }
}

type ApiResult<T> = Result<T, RelayError>;

fn ok_json(v: serde_json::Value) -> Response {
    (StatusCode::OK, axum::Json(v)).into_response()
}

// ---------------------------------------------------------------------------
// Auth helpers
// ---------------------------------------------------------------------------

fn host_allowed(headers: &HeaderMap, port: u16) -> bool {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    host == format!("127.0.0.1:{port}")
}

fn session_from(headers: &HeaderMap, state: &ServerState) -> RelayResult<(String, String)> {
    let cookie = headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let mut token = None;
    for pair in cookie.split(';') {
        let Some((k, v)) = pair.split_once('=') else { continue };
        if k.trim() == COOKIE_NAME {
            token = Some(v.trim().to_string());
        }
    }
    let Some(token) = token else {
        return Err(RelayError::new(Code::EUnauthorized, "no session; open the app from the launcher URL"));
    };
    let token_hash = crate::ids::sha256_hex(token.as_bytes());
    let now = crate::ids::now_ms();
    let Some(csrf) = state.ctx.store.session_valid(&token_hash, now, SESSION_IDLE_MS)? else {
        return Err(RelayError::new(Code::EUnauthorized, "session expired; reopen the launcher URL"));
    };
    state.ctx.store.touch_session(&token_hash, now)?;
    Ok((token_hash, csrf))
}

fn require_session(headers: &HeaderMap, state: &ServerState) -> RelayResult<String> {
    session_from(headers, state).map(|(_, csrf)| csrf)
}

fn require_cli_bearer(headers: &HeaderMap, state: &ServerState) -> RelayResult<()> {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let Some(bearer) = auth.strip_prefix("Bearer ") else {
        return Err(RelayError::new(Code::EUnauthorized, "CLI channel requires the Bearer token"));
    };
    // Constant-ish comparison; token is local-only anyway.
    if !secure_eq(bearer, &state.cli_token) {
        return Err(RelayError::new(Code::EUnauthorized, "invalid CLI bearer token"));
    }
    Ok(())
}

fn secure_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Write-request guard: session + CSRF header + same-origin Origin header.
fn require_write(headers: &HeaderMap, state: &ServerState) -> RelayResult<String> {
    let (_, csrf) = session_from(headers, state)?;
    let got = headers
        .get("x-relaycheck-csrf")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !secure_eq(got, &csrf) {
        return Err(RelayError::new(Code::EForbidden, "missing or wrong CSRF token"));
    }
    let origin = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let expected = format!("http://127.0.0.1:{}", state.port);
    if origin != expected {
        return Err(RelayError::new(Code::EForbidden, "cross-origin write requests are rejected"));
    }
    Ok(csrf)
}

// ---------------------------------------------------------------------------
// Idempotency + If-Match helpers
// ---------------------------------------------------------------------------

fn idempotency_key(headers: &HeaderMap) -> Option<String> {
    headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(128).collect())
}

fn if_match_revision(headers: &HeaderMap) -> RelayResult<i64> {
    let v = headers
        .get("if-match")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| RelayError::validation("If-Match: <revision> header is required for this request"))?;
    v.trim().parse::<i64>().map_err(|_| RelayError::validation("If-Match must be the task revision number"))
}

fn digest_of(parts: &[&[u8]]) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p);
    }
    hex::encode(h.finalize())
}

/// Checks/replays idempotent POSTs. Returns Some(response) for replay.
fn idem_replay(
    state: &ServerState,
    key: &str,
    digest: &str,
) -> Option<serde_json::Value> {
    let g = state.idem.lock().unwrap();
    if let Some((stored_digest, response)) = g.get(key) {
        if stored_digest == digest {
            return Some(response.clone());
        }
        return Some(json!({
            "error": {"code": "E_REQUEST_CONFLICT", "message": "same Idempotency-Key used with a different request body"}
        }));
    }
    None
}

fn idem_store(state: &ServerState, key: &str, digest: &str, response: &serde_json::Value) {
    let mut g = state.idem.lock().unwrap();
    if g.len() > 512 {
        g.clear(); // bounded cache
    }
    g.insert(key.to_string(), (digest.to_string(), response.clone()));
}

// ---------------------------------------------------------------------------
// Operation plumbing (async capture/verify on worker threads)
// ---------------------------------------------------------------------------

pub fn spawn_operation(
    state: &Arc<ServerState>,
    kind: &str,
    task_id: Option<&str>,
    idem_key: Option<String>,
    request_digest: &str,
    work: impl FnOnce(&Ctx, &str, crate::checks::CancelFlag) -> RelayResult<serde_json::Value> + Send + 'static,
) -> RelayResult<String> {
    let op_id = crate::ids::new_uuid();
    state
        .ctx
        .store
        .insert_operation(&op_id, kind, task_id, idem_key.as_deref(), request_digest)?;
    let cancel = state.ctx.ops.begin(&op_id, kind)?;
    let st = state.clone();
    let op_id2 = op_id.clone();
    let kind2 = kind.to_string();
    std::thread::spawn(move || {
        st.ctx.store.update_operation(&op_id2, "running", None, None)?;
        let result = work(&st.ctx, &op_id2, cancel);
        match result {
            Ok(v) => {
                st.ctx.store.update_operation(&op_id2, "completed", Some(&v.to_string()), None)?;
            }
            Err(e) => {
                st.ctx.store.update_operation(
                    &op_id2,
                    "failed",
                    None,
                    Some(
                        &json!({"code": e.code.as_str(), "message": e.message, "details": e.details})
                            .to_string(),
                    ),
                )?;
            }
        }
        let _ = kind2;
        let _ = st
            .events_tx
            .send(st.ctx.store.event_bounds().map(|(_, max)| max).unwrap_or(0));
        Ok::<(), RelayError>(())
    });
    Ok(op_id)
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/", get(ui_index))
        .route("/assets/{*path}", get(ui_asset))
        .route("/v1/session/exchange", post(session_exchange))
        .route("/v1/session/bootstrap", post(session_bootstrap))
        .route("/v1/me", get(me))
        .route("/v1/version", get(version))
        .route("/v1/state", get(state_snapshot))
        .route("/v1/tasks", post(create_task))
        .route("/v1/tasks/{id}", get(task_detail))
        .route("/v1/tasks/{id}", post(update_task_ep))
        .route("/v1/tasks/{id}/capture", post(capture_ep))
        .route("/v1/tasks/{id}/capture/plan", post(capture_plan_ep))
        .route("/v1/tasks/{id}/profile", post(save_profile_ep))
        .route("/v1/tasks/{id}/verify", post(verify_ep))
        .route("/v1/tasks/{id}/handoffs", post(create_handoff_ep))
        .route("/v1/handoffs/{id}/package", get(handoff_package))
        .route("/v1/tasks/{id}/imports", post(import_ep))
        .route("/v1/tasks/{id}/candidate", post(confirm_candidate_ep))
        .route("/v1/tasks/{id}/diff", get(diff_ep))
        .route("/v1/tasks/{id}/acceptances", post(accept_ep))
        .route("/v1/operations/{id}", get(operation_ep))
        .route("/v1/operations/{id}/cancel", post(cancel_ep))
        .route("/v1/events", get(events_ep))
        .route("/v1/claims", post(claim_ep))
        .route("/v1/demo", post(demo_ep))
        .route("/v1/cli/attach", post(cli_attach))
        .route("/v1/cli/capture", post(cli_capture))
        .route("/v1/cli/verify", post(cli_verify))
        .route("/v1/cli/tasks", get(cli_tasks))
        .fallback(fallback)
        .with_state(state)
}

async fn fallback() -> Response {
    RelayError::not_found("no such endpoint").into_response()
}

// ---------------------------------------------------------------------------
// Static UI (anonymous but Host-checked; only the fixed shell, no disk paths)
// ---------------------------------------------------------------------------

async fn ui_index(headers: HeaderMap, State(state): S) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    let index = crate::ui::dist()
        .get_file("index.html")
        .ok_or_else(|| RelayError::internal("UI assets are missing"))?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"),
         (axum::http::header::CACHE_CONTROL, "no-store")],
        index.contents().to_vec(),
    )
        .into_response())
}

async fn ui_asset(
    headers: HeaderMap,
    State(state): S,
    AxPath(path): AxPath<String>,
) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    let tail = path.trim_start_matches('/');
    // No traversal possible: the embedded dir is fixed; still reject "..".
    if tail.contains("..") {
        return Err(RelayError::new(Code::EPathBoundary, "bad asset path"));
    }
    // The URL /assets/<name> maps to dist/assets/<name>; also allow bare
    // lookups for root-level files.
    let file = crate::ui::dist()
        .get_file(format!("assets/{tail}"))
        .or_else(|| crate::ui::dist().get_file(tail))
        .ok_or_else(|| RelayError::not_found("asset not found"))?;
    let mime = match tail.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "woff2" => "font/woff2",
        "json" | "map" => "application/json",
        _ => "application/octet-stream",
    };
    Ok((
        [(axum::http::header::CONTENT_TYPE, mime), (axum::http::header::CACHE_CONTROL, "no-store")],
        file.contents().to_vec(),
    )
        .into_response())
}

fn check_host(headers: &HeaderMap, state: &ServerState) -> RelayResult<()> {
    if !host_allowed(headers, state.port) {
        return Err(RelayError::new(Code::EForbidden, "Host header mismatch"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Session endpoints
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct ExchangeBody {
    token: String,
}

async fn session_exchange(
    headers: HeaderMap,
    State(state): S,
    body: Option<axum::Json<ExchangeBody>>,
) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    // Strict same-origin POST with JSON content-type only.
    let origin = headers.get(axum::http::header::ORIGIN).and_then(|v| v.to_str().ok()).unwrap_or("");
    if !origin.is_empty() && origin != format!("http://127.0.0.1:{}", state.port) {
        return Err(RelayError::new(Code::EForbidden, "cross-origin bootstrap is rejected"));
    }
    let ct = headers.get(axum::http::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    if !ct.starts_with("application/json") {
        return Err(RelayError::validation("content-type must be application/json"));
    }
    let Some(axum::Json(body)) = body else {
        return Err(RelayError::validation("token required"));
    };
    if body.token.len() > 256 || body.token.is_empty() {
        return Err(RelayError::validation("bad token"));
    }
    let token_hash = crate::ids::sha256_hex(body.token.as_bytes());
    let now = crate::ids::now_ms();
    let won = state.ctx.store.consume_bootstrap_token(&token_hash, now, BOOTSTRAP_MAX_AGE_MS)?;
    if !won {
        return Err(RelayError::new(
            Code::EUnauthorized,
            "bootstrap token is unknown, already used, or expired; use the existing tab or a fresh launcher URL",
        ));
    }
    let session_id = crate::ids::new_token(32);
    let sid_hash = crate::ids::sha256_hex(session_id.as_bytes());
    let csrf = crate::ids::new_token(32);
    state.ctx.store.insert_session(&sid_hash, &csrf, now, now + SESSION_ABS_MS)?;
    let _ = state.ctx.store.append_event("session.created", &json!({}));
    let cookie = format!(
        "{COOKIE_NAME}={session_id}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
        SESSION_ABS_MS / 1000
    );
    Ok((
        [(axum::http::header::SET_COOKIE, cookie)],
        axum::Json(json!({"ok": true, "csrf": csrf})),
    )
        .into_response())
}

async fn session_bootstrap(headers: HeaderMap, State(state): S) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    require_write(&headers, &state)?;
    let now = crate::ids::now_ms();
    let active = state.ctx.store.bootstrap_tokens_active(now, BOOTSTRAP_MAX_AGE_MS)?;
    if active >= 5 {
        return Err(RelayError::new(Code::ELimit, "too many active bootstrap tokens"));
    }
    let token = crate::ids::new_token(24);
    let hash = crate::ids::sha256_hex(token.as_bytes());
    state.ctx.store.insert_bootstrap_token(&hash)?;
    Ok(ok_json(json!({"token": token, "url_fragment": format!("#rcb={token}")})))
}

async fn me(headers: HeaderMap, State(state): S) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    let csrf = require_session(&headers, &state)?;
    Ok(ok_json(json!({"csrf": csrf, "version": env!("CARGO_PKG_VERSION")})))
}

async fn version(headers: HeaderMap, State(state): S) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    let _ = state;
    Ok(ok_json(json!({
        "name": "RelayCheck",
        "version": env!("CARGO_PKG_VERSION"),
        "schema_version": crate::model::RECEIPT_SCHEMA_VERSION,
    })))
}

// ---------------------------------------------------------------------------
// Read endpoints
// ---------------------------------------------------------------------------

async fn state_snapshot(headers: HeaderMap, State(state): S) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    require_session(&headers, &state)?;
    let store = &state.ctx.store;
    let projects = store.list_projects()?;
    let tasks = store.list_tasks()?;
    let (_, max_seq) = store.event_bounds()?;
    Ok(ok_json(json!({
        "projects": projects,
        "tasks": tasks,
        "environment_epoch": store.environment_epoch(),
        "events_head": max_seq,
        "version": env!("CARGO_PKG_VERSION"),
    })))
}

async fn task_detail(
    headers: HeaderMap,
    State(state): S,
    AxPath(task_id): AxPath<String>,
) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    require_session(&headers, &state)?;
    let detail = build_task_detail(&state, &task_id)?;
    Ok(ok_json(json!({"operation": null, "result": detail})))
}

pub fn build_task_detail(state: &ServerState, task_id: &str) -> RelayResult<serde_json::Value> {
    let store = &state.ctx.store;
    let task = store.task_snapshot(task_id)?;
    let active = task.get("active_snapshot").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let snapshots: Vec<serde_json::Value> = if active.is_empty() {
        Vec::new()
    } else {
        collect_task_snapshots(store, task_id)?
    };
    let verifications: Vec<serde_json::Value> = store
        .verifications_for_task(task_id)?
        .into_iter()
        .map(|v| {
            json!({
                "id": v.id,
                "snapshot_id": v.snapshot_id,
                "profile_hash": v.profile_hash,
                "task_revision": v.task_revision,
                "environment_epoch": v.environment_epoch,
                "candidate_epoch": v.candidate_epoch,
                "round_status": v.round_status,
                "results": v.results.iter().map(|r| r.to_json()).collect::<Vec<_>>(),
                "tool_identity": v.tool_identity,
                "created_at_ms": v.created_at_ms,
            })
        })
        .collect();
    let handoffs: Vec<serde_json::Value> = store
        .handoffs_for_task(task_id)?
        .into_iter()
        .map(|h| store.handoff_json(&h))
        .collect::<RelayResult<Vec<_>>>()?;
    let imported = store.imported_reports_for_task(task_id)?;
    let acceptances = store.acceptances_for_task(task_id)?;
    let profile = {
        let pid = store
            .task_field_opt(task_id, "profile_id")?
            .unwrap_or_default();
        if pid.is_empty() {
            None
        } else {
            Some(json!({
                "profile": store.profile_json(&pid).ok(),
                "trusted": store.profile_trusted(&pid).unwrap_or(false),
            }))
        }
    };
    let acceptance = service::acceptance_check(&state.ctx, task_id)?;

    // Stale banner: a passed verification exists but not for the active
    // snapshot/revision/epoch combination.
    let stale_banner = !verifications.is_empty()
        && !verifications.iter().any(|v| {
            v.get("snapshot_id").and_then(|x| x.as_str()) == Some(active.as_str())
                && v.get("round_status").and_then(|x| x.as_str()) == Some("passed")
        });

    // Primary action (spec §1.2).
    let workflow = task.get("workflow_state").and_then(|v| v.as_str()).unwrap_or("draft");
    let parent_handoff = task.get("parent_handoff").and_then(|v| v.as_str()).map(|s| s.to_string());
    let primary = compute_primary(workflow, active.is_empty(), parent_handoff.as_deref());

    // A→B diff when a parent handoff links a baseline.
    let mut diff = serde_json::Value::Null;
    if let Some(hid) = &parent_handoff {
        if let Ok(h) = store.handoff_json(hid) {
            let baseline = h.get("candidate_snapshot").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if !baseline.is_empty() && baseline != active && !active.is_empty() {
                let baseline_available = store.snapshot_exists(&baseline);
                if baseline_available {
                    let blobs_ok = store
                        .snapshot_files(&baseline)?
                        .iter()
                        .all(|f| state.ctx.blobs.exists(&f.sha256));
                    if blobs_ok {
                        let files = service::diff_snapshots(&state.ctx, &baseline, &active, true)?;
                        diff = json!({
                            "baseline": baseline,
                            "candidate": active,
                            "baseline_available": true,
                            "files": files.iter().map(|f| json!({
                                "path": f.path, "kind": f.kind, "plus": f.plus, "minus": f.minus,
                                "binary": f.binary, "unified": f.unified, "truncated": f.truncated,
                                "old_size": f.old_size, "new_size": f.new_size,
                            })).collect::<Vec<_>>(),
                        });
                    } else {
                        diff = json!({"baseline": baseline, "baseline_available": false,
                                      "reason": "baseline blobs are missing; diff is read-only and the baseline will not be substituted"});
                    }
                } else {
                    diff = json!({"baseline": baseline, "baseline_available": false,
                                  "reason": "baseline snapshot is unavailable; diff is read-only"});
                }
            }
        }
    }

    Ok(json!({
        "task": task,
        "snapshots": snapshots,
        "verifications": verifications,
        "handoffs": handoffs,
        "imported_reports": imported,
        "acceptances": acceptances,
        "profile": profile,
        "acceptance": {"ok": acceptance.ok, "missing": acceptance.missing},
        "primary": primary,
        "stale_banner": stale_banner,
        "diff": diff,
    }))
}

fn collect_task_snapshots(store: &Store, task_id: &str) -> RelayResult<Vec<serde_json::Value>> {
    // Snapshots referenced by this task's verifications/handoffs + active.
    let mut ids: Vec<String> = Vec::new();
    for v in store.verifications_for_task(task_id)? {
        if !ids.contains(&v.snapshot_id) {
            ids.push(v.snapshot_id);
        }
    }
    for h in store.handoffs_for_task(task_id)? {
        let h = store.handoff_json(&h)?;
        if let Some(s) = h.get("candidate_snapshot").and_then(|v| v.as_str()) {
            if !ids.contains(&s.to_string()) {
                ids.push(s.to_string());
            }
        }
    }
    let mut out = Vec::new();
    for id in ids {
        if store.snapshot_exists(&id) {
            let mut j = store.snapshot_json(&id)?;
            if let Some(m) = j.get_mut("manifest") {
                if let Some(files) = m.get_mut("files") {
                    files.take();
                }
                if let Some(ex) = m.get_mut("excluded") {
                    ex.take();
                }
            }
            out.push(j);
        }
    }
    Ok(out)
}

fn compute_primary(workflow: &str, no_snapshot: bool, parent_handoff: Option<&str>) -> serde_json::Value {
    use serde_json::json;
    if no_snapshot {
        return json!({"action": "capture", "reason": "draft: no candidate captured yet"});
    }
    match workflow {
        "checking" => json!({"action": null, "reason": "checks are running"}),
        "rework" => json!({"action": "view_issues", "reason": "checks did not pass on this candidate"}),
        "candidate" => {
            if parent_handoff.is_some() {
                json!({"action": "run_checks", "reason": "a candidate came back; verify it locally"})
            } else {
                json!({"action": "run_checks", "reason": "candidate has no current local verification"})
            }
        }
        "ready_for_review" => {
            if parent_handoff.is_some() {
                json!({"action": "accept", "reason": "all required checks passed on the returned candidate"})
            } else {
                json!({"action": "prepare_handoff", "reason": "ready to prepare the handoff card"})
            }
        }
        "accepted" => json!({"action": null, "reason": "accepted; changes are not applied or merged automatically"}),
        _ => json!({"action": null, "reason": ""}),
    }
}

// ---------------------------------------------------------------------------
// Task endpoints
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct CreateTaskBody {
    project_id: String,
    goal: String,
    #[serde(default)]
    constraints: Vec<String>,
    #[serde(default)]
    criteria: Vec<Criterion>,
    #[serde(default)]
    is_demo: bool,
}

async fn create_task(
    headers: HeaderMap,
    State(state): S,
    body: Result<axum::Json<CreateTaskBody>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    require_write(&headers, &state)?;
    let Ok(axum::Json(body)) = body else {
        return Err(RelayError::validation("invalid task JSON"));
    };
    let id = service::create_task(&state.ctx, TaskDraft {
        project_id: body.project_id,
        goal: body.goal,
        constraints: body.constraints,
        criteria: body.criteria,
    }, body.is_demo)?;
    let _ = state.events_tx.send(head_seq(&state));
    Ok(ok_json(json!({"operation": null, "revision": 1, "result": {"task_id": id}})))
}

#[derive(serde::Deserialize)]
struct UpdateTaskBody {
    #[serde(default)]
    goal: Option<String>,
    #[serde(default)]
    constraints: Option<Vec<String>>,
    #[serde(default)]
    criteria: Option<Vec<Criterion>>,
}

async fn update_task_ep(
    headers: HeaderMap,
    State(state): S,
    AxPath(task_id): AxPath<String>,
    body: Result<axum::Json<UpdateTaskBody>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    require_write(&headers, &state)?;
    let if_match = if_match_revision(&headers)?;
    let Ok(axum::Json(body)) = body else {
        return Err(RelayError::validation("invalid JSON"));
    };
    let rev = service::update_task(&state.ctx, &task_id, if_match, body.goal, body.constraints, body.criteria)?;
    let _ = state.events_tx.send(head_seq(&state));
    Ok(ok_json(json!({"operation": null, "revision": rev, "result": {"ok": true}})))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CaptureBody {
    #[serde(default)]
    untracked_selected: Vec<String>,
    #[serde(default)]
    extra_excludes: Vec<String>,
    #[serde(default)]
    attest_stopped: bool,
}

async fn capture_plan_ep(
    headers: HeaderMap,
    State(state): S,
    AxPath(task_id): AxPath<String>,
    body: Result<axum::Json<CaptureBody>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    require_session(&headers, &state)?;
    let Ok(axum::Json(body)) = body else {
        return Err(RelayError::validation("invalid JSON"));
    };
    let plan = service::plan_capture(&state.ctx, &task_id, &body.untracked_selected, &body.extra_excludes)?;
    Ok(ok_json(json!({
        "operation": null,
        "result": {
            "included": plan.included,
            "excluded": plan.excluded,
            "base_head": plan.base_head,
            "dirty": plan.dirty,
        }
    })))
}

async fn capture_ep(
    headers: HeaderMap,
    State(state): S,
    AxPath(task_id): AxPath<String>,
    body: Result<axum::Json<CaptureBody>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    require_write(&headers, &state)?;
    let Ok(axum::Json(body)) = body else {
        return Err(RelayError::validation("invalid JSON"));
    };
    if state.ctx.ops.any_active() {
        return Err(RelayError::new(Code::ELimit, "another capture/check/import job is active"));
    }
    let digest = digest_of(&[task_id.as_bytes(), serde_json::to_vec(&body).unwrap_or_default().as_slice()]);
    let idem = idempotency_key(&headers);
    if let Some(k) = &idem {
        if let Some(replay) = idem_replay(&state, k, &digest) {
            return if replay.get("error").is_some() {
                Err(RelayError::new(Code::ERequestConflict, "same Idempotency-Key with a different body"))
            } else {
                Ok(ok_json(replay))
            };
        }
    }
    let st = state.clone();
    let t = task_id.clone();
    let attest = body.attest_stopped;
    let untracked = body.untracked_selected.clone();
    let excludes = body.extra_excludes.clone();
    let op_id = spawn_operation(&state, "capture", Some(&task_id), idem, &digest, move |ctx, op_id, _cancel| {
        service::run_capture(ctx, &service::CaptureRequest {
            task_id: t,
            untracked_selected: untracked,
            extra_excludes: excludes,
            attest_stopped: attest,
        }, op_id)
        .map(|r| json!({
            "snapshot_id": r.snapshot_id,
            "file_count": r.file_count,
            "total_size": r.total_size,
            "excluded_count": r.excluded_count,
            "is_return": r.is_return,
        }))
    })?;
    let _ = st.events_tx.send(head_seq(&state));
    Ok((StatusCode::ACCEPTED, axum::Json(json!({"operation": {"id": op_id, "kind": "capture", "state": "pending"}}))).into_response())
}

#[derive(serde::Deserialize)]
struct ProfileBody {
    name: String,
    checks: Vec<CheckStep>,
    #[serde(default)]
    prep: Vec<PrepStep>,
    #[serde(default)]
    outputs: Vec<String>,
}

async fn save_profile_ep(
    headers: HeaderMap,
    State(state): S,
    AxPath(task_id): AxPath<String>,
    body: Result<axum::Json<ProfileBody>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    require_write(&headers, &state)?;
    let Ok(axum::Json(body)) = body else {
        return Err(RelayError::validation("invalid JSON"));
    };
    let profile = service::save_profile(&state.ctx, &task_id, &body.name, body.checks, body.prep, body.outputs)?;
    let _ = state.events_tx.send(head_seq(&state));
    Ok(ok_json(json!({
        "operation": null,
        "revision": state.ctx.store.task_int(&task_id, "revision").unwrap_or(0),
        "result": {"profile_id": profile.id, "hash": profile.hash}
    })))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct VerifyBody {
    trust_confirmed: bool,
}

async fn verify_ep(
    headers: HeaderMap,
    State(state): S,
    AxPath(task_id): AxPath<String>,
    body: Result<axum::Json<VerifyBody>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    require_write(&headers, &state)?;
    let Ok(axum::Json(body)) = body else {
        return Err(RelayError::validation("invalid JSON"));
    };
    if state.ctx.ops.any_active() {
        return Err(RelayError::new(Code::ELimit, "another capture/check/import job is active"));
    }
    let digest = digest_of(&[task_id.as_bytes(), body.trust_confirmed.to_string().as_bytes()]);
    let idem = idempotency_key(&headers);
    if let Some(k) = &idem {
        if let Some(replay) = idem_replay(&state, k, &digest) {
            return if replay.get("error").is_some() {
                Err(RelayError::new(Code::ERequestConflict, "same Idempotency-Key with a different body"))
            } else {
                Ok(ok_json(replay))
            };
        }
    }
    let t = task_id.clone();
    let trust = body.trust_confirmed;
    let op_id = spawn_operation(&state, "verify", Some(&task_id), idem, &digest, move |ctx, op_id, cancel| {
        let store = ctx.store.clone();
        let t_step = t.clone();
        let result = service::run_verify(ctx, &service::VerifyRequest { task_id: t, trust_confirmed: trust }, &cancel, op_id, move |msg| {
            // step notifications become events for the UI
            let _ = store.append_event("check.step", &json!({"task_id": t_step, "step": msg}));
        });
        result.map(|r| json!({
            "verification_id": r.verification_id,
            "round_status": r.round_status,
            "results": r.results.iter().map(|x| x.to_json()).collect::<Vec<_>>(),
        }))
    })?;
    let _ = state.events_tx.send(head_seq(&state));
    Ok((StatusCode::ACCEPTED, axum::Json(json!({"operation": {"id": op_id, "kind": "verify", "state": "pending"}}))).into_response())
}

#[derive(serde::Deserialize)]
struct HandoffBodyReq {
    source_mode: String,
    next_action: String,
    #[serde(default)]
    open_items: Vec<OpenItem>,
    #[serde(default)]
    notes_constraints: Vec<String>,
    #[serde(default)]
    edited_fields: Vec<String>,
}

async fn create_handoff_ep(
    headers: HeaderMap,
    State(state): S,
    AxPath(task_id): AxPath<String>,
    body: Result<axum::Json<HandoffBodyReq>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    require_write(&headers, &state)?;
    let Ok(axum::Json(body)) = body else {
        return Err(RelayError::validation("invalid JSON"));
    };
    let digest = digest_of(&[task_id.as_bytes(), body.next_action.as_bytes(), body.source_mode.as_bytes()]);
    let idem = idempotency_key(&headers);
    if let Some(k) = &idem {
        if let Some(replay) = idem_replay(&state, k, &digest) {
            return if replay.get("error").is_some() {
                Err(RelayError::new(Code::ERequestConflict, "same Idempotency-Key with a different body"))
            } else {
                Ok(ok_json(replay))
            };
        }
    }
    let (handoff_id, package_sha, md) = service::create_handoff(&state.ctx, &service::HandoffRequest {
        task_id: task_id.clone(),
        source_mode: body.source_mode,
        next_action: body.next_action,
        open_items: body.open_items,
        notes_constraints: body.notes_constraints,
        edited_fields: body.edited_fields,
    })?;
    let response = json!({
        "operation": null,
        "result": {
            "handoff_id": handoff_id,
            "package_sha256": package_sha,
            "handoff_md": md,
        }
    });
    if let Some(k) = &idem {
        idem_store(&state, k, &digest, &response);
    }
    let _ = state.events_tx.send(head_seq(&state));
    Ok(ok_json(response))
}

async fn handoff_package(
    headers: HeaderMap,
    State(state): S,
    AxPath(handoff_id): AxPath<String>,
) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    require_session(&headers, &state)?;
    // Preview and download are the SAME immutable bytes.
    let path = state.ctx.store.handoff_package_path(&handoff_id)?;
    let data = std::fs::read(&path).map_err(|_| RelayError::new(Code::ECorrupt, "stored package missing"))?;
    let sha = crate::ids::sha256_hex(&data);
    let stored_sha = state
        .ctx
        .store
        .handoff_json(&handoff_id)?
        .get("package_sha256")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if sha != stored_sha {
        return Err(RelayError::new(Code::ECorrupt, "stored package hash mismatch"));
    }
    let headers2 = [
        (axum::http::header::CONTENT_TYPE, "application/zip".to_string()),
        (axum::http::header::CONTENT_DISPOSITION, format!("attachment; filename=\"relaycheck-handoff-{handoff_id}.zip\"")),
        (axum::http::HeaderName::from_static("x-package-sha256"), sha),
    ];
    Ok((StatusCode::OK, headers2, data).into_response())
}

async fn import_ep(
    headers: HeaderMap,
    State(state): S,
    AxPath(task_id): AxPath<String>,
    body: Bytes,
) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    require_write(&headers, &state)?;
    if body.len() as i64 > crate::model::MAX_PACKAGE_EXPANDED_BYTES {
        return Err(RelayError::new(Code::ELimit, "upload too large").into());
    }
    if state.ctx.ops.any_active() {
        return Err(RelayError::new(Code::ELimit, "another capture/check/import job is active"));
    }
    // Import is synchronous but still claims the single write slot.
    let op_id = crate::ids::new_uuid();
    state.ctx.store.insert_operation(&op_id, "import", Some(&task_id), None, "")?;
    let cancel = state.ctx.ops.begin(&op_id, "import")?;
    let result = service::import_package(&state.ctx, &body.as_ref(), Some(&task_id));
    state.ctx.ops.finish(&op_id);
    let _ = cancel;
    let outcome = result?;
    let _ = state.events_tx.send(head_seq(&state));
    Ok(ok_json(json!({
        "operation": {"id": op_id, "kind": "import", "state": "completed"},
        "result": {
            "imported_report_id": outcome.imported_report_id,
            "association": outcome.association,
            "snapshot_id": outcome.snapshot_id,
            "receipt": serde_json::to_value(&outcome.receipt).map_err(RelayError::from)?,
        }
    })))
}

#[derive(serde::Deserialize)]
struct CandidateBody {
    snapshot_id: String,
}

async fn confirm_candidate_ep(
    headers: HeaderMap,
    State(state): S,
    AxPath(task_id): AxPath<String>,
    body: Result<axum::Json<CandidateBody>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    require_write(&headers, &state)?;
    let if_match = if_match_revision(&headers)?;
    let Ok(axum::Json(body)) = body else {
        return Err(RelayError::validation("invalid JSON"));
    };
    service::confirm_candidate(&state.ctx, &task_id, &body.snapshot_id, if_match)?;
    let _ = state.events_tx.send(head_seq(&state));
    Ok(ok_json(json!({
        "operation": null,
        "revision": state.ctx.store.task_int(&task_id, "revision").unwrap_or(0),
        "result": {"ok": true}
    })))
}

async fn diff_ep(
    headers: HeaderMap,
    State(state): S,
    AxPath(task_id): AxPath<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    require_session(&headers, &state)?;
    let from = q.get("from").ok_or_else(|| RelayError::validation("from snapshot required"))?;
    let to = q.get("to").ok_or_else(|| RelayError::validation("to snapshot required"))?;
    crate::ids::parse_hash_id(from)?;
    crate::ids::parse_hash_id(to)?;
    let _ = task_id;
    let files = service::diff_snapshots(&state.ctx, from, to, true)?;
    Ok(ok_json(json!({
        "operation": null,
        "result": {"files": files.iter().map(|f| json!({
            "path": f.path, "kind": f.kind, "plus": f.plus, "minus": f.minus,
            "binary": f.binary, "unified": f.unified, "truncated": f.truncated,
            "old_size": f.old_size, "new_size": f.new_size,
        })).collect::<Vec<_>>()}
    })))
}

#[derive(serde::Deserialize)]
struct AcceptBody {
    #[serde(default)]
    criteria_ack: Vec<String>,
}

async fn accept_ep(
    headers: HeaderMap,
    State(state): S,
    AxPath(task_id): AxPath<String>,
    body: Result<axum::Json<AcceptBody>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    require_write(&headers, &state)?;
    let if_match = if_match_revision(&headers)?;
    let Ok(axum::Json(body)) = body else {
        return Err(RelayError::validation("invalid JSON"));
    };
    let id = service::accept_candidate(&state.ctx, &task_id, &body.criteria_ack, if_match)?;
    let _ = state.events_tx.send(head_seq(&state));
    Ok(ok_json(json!({
        "operation": null,
        "revision": state.ctx.store.task_int(&task_id, "revision").unwrap_or(0),
        "result": {"acceptance_id": id}
    })))
}

// ---------------------------------------------------------------------------
// Operations + SSE
// ---------------------------------------------------------------------------

async fn operation_ep(
    headers: HeaderMap,
    State(state): S,
    AxPath(op_id): AxPath<String>,
) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    require_session(&headers, &state)?;
    let op = state.ctx.store.operation_state(&op_id)?;
    Ok(ok_json(op))
}

async fn cancel_ep(
    headers: HeaderMap,
    State(state): S,
    AxPath(op_id): AxPath<String>,
) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    require_write(&headers, &state)?;
    let op = state.ctx.store.operation_state(&op_id)?;
    let op_state = op.get("state").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if matches!(op_state.as_str(), "completed" | "failed" | "cancelled" | "interrupted") {
        return Err(RelayError::state("operation already finished").into());
    }
    // Flag + DB state; the worker thread confirms the actual stop.
    let _ = state.ctx.store.update_operation(&op_id, "cancelling", None, None);
    if !state.ctx.ops.cancel(&op_id)? {
        // Active in DB but not in memory (e.g. restart): mark interrupted.
        state.ctx.store.update_operation(&op_id, "interrupted", None, Some(&json!({"code": "E_STOP_UNCONFIRMED", "message": "operation not active in this process; marked interrupted"}).to_string()))?;
    }
    let _ = state.events_tx.send(head_seq(&state));
    Ok(ok_json(json!({"operation": {"id": op_id, "state": "cancelling"}})))
}

fn head_seq(state: &ServerState) -> i64 {
    state.ctx.store.event_bounds().map(|(_, m)| m).unwrap_or(0)
}

async fn events_ep(
    headers: HeaderMap,
    State(state): S,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Sse<impl futures_util::Stream<Item = Result<SseEvent, std::convert::Infallible>>>> {
    check_host(&headers, &state)?;
    require_session(&headers, &state)?;
    let after: i64 = q.get("after").and_then(|v| v.parse().ok()).unwrap_or(0);
    let (_, max) = state.ctx.store.event_bounds()?;
    if after > max || (after > 0 && after < min_seq(&state)) {
        // Gap: client must re-read the snapshot, never rebuild from old events.
        return Err(RelayError::new(Code::EEventGap, "event gap; re-read the current snapshot").into());
    }
    let st = state.clone();
    let rx = state.events_tx.subscribe();
    let stream = event_stream(after, st, rx);
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)).text("keep-alive")))
}

fn min_seq(state: &ServerState) -> i64 {
    state.ctx.store.event_bounds().map(|(m, _)| m).unwrap_or(0)
}

fn event_stream(
    mut after: i64,
    state: Arc<ServerState>,
    mut rx: broadcast::Receiver<i64>,
) -> impl futures_util::Stream<Item = Result<SseEvent, std::convert::Infallible>> {
    use std::collections::VecDeque;
    futures_util::stream::unfold(
        (after, state, rx, VecDeque::new()),
        |(mut after, state, mut rx, mut queue)| async move {
            loop {
                if let Some(ev) = queue.pop_front() {
                    return Some((ev, (after, state, rx, queue)));
                }
                // Drain any DB events after `after` (replay + gap coverage).
                let rows = state.ctx.store.events_after(after, 200).unwrap_or_default();
                if !rows.is_empty() {
                    after = rows.last().map(|r| r.0).unwrap_or(after);
                    for (seq, kind, payload) in rows {
                        queue.push_back(Ok(
                            SseEvent::default().event(kind).id(seq.to_string()).data(payload)
                        ));
                    }
                    continue;
                }
                // Nothing new: wait for a live signal or timeout (keepalive).
                match tokio::time::timeout(Duration::from_secs(20), rx.recv()).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
                    _ => {}
                }
            }
        },
    )
}

// ---------------------------------------------------------------------------
// Claims (registration codes) + CLI channel
// ---------------------------------------------------------------------------

async fn demo_ep(headers: HeaderMap, State(state): S) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    require_write(&headers, &state)?;
    if state.ctx.ops.any_active() {
        return Err(RelayError::new(Code::ELimit, "wait for the active job to finish").into());
    }
    let task_id = service::create_demo(&state.ctx)?;
    let _ = state.events_tx.send(head_seq(&state));
    Ok(ok_json(json!({"operation": null, "result": {"task_id": task_id}})))
}

#[derive(serde::Deserialize)]
struct ClaimBody {
    code: String,
}

async fn claim_ep(
    headers: HeaderMap,
    State(state): S,
    body: Result<axum::Json<ClaimBody>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    require_write(&headers, &state)?;
    let Ok(axum::Json(body)) = body else {
        return Err(RelayError::validation("invalid JSON"));
    };
    let project_id = service::claim_project(&state.ctx, &body.code)?;
    let _ = state.events_tx.send(head_seq(&state));
    Ok(ok_json(json!({"operation": null, "result": {"project_id": project_id}})))
}

#[derive(serde::Deserialize)]
struct CliAttachBody {
    root: String,
    #[serde(default)]
    display_name: Option<String>,
}

async fn cli_attach(
    headers: HeaderMap,
    State(state): S,
    body: Result<axum::Json<CliAttachBody>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    require_cli_bearer(&headers, &state)?;
    let Ok(axum::Json(body)) = body else {
        return Err(RelayError::validation("invalid JSON"));
    };
    let (project_id, code) = service::attach_project(&state.ctx, &body.root, body.display_name.as_deref())?;
    let _ = state.events_tx.send(head_seq(&state));
    Ok(ok_json(json!({"operation": null, "result": {"project_id": project_id, "claim_code": code}})))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CliOpBody {
    task_id: String,
    #[serde(default)]
    untracked_selected: Vec<String>,
    #[serde(default)]
    extra_excludes: Vec<String>,
    #[serde(default = "default_true")]
    attest_stopped: bool,
    #[serde(default = "default_true")]
    trust_confirmed: bool,
}

fn default_true() -> bool {
    true
}

async fn cli_capture(
    headers: HeaderMap,
    State(state): S,
    body: Result<axum::Json<CliOpBody>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    require_cli_bearer(&headers, &state)?;
    let Ok(axum::Json(body)) = body else {
        return Err(RelayError::validation("invalid JSON"));
    };
    if state.ctx.ops.any_active() {
        return Err(RelayError::new(Code::ELimit, "another capture/check/import job is active"));
    }
    let digest = digest_of(&[body.task_id.as_bytes(), serde_json::to_vec(&body).unwrap_or_default().as_slice()]);
    let t = body.task_id.clone();
    let (u, e, a) = (body.untracked_selected.clone(), body.extra_excludes.clone(), body.attest_stopped);
    let op_id = spawn_operation(&state, "capture", Some(&body.task_id), None, &digest, move |ctx, op_id, _c| {
        service::run_capture(ctx, &service::CaptureRequest {
            task_id: t,
            untracked_selected: u,
            extra_excludes: e,
            attest_stopped: a,
        }, op_id)
        .map(|r| json!({"snapshot_id": r.snapshot_id, "file_count": r.file_count}))
    })?;
    Ok(ok_json(json!({"operation": {"id": op_id, "kind": "capture", "state": "pending"}})))
}

async fn cli_verify(
    headers: HeaderMap,
    State(state): S,
    body: Result<axum::Json<CliOpBody>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    require_cli_bearer(&headers, &state)?;
    let Ok(axum::Json(body)) = body else {
        return Err(RelayError::validation("invalid JSON"));
    };
    if state.ctx.ops.any_active() {
        return Err(RelayError::new(Code::ELimit, "another capture/check/import job is active"));
    }
    let digest = digest_of(&[body.task_id.as_bytes(), b"verify"]);
    let t = body.task_id.clone();
    let trust = body.trust_confirmed;
    let op_id = spawn_operation(&state, "verify", Some(&body.task_id), None, &digest, move |ctx, op_id, cancel| {
        service::run_verify(ctx, &service::VerifyRequest { task_id: t, trust_confirmed: trust }, &cancel, op_id, |_| {})
            .map(|r| json!({"verification_id": r.verification_id, "round_status": r.round_status,
                             "results": r.results.iter().map(|x| x.to_json()).collect::<Vec<_>>()}))
    })?;
    Ok(ok_json(json!({"operation": {"id": op_id, "kind": "verify", "state": "pending"}})))
}

async fn cli_tasks(headers: HeaderMap, State(state): S) -> ApiResult<Response> {
    check_host(&headers, &state)?;
    require_cli_bearer(&headers, &state)?;
    let tasks = state.ctx.store.list_tasks()?;
    let projects = state.ctx.store.list_projects()?;
    Ok(ok_json(json!({"operation": null, "result": {"tasks": tasks, "projects": projects}})))
}
