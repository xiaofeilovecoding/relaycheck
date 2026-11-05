//! Path boundary enforcement for capture and import (spec §4).
//!
//! Fail closed: reject UNC, device paths, reparse points, out-of-bounds
//! traversal, case-insensitive conflicts, ADS, illegal Windows names and
//! oversized paths before any file is read or written.

use crate::errors::{Code, RelayError, RelayResult};
use std::path::Path;

/// Reserved Windows device names (case-insensitive, with or without extension).
const RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

pub fn path_boundary_error(message: impl Into<String>) -> RelayError {
    RelayError::new(Code::EPathBoundary, message)
}

/// Validates an absolute Windows path given by the user (CLI attach only).
/// Must be a plain local-drive path: `X:\...`, not UNC, not device.
pub fn validate_root(raw: &str) -> RelayResult<String> {
    let trimmed = raw.trim();
    if trimmed.len() > 240 {
        return Err(path_boundary_error("path too long"));
    }
    if trimmed.starts_with("\\\\") {
        return Err(path_boundary_error(
            "UNC and device paths are not accepted; use a plain local drive path",
        ));
    }
    let bytes = trimmed.as_bytes();
    let ok_drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    if !ok_drive {
        return Err(path_boundary_error(
            "expected an absolute local path like D:\\path\\to\\repo",
        ));
    }
    let normalized = trimmed.replace('/', "\\");
    let mut p = Path::new(&normalized).to_path_buf();
    // Absolute path required after normalization.
    if !p.is_absolute() {
        return Err(path_boundary_error("path is not absolute"));
    }
    // Component checks for the root (no empty components, no reserved names).
    for comp in p.components() {
        let name = comp.as_os_str().to_string_lossy();
        if name.contains(':') && name.len() > 2 {
            return Err(path_boundary_error("illegal path component"));
        }
        check_component(&name)?;
    }
    // Canonicalize (no symlink resolution across UNC; \\?\ keeps it local).
    let canonical = std::fs::canonicalize(&p)
        .map_err(|e| path_boundary_error(format!("path not accessible: {e}")))?;
    let as_str = canonical.to_string_lossy().to_string();
    if as_str.starts_with("\\\\?\\UNC\\") || as_str.starts_with("\\\\") && !as_str.starts_with("\\\\?\\") {
        return Err(path_boundary_error("resolved path is not a local drive path"));
    }
    p = canonical;
    // Strip the \\?\ prefix for storage; re-add for IO.
    let clean = as_str.strip_prefix(r"\\?\").unwrap_or(&as_str).to_string();
    Ok(clean)
}

/// Extends the root with a relative path and enforces every component rule.
/// Returns the joined path.
pub fn join_checked(root: &str, rel: &str) -> RelayResult<std::path::PathBuf> {
    if rel.is_empty() {
        return Ok(Path::new(root).to_path_buf());
    }
    if rel.len() > 512 {
        return Err(path_boundary_error("relative path too long"));
    }
    if rel.starts_with('\\') || rel.starts_with('/') {
        return Err(path_boundary_error("path must be relative"));
    }
    if rel.contains('\0') {
        return Err(path_boundary_error("illegal NUL in path"));
    }
    if rel.contains(':') {
        return Err(path_boundary_error(
            "drive-relative paths and alternate data streams are not accepted",
        ));
    }
    for seg in rel.split(['\\', '/']) {
        check_component(seg)?;
    }
    let joined = Path::new(root).join(rel.replace('/', "\\"));
    // Lexical traversal check (byte-level, not just FS behavior).
    let mut depth = 0i32;
    for seg in rel.split(['\\', '/']) {
        match seg {
            "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return Err(path_boundary_error("path escapes the project root"));
                }
            }
            _ => depth += 1,
        }
    }
    Ok(joined)
}

/// Component rule (public: also used by zip entry validation on import).
pub fn check_component(name: &str) -> RelayResult<()> {
    if name.is_empty() {
        return Err(path_boundary_error("empty path component"));
    }
    if name == "." || name == ".." {
        return Ok(());
    }
    if matches!(name, "." | "..") {
        return Ok(());
    }
    let stem = name.split('.').next().unwrap_or(name);
    if RESERVED_NAMES.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
        return Err(path_boundary_error(format!(
            "reserved Windows device name: {stem}"
        )));
    }
    for ch in name.chars() {
        match ch {
            '<' | '>' | '"' | '|' | '?' | '*' | ':' => {
                return Err(path_boundary_error(format!(
                    "illegal character {ch:?} in file name"
                )))
            }
            c if (c as u32) < 0x20 => {
                return Err(path_boundary_error("control character in file name"))
            }
            _ => {}
        }
    }
    let last = name.chars().last().unwrap();
    if last == '.' || last == ' ' {
        return Err(path_boundary_error(
            "trailing dot or space in file name is not portable",
        ));
    }
    Ok(())
}

/// Case-insensitive duplicate detection across a whole path list (Windows
/// file systems are case-insensitive; two entries mapping to one file would
/// make the manifest ambiguous).
pub fn check_case_conflicts(paths: &[String]) -> RelayResult<()> {
    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for p in paths {
        let key = p.to_lowercase();
        if let Some(first) = seen.get(&key) {
            if first != p {
                return Err(path_boundary_error(format!(
                    "case-insensitive path conflict: '{first}' vs '{p}'"
                )));
            }
        } else {
            seen.insert(key, p.clone());
        }
    }
    Ok(())
}

#[cfg(windows)]
pub fn is_reparse_point(path: &Path) -> RelayResult<bool> {
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileAttributesW, FILE_ATTRIBUTE_REPARSE_POINT,
    };
    let wide = to_wide(&path.to_string_lossy());
    let attrs = unsafe { GetFileAttributesW(wide.as_ptr()) };
    if attrs == u32::MAX {
        return Err(path_boundary_error("cannot stat path"));
    }
    Ok(attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

#[cfg(not(windows))]
pub fn is_reparse_point(path: &Path) -> RelayResult<bool> {
    let md = std::fs::symlink_metadata(path)
        .map_err(|e| path_boundary_error(format!("cannot stat path: {e}")))?;
    Ok(md.file_type().is_symlink())
}

#[cfg(windows)]
pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Prepend the extended-length prefix for local file IO on Windows.
#[cfg(windows)]
pub fn extended_path(p: &Path) -> std::path::PathBuf {
    let s = p.to_string_lossy().to_string();
    if s.starts_with(r"\\?\") {
        std::path::PathBuf::from(s)
    } else {
        std::path::PathBuf::from(format!(r"\\?\{}", s))
    }
}

#[cfg(not(windows))]
pub fn extended_path(p: &Path) -> std::path::PathBuf {
    p.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unc_device_and_relative_roots() {
        assert_eq!(validate_root("\\\\server\\share").unwrap_err().code, Code::EPathBoundary);
        assert_eq!(validate_root("\\\\.\\C:\\x").unwrap_err().code, Code::EPathBoundary);
        assert_eq!(validate_root("relative\\path").unwrap_err().code, Code::EPathBoundary);
        assert_eq!(validate_root("C:").unwrap_err().code, Code::EPathBoundary);
    }

    #[test]
    fn join_checks_traversal_ads_and_reserved() {
        assert_eq!(join_checked("D:\\r", "..\\x").unwrap_err().code, Code::EPathBoundary);
        assert_eq!(join_checked("D:\\r", "a:stream").unwrap_err().code, Code::EPathBoundary);
        assert_eq!(join_checked("D:\\r", "CON.txt").unwrap_err().code, Code::EPathBoundary);
        assert_eq!(join_checked("D:\\r", "a<b.txt").unwrap_err().code, Code::EPathBoundary);
        assert_eq!(join_checked("D:\\r", "sub\\file.txt ").unwrap_err().code, Code::EPathBoundary);
        assert!(join_checked("D:\\r", "sub\\file.txt").is_ok());
        assert!(join_checked("D:\\r", "..\\..").is_err());
    }

    #[test]
    fn case_conflict_detected() {
        assert!(check_case_conflicts(&["a\\B.txt".into(), "A\\b.txt".into()]).is_err());
        assert!(check_case_conflicts(&["a\\b.txt".into(), "a\\b.txt".into()]).is_ok());
        assert!(check_case_conflicts(&["a\\b.txt".into(), "a\\c.txt".into()]).is_ok());
    }
}
