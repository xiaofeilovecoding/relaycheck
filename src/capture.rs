//! Snapshot capture (spec §4): tracked files by default, untracked files by
//! explicit selection, secrets blocked, limits enforced, byte fidelity
//! preserved, and a post-copy stability re-check before activation.

use crate::blobstore::BlobStore;
use crate::canonical::canonical_sha256;
use crate::errors::{Code, RelayError, RelayResult};
use crate::gitx;
use crate::ids::{now_ms, sha256_hex};
use crate::model::{
    ExcludedEntry, ManifestFile, SnapshotManifest, MAX_CANDIDATE_BYTES, MAX_CANDIDATE_FILES,
    MAX_FILE_BYTES, SNAPSHOT_SCHEMA_VERSION,
};
use crate::pathsafe;
use crate::secrets;
use std::path::Path;

/// Heuristic secret-path blockers. `.env.example` is allowed but its content
/// still goes through the secret scan (spec §4).
pub fn is_secret_path(rel: &str) -> bool {
    let name = rel.rsplit(['\\', '/']).next().unwrap_or(rel).to_lowercase();
    let lower = rel.to_lowercase();
    for pat in [
        ".env",
        ".env.local",
        ".env.production",
        ".env.development",
        "id_rsa",
        "id_dsa",
        "id_ecdsa",
        "id_ed25519",
        ".git-credentials",
        ".npmrc",
        ".pypirc",
        "credentials.json",
        "secrets.json",
        ".netrc",
        ".htpasswd",
    ] {
        if name == pat {
            return true;
        }
    }
    for ext in [".pem", ".key", ".pfx", ".p12", ".pkbd", ".jks", ".keystore"] {
        if name.ends_with(ext) {
            return true;
        }
    }
    // Any directory component that looks like secret storage.
    for seg in rel.split(['\\', '/']) {
        let s = seg.to_lowercase();
        if s == ".aws" || s == ".ssh" || s == ".gnupg" || s == ".kube" {
            return true;
        }
    }
    if lower.contains("private_key") || lower.contains("credentials") {
        return true;
    }
    false
}

pub struct CapturePlan {
    pub included: Vec<String>,
    pub excluded: Vec<ExcludedEntry>,
    pub base_head: String,
    pub dirty: bool,
}

/// Builds the file selection for a capture: tracked files by default plus
/// explicitly selected untracked files; blocked secret paths are excluded
/// with reasons (never silently dropped), limits are checked up front.
pub fn plan_capture(
    repo_root: &str,
    isolated_home: &Path,
    untracked_selected: &[String],
    extra_excludes: &[String],
) -> RelayResult<CapturePlan> {
    let tracked = gitx::ls_files_cached(repo_root, isolated_home)?;
    let untracked = gitx::ls_untracked(repo_root, isolated_home)?;
    let base_head_bytes = gitx::git_run(repo_root, &["rev-parse", "HEAD"], isolated_home)
        .unwrap_or_default();
    let base_head = String::from_utf8_lossy(&base_head_bytes).trim().to_string();
    let dirty = gitx::is_dirty(repo_root, isolated_home)?;

    let mut selected: Vec<String> = Vec::new();
    let mut excluded: Vec<ExcludedEntry> = Vec::new();

    let mut untracked_set: std::collections::HashSet<String> =
        untracked.iter().cloned().collect();
    for sel in untracked_selected {
        if !untracked_set.contains(sel) {
            return Err(RelayError::validation(format!(
                "selected path is not an untracked file: {}",
                safe_rel(sel)
            )));
        }
    }
    untracked_set.retain(|p| untracked_selected.contains(p));
    let _ = untracked_set;

    let mut all: Vec<String> = Vec::new();
    for (_, p) in tracked {
        all.push(p);
    }
    for p in untracked_selected {
        all.push(p.clone());
    }

    if all.len() > MAX_CANDIDATE_FILES {
        return Err(RelayError::new(
            Code::ELimit,
            format!(
                "candidate would exceed the file limit of {MAX_CANDIDATE_FILES}; narrow the selection"
            ),
        ));
    }

    // Case-insensitive conflict check (Windows file systems).
    pathsafe::check_case_conflicts(&all)?;

    // Per-path boundary validation (components, traversal, ADS, names).
    for rel in &all {
        pathsafe::join_checked(repo_root, rel)?;
    }

    // LFS filter on selected files → reject with actionable message.
    let refs: Vec<&str> = all.iter().map(|s| s.as_str()).collect();
    let attrs = gitx::lfs_attr(repo_root, &refs, isolated_home)?;
    for (rel, attr) in all.iter().zip(attrs) {
        if attr == "lfs" {
            return Err(RelayError::validation(format!(
                "file uses Git LFS which requires a remote; RelayCheck 0.1 captures plain worktree files: {}",
                safe_rel(rel)
            )));
        }
    }

    let exclude_set: std::collections::HashSet<&String> = extra_excludes.iter().collect();
    for rel in all {
        if exclude_set.contains(&rel) {
            excluded.push(ExcludedEntry { path: rel, reason: "user_excluded".into() });
            continue;
        }
        if is_secret_path(&rel) {
            excluded.push(ExcludedEntry {
                path: rel,
                reason: "secret_path_blocked".into(),
            });
            continue;
        }
        selected.push(rel);
    }
    selected.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    Ok(CapturePlan { included: selected, excluded, base_head, dirty })
}

fn safe_rel(p: &str) -> String {
    // Display-safe: only the relative path itself, already validated.
    p.chars().filter(|c| !c.is_control()).collect()
}

pub struct CaptureOutcome {
    pub snapshot_id: String,
    pub manifest_json: String,
    pub file_count: usize,
    pub total_size: i64,
}

/// Runs the full capture: read + hash every file, verify stability after the
/// copy phase, store blobs, then commit the manifest. The original repo is
/// never modified — all IO is read-only.
pub fn capture_from_repo(
    repo_root: &str,
    plan: &CapturePlan,
    blobs: &BlobStore,
    parent_snapshot: Option<&str>,
    scope: &str,
) -> RelayResult<CaptureOutcome> {
    capture_from_paths(Path::new(repo_root), plan, blobs, parent_snapshot, scope)
}

/// Same pipeline over an arbitrary directory (used for portable-source
/// import quarantine packs — still read-only on the source).
pub fn capture_from_paths(
    root: &Path,
    plan: &CapturePlan,
    blobs: &BlobStore,
    parent_snapshot: Option<&str>,
    scope: &str,
) -> RelayResult<CaptureOutcome> {
    if plan.included.len() > MAX_CANDIDATE_FILES {
        return Err(RelayError::new(Code::ELimit, "too many files"));
    }
    let mut files: Vec<ManifestFile> = Vec::with_capacity(plan.included.len());
    let mut total: i64 = 0;

    // First pass: stat + hash + blob store.
    for rel in &plan.included {
        let full = pathsafe::join_checked(&root.to_string_lossy(), rel)?;
        // Reject reparse points on the file and every directory component.
        check_tree_not_reparse(root, &full)?;
        let md = std::fs::metadata(pathsafe::extended_path(&full))
            .map_err(|e| RelayError::validation(format!("cannot read file: {e}")))?;
        if !md.is_file() {
            return Err(RelayError::validation(format!(
                "not a regular file: {}",
                safe_rel(rel)
            )));
        }
        if md.len() > MAX_FILE_BYTES as u64 {
            return Err(RelayError::new(
                Code::ELimit,
                format!(
                    "file exceeds the {MAX_FILE_BYTES}-byte per-file limit: {}",
                    safe_rel(rel)
                ),
            ));
        }
        let (hash, size) = blobs.put_file_streaming(&full, MAX_FILE_BYTES)?;
        total += size;
        if total > MAX_CANDIDATE_BYTES {
            return Err(RelayError::new(
                Code::ELimit,
                format!("candidate exceeds the {MAX_CANDIDATE_BYTES}-byte total limit"),
            ));
        }
        files.push(ManifestFile { path: rel.clone(), mode: "100644".into(), size, sha256: hash });
    }

    // Second pass: identity/size/hash re-check (capture stability).
    for f in &files {
        let full = pathsafe::join_checked(&root.to_string_lossy(), &f.path)?;
        let md = std::fs::metadata(pathsafe::extended_path(&full))
            .map_err(|_| RelayError::new(Code::ECaptureUnstable, "file disappeared during capture"))?;
        if md.len() != f.size as u64 {
            return Err(RelayError::new(
                Code::ECaptureUnstable,
                "file size changed during capture; snapshot aborted",
            ));
        }
        let rehash = crate::ids::sha256_file(&pathsafe::extended_path(&full))?;
        if rehash != f.sha256 {
            return Err(RelayError::new(
                Code::ECaptureUnstable,
                "file content changed during capture; snapshot aborted",
            ));
        }
    }

    // Manifest files are sorted by UTF-8 bytes of the relative path.
    files.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    let manifest = SnapshotManifest {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        kind: "relaycheck_snapshot".into(),
        scope: scope.to_string(),
        files,
        excluded: plan.excluded.clone(),
    };
    let value = serde_json::to_value(&manifest)?;
    let canonical = crate::canonical::canonicalize(&value)?;
    let manifest_json = String::from_utf8(canonical.clone())
        .map_err(|_| RelayError::internal("manifest not utf-8"))?;
    let manifest_sha = sha256_hex(&canonical);
    let snapshot_id = format!("sha256:{manifest_sha}");
    Ok(CaptureOutcome { snapshot_id, manifest_json, file_count: plan.included.len(), total_size: total })
}

fn check_tree_not_reparse(root: &Path, full: &Path) -> RelayResult<()> {
    // Check each directory component between root and file, plus the file.
    let rel = full.strip_prefix(root).unwrap_or(full);
    let mut cur = root.to_path_buf();
    for comp in rel.components() {
        cur.push(comp);
        if pathsafe::is_reparse_point(&cur)? {
            return Err(pathsafe::path_boundary_error(format!(
                "reparse point (symlink/junction) is not accepted: {}",
                comp.as_os_str().to_string_lossy()
            )));
        }
    }
    Ok(())
}

/// Secret-scans every included file before the manifest is committed.
/// A hit blocks the whole capture with the offending path listed — files are
/// never silently dropped (spec R11).
pub fn scan_plan_secrets(root: &Path, plan: &CapturePlan) -> RelayResult<Vec<String>> {
    let mut offenders = Vec::new();
    for rel in &plan.included {
        let full = pathsafe::join_checked(&root.to_string_lossy(), rel)?;
        let data = std::fs::read(pathsafe::extended_path(&full))
            .map_err(|e| RelayError::internal(format!("read failed: {e}")))?;
        if secrets::scan_bytes(&data).is_some() {
            offenders.push(safe_rel(rel));
        }
    }
    Ok(offenders)
}

pub fn manifest_hash(manifest_json: &str) -> RelayResult<String> {
    let value: serde_json::Value = serde_json::from_str(manifest_json)?;
    canonical_sha256(&value).map(|h| h.to_string())
}

pub fn now_ms_pub() -> i64 {
    now_ms()
}
