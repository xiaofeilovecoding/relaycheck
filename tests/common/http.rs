//! Shared HTTP test harness (axum server in-process + ureq client).
#![allow(dead_code)]

use super::*;
use relaycheck::http::ServerState;
use std::io::Read;
use std::sync::Arc;

pub struct HttpTest {
    pub base: String,
    pub port: u16,
    pub cookie: String,
    pub csrf: String,
    pub cli_token: String,
    _rt: Option<tokio::runtime::Runtime>,
}

impl Drop for HttpTest {
    fn drop(&mut self) {
        if let Some(rt) = self._rt.take() {
            rt.shutdown_background();
        }
    }
}

pub fn start_http(env: &TestEnv) -> HttpTest {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    // Bind first, then build the state with the real port (Host/Origin
    // checks compare against it) — same order as the real launcher.
    let (port, _rx) = rt.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::broadcast::channel::<i64>(256);
        let cli_token = "test-cli-token-0123456789abcdef".to_string();
        let state = Arc::new(ServerState {
            ctx: env.ctx.clone(),
            port,
            cli_token,
            events_tx: tx,
            admission: std::sync::Mutex::new(()),
        });
        let app = relaycheck::http::router(state);
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (port, rx)
    });
    let cli_token = "test-cli-token-0123456789abcdef".to_string();
    // Give the server a moment.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let base = format!("http://127.0.0.1:{port}");

    // Session bootstrap through the real one-time token path.
    let token = relaycheck::ids::new_token(16);
    let hash = relaycheck::ids::sha256_hex(token.as_bytes());
    env.ctx.store.insert_bootstrap_token(&hash).unwrap();
    let resp = ureq::post(&format!("{base}/v1/session/exchange"))
        .set("Content-Type", "application/json")
        .set("Origin", &base)
        .send_json(serde_json::json!({ "token": token }))
        .expect("exchange");
    let set_cookie = resp.header("set-cookie").expect("cookie").to_string();
    let cookie = set_cookie
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let csrf = resp.into_json::<serde_json::Value>().unwrap()["csrf"]
        .as_str()
        .unwrap()
        .to_string();
    HttpTest { base, port, cookie, csrf, cli_token, _rt: Some(rt) }
}

impl HttpTest {
    pub fn get(&self, path: &str) -> ureq::Response {
        ureq::get(&format!("{}{}", self.base, path))
            .set("Cookie", &self.cookie)
            .set("Host", &format!("127.0.0.1:{}", self.port))
            .timeout(std::time::Duration::from_secs(10))
            .call()
            .map_err(|e| panic!("GET {path} failed: {e}"))
            .unwrap()
    }

    pub fn get_raw(&self, path: &str) -> Result<ureq::Response, ureq::Error> {
        ureq::get(&format!("{}{}", self.base, path))
            .set("Cookie", &self.cookie)
            .set("Host", &format!("127.0.0.1:{}", self.port))
            .timeout(std::time::Duration::from_secs(10))
            .call()
    }

    pub fn post_idem(&self, path: &str, body: serde_json::Value, key: &str) -> Result<ureq::Response, ureq::Error> {
        self.post_full(path, body, Some(key), None)
    }

    /// POST with CSRF/Origin/Idempotency-Key/If-Match — the full write contract.
    pub fn post_full(
        &self,
        path: &str,
        body: serde_json::Value,
        key: Option<&str>,
        if_match: Option<i64>,
    ) -> Result<ureq::Response, ureq::Error> {
        let mut req = ureq::post(&format!("{}{}", self.base, path))
            .set("Cookie", &self.cookie)
            .set("Host", &format!("127.0.0.1:{}", self.port))
            .set("X-RelayCheck-CSRF", &self.csrf)
            .set("Origin", &self.base)
            .set("Content-Type", "application/json");
        if let Some(k) = key {
            req = req.set("Idempotency-Key", k);
        }
        if let Some(r) = if_match {
            req = req.set("If-Match", &r.to_string());
        }
        req.send_json(body)
    }

    pub fn post(&self, path: &str, body: serde_json::Value, with_csrf: bool, origin: Option<&str>) -> Result<ureq::Response, ureq::Error> {
        let mut req = ureq::post(&format!("{}{}", self.base, path))
            .set("Cookie", &self.cookie)
            .set("Host", &format!("127.0.0.1:{}", self.port))
            .set("Content-Type", "application/json");
        if with_csrf {
            req = req.set("X-RelayCheck-CSRF", &self.csrf);
        }
        if let Some(o) = origin {
            req = req.set("Origin", o);
        } else {
            req = req.set("Origin", &self.base);
        }
        req.send_json(body)
    }

    pub fn post_raw(&self, path: &str, body: Vec<u8>, content_type: &str) -> Result<ureq::Response, ureq::Error> {
        ureq::post(&format!("{}{}", self.base, path))
            .set("Cookie", &self.cookie)
            .set("Host", &format!("127.0.0.1:{}", self.port))
            .set("X-RelayCheck-CSRF", &self.csrf)
            .set("Origin", &self.base)
            .set("Content-Type", content_type)
            .send_bytes(&body)
    }

    pub fn wait_op(&self, op_id: &str) -> serde_json::Value {
        for _ in 0..240 {
            let op = self.get(&format!("/v1/operations/{op_id}")).into_json::<serde_json::Value>().unwrap();
            let state = op["state"].as_str().unwrap_or("");
            if state == "completed" || state == "failed" || state == "interrupted" || state == "cancelled" {
                return op;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        panic!("operation {op_id} did not finish");
    }
}

/// Current task revision via the detail endpoint.
pub fn task_revision(http: &HttpTest, task_id: &str) -> i64 {
    task_detail(http, task_id)["task"]["revision"].as_i64().unwrap()
}

pub fn task_detail(http: &HttpTest, task_id: &str) -> serde_json::Value {
    http.get(&format!("/v1/tasks/{task_id}"))
        .into_json::<serde_json::Value>()
        .unwrap()["result"]
        .clone()
}

/// POST /verify with the RC01 identity contract (profile id/hash + If-Match).
pub fn post_verify(http: &HttpTest, task_id: &str, key: Option<&str>) -> Result<ureq::Response, ureq::Error> {
    let d = task_detail(http, task_id);
    let profile = d["profile"]["profile"].clone();
    let body = serde_json::json!({
        "trust_confirmed": true,
        "profile_id": profile["id"].as_str().unwrap_or(""),
        "profile_hash": profile["hash"].as_str().unwrap_or(""),
    });
    http.post_full(
        &format!("/v1/tasks/{task_id}/verify"),
        body,
        key,
        Some(d["task"]["revision"].as_i64().unwrap()),
    )
}

/// Extracts the HTTP status from a ureq result (consumes it).
pub fn status_of(r: Result<ureq::Response, ureq::Error>) -> u16 {
    match r {
        Ok(resp) => resp.status(),
        Err(ureq::Error::Status(c, _)) => c,
        Err(_) => 0,
    }
}


/// Setup: repo + attach + task + capture + passing profile, ready for
/// handoff/verify flows.
pub fn setup_capture_ready(http: &HttpTest, env: &TestEnv) -> (Repo, String) {
    let repo = env.make_git_repo("http-ready");
    repo.write("a.txt", b"one
");
    repo.write("src/lib.py", b"print(1)
");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "base"]);
    let (project_id, _code) = relaycheck::service::attach_project(
        &env.ctx,
        &repo.root.to_string_lossy(),
        Some("http-repo"),
    )
    .unwrap();
    let task_id = relaycheck::service::create_task(
        &env.ctx,
        relaycheck::model::TaskDraft {
            project_id,
            goal: "http ready".into(),
            constraints: vec![],
            criteria: vec![relaycheck::model::Criterion {
                id: "SC1".into(),
                text: "done".into(),
                required: true,
            }],
        },
        false,
    )
    .unwrap();
    // capture at rev 1
    let r = http
        .post_full(
            &format!("/v1/tasks/{task_id}/capture"),
            serde_json::json!({"attest_stopped": true, "untracked_selected": [], "extra_excludes": []}),
            None,
            Some(1),
        )
        .unwrap();
    let op = r.into_json::<serde_json::Value>().unwrap()["operation"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    http.wait_op(&op);
    // profile + verify via the identity contract
    let python = find_python();
    let script = env.scratch.join("ready.py");
    std::fs::write(&script, b"import sys
sys.exit(0)
").unwrap();
    let rev = task_revision(http, &task_id);
    let save = http
        .post_full(
            &format!("/v1/tasks/{task_id}/profile"),
            serde_json::json!({
                "name": "ready-checks",
                "checks": [{"id": "c", "exe": python.to_string_lossy(), "args": [script.to_string_lossy()],
                            "cwd_relative": "", "timeout_seconds": 60, "required": true}],
                "prep": [],
                "outputs": []
            }),
            None,
            Some(rev),
        )
        .unwrap()
        .into_json::<serde_json::Value>()
        .unwrap();
    let _ = save;
    let r = post_verify(http, &task_id, None).unwrap();
    let op = r.into_json::<serde_json::Value>().unwrap()["operation"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    http.wait_op(&op);
    (repo, task_id)
}
