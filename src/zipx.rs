//! Relay package export/import (spec §6, R04, R08, R13).
//!
//! Export: open ZIP schema 1 — context-only (no source) or portable-source
//! (adds `blobs/<sha256>`). Always: README.md, handoff.md, receipt.json,
//! manifest.json. The bytes are built once and stored immutably; preview and
//! download serve the same stored file.
//!
//! Import: everything is unpacked into a quarantine directory after strict
//! validation. Any bad entry fails the whole package (no partial activation):
//! path traversal, drive/ADS names, duplicate (case-insensitive) entries,
//! symlink entries, declared-size mismatch, zip bombs, unknown schema major,
//! missing blobs. A receipt can only ever produce `external_reported` claims.

use crate::errors::{Code, RelayError, RelayResult};
use crate::model::{
    Claim, ExcludedEntry, ManifestFile, OpenItem, Receipt, SnapshotManifest,
    MAX_PACKAGE_ENTRIES, MAX_PACKAGE_EXPANDED_BYTES, MAX_PACKAGE_FILE_BYTES,
};
use crate::pathsafe;
use std::io::{Read, Write};
use std::path::Path;

pub const PKG_README: &str = include_str!("../assets/package-readme.md");

pub struct ExportInput<'a> {
    pub receipt: &'a Receipt,
    pub handoff_md: &'a str,
    pub manifest: &'a SnapshotManifest,
    pub blobs: Option<&'a crate::blobstore::BlobStore>,
}

/// Builds the ZIP package bytes. Context-only packages carry no source;
/// portable-source packages embed each file's blob (verified by hash on the
/// way in).
pub fn build_package(input: &ExportInput) -> RelayResult<Vec<u8>> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .large_file(false);

        let manifest_value = serde_json::to_value(input.manifest)?;
        let manifest_canon = crate::canonical::canonicalize(&manifest_value)?;
        let receipt_json = serde_json::to_string_pretty(input.receipt)?;

        add_file(&mut zip, "README.md", PKG_README.as_bytes(), opts)?;
        add_file(&mut zip, "handoff.md", input.handoff_md.as_bytes(), opts)?;
        add_file(&mut zip, "receipt.json", receipt_json.as_bytes(), opts)?;
        add_file(&mut zip, "manifest.json", &manifest_canon, opts)?;

        if input.receipt.source_mode == "portable_source" {
            let blobs = input
                .blobs
                .ok_or_else(|| RelayError::internal("portable_source requires blob store"))?;
            let mut total = 0i64;
            for f in &input.manifest.files {
                let data = blobs.get(&f.sha256)?;
                if data.len() as i64 != f.size {
                    return Err(RelayError::new(Code::ECorrupt, "blob size mismatch during export"));
                }
                total += f.size;
                if total > MAX_PACKAGE_EXPANDED_BYTES {
                    return Err(RelayError::new(Code::ELimit, "package exceeds size budget"));
                }
                add_file(&mut zip, &format!("blobs/{}", f.sha256), &data, opts)?;
            }
        }
        zip.finish().map_err(|e| RelayError::internal(format!("zip finish: {e}")))?;
    }
    Ok(buf.into_inner())
}

fn add_file(
    zip: &mut zip::ZipWriter<&mut std::io::Cursor<Vec<u8>>>,
    name: &str,
    data: &[u8],
    opts: zip::write::SimpleFileOptions,
) -> RelayResult<()> {
    zip.start_file(name, opts)
        .map_err(|e| RelayError::internal(format!("zip start_file {name}: {e}")))?;
    zip.write_all(data)
        .map_err(|e| RelayError::internal(format!("zip write {name}: {e}")))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Import validation
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ValidatedPackage {
    pub receipt: Receipt,
    pub manifest: SnapshotManifest,
    /// Files extracted to the quarantine dir: relative path (e.g.
    /// `blobs/<sha>`) → size. Populated only from safe entries.
    pub quarantine_dir: std::path::PathBuf,
    pub source_blobs: Vec<(String, String)>, // (sha256, quarantine blob path)
}

fn parse_receipt(raw: &[u8]) -> RelayResult<Receipt> {
    let v: serde_json::Value = serde_json::from_slice(raw)
        .map_err(|_| RelayError::new(Code::EUnverifiedImport, "receipt.json is not valid JSON"))?;
    let receipt: Receipt = serde_json::from_value(v.clone()).map_err(|e| {
        RelayError::new(
            Code::EUnverifiedImport,
            format!("receipt.json does not satisfy schema 1: {e}"),
        )
    })?;
    // Strict field checks beyond serde defaults.
    if receipt.schema_version != 1 {
        return Err(RelayError::new(
            Code::EUnverifiedImport,
            format!("unsupported receipt schema_version: {}", receipt.schema_version),
        ));
    }
    if receipt.kind != "handoff" && receipt.kind != "return" {
        return Err(RelayError::new(Code::EUnverifiedImport, "receipt kind must be handoff or return"));
    }
    if receipt.source_mode != "context_only" && receipt.source_mode != "portable_source" {
        return Err(RelayError::new(
            Code::EUnverifiedImport,
            "receipt source_mode must be context_only or portable_source",
        ));
    }
    crate::ids::parse_hash_id(&receipt.snapshot_id).map_err(|_| {
        RelayError::new(Code::EUnverifiedImport, "receipt snapshot_id must be sha256:<64hex>")
    })?;
    if !is_uuid(&receipt.handoff_id) || !is_uuid(&receipt.task_id) {
        return Err(RelayError::new(Code::EUnverifiedImport, "receipt ids must be UUIDs"));
    }
    if let Some(p) = &receipt.parent_handoff_id {
        if !is_uuid(p) {
            return Err(RelayError::new(Code::EUnverifiedImport, "parent_handoff_id must be a UUID"));
        }
    }
    for c in &receipt.claims {
        // Claims are ALWAYS external: a receipt claiming machine observation
        // is rejected outright (spec §6: 伪造 passed/改 source=machine 不能绕).
        if c.origin != "exporter_reported" {
            return Err(RelayError::new(
                Code::EUnverifiedImport,
                "receipt claims must carry origin=exporter_reported",
            ));
        }
        if c.kind != "verification" {
            return Err(RelayError::new(Code::EUnverifiedImport, "unknown claim kind"));
        }
        crate::ids::parse_hash_id(&c.snapshot_id).map_err(|_| {
            RelayError::new(Code::EUnverifiedImport, "claim snapshot_id must be sha256:<64hex>")
        })?;
        if !matches!(c.status.as_str(), "passed" | "failed" | "blocked" | "interrupted") {
            return Err(RelayError::new(Code::EUnverifiedImport, "claim status invalid"));
        }
    }
    let _ = v;
    Ok(receipt)
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.as_bytes()[8] == b'-'
        && s.as_bytes()[13] == b'-'
        && s.as_bytes()[18] == b'-'
        && s.as_bytes()[23] == b'-'
        && s.bytes().enumerate().all(|(i, b)| {
            matches!(i, 8 | 13 | 18 | 23) || matches!(b, b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F')
        })
}

fn parse_manifest(raw: &[u8]) -> RelayResult<SnapshotManifest> {
    let v: serde_json::Value = serde_json::from_slice(raw)
        .map_err(|_| RelayError::new(Code::EUnverifiedImport, "manifest.json is not valid JSON"))?;
    if v.get("schema_version").and_then(|x| x.as_i64()) != Some(1) {
        return Err(RelayError::new(Code::EUnverifiedImport, "unsupported manifest schema_version"));
    }
    if v.get("kind").and_then(|x| x.as_str()) != Some("relaycheck_snapshot") {
        return Err(RelayError::new(Code::EUnverifiedImport, "manifest kind mismatch"));
    }
    let manifest: SnapshotManifest = serde_json::from_value(v)
        .map_err(|e| RelayError::new(Code::EUnverifiedImport, format!("manifest invalid: {e}")))?;
    if manifest.files.len() > MAX_PACKAGE_ENTRIES {
        return Err(RelayError::new(Code::ELimit, "manifest has too many files"));
    }
    Ok(manifest)
}

/// Validates entry names in the strictest reading: relative, no traversal,
/// no drive/ADS, no case duplicates, no symlink entries, bounded sizes.
pub fn validate_entry_name(name: &str) -> RelayResult<()> {
    if name.is_empty() {
        return Err(RelayError::new(Code::EUnverifiedImport, "empty zip entry name"));
    }
    if name.contains('\\') || name.contains('\0') || name.contains(':') {
        return Err(RelayError::new(
            Code::EUnverifiedImport,
            "zip entry names must be plain relative paths",
        ));
    }
    if name.starts_with('/') {
        return Err(RelayError::new(Code::EUnverifiedImport, "absolute zip entry name"));
    }
    for seg in name.split('/') {
        if seg == ".." || seg == "." {
            return Err(RelayError::new(
                Code::EUnverifiedImport,
                "zip entry path traversal",
            ));
        }
        pathsafe::check_component(seg)?;
    }
    Ok(())
}

/// Shared read-only entry collection for both the import path and the
/// offline inspect/verify path: per-entry name validation, case-conflict
/// detection, symlink flagging, declared-size match and size budgets.
/// Nothing is written to disk here.
fn collect_entries<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
) -> RelayResult<Vec<(String, Vec<u8>, bool)>> {
    if archive.len() > MAX_PACKAGE_ENTRIES {
        return Err(RelayError::new(Code::ELimit, "package has too many entries"));
    }
    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut entries: Vec<(String, Vec<u8>, bool)> = Vec::new(); // (name, data, is_symlink)
    let mut expanded_total: i64 = 0;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|e| {
            RelayError::new(Code::EUnverifiedImport, format!("zip entry unreadable: {e}"))
        })?;
        let name = file.name().to_string();
        validate_entry_name(&name)?;
        let key = name.to_lowercase();
        if seen.insert(key, name.clone()).is_some() {
            return Err(RelayError::new(
                Code::EUnverifiedImport,
                "duplicate or case-conflicting zip entries",
            ));
        }
        let is_symlink = file.is_symlink();
        let declared = file.size();
        if declared as i64 > MAX_PACKAGE_FILE_BYTES {
            return Err(RelayError::new(Code::ELimit, "zip entry exceeds per-file limit"));
        }
        let mut buf = Vec::new();
        let taken = file
            .by_ref()
            .take((MAX_PACKAGE_FILE_BYTES + 1) as u64)
            .read_to_end(&mut buf)
            .map_err(|e| RelayError::new(Code::EUnverifiedImport, format!("zip read failed: {e}")))?;
        if taken as u64 != declared {
            return Err(RelayError::new(
                Code::EUnverifiedImport,
                "zip entry actual size does not match its declared size",
            ));
        }
        expanded_total += taken as i64;
        if expanded_total > MAX_PACKAGE_EXPANDED_BYTES {
            return Err(RelayError::new(Code::ELimit, "package expands beyond the size budget (zip bomb protection)"));
        }
        entries.push((name, buf, is_symlink));
    }
    Ok(entries)
}

/// Reads and validates a package from bytes. On success the package is
/// unpacked into `quarantine_root/import-<id>` — never into any repo.
pub fn unpack_package(
    data: &[u8],
    quarantine_root: &Path,
) -> RelayResult<ValidatedPackage> {
    if data.len() as i64 > MAX_PACKAGE_EXPANDED_BYTES {
        return Err(RelayError::new(Code::ELimit, "package file too large"));
    }
    let reader = std::io::Cursor::new(data);
    let mut archive = zip::ZipArchive::new(reader)
        .map_err(|e| RelayError::new(Code::EUnverifiedImport, format!("not a readable zip: {e}")))?;
    let entries = collect_entries(&mut archive)?;

    // RC03: EVERY entry is secret-scanned before anything is parsed,
    // persisted or shown — a receipt/markdown/manifest/source carrying
    // high-confidence secret material fails the WHOLE package (no partial
    // activation), and the error names the entry without echoing the secret.
    let mut offenders: Vec<String> = Vec::new();
    for (name, data, _) in &entries {
        if crate::secrets::scan_bytes(data).is_some() {
            offenders.push(name.clone());
            if offenders.len() >= 20 {
                break;
            }
        }
    }
    if !offenders.is_empty() {
        return Err(RelayError::new(
            Code::ESecretDetected,
            "package contains high-confidence secret material; import blocked",
        )
        .with_details(offenders));
    }

    let receipt_entry = entries.iter().find(|(n, _, _)| n == "receipt.json");
    let manifest_entry = entries.iter().find(|(n, _, _)| n == "manifest.json");
    let Some((_, receipt_raw, _)) = receipt_entry else {
        return Err(RelayError::new(Code::EUnverifiedImport, "package has no receipt.json"));
    };
    let Some((_, manifest_raw, _)) = manifest_entry else {
        return Err(RelayError::new(Code::EUnverifiedImport, "package has no manifest.json"));
    };
    let receipt = parse_receipt(receipt_raw)?;
    let manifest = parse_manifest(manifest_raw)?;
    if manifest.schema_version != crate::model::SNAPSHOT_SCHEMA_VERSION {
        return Err(RelayError::new(Code::EUnverifiedImport, "unsupported manifest version"));
    }

    // Unpack into quarantine.
    let dir = quarantine_root.join(format!("import-{}", crate::ids::new_token(12)));
    std::fs::create_dir_all(&dir)?;
    for (name, data, is_symlink) in &entries {
        if *is_symlink {
            cleanup(&dir);
            return Err(RelayError::new(Code::EUnverifiedImport, "symlink zip entries are rejected"));
        }
        // Nested archives are data, not auto-expanded (spec §6).
        let dest = dir.join(name);
        if let Some(parent) = dest.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                cleanup(&dir);
                return Err(RelayError::new(Code::EUnverifiedImport, "quarantine unpack failed"));
            }
        }
        if std::fs::write(&dest, data).is_err() {
            cleanup(&dir);
            return Err(RelayError::new(Code::EUnverifiedImport, "quarantine unpack failed"));
        }
    }

    // Portable-source: every manifest blob must be present and hash-true.
    let mut source_blobs = Vec::new();
    if receipt.source_mode == "portable_source" {
        for f in &manifest.files {
            let name = format!("blobs/{}", f.sha256);
            let p = dir.join(&name);
            let data = std::fs::read(&p).map_err(|_| {
                cleanup(&dir);
                RelayError::new(
                    Code::EUnverifiedImport,
                    format!("portable package is missing blob {}", &f.sha256[..12.min(f.sha256.len())]),
                )
            })?;
            let actual = crate::ids::sha256_hex(&data);
            if actual != f.sha256 || data.len() as i64 != f.size {
                cleanup(&dir);
                return Err(RelayError::new(
                    Code::EUnverifiedImport,
                    "packaged blob does not match its manifest hash",
                ));
            }
            source_blobs.push((f.sha256.clone(), name));
        }
    }

    Ok(ValidatedPackage {
        receipt,
        manifest,
        quarantine_dir: dir,
        source_blobs,
    })
}

fn cleanup(dir: &Path) {
    std::fs::remove_dir_all(dir).ok();
}

/// Read-only package inspection with the SAME entry and identity validation
/// as the import path (R02: shared ZIP entry + identity checks), minus the
/// secret scan, quarantine unpack and activation. For portable_source
/// packages every manifest blob is additionally verified present, size-true
/// and hash-true. Nothing is extracted; nothing is registered; no Git or DB
/// writes.
pub fn inspect_package(data: &[u8]) -> RelayResult<(Receipt, SnapshotManifest)> {
    if data.len() as i64 > MAX_PACKAGE_EXPANDED_BYTES {
        return Err(RelayError::new(Code::ELimit, "package file too large"));
    }
    let reader = std::io::Cursor::new(data);
    let mut archive = zip::ZipArchive::new(reader)
        .map_err(|e| RelayError::new(Code::EUnverifiedImport, format!("not a readable zip: {e}")))?;
    let entries = collect_entries(&mut archive)?;
    for (name, _, is_symlink) in &entries {
        if *is_symlink {
            return Err(RelayError::new(Code::EUnverifiedImport, "symlink zip entries are rejected"));
        }
        let _ = name;
    }
    let read_entry = |name: &str| -> RelayResult<Vec<u8>> {
        entries
            .iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, d, _)| d.clone())
            .ok_or_else(|| RelayError::new(Code::EUnverifiedImport, format!("missing {name}")))
    };
    let receipt = parse_receipt(&read_entry("receipt.json")?)?;
    let manifest = parse_manifest(&read_entry("manifest.json")?)?;
    if manifest.schema_version != crate::model::SNAPSHOT_SCHEMA_VERSION {
        return Err(RelayError::new(Code::EUnverifiedImport, "unsupported manifest version"));
    }
    // Portable-source: every manifest blob must be present and hash-true —
    // the package content hash binds content, so a damaged blob cannot pass
    // a content comparison against the manifest.
    if receipt.source_mode == "portable_source" {
        for f in &manifest.files {
            let name = format!("blobs/{}", f.sha256);
            let data = read_entry(&name).map_err(|_| {
                RelayError::new(
                    Code::EUnverifiedImport,
                    format!("portable package is missing blob {}", &f.sha256[..12.min(f.sha256.len())]),
                )
            })?;
            if data.len() as i64 != f.size || crate::ids::sha256_hex(&data) != f.sha256 {
                return Err(RelayError::new(
                    Code::EUnverifiedImport,
                    "packaged blob does not match its manifest hash",
                ));
            }
        }
    }
    Ok((receipt, manifest))
}

/// Helper used by validation: single path component check.
pub fn check_component(seg: &str) -> RelayResult<()> {
    pathsafe::check_component(seg).map_err(|mut e| {
        e.message = format!("zip entry: {}", e.message);
        e
    })
}

/// Convenience: build the excluded list rendering for a snapshot manifest.
pub fn excluded_lines(excluded: &[ExcludedEntry]) -> Vec<String> {
    excluded
        .iter()
        .map(|e| format!("{} ({})", e.path, e.reason))
        .collect()
}

/// Builds a manifest from already-extracted quarantine files (import path).
#[allow(dead_code)]
pub fn manifest_from_files(files: Vec<ManifestFile>, excluded: Vec<ExcludedEntry>) -> SnapshotManifest {
    SnapshotManifest {
        schema_version: crate::model::SNAPSHOT_SCHEMA_VERSION,
        kind: "relaycheck_snapshot".into(),
        scope: "selected_sources".into(),
        files,
        excluded,
    }
}

pub fn open_item(id: &str, text: &str) -> OpenItem {
    OpenItem { id: id.to_string(), text: text.to_string() }
}

pub fn claim(check_id: &str, status: &str, snapshot_id: &str) -> Claim {
    Claim {
        kind: "verification".into(),
        origin: "exporter_reported".into(),
        check_id: check_id.to_string(),
        status: status.to_string(),
        snapshot_id: snapshot_id.to_string(),
    }
}
