//! SQLite persistence. The database is the only writer; WAL + foreign keys +
//! transactions. All multi-row mutations happen in transactions.

use crate::errors::{RelayError, RelayResult};
use crate::ids::now_ms;
use crate::model::{
    CheckProfile, CheckResult, Criterion, ExcludedEntry, HandoffBody, HandoffMeta, ManifestFile,
    OpenItem, Receipt, SnapshotManifest, VerificationRecord,
};
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct Store {
    conn: Mutex<Connection>,
    pub data_dir: PathBuf,
}

pub const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS projects (
  id TEXT PRIMARY KEY,
  display_name TEXT NOT NULL,
  repo_root TEXT NOT NULL UNIQUE,
  common_dir TEXT NOT NULL,
  head_commit TEXT NOT NULL DEFAULT '',
  registered_via TEXT NOT NULL DEFAULT 'cli',
  trust_confirmed INTEGER NOT NULL DEFAULT 0,
  created_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS tasks (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id),
  goal TEXT NOT NULL,
  constraints_json TEXT NOT NULL DEFAULT '[]',
  criteria_json TEXT NOT NULL DEFAULT '[]',
  revision INTEGER NOT NULL DEFAULT 1,
  active_snapshot TEXT,
  parent_handoff TEXT,
  profile_id TEXT,
  candidate_epoch INTEGER NOT NULL DEFAULT 1,
  workflow_state TEXT NOT NULL DEFAULT 'draft',
  is_demo INTEGER NOT NULL DEFAULT 0,
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS snapshots (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id),
  base_head TEXT NOT NULL DEFAULT '',
  parent_snapshot TEXT,
  dirty INTEGER NOT NULL DEFAULT 0,
  manifest_json TEXT NOT NULL,
  manifest_sha256 TEXT NOT NULL,
  total_size INTEGER NOT NULL,
  file_count INTEGER NOT NULL,
  created_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS snapshot_files (
  snapshot_id TEXT NOT NULL REFERENCES snapshots(id),
  path TEXT NOT NULL,
  blob_sha256 TEXT NOT NULL,
  size INTEGER NOT NULL,
  mode TEXT NOT NULL,
  PRIMARY KEY (snapshot_id, path)
);
CREATE TABLE IF NOT EXISTS check_profiles (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id),
  name TEXT NOT NULL,
  checks_json TEXT NOT NULL,
  prep_json TEXT NOT NULL,
  outputs_json TEXT NOT NULL,
  revision INTEGER NOT NULL DEFAULT 1,
  hash TEXT NOT NULL,
  trusted INTEGER NOT NULL DEFAULT 0,
  created_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS verifications (
  id TEXT PRIMARY KEY,
  task_id TEXT NOT NULL REFERENCES tasks(id),
  snapshot_id TEXT NOT NULL,
  profile_id TEXT NOT NULL,
  profile_hash TEXT NOT NULL,
  task_revision INTEGER NOT NULL,
  environment_epoch INTEGER NOT NULL,
  candidate_epoch INTEGER NOT NULL,
  tool_identity_json TEXT NOT NULL,
  results_json TEXT NOT NULL,
  round_status TEXT NOT NULL,
  created_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS handoffs (
  id TEXT PRIMARY KEY,
  task_id TEXT NOT NULL REFERENCES tasks(id),
  task_revision INTEGER NOT NULL,
  candidate_snapshot TEXT NOT NULL,
  parent_handoff TEXT,
  source_mode TEXT NOT NULL,
  body_json TEXT NOT NULL,
  edited_fields_json TEXT NOT NULL DEFAULT '[]',
  receipt_json TEXT NOT NULL,
  package_sha256 TEXT NOT NULL,
  package_path TEXT NOT NULL,
  created_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS imported_reports (
  id TEXT PRIMARY KEY,
  task_id TEXT REFERENCES tasks(id),
  handoff_id TEXT,
  receipt_json TEXT NOT NULL,
  claims_json TEXT NOT NULL,
  origin TEXT NOT NULL,
  association TEXT NOT NULL DEFAULT 'linked',
  note TEXT NOT NULL DEFAULT '',
  created_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS acceptances (
  id TEXT PRIMARY KEY,
  task_id TEXT NOT NULL REFERENCES tasks(id),
  task_revision INTEGER NOT NULL,
  snapshot_id TEXT NOT NULL,
  profile_hash TEXT NOT NULL,
  environment_epoch INTEGER NOT NULL,
  evidence_ids TEXT NOT NULL,
  accepted_by TEXT NOT NULL DEFAULT 'local_user',
  created_at_ms INTEGER NOT NULL,
  UNIQUE(task_id, task_revision, snapshot_id, profile_hash, environment_epoch)
);
CREATE TABLE IF NOT EXISTS operations (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  task_id TEXT,
  idempotency_key TEXT UNIQUE,
  request_digest TEXT NOT NULL DEFAULT '',
  state TEXT NOT NULL,
  result_json TEXT,
  error_json TEXT,
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  kind TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  created_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
  id TEXT PRIMARY KEY,
  csrf TEXT NOT NULL,
  created_at_ms INTEGER NOT NULL,
  last_seen_ms INTEGER NOT NULL,
  abs_expire_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS bootstrap_tokens (
  token_hash TEXT PRIMARY KEY,
  used INTEGER NOT NULL DEFAULT 0,
  created_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS register_claims (
  code TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  used INTEGER NOT NULL DEFAULT 0,
  created_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS settings (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
"#;

impl Store {
    pub fn open(data_dir: &Path) -> RelayResult<Store> {
        std::fs::create_dir_all(data_dir.join("db"))?;
        let path = data_dir.join("db").join("relaycheck.db");
        let conn = Connection::open(&path)
            .map_err(|e| RelayError::internal(format!("cannot open database: {e}")))?;
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "foreign_keys", "ON").ok();
        conn.pragma_update(None, "busy_timeout", 5000).ok();
        conn.pragma_update(None, "synchronous", "NORMAL").ok();
        conn.execute_batch(SCHEMA_SQL)?;
        conn.execute(
            "INSERT OR IGNORE INTO settings(key,value) VALUES('environment_epoch','1')",
            [],
        )?;
        Ok(Store { conn: Mutex::new(conn), data_dir: data_dir.to_path_buf() })
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("store mutex poisoned")
    }

    pub fn get_setting(&self, key: &str) -> Option<String> {
        self.lock()
            .query_row("SELECT value FROM settings WHERE key=?1", params![key], |r| {
                r.get::<_, String>(0)
            })
            .optional()
            .ok()
            .flatten()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> RelayResult<()> {
        self.lock().execute(
            "INSERT INTO settings(key,value) VALUES(?1,?2)
             ON CONFLICT(key) DO UPDATE SET value=?2",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn environment_epoch(&self) -> i64 {
        self.get_setting("environment_epoch")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1)
    }

    pub fn bump_environment_epoch(&self) -> i64 {
        let epoch = self.environment_epoch() + 1;
        let _ = self.set_setting("environment_epoch", &epoch.to_string());
        epoch
    }

    // ---- events ----

    pub fn append_event(&self, kind: &str, payload: &serde_json::Value) -> RelayResult<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO events(kind,payload_json,created_at_ms) VALUES(?1,?2,?3)",
            params![kind, payload.to_string(), now_ms()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn events_after(&self, after: i64, limit: i64) -> RelayResult<Vec<(i64, String, String)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT seq,kind,payload_json FROM events WHERE seq>?1 ORDER BY seq LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![after, limit], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn event_bounds(&self) -> RelayResult<(i64, i64)> {
        let conn = self.lock();
        let min: Option<i64> = conn
            .query_row("SELECT MIN(seq) FROM events", [], |r| r.get(0))
            .optional()?
            .flatten();
        let max: Option<i64> = conn
            .query_row("SELECT MAX(seq) FROM events", [], |r| r.get(0))
            .optional()?
            .flatten();
        Ok((min.unwrap_or(0), max.unwrap_or(0)))
    }

    // ---- projects ----

    pub fn insert_project(
        &self,
        id: &str,
        display_name: &str,
        repo_root: &str,
        common_dir: &str,
        head_commit: &str,
    ) -> RelayResult<()> {
        self.lock().execute(
            "INSERT INTO projects(id,display_name,repo_root,common_dir,head_commit,registered_via,trust_confirmed,created_at_ms)
             VALUES(?1,?2,?3,?4,?5,'cli',1,?6)",
            params![id, display_name, repo_root, common_dir, head_commit, now_ms()],
        )?;
        Ok(())
    }

    pub fn project_by_root(&self, repo_root: &str) -> Option<(String, String)> {
        self.lock()
            .query_row(
                "SELECT id,display_name FROM projects WHERE repo_root=?1",
                params![repo_root],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()
            .ok()
            .flatten()
    }

    pub fn project_root(&self, project_id: &str) -> RelayResult<String> {
        self.lock()
            .query_row(
                "SELECT repo_root FROM projects WHERE id=?1",
                params![project_id],
                |r| r.get::<_, String>(0),
            )
            .map_err(|_| RelayError::not_found("project not found"))
    }

    pub fn list_projects(&self) -> RelayResult<Vec<serde_json::Value>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id,display_name,repo_root,head_commit,created_at_ms FROM projects ORDER BY created_at_ms",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, String>(0)?,
                    "display_name": r.get::<_, String>(1)?,
                    "repo_root": r.get::<_, String>(2)?,
                    "head_commit": r.get::<_, String>(3)?,
                    "created_at_ms": r.get::<_, i64>(4)?,
                }))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ---- tasks ----

    pub fn insert_task(
        &self,
        id: &str,
        project_id: &str,
        goal: &str,
        constraints: &[String],
        criteria: &[Criterion],
        is_demo: bool,
    ) -> RelayResult<()> {
        let now = now_ms();
        self.lock().execute(
            "INSERT INTO tasks(id,project_id,goal,constraints_json,criteria_json,workflow_state,is_demo,created_at_ms,updated_at_ms)
             VALUES(?1,?2,?3,?4,?5,'draft',?6,?7,?7)",
            params![
                id,
                project_id,
                goal,
                serde_json::to_string(constraints).unwrap_or_default(),
                serde_json::to_string(criteria).unwrap_or_default(),
                is_demo as i64,
                now
            ],
        )?;
        Ok(())
    }

    pub fn task_field(&self, task_id: &str, field: &str) -> RelayResult<String> {
        // field is an internal constant, never user input.
        let allowed = [
            "id",
            "project_id",
            "goal",
            "constraints_json",
            "criteria_json",
            "active_snapshot",
            "parent_handoff",
            "profile_id",
            "workflow_state",
        ];
        if !allowed.contains(&field) {
            return Err(RelayError::internal("bad field name"));
        }
        let sql = format!("SELECT {field} FROM tasks WHERE id=?1");
        self.lock()
            .query_row(&sql, params![task_id], |r| r.get::<_, Option<String>>(0))
            .optional()?
            .flatten()
            .ok_or_else(|| RelayError::not_found("task not found"))
    }

    /// Nullable column read: distinguishes "task missing" from "column NULL".
    pub fn task_field_opt(&self, task_id: &str, field: &str) -> RelayResult<Option<String>> {
        let allowed = [
            "id",
            "project_id",
            "goal",
            "constraints_json",
            "criteria_json",
            "active_snapshot",
            "parent_handoff",
            "profile_id",
            "workflow_state",
        ];
        if !allowed.contains(&field) {
            return Err(RelayError::internal("bad field name"));
        }
        let sql = format!("SELECT {field} FROM tasks WHERE id=?1");
        let row = self
            .lock()
            .query_row(&sql, params![task_id], |r| r.get::<_, Option<String>>(0))
            .optional()?;
        row.ok_or_else(|| RelayError::not_found("task not found"))
    }

    pub fn task_int(&self, task_id: &str, field: &str) -> RelayResult<i64> {
        let allowed = ["revision", "candidate_epoch", "is_demo"];
        if !allowed.contains(&field) {
            return Err(RelayError::internal("bad field name"));
        }
        let sql = format!("SELECT {field} FROM tasks WHERE id=?1");
        self.lock()
            .query_row(&sql, params![task_id], |r| r.get::<_, i64>(0))
            .map_err(|_| RelayError::not_found("task not found"))
    }

    pub fn update_task_fields(&self, task_id: &str, sets: &[(&str, serde_json::Value)]) -> RelayResult<()> {
        let allowed = [
            "goal",
            "constraints_json",
            "criteria_json",
            "active_snapshot",
            "parent_handoff",
            "profile_id",
            "workflow_state",
            "revision",
            "candidate_epoch",
            "updated_at_ms",
        ];
        let mut sql = String::from("UPDATE tasks SET ");
        for (i, (k, _)) in sets.iter().enumerate() {
            if !allowed.contains(k) {
                return Err(RelayError::internal("bad field name"));
            }
            if i > 0 {
                sql.push_str(", ");
            }
            sql.push_str(k);
            sql.push_str("=?");
            sql.push_str(&(i + 1).to_string());
        }
        sql.push_str(" WHERE id=?");
        sql.push_str(&(sets.len() + 1).to_string());
        let conn = self.lock();
        let mut query = conn.prepare(&sql)?;
        use rusqlite::ToSql;
        let mut raw: Vec<Box<dyn ToSql>> = Vec::new();
        for (_, v) in sets {
            let boxed: Box<dyn ToSql> = match v {
                serde_json::Value::Null => Box::new(Option::<String>::None),
                serde_json::Value::String(s) => Box::new(s.clone()),
                serde_json::Value::Number(n) => {
                    if let Some(i) = n.as_i64() {
                        Box::new(i)
                    } else {
                        Box::new(n.to_string())
                    }
                }
                other => Box::new(other.to_string()),
            };
            raw.push(boxed);
        }
        raw.push(Box::new(task_id.to_string()));
        query.execute(rusqlite::params_from_iter(raw.iter().map(|b| b.as_ref())))?;
        Ok(())
    }

    pub fn bump_task_revision(&self, task_id: &str) -> RelayResult<i64> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET revision=revision+1, updated_at_ms=?2 WHERE id=?1",
            params![task_id, now_ms()],
        )?;
        let rev: i64 =
            conn.query_row("SELECT revision FROM tasks WHERE id=?1", params![task_id], |r| {
                r.get(0)
            })?;
        Ok(rev)
    }

    pub fn bump_candidate_epoch(&self, task_id: &str) -> RelayResult<i64> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET candidate_epoch=candidate_epoch+1 WHERE id=?1",
            params![task_id],
        )?;
        let epoch: i64 = conn.query_row(
            "SELECT candidate_epoch FROM tasks WHERE id=?1",
            params![task_id],
            |r| r.get(0),
        )?;
        Ok(epoch)
    }

    pub fn task_snapshot(&self, task_id: &str) -> RelayResult<serde_json::Value> {
        let conn = self.lock();
        let row = conn
            .query_row(
                "SELECT t.id,t.project_id,t.goal,t.constraints_json,t.criteria_json,t.revision,
                        t.active_snapshot,t.parent_handoff,t.profile_id,t.candidate_epoch,
                        t.workflow_state,t.is_demo,t.created_at_ms,t.updated_at_ms,
                        p.display_name
                 FROM tasks t JOIN projects p ON p.id=t.project_id WHERE t.id=?1",
                params![task_id],
                |r| {
                    Ok(serde_json::json!({
                        "id": r.get::<_, String>(0)?,
                        "project_id": r.get::<_, String>(1)?,
                        "goal": r.get::<_, String>(2)?,
                        "constraints": serde_json::from_str::<Vec<String>>(&r.get::<_, String>(3)?).unwrap_or_default(),
                        "criteria": serde_json::from_str::<Vec<Criterion>>(&r.get::<_, String>(4)?).unwrap_or_default(),
                        "revision": r.get::<_, i64>(5)?,
                        "active_snapshot": r.get::<_, Option<String>>(6)?,
                        "parent_handoff": r.get::<_, Option<String>>(7)?,
                        "profile_id": r.get::<_, Option<String>>(8)?,
                        "candidate_epoch": r.get::<_, i64>(9)?,
                        "workflow_state": r.get::<_, String>(10)?,
                        "is_demo": r.get::<_, i64>(11)? != 0,
                        "created_at_ms": r.get::<_, i64>(12)?,
                        "updated_at_ms": r.get::<_, i64>(13)?,
                        "project_name": r.get::<_, String>(14)?,
                    }))
                },
            )
            .optional()?;
        row.ok_or_else(|| RelayError::not_found("task not found"))
    }

    pub fn list_tasks(&self) -> RelayResult<Vec<serde_json::Value>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT t.id,t.project_id,t.goal,t.revision,t.active_snapshot,t.workflow_state,t.is_demo,
                    t.parent_handoff,t.updated_at_ms,p.display_name
             FROM tasks t JOIN projects p ON p.id=t.project_id
             ORDER BY t.created_at_ms DESC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, String>(0)?,
                    "project_id": r.get::<_, String>(1)?,
                    "goal": r.get::<_, String>(2)?,
                    "revision": r.get::<_, i64>(3)?,
                    "active_snapshot": r.get::<_, Option<String>>(4)?,
                    "workflow_state": r.get::<_, String>(5)?,
                    "is_demo": r.get::<_, i64>(6)? != 0,
                    "parent_handoff": r.get::<_, Option<String>>(7)?,
                    "updated_at_ms": r.get::<_, i64>(8)?,
                    "project_name": r.get::<_, String>(9)?,
                }))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ---- snapshots ----

    pub fn insert_snapshot(
        &self,
        id: &str,
        project_id: &str,
        base_head: &str,
        parent_snapshot: Option<&str>,
        dirty: bool,
        manifest: &SnapshotManifest,
        manifest_json: &str,
        manifest_sha256: &str,
    ) -> RelayResult<()> {
        let total_size: i64 = manifest.files.iter().map(|f| f.size).sum();
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO snapshots(id,project_id,base_head,parent_snapshot,dirty,manifest_json,manifest_sha256,total_size,file_count,created_at_ms)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                id,
                project_id,
                base_head,
                parent_snapshot,
                dirty as i64,
                manifest_json,
                manifest_sha256,
                total_size,
                manifest.files.len() as i64,
                now_ms()
            ],
        )?;
        for f in &manifest.files {
            tx.execute(
                "INSERT INTO snapshot_files(snapshot_id,path,blob_sha256,size,mode) VALUES(?1,?2,?3,?4,?5)",
                params![id, f.path, f.sha256, f.size, f.mode],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn snapshot_exists(&self, id: &str) -> bool {
        self.lock()
            .query_row("SELECT 1 FROM snapshots WHERE id=?1", params![id], |_| Ok(()))
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    pub fn snapshot_json(&self, id: &str) -> RelayResult<serde_json::Value> {
        let conn = self.lock();
        let row: Option<(String, String, String, Option<String>, i64, String, i64, i64, i64)> = conn
            .query_row(
                "SELECT id,project_id,base_head,parent_snapshot,dirty,manifest_json,total_size,file_count,created_at_ms
                 FROM snapshots WHERE id=?1",
                params![id],
                |r| {
                    Ok((
                        r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?,
                        r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?,
                    ))
                },
            )
            .optional()?;
        let Some((sid, project_id, base_head, parent, dirty, manifest_json, total, count, created)) = row
        else {
            return Err(RelayError::not_found("snapshot not found"));
        };
        let manifest: SnapshotManifest = serde_json::from_str(&manifest_json)
            .map_err(|e| RelayError::new(crate::errors::Code::ECorrupt, format!("stored manifest unreadable: {e}")))?;
        Ok(serde_json::json!({
            "id": sid,
            "project_id": project_id,
            "base_head": base_head,
            "parent_snapshot": parent,
            "dirty": dirty != 0,
            "manifest": manifest,
            "total_size": total,
            "file_count": count,
            "created_at_ms": created,
        }))
    }

    pub fn snapshot_files(&self, id: &str) -> RelayResult<Vec<ManifestFile>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT path,blob_sha256,size,mode FROM snapshot_files WHERE snapshot_id=?1 ORDER BY path",
        )?;
        let rows = stmt
            .query_map(params![id], |r| {
                Ok(ManifestFile {
                    path: r.get(0)?,
                    sha256: r.get(1)?,
                    size: r.get(2)?,
                    mode: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn excluded_for_snapshot(&self, id: &str) -> RelayResult<Vec<ExcludedEntry>> {
        let snap = self.snapshot_json(id)?;
        let m: SnapshotManifest =
            serde_json::from_value(snap.get("manifest").cloned().unwrap_or_default())?;
        Ok(m.excluded)
    }

    // ---- profiles ----

    pub fn insert_profile(&self, p: &CheckProfile, project_id: &str) -> RelayResult<()> {
        self.lock().execute(
            "INSERT INTO check_profiles(id,project_id,name,checks_json,prep_json,outputs_json,revision,hash,trusted,created_at_ms)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,0,?9)",
            params![
                p.id,
                project_id,
                p.name,
                serde_json::to_string(&p.checks)?,
                serde_json::to_string(&p.prep)?,
                serde_json::to_string(&p.outputs)?,
                p.revision,
                p.hash,
                now_ms()
            ],
        )?;
        Ok(())
    }

    pub fn profile_json(&self, profile_id: &str) -> RelayResult<CheckProfile> {
        let conn = self.lock();
        let (checks, prep, outputs, name, hash, revision): (String, String, String, String, String, i64) = conn
            .query_row(
                "SELECT checks_json,prep_json,outputs_json,name,hash,revision FROM check_profiles WHERE id=?1",
                params![profile_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .map_err(|_| RelayError::not_found("profile not found"))?;
        Ok(CheckProfile {
            id: profile_id.to_string(),
            name,
            checks: serde_json::from_str(&checks)?,
            prep: serde_json::from_str(&prep)?,
            outputs: serde_json::from_str(&outputs)?,
            revision,
            hash,
        })
    }

    pub fn profile_trusted(&self, profile_id: &str) -> RelayResult<bool> {
        let conn = self.lock();
        let v: i64 = conn
            .query_row(
                "SELECT trusted FROM check_profiles WHERE id=?1",
                params![profile_id],
                |r| r.get(0),
            )
            .map_err(|_| RelayError::not_found("profile not found"))?;
        Ok(v != 0)
    }

    pub fn set_profile_trusted(&self, profile_id: &str, trusted: bool) -> RelayResult<()> {
        self.lock().execute(
            "UPDATE check_profiles SET trusted=?2 WHERE id=?1",
            params![profile_id, trusted as i64],
        )?;
        Ok(())
    }

    // ---- verifications ----

    pub fn insert_verification(&self, v: &VerificationRecord, profile_id: &str) -> RelayResult<()> {
        self.lock().execute(
            "INSERT INTO verifications(id,task_id,snapshot_id,profile_id,profile_hash,task_revision,environment_epoch,candidate_epoch,tool_identity_json,results_json,round_status,created_at_ms)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                v.id,
                v.task_id,
                v.snapshot_id,
                profile_id,
                v.profile_hash,
                v.task_revision,
                v.environment_epoch,
                v.candidate_epoch,
                serde_json::to_string(&v.tool_identity)?,
                serde_json::to_string(&v.results)?,
                v.round_status,
                v.created_at_ms
            ],
        )?;
        Ok(())
    }

    pub fn verifications_for_task(&self, task_id: &str) -> RelayResult<Vec<VerificationRecord>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id,snapshot_id,profile_hash,task_revision,environment_epoch,candidate_epoch,
                    tool_identity_json,results_json,round_status,created_at_ms
             FROM verifications WHERE task_id=?1 ORDER BY created_at_ms DESC",
        )?;
        let rows = stmt
            .query_map(params![task_id], row_to_verification)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn latest_valid_verification(
        &self,
        task_id: &str,
        snapshot_id: &str,
    ) -> RelayResult<Option<VerificationRecord>> {
        let conn = self.lock();
        let (rev, epoch, cepoch, profile_id): (i64, i64, i64, Option<String>) = conn.query_row(
            "SELECT revision,(SELECT CAST(value AS INTEGER) FROM settings WHERE key='environment_epoch'),candidate_epoch,profile_id FROM tasks WHERE id=?1",
            params![task_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let Some(profile_id) = profile_id else { return Ok(None) };
        let profile_hash: String = conn.query_row(
            "SELECT hash FROM check_profiles WHERE id=?1",
            params![profile_id],
            |r| r.get(0),
        )?;
        let row = conn
            .query_row(
                "SELECT id,snapshot_id,profile_hash,task_revision,environment_epoch,candidate_epoch,
                        tool_identity_json,results_json,round_status,created_at_ms
                 FROM verifications
                 WHERE task_id=?1 AND snapshot_id=?2 AND profile_hash=?3 AND task_revision=?4
                       AND environment_epoch=?5 AND candidate_epoch=?6 AND round_status='passed'
                 ORDER BY created_at_ms DESC LIMIT 1",
                params![task_id, snapshot_id, profile_hash, rev, epoch, cepoch],
                row_to_verification,
            )
            .optional()?;
        Ok(row)
    }

    // ---- handoffs ----

    pub fn insert_handoff(
        &self,
        meta: &HandoffMeta,
        receipt: &Receipt,
        package_sha256: &str,
        package_path: &str,
    ) -> RelayResult<()> {
        self.lock().execute(
            "INSERT INTO handoffs(id,task_id,task_revision,candidate_snapshot,parent_handoff,source_mode,body_json,edited_fields_json,receipt_json,package_sha256,package_path,created_at_ms)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                meta.handoff_id,
                meta.task_id,
                meta.task_revision,
                meta.candidate_snapshot,
                meta.parent_handoff,
                meta.source_mode,
                serde_json::to_string(&meta.body)?,
                serde_json::to_string(&meta.edited_fields)?,
                serde_json::to_string(receipt)?,
                package_sha256,
                package_path,
                meta.created_at_ms
            ],
        )?;
        Ok(())
    }

    pub fn handoff_json(&self, handoff_id: &str) -> RelayResult<serde_json::Value> {
        let conn = self.lock();
        let row = conn
            .query_row(
                "SELECT id,task_id,task_revision,candidate_snapshot,parent_handoff,source_mode,
                        body_json,edited_fields_json,receipt_json,package_sha256,created_at_ms
                 FROM handoffs WHERE id=?1",
                params![handoff_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?,
                        r.get::<_, String>(3)?, r.get::<_, Option<String>>(4)?,
                        r.get::<_, String>(5)?, r.get::<_, String>(6)?,
                        r.get::<_, String>(7)?, r.get::<_, String>(8)?,
                        r.get::<_, String>(9)?, r.get::<_, i64>(10)?,
                    ))
                },
            )
            .optional()?;
        let Some((hid, task_id, rev, snap, parent, mode, body_s, edited_s, receipt_s, sha, created)) = row
        else {
            return Err(RelayError::not_found("handoff not found"));
        };
        let body: HandoffBody = serde_json::from_str(&body_s)
            .map_err(|e| RelayError::new(crate::errors::Code::ECorrupt, format!("stored handoff body unreadable: {e}")))?;
        let edited: Vec<String> = serde_json::from_str(&edited_s).unwrap_or_default();
        let receipt: Receipt = serde_json::from_str(&receipt_s)
            .map_err(|e| RelayError::new(crate::errors::Code::ECorrupt, format!("stored receipt unreadable: {e}")))?;
        Ok(serde_json::json!({
            "id": hid,
            "task_id": task_id,
            "task_revision": rev,
            "candidate_snapshot": snap,
            "parent_handoff": parent,
            "source_mode": mode,
            "body": body,
            "edited_fields": edited,
            "receipt": receipt,
            "package_sha256": sha,
            "created_at_ms": created,
        }))
    }

    pub fn handoff_package_path(&self, handoff_id: &str) -> RelayResult<PathBuf> {
        let conn = self.lock();
        let p: String = conn
            .query_row(
                "SELECT package_path FROM handoffs WHERE id=?1",
                params![handoff_id],
                |r| r.get(0),
            )
            .map_err(|_| RelayError::not_found("handoff not found"))?;
        Ok(PathBuf::from(p))
    }

    pub fn handoffs_for_task(&self, task_id: &str) -> RelayResult<Vec<String>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT id FROM handoffs WHERE task_id=?1 ORDER BY created_at_ms")?;
        let rows = stmt
            .query_map(params![task_id], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ---- imported reports ----

    pub fn insert_imported_report(
        &self,
        id: &str,
        task_id: Option<&str>,
        handoff_id: Option<&str>,
        receipt_json: &str,
        claims_json: &str,
        origin: &str,
        association: &str,
        note: &str,
    ) -> RelayResult<()> {
        self.lock().execute(
            "INSERT INTO imported_reports(id,task_id,handoff_id,receipt_json,claims_json,origin,association,note,created_at_ms)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![id, task_id, handoff_id, receipt_json, claims_json, origin, association, note, now_ms()],
        )?;
        Ok(())
    }

    pub fn imported_reports_for_task(&self, task_id: &str) -> RelayResult<Vec<serde_json::Value>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id,handoff_id,receipt_json,claims_json,origin,association,note,created_at_ms
             FROM imported_reports WHERE task_id=?1 ORDER BY created_at_ms",
        )?;
        let rows = stmt
            .query_map(params![task_id], |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, String>(0)?,
                    "handoff_id": r.get::<_, Option<String>>(1)?,
                    "receipt": serde_json::from_str::<serde_json::Value>(&r.get::<_, String>(2)?).unwrap_or_default(),
                    "claims": serde_json::from_str::<serde_json::Value>(&r.get::<_, String>(3)?).unwrap_or_default(),
                    "origin": r.get::<_, String>(4)?,
                    "association": r.get::<_, String>(5)?,
                    "note": r.get::<_, String>(6)?,
                    "created_at_ms": r.get::<_, i64>(7)?,
                }))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ---- acceptances ----

    pub fn acceptance_exists(
        &self,
        task_id: &str,
        revision: i64,
        snapshot_id: &str,
        profile_hash: &str,
        epoch: i64,
    ) -> RelayResult<bool> {
        let conn = self.lock();
        let found = conn
            .query_row(
                "SELECT 1 FROM acceptances WHERE task_id=?1 AND task_revision=?2 AND snapshot_id=?3 AND profile_hash=?4 AND environment_epoch=?5",
                params![task_id, revision, snapshot_id, profile_hash, epoch],
                |_| Ok(()),
            )
            .optional()?;
        Ok(found.is_some())
    }

    pub fn insert_acceptance(
        &self,
        id: &str,
        task_id: &str,
        revision: i64,
        snapshot_id: &str,
        profile_hash: &str,
        epoch: i64,
        evidence_ids: &str,
    ) -> RelayResult<bool> {
        let conn = self.lock();
        // CAS inside one transaction: revision and active snapshot must be
        // unchanged and no acceptance for the same key exists.
        let tx = conn.unchecked_transaction()?;
        let (cur_rev, cur_snap): (i64, Option<String>) = tx.query_row(
            "SELECT revision,active_snapshot FROM tasks WHERE id=?1",
            params![task_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if cur_rev != revision || cur_snap.as_deref() != Some(snapshot_id) {
            return Ok(false);
        }
        tx.execute(
            "INSERT OR IGNORE INTO acceptances(id,task_id,task_revision,snapshot_id,profile_hash,environment_epoch,evidence_ids,accepted_by,created_at_ms)
             VALUES(?1,?2,?3,?4,?5,?6,?7,'local_user',?8)",
            params![id, task_id, revision, snapshot_id, profile_hash, epoch, evidence_ids, now_ms()],
        )?;
        let inserted: i64 =
            tx.query_row("SELECT changes()", [], |r| r.get(0))?;
        if inserted == 0 {
            return Ok(false);
        }
        tx.execute(
            "UPDATE tasks SET workflow_state='accepted', updated_at_ms=?2 WHERE id=?1",
            params![task_id, now_ms()],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn acceptances_for_task(&self, task_id: &str) -> RelayResult<Vec<serde_json::Value>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id,task_revision,snapshot_id,profile_hash,environment_epoch,evidence_ids,created_at_ms
             FROM acceptances WHERE task_id=?1 ORDER BY created_at_ms",
        )?;
        let rows = stmt
            .query_map(params![task_id], |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, String>(0)?,
                    "task_revision": r.get::<_, i64>(1)?,
                    "snapshot_id": r.get::<_, String>(2)?,
                    "profile_hash": r.get::<_, String>(3)?,
                    "environment_epoch": r.get::<_, i64>(4)?,
                    "evidence_ids": r.get::<_, String>(5)?,
                    "created_at_ms": r.get::<_, i64>(6)?,
                }))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ---- operations ----

    pub fn insert_operation(
        &self,
        id: &str,
        kind: &str,
        task_id: Option<&str>,
        idempotency_key: Option<&str>,
        request_digest: &str,
    ) -> RelayResult<()> {
        let now = now_ms();
        self.lock().execute(
            "INSERT INTO operations(id,kind,task_id,idempotency_key,request_digest,state,created_at_ms,updated_at_ms)
             VALUES(?1,?2,?3,?4,?5,'pending',?6,?6)",
            params![id, kind, task_id, idempotency_key, request_digest, now],
        )?;
        Ok(())
    }

    pub fn operation_state(&self, op_id: &str) -> RelayResult<serde_json::Value> {
        let conn = self.lock();
        let row: Option<(String, String, Option<String>, String, Option<String>, Option<String>, i64, i64)> = conn
            .query_row(
                "SELECT id,kind,task_id,state,result_json,error_json,created_at_ms,updated_at_ms
                 FROM operations WHERE id=?1",
                params![op_id],
                |r| {
                    Ok((
                        r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?,
                        r.get(6)?, r.get(7)?,
                    ))
                },
            )
            .optional()?;
        let Some((id, kind, task_id, state, result, error, created, updated)) = row else {
            return Err(RelayError::not_found("operation not found"));
        };
        let parse = |s: &Option<String>| s.clone().and_then(|x| serde_json::from_str::<serde_json::Value>(&x).ok());
        Ok(serde_json::json!({
            "id": id,
            "kind": kind,
            "task_id": task_id,
            "state": state,
            "result": parse(&result),
            "error": parse(&error),
            "created_at_ms": created,
            "updated_at_ms": updated,
        }))
    }

    pub fn update_operation(
        &self,
        op_id: &str,
        state: &str,
        result_json: Option<&str>,
        error_json: Option<&str>,
    ) -> RelayResult<()> {
        self.lock().execute(
            "UPDATE operations SET state=?2,result_json=?3,error_json=?4,updated_at_ms=?5 WHERE id=?1",
            params![op_id, state, result_json, error_json, now_ms()],
        )?;
        Ok(())
    }

    pub fn find_operation_by_idempotency(&self, key: &str) -> RelayResult<Option<(String, String, String, String)>> {
        let conn = self.lock();
        let row = conn
            .query_row(
                "SELECT id,kind,state,request_digest FROM operations WHERE idempotency_key=?1",
                params![key],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)),
            )
            .optional()?;
        Ok(row)
    }

    /// RC09: full operation row for an idempotency key — the replay path
    /// survives restarts because it reads the DB, not a memory cache.
    pub fn operation_by_key(&self, key: &str) -> RelayResult<Option<serde_json::Value>> {
        let conn = self.lock();
        let id: Option<String> = conn
            .query_row(
                "SELECT id FROM operations WHERE idempotency_key=?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?;
        drop(conn);
        match id {
            Some(id) => Ok(Some(self.operation_state(&id)?)),
            None => Ok(None),
        }
    }

    /// Operations that looked active when the process died are marked
    /// interrupted at startup — never auto-rerun (spec §5).
    pub fn mark_stale_running_interrupted(&self) -> RelayResult<usize> {
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE operations SET state='interrupted',
                error_json=json_object('code','E_STOP_UNCONFIRMED','message','service restarted while operation was active; rerun manually'),
                updated_at_ms=?1
             WHERE state IN ('pending','running','stopping','cancelling')",
            params![now_ms()],
        )?;
        Ok(n)
    }

    pub fn running_write_operation(&self) -> RelayResult<Option<String>> {
        let conn = self.lock();
        let row = conn
            .query_row(
                "SELECT id FROM operations WHERE state IN ('pending','running','stopping') AND kind IN ('capture','verify','import') LIMIT 1",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(row)
    }

    // ---- sessions / bootstrap / claims ----

    pub fn insert_session(&self, id: &str, csrf: &str, now: i64, abs_expire: i64) -> RelayResult<()> {
        self.lock().execute(
            "INSERT INTO sessions(id,csrf,created_at_ms,last_seen_ms,abs_expire_ms) VALUES(?1,?2,?3,?3,?4)",
            params![id, csrf, now, abs_expire],
        )?;
        Ok(())
    }

    pub fn session_valid(&self, id: &str, now: i64, idle_ms: i64) -> RelayResult<Option<String>> {
        let conn = self.lock();
        let row: Option<(String, i64, i64)> = conn
            .query_row(
                "SELECT csrf,last_seen_ms,abs_expire_ms FROM sessions WHERE id=?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        Ok(match row {
            Some((csrf, last_seen, abs_expire)) if now <= abs_expire && now - last_seen <= idle_ms => {
                Some(csrf)
            }
            _ => None,
        })
    }

    pub fn touch_session(&self, id: &str, now: i64) -> RelayResult<()> {
        self.lock().execute(
            "UPDATE sessions SET last_seen_ms=?2 WHERE id=?1",
            params![id, now],
        )?;
        Ok(())
    }

    pub fn delete_expired_sessions(&self, now: i64) -> RelayResult<()> {
        self.lock()
            .execute("DELETE FROM sessions WHERE abs_expire_ms<?1", params![now])?;
        Ok(())
    }

    pub fn insert_bootstrap_token(&self, token_hash: &str) -> RelayResult<()> {
        self.lock().execute(
            "INSERT INTO bootstrap_tokens(token_hash,used,created_at_ms) VALUES(?1,0,?2)",
            params![token_hash, now_ms()],
        )?;
        Ok(())
    }

    /// One-time consume: exactly one caller wins the race.
    pub fn consume_bootstrap_token(&self, token_hash: &str, now: i64, max_age_ms: i64) -> RelayResult<bool> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let row: Option<(i64, i64)> = tx
            .query_row(
                "SELECT used,created_at_ms FROM bootstrap_tokens WHERE token_hash=?1",
                params![token_hash],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((used, created)) = row else { return Ok(false) };
        if used != 0 || now - created > max_age_ms {
            return Ok(false);
        }
        tx.execute(
            "UPDATE bootstrap_tokens SET used=1 WHERE token_hash=?1",
            params![token_hash],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn bootstrap_tokens_active(&self, now: i64, max_age_ms: i64) -> RelayResult<usize> {
        let conn = self.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM bootstrap_tokens WHERE used=0 AND created_at_ms>?1",
            params![now - max_age_ms],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    pub fn insert_register_claim(&self, code: &str, project_id: &str) -> RelayResult<()> {
        self.lock().execute(
            "INSERT INTO register_claims(code,project_id,used,created_at_ms) VALUES(?1,?2,0,?3)",
            params![code, project_id, now_ms()],
        )?;
        Ok(())
    }

    pub fn consume_register_claim(&self, code: &str) -> RelayResult<Option<String>> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let row: Option<(i64, String)> = tx
            .query_row(
                "SELECT used,project_id FROM register_claims WHERE code=?1",
                params![code],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((used, project_id)) = row else { return Ok(None) };
        if used != 0 {
            return Ok(None);
        }
        tx.execute("UPDATE register_claims SET used=1 WHERE code=?1", params![code])?;
        tx.commit()?;
        Ok(Some(project_id))
    }
}

fn row_to_verification(r: &Row) -> rusqlite::Result<VerificationRecord> {
    let tool: String = r.get(6)?;
    let results: String = r.get(7)?;
    Ok(VerificationRecord {
        id: r.get(0)?,
        task_id: String::new(),
        snapshot_id: r.get(1)?,
        profile_hash: r.get(2)?,
        task_revision: r.get(3)?,
        environment_epoch: r.get(4)?,
        candidate_epoch: r.get(5)?,
        tool_identity: serde_json::from_str(&tool).unwrap_or_default(),
        results: serde_json::from_str(&results).unwrap_or_default(),
        round_status: r.get(8)?,
        created_at_ms: r.get(9)?,
    })
}

impl CheckResult {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "check_id": self.check_id,
            "status": self.status,
            "exit_code": self.exit_code,
            "summary": self.summary,
            "truncated": self.truncated,
            "redactions": self.redactions,
            "duration_ms": self.duration_ms,
            "detail": self.detail,
        })
    }
}
