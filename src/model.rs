//! Typed models shared by the service, HTTP layer and CLI.

use serde::{Deserialize, Serialize};

pub const SNAPSHOT_SCHEMA_VERSION: i64 = 1;
pub const RECEIPT_SCHEMA_VERSION: i64 = 1;
pub const HANDOFF_MAX_BYTES: usize = 8 * 1024;

pub const MAX_FILE_BYTES: i64 = 10 * 1024 * 1024;
pub const MAX_CANDIDATE_BYTES: i64 = 100 * 1024 * 1024;
pub const MAX_CANDIDATE_FILES: usize = 10_000;
pub const MAX_PACKAGE_EXPANDED_BYTES: i64 = 100 * 1024 * 1024;
pub const MAX_PACKAGE_ENTRIES: usize = 10_000;
pub const MAX_PACKAGE_FILE_BYTES: i64 = 10 * 1024 * 1024;

/// Workflow states (frozen set, spec §3).
pub const WORKFLOW_DRAFT: &str = "draft";
pub const WORKFLOW_CANDIDATE: &str = "candidate";
pub const WORKFLOW_CHECKING: &str = "checking";
pub const WORKFLOW_READY_FOR_REVIEW: &str = "ready_for_review";
pub const WORKFLOW_ACCEPTED: &str = "accepted";
pub const WORKFLOW_REWORK: &str = "rework";

/// Check-step statuses (frozen set, spec §3).
pub const CHECK_PENDING: &str = "pending";
pub const CHECK_RUNNING: &str = "running";
pub const CHECK_PASSED: &str = "passed";
pub const CHECK_FAILED: &str = "failed";
pub const CHECK_BLOCKED: &str = "blocked";
pub const CHECK_INTERRUPTED: &str = "interrupted";
pub const CHECK_INVALIDATED: &str = "invalidated";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Criterion {
    pub id: String,
    pub text: String,
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskDraft {
    pub project_id: String,
    pub goal: String,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub criteria: Vec<Criterion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckStep {
    pub id: String,
    pub exe: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd_relative: String,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_true")]
    pub required: bool,
}

fn default_timeout() -> u64 {
    600
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrepStep {
    pub id: String,
    pub exe: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd_relative: String,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckProfile {
    pub id: String,
    pub name: String,
    pub checks: Vec<CheckStep>,
    #[serde(default)]
    pub prep: Vec<PrepStep>,
    /// Workdir-relative paths that check steps are allowed to write
    /// (in addition to not touching inputs). Empty means no writes allowed.
    #[serde(default)]
    pub outputs: Vec<String>,
    #[serde(default)]
    pub revision: i64,
    #[serde(default)]
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestFile {
    pub path: String,
    pub mode: String,
    pub size: i64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExcludedEntry {
    pub path: String,
    pub reason: String,
}

/// Snapshot manifest — the canonical-JSON payload whose sha256 is the
/// snapshot id. Content-only: base_head/parent relations live in candidate
/// metadata (spec §4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotManifest {
    pub schema_version: i64,
    pub kind: String, // "relaycheck_snapshot"
    pub scope: String,
    pub files: Vec<ManifestFile>,
    pub excluded: Vec<ExcludedEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claim {
    /// e.g. "verification"
    pub kind: String,
    /// "exporter_reported" — receipts cannot originate machine observations
    pub origin: String,
    pub check_id: String,
    pub status: String,
    pub snapshot_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub schema_version: i64,
    pub kind: String, // "handoff" | "return"
    pub handoff_id: String,
    #[serde(default)]
    pub parent_handoff_id: Option<String>,
    pub task_id: String,
    pub task_revision: i64,
    pub snapshot_id: String,
    #[serde(default)]
    pub base_head: String,
    pub source_mode: String, // "context_only" | "portable_source"
    pub coverage: String,
    pub goal: String,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub open_items: Vec<OpenItem>,
    pub next_action: String,
    #[serde(default)]
    pub claims: Vec<Claim>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenItem {
    pub id: String,
    pub text: String,
}

/// The seven-section handoff body (spec §6): 目标/约束/已改/已验证/未决/下一步/证据索引.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffBody {
    pub goal: String,
    pub constraints: Vec<String>,
    /// What changed relative to the parent baseline (machine-filled facts)
    pub changed: Vec<String>,
    /// local_verified_summary: which checks passed on which snapshot —
    /// ONLY rounds valid for the complete current identity (RC10).
    pub verified: Vec<String>,
    /// Past passing rounds explicitly marked NOT valid for the current
    /// candidate (history is kept, never silently dropped).
    #[serde(default)]
    pub verified_historical: Vec<String>,
    pub open_items: Vec<OpenItem>,
    pub next_action: String,
    /// Evidence index lines (receipt.json, manifest.json, ...)
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffMeta {
    pub handoff_id: String,
    pub task_id: String,
    pub task_revision: i64,
    pub candidate_snapshot: String,
    pub parent_handoff: Option<String>,
    pub source_mode: String,
    pub body: HandoffBody,
    /// Fields the user edited by hand (provenance markers)
    #[serde(default)]
    pub edited_fields: Vec<String>,
    pub created_at_ms: i64,
}

/// Status of one check step within a verification round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub check_id: String,
    pub status: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Sanitized bounded output summary (redacted; never the full log).
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub redactions: usize,
    #[serde(default)]
    pub duration_ms: i64,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationRecord {
    pub id: String,
    pub task_id: String,
    pub snapshot_id: String,
    pub profile_hash: String,
    pub task_revision: i64,
    pub environment_epoch: i64,
    pub candidate_epoch: i64,
    pub tool_identity: Vec<String>,
    pub results: Vec<CheckResult>,
    pub round_status: String, // passed/failed/blocked/interrupted/invalidated
    pub created_at_ms: i64,
}
