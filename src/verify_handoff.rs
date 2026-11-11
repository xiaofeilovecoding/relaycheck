//! Offline `verify-handoff` (spec ADOPTION R01/R02): compare the files listed
//! in a handoff package's manifest against an explicitly designated Git
//! working directory — by actual bytes, size and SHA-256 — without a web
//! session, without unpacking, without registering anything and without any
//! Git or DB writes.
//!
//! Result is `match`, `drift` or `blocked`. Every manifest path is
//! classified matched / changed / missing / unreadable and the list is
//! sorted by path. The base HEAD comparison is reported independently of
//! the content result; the worktree scope only ever proves the listed
//! files — extra checkout files and Git ancestry stay unverified. Sender
//! claims travel in the receipt as exporter-reported data and are rendered
//! separately from locally computed results.

use crate::errors::{Code, RelayError, RelayResult};
use crate::gitx;
use crate::model::{Receipt, SnapshotManifest, MAX_PACKAGE_EXPANDED_BYTES, MAX_PACKAGE_FILE_BYTES};
use crate::pathsafe;
use crate::zipx;
use serde::Serialize;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const VERIFY_RESULT_SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Clone, Serialize)]
pub struct FileOutcome {
    pub path: String,
    /// matched | changed | missing | unreadable
    pub status: String,
    /// short human summary; never contains file bodies, commands or absolute paths
    pub detail: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Counts {
    pub matched: usize,
    pub changed: usize,
    pub missing: usize,
    pub unreadable: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClaimView {
    pub check_id: String,
    pub status: String,
    pub snapshot_id: String,
    pub origin: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifyOutcome {
    pub schema_version: i64,
    /// match | drift | blocked
    pub result: String,
    pub snapshot_id: String,
    pub source_mode: String,
    pub package_sha256: String,
    /// base_head declared by the receipt (may be empty)
    pub base_head_declared: String,
    /// HEAD observed in the designated checkout, when readable (None = unverified)
    pub base_head_observed: Option<String>,
    /// Some(bool) only when both sides were readable; independent of `result`
    pub base_head_match: Option<bool>,
    pub counts: Counts,
    /// stable sort by path
    pub files: Vec<FileOutcome>,
    /// what this run explicitly did NOT verify
    pub unverified: Vec<String>,
    /// sender-declared claims, presented separately from local results
    pub receipt_claims: Vec<ClaimView>,
}

/// Blocked outcome for any condition where the comparison cannot proceed
/// safely (package invalid, unsafe path, refused read, unreadable file).
fn blocked(message: impl Into<String>) -> RelayError {
    RelayError::new(Code::EUnverifiedImport, message)
}

/// Normalizes a manifest path and refuses traversal, drive/ADS forms,
/// reserved names and Git metadata. Returns the normalized relative path
/// (forward slashes preserved; joined against the root at read time).
fn normalize_manifest_path(raw: &str) -> RelayResult<String> {
    if raw.is_empty() {
        return Err(blocked("manifest contains an empty path"));
    }
    if raw.len() > 512 {
        return Err(blocked("manifest path is too long"));
    }
    let p = raw.replace('\\', "/");
    if p.starts_with('/') {
        return Err(blocked("manifest contains an absolute path"));
    }
    if p.contains(':') {
        return Err(blocked("manifest path uses a drive or stream form"));
    }
    if p.contains('\0') {
        return Err(blocked("manifest path contains a control character"));
    }
    let mut depth: i32 = 0;
    for seg in p.split('/') {
        if seg == ".." {
            depth -= 1;
            if depth < 0 {
                return Err(blocked("manifest path escapes the project root"));
            }
        } else if seg == "." {
            return Err(blocked("manifest path contains a redundant segment"));
        } else {
            depth += 1;
            if seg == ".git" {
                return Err(blocked("manifest lists Git metadata; refusing to verify"));
            }
            pathsafe::check_component(seg).map_err(|e| blocked(format!("manifest path not portable: {}", e.message)))?;
        }
    }
    Ok(p)
}

/// Reads one listed file from the checkout with the confirmed-safe primitives:
/// lexical boundary check, per-level reparse-point refusal down the whole
/// ancestor chain, one handle opened with write-sharing DENIED, that handle's
/// own final-path identity proven against the expected location, a bounded
/// read through that same handle, and a pre/post metadata stability check so
/// a concurrent same-size write is blocked rather than trusted.
enum ReadOutcome {
    Bytes(Vec<u8>),
    Missing,
    /// refused for safety: reparse point, boundary violation, stat failure
    Refused(&'static str),
    /// plain IO failure while reading a regular file
    Unreadable,
}

/// Normalizes an absolute path for identity comparison: canonical-prefix
/// stripped, separators unified, case-insensitive on Windows.
fn normalize_for_identity(p: &Path) -> String {
    let s = p.to_string_lossy().replace('/', "\\");
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s).to_lowercase();
    s.trim_end_matches('\\').to_string()
}

/// The final path of the ACTUAL opened handle (R03: the handle participates
/// in the identity check — a path re-resolution cannot vouch for what was
/// really opened). None when the platform call fails.
#[cfg(windows)]
fn handle_final_path(f: &std::fs::File) -> Option<String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFinalPathNameByHandleW, FILE_NAME_NORMALIZED,
    };
    let mut buf = [0u16; 1024];
    let n = unsafe {
        GetFinalPathNameByHandleW(f.as_raw_handle() as _, buf.as_mut_ptr(), buf.len() as u32, FILE_NAME_NORMALIZED)
    };
    if n == 0 || (n as usize) > buf.len() {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..n as usize]))
}

#[cfg(not(windows))]
fn handle_final_path(_f: &std::fs::File) -> Option<String> {
    None
}

/// Opens a listed file with write sharing denied: while this handle lives,
/// another writer cannot open the file for writing, so the bounded read is
/// stable by construction; a file already open for writing fails to open.
#[cfg(windows)]
fn open_read_handle(joined: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    // FILE_SHARE_READ only: deny write and delete sharing for the read.
    std::fs::OpenOptions::new().read(true).share_mode(1).open(joined)
}

#[cfg(not(windows))]
fn open_read_handle(joined: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(joined)
}

fn read_checked(root_norm: &str, rel: &str) -> ReadOutcome {
    let joined: PathBuf = match pathsafe::join_checked(root_norm, rel) {
        Ok(p) => p,
        Err(_) => return ReadOutcome::Refused("path failed the boundary check"),
    };
    // Existence first: a missing checkout file is a drift signal, never a
    // safety refusal (stat on a non-existent path must not read as "refused").
    match std::fs::metadata(&joined) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ReadOutcome::Missing,
        Err(_) => return ReadOutcome::Unreadable,
        Ok(_) => {}
    }
    // Per-level check: EVERY ancestor directory between the checkout root and
    // the leaf must be a plain directory — a reparse point anywhere along the
    // chain could redirect the read outside the checkout.
    let mut ancestor = joined.clone();
    loop {
        match pathsafe::is_reparse_point(&ancestor) {
            Ok(true) => return ReadOutcome::Refused("path traverses a link or reparse point"),
            Ok(false) => {}
            Err(_) => return ReadOutcome::Refused("path cannot be stat-ed"),
        }
        match ancestor.parent() {
            Some(parent) if parent != ancestor => ancestor = parent.to_path_buf(),
            _ => break,
        }
    }
    // Open ONE handle (write-sharing denied) and prove THE HANDLE's own
    // identity: the final path resolved from the handle must be the expected
    // location. A path-level re-resolution cannot vouch for what was really
    // opened; anything uncertain is refused, never read through.
    let f = match open_read_handle(&joined) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ReadOutcome::Missing,
        Err(_) => {
            // already open for writing elsewhere, or denied: an uncertain
            // read is blocked, never trusted
            return ReadOutcome::Refused("file cannot be opened with exclusive read sharing");
        }
    };
    match handle_final_path(&f) {
        Some(resolved) => {
            if normalize_for_identity(Path::new(&resolved)) != normalize_for_identity(&joined) {
                return ReadOutcome::Refused("read handle resolves off the expected path");
            }
        }
        None => {
            // R04: fail closed — when the handle's own identity cannot be
            // obtained on this platform the read NEVER proceeds; only the
            // non-Windows dev/ino fallback may vouch for the handle.
            #[cfg(windows)]
            {
                return ReadOutcome::Refused("read handle identity cannot be resolved");
            }
            #[cfg(not(windows))]
            {
                use std::os::unix::fs::MetadataExt;
                let handle_md = match f.metadata() {
                    Ok(m) => m,
                    Err(_) => return ReadOutcome::Unreadable,
                };
                let path_md = match std::fs::metadata(&joined) {
                    Ok(m) => m,
                    Err(_) => return ReadOutcome::Unreadable,
                };
                if handle_md.dev() != path_md.dev() || handle_md.ino() != path_md.ino() {
                    return ReadOutcome::Refused("read handle resolves off the expected path");
                }
            }
        }
    }
    let md = match f.metadata() {
        Ok(m) => m,
        Err(_) => return ReadOutcome::Unreadable,
    };
    if !md.is_file() {
        return ReadOutcome::Refused("target is not a regular file");
    }
    let before_len = md.len();
    let before_modified = md.modified().ok();
    let mut buf = Vec::new();
    let mut handle = f;
    let taken = match handle
        .by_ref()
        .take((MAX_PACKAGE_FILE_BYTES + 1) as u64)
        .read_to_end(&mut buf)
    {
        Ok(n) => n,
        Err(_) => return ReadOutcome::Unreadable,
    };
    if taken as i64 > MAX_PACKAGE_FILE_BYTES {
        return ReadOutcome::Refused("file exceeds the per-file read limit");
    }
    // Stability: the same handle must still report the size and modification
    // time it opened with — a same-size concurrent write is detected (mtime)
    // and blocked, never trusted as a match.
    match handle.metadata() {
        Ok(after) => {
            if after.len() != before_len {
                return ReadOutcome::Refused("file changed while being read");
            }
            if let (Some(before), Ok(after_t)) = (before_modified, after.modified()) {
                if before != after_t {
                    return ReadOutcome::Refused("file changed while being read");
                }
            }
        }
        Err(_) => return ReadOutcome::Refused("file stability cannot be confirmed"),
    }
    ReadOutcome::Bytes(buf)
}

/// Public wrappers so the CLI can apply the same handle-identity rules to
/// the ZIP input it reads. Controlled outputs only: normalized path text.
pub fn normalize_path_public(p: &Path) -> String {
    normalize_for_identity(p)
}

#[cfg(windows)]
pub fn handle_final_path_public(f: &std::fs::File) -> Option<String> {
    handle_final_path(f)
}

#[cfg(not(windows))]
pub fn handle_final_path_public(_f: &std::fs::File) -> Option<String> {
    None
}

fn observe_head(repo_root: &str, isolated_home: &Path) -> Option<String> {
    let bytes = gitx::git_run(repo_root, &["rev-parse", "HEAD"], isolated_home).ok()?;
    let head = String::from_utf8_lossy(&bytes).trim().to_lowercase();
    if head.len() == 40 && head.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(head)
    } else {
        None
    }
}

/// Runs the offline comparison. `repo_root` must already be validated with
/// `pathsafe::validate_root`. The ZIP bytes are hashed for binding; the
/// package is inspected with the shared strict validation and never unpacked.
pub fn verify(
    data: &[u8],
    repo_root: &str,
    isolated_home: &Path,
) -> RelayResult<VerifyOutcome> {
    let package_sha256 = crate::ids::sha256_hex(data);
    let (receipt, manifest) = zipx::inspect_package(data).map_err(|e| {
        blocked(format!(
            "package cannot be verified safely: {} {}",
            e.code.as_str(),
            e.message
        ))
    })?;

    // Content identity: the receipt's snapshot id must equal the SHA-256 of
    // the canonical manifest actually in the package — a receipt describing a
    // different manifest can never be verified as a match.
    let manifest_value = serde_json::to_value(&manifest)
        .map_err(|_| blocked("manifest is not representable as canonical JSON"))?;
    let manifest_sha = crate::canonical::canonical_sha256(&manifest_value)
        .map_err(|_| blocked("manifest cannot be canonicalized"))?;
    if receipt.snapshot_id != format!("sha256:{manifest_sha}") {
        return Err(blocked(
            "receipt snapshot id does not match the canonical manifest digest; package is internally inconsistent",
        ));
    }
    // Frozen scope: only the capture-produced scope is interpretable here.
    if manifest.scope != "selected_sources" {
        return Err(blocked("manifest scope is not a recognized capture scope"));
    }

    // Normalize + conflict-check every manifest path before touching disk.
    let mut normalized: Vec<(String, crate::model::ManifestFile)> = Vec::new();
    let mut paths_for_conflict: Vec<String> = Vec::new();
    for f in &manifest.files {
        let rel = normalize_manifest_path(&f.path)?;
        if normalized.iter().any(|(seen, _)| seen == &rel) {
            return Err(blocked("manifest lists the same path twice"));
        }
        paths_for_conflict.push(rel.clone());
        normalized.push((rel, f.clone()));
    }
    if pathsafe::check_case_conflicts(&paths_for_conflict).is_err() {
        return Err(blocked("manifest paths collide case-insensitively"));
    }

    // The root itself must be a plain directory, not a link.
    let root_path = Path::new(repo_root);
    if pathsafe::is_reparse_point(root_path).unwrap_or(true) {
        return Err(blocked("checkout root is a link or reparse point; refusing to verify"));
    }

    // Base HEAD is reported independently of the content comparison. A
    // declared value must at least be a commit-shaped identifier; anything
    // else makes the receipt unverifiable.
    let declared = receipt.base_head.trim().to_lowercase();
    if !declared.is_empty()
        && (declared.len() != 40 || !declared.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(blocked("declared base_head is not a commit id"));
    }
    let observed = observe_head(repo_root, isolated_home);
    let base_head_match = match (declared.is_empty(), &observed) {
        (true, _) => None,
        (_, None) => None,
        (_, Some(o)) => Some(declared == *o),
    };

    // Per-file comparison.
    let mut files: Vec<FileOutcome> = Vec::new();
    let mut counts = Counts::default();
    let mut blocked_reason: Option<String> = None;
    for (rel, f) in &normalized {
        match read_checked(repo_root, rel) {
            ReadOutcome::Bytes(bytes) => {
                let actual = crate::ids::sha256_hex(&bytes);
                if bytes.len() as i64 == f.size && actual == f.sha256 {
                    counts.matched += 1;
                    files.push(FileOutcome {
                        path: rel.clone(),
                        status: "matched".into(),
                        detail: format!("size and sha256 match the manifest ({})", &f.sha256[..12]),
                    });
                } else {
                    counts.changed += 1;
                    files.push(FileOutcome {
                        path: rel.clone(),
                        status: "changed".into(),
                        detail: "size or sha256 differs from the manifest".into(),
                    });
                }
            }
            ReadOutcome::Missing => {
                counts.missing += 1;
                files.push(FileOutcome {
                    path: rel.clone(),
                    status: "missing".into(),
                    detail: "not present in the checkout".into(),
                });
            }
            ReadOutcome::Refused(reason) => {
                if std::env::var_os("RELAYCHECK_VERIFY_DEBUG").is_some() {
                    eprintln!("[verify-handoff][debug] refused {rel}: {reason}");
                }
                return Err(blocked(format!(
                    "listed file refused for safety: {reason}"
                )));
            }
            ReadOutcome::Unreadable => {
                counts.unreadable += 1;
                files.push(FileOutcome {
                    path: rel.clone(),
                    status: "unreadable".into(),
                    detail: "read did not succeed; treated as unverified, never as success".into(),
                });
                blocked_reason.get_or_insert_with(|| "a listed file could not be read".into());
            }
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));

    let result = if blocked_reason.is_some() || counts.unreadable > 0 {
        "blocked"
    } else if counts.changed > 0 || counts.missing > 0 {
        "drift"
    } else {
        "match"
    };

    let mut unverified = vec![
        "files present in the checkout but not listed in the manifest are not verified".into(),
        "git ancestry (history beyond the working tree) is not verified".into(),
    ];
    if observed.is_none() {
        unverified.insert(0, "checkout HEAD could not be observed; base_head comparison not made".into());
    }

    Ok(VerifyOutcome {
        schema_version: VERIFY_RESULT_SCHEMA_VERSION,
        result: result.into(),
        snapshot_id: receipt.snapshot_id.clone(),
        source_mode: receipt.source_mode.clone(),
        package_sha256,
        base_head_declared: receipt.base_head.clone(),
        base_head_observed: observed,
        base_head_match,
        counts,
        files,
        unverified,
        receipt_claims: receipt
            .claims
            .iter()
            .map(|c| ClaimView {
                // sender-declared identifiers are rendered only when they are
                // plain safe identifiers; anything else stays unprintable
                check_id: safe_identifier(&c.check_id),
                status: c.status.clone(),
                snapshot_id: c.snapshot_id.clone(),
                origin: c.origin.clone(),
            })
            .collect(),
    })
}

/// Returns the identifier when it is a plain bounded token (letters, digits
/// and a few separator characters); otherwise a fixed placeholder, so
/// arbitrary sender text never reaches the output.
fn safe_identifier(raw: &str) -> String {
    let ok = !raw.is_empty()
        && raw.len() <= 64
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '@' | '+'));
    if ok {
        raw.to_string()
    } else {
        "(unprintable-id)".to_string()
    }
}

/// Renders the outcome as the human CLI summary. Paths are relative names
/// only; receipt claims are labeled as sender-declared.
pub fn render_summary(o: &VerifyOutcome) -> String {
    let mut s = String::new();
    s.push_str(&format!("verify-handoff: {}\n", o.result));
    s.push_str(&format!(
        "snapshot {} ({}), package sha256 {}\n",
        &o.snapshot_id[..12.min(o.snapshot_id.len())],
        o.source_mode,
        &o.package_sha256[..12]
    ));
    match (&o.base_head_observed, o.base_head_match) {
        (Some(_), Some(true)) => s.push_str("base HEAD: matches the declared base\n"),
        (Some(_), Some(false)) => s.push_str("base HEAD: differs from the declared base\n"),
        (Some(_), None) => s.push_str("base HEAD: declared value missing; not compared\n"),
        (None, _) => s.push_str("base HEAD: could not be observed in the checkout\n"),
    }
    s.push_str(&format!(
        "files: {} matched, {} changed, {} missing, {} unreadable\n",
        o.counts.matched, o.counts.changed, o.counts.missing, o.counts.unreadable
    ));
    for f in &o.files {
        s.push_str(&format!("  {}: {}\n", f.path, f.status));
    }
    for u in &o.unverified {
        s.push_str(&format!("unverified: {u}\n"));
    }
    if !o.receipt_claims.is_empty() {
        s.push_str("sender-declared claims (exporter_reported, not verified by this run):\n");
        for c in &o.receipt_claims {
            s.push_str(&format!("  {}: {} ({})\n", c.check_id, c.status, c.origin));
        }
    }
    s
}

/// Total sanity bound re-used from the package limits (kept local for clarity).
pub fn max_package_bytes() -> i64 {
    MAX_PACKAGE_EXPANDED_BYTES
}

/// Validates the receipt/manifest pair the same way inspect does; exposed for
/// tests that want to drive the shared validation directly.
pub fn inspect(data: &[u8]) -> RelayResult<(Receipt, SnapshotManifest)> {
    zipx::inspect_package(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_rejects_traversal_git_ads_and_reserved() {
        assert!(normalize_manifest_path("docs/plan.md").is_ok());
        assert!(normalize_manifest_path("a/b/c.txt").is_ok());
        for bad in [
            "../escape.txt",
            "/absolute.txt",
            "docs/plan.md:stream",
            ".git/config",
            "sub/.git/HEAD",
            "CON.txt",
            "a<b.txt",
            "trailing.txt ",
            "",
        ] {
            assert!(normalize_manifest_path(bad).is_err(), "must refuse: {bad}");
        }
    }

    #[test]
    fn handle_final_path_vouches_the_opened_file() {
        // deterministic positive: the handle of a really-opened file resolves
        // to the same normalized identity as its path
        let dir = std::env::temp_dir().join(format!("r11-handle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("target.txt");
        std::fs::write(&p, b"content").unwrap();
        let f = open_read_handle(&p).expect("open");
        let resolved = handle_final_path(&f).expect("final path available");
        assert_eq!(
            normalize_for_identity(Path::new(&resolved)),
            normalize_for_identity(&p),
            "the handle identity matches the opened path"
        );
        // a different location must not compare equal (the refusal branch)
        assert_ne!(
            normalize_for_identity(Path::new(&resolved)),
            normalize_for_identity(&dir.join("other.txt")),
        );
        drop(f);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn render_summary_keeps_relative_names_and_separate_claims() {
        let out = VerifyOutcome {
            schema_version: 1,
            result: "drift".into(),
            snapshot_id: format!("sha256:{}", "a".repeat(64)),
            source_mode: "context_only".into(),
            package_sha256: format!("{}", "b".repeat(64)),
            base_head_declared: format!("{}", "c".repeat(40)),
            base_head_observed: Some(format!("{}", "d".repeat(40))),
            base_head_match: Some(false),
            counts: Counts { matched: 1, changed: 1, missing: 0, unreadable: 0 },
            files: vec![FileOutcome {
                path: "docs/plan.md".into(),
                status: "changed".into(),
                detail: "size or sha256 differs".into(),
            }],
            unverified: vec!["git ancestry (history beyond the working tree) is not verified".into()],
            receipt_claims: vec![ClaimView {
                check_id: "unit".into(),
                status: "passed".into(),
                snapshot_id: format!("sha256:{}", "a".repeat(64)),
                origin: "exporter_reported".into(),
            }],
        };
        let s = render_summary(&out);
        assert!(s.contains("verify-handoff: drift"));
        assert!(s.contains("docs/plan.md: changed"));
        assert!(s.contains("differs from the declared base"));
        assert!(s.contains("sender-declared claims"));
        assert!(s.contains("exporter_reported"));
        assert!(!s.contains(&"a".repeat(64)), "snapshot id stays truncated");
    }
}
