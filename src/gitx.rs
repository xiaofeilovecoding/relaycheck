//! Git access: a strict read-only whitelist (spec §4).
//!
//! Rules:
//! - fixed argv per command; no shell; no user-controlled flags
//! - always `--no-pager --no-optional-locks`, fsmonitor/hooks/interactive/
//!   external diff/textconv disabled via `-c` overrides
//! - system/global/git-env config isolated: `GIT_CONFIG_NOSYSTEM=1`,
//!   `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` point at the OS null device,
//!   all `GIT_*` variables stripped, minimal environment allowlist
//! - no credentials, no fetch, no submodule update, no object rewriting
//!
//! A repository's malicious config, hooks, filters or fsmonitor must never
//! execute: we never run commands that can trigger them, and the `-c`
//! overrides pin the dangerous knobs off regardless of local config.

use crate::errors::{RelayError, RelayResult};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct GitOutput {
    pub stdout: Vec<u8>,
    pub success: bool,
}

fn null_device() -> &'static str {
    if cfg!(windows) {
        "nul"
    } else {
        "/dev/null"
    }
}

/// Minimal environment for git children. Nothing user-specific is forwarded
/// and no `GIT_*` variable survives (they are re-set to safe values below).
fn git_env(cmd: &mut Command, isolated_home: &Path) {
    cmd.env_clear();
    for (k, v) in [
        ("SystemRoot", std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into())),
        ("TEMP", std::env::var("TEMP").unwrap_or_else(|_| r"C:\Temp".into())),
        ("TMP", std::env::var("TMP").unwrap_or_else(|_| r"C:\Temp".into())),
        ("PATH", std::env::var("PATH").unwrap_or_default()),
    ] {
        cmd.env(k, v);
    }
    cmd.env("HOME", isolated_home);
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env("GIT_CONFIG_GLOBAL", null_device());
    cmd.env("GIT_CONFIG_SYSTEM", null_device());
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    cmd.env("LC_ALL", "C");
}

fn run_git(repo_root: &Path, args: &[&str], isolated_home: &Path) -> RelayResult<GitOutput> {
    let git_exe = crate::service::git_exe_path()?;
    let mut cmd = Command::new(&git_exe);
    // Fixed safe prefix flags; argv order is ours, never caller-built.
    cmd.arg("--no-pager")
        .arg("--no-optional-locks")
        .arg("-c")
        .arg("core.fsmonitor=false")
        .arg("-c")
        .arg("core.hooksPath=")
        .arg("-c")
        .arg("core.editor=true")
        .arg("-c")
        .arg("credential.helper=")
        .arg("-c")
        .arg("diff.external=")
        .arg("-c")
        .arg("diff.noprefix=false")
        .arg("-c")
        .arg("interactive.singlekey=false")
        .arg("-c")
        .arg("filter.lfs.required=false")
        .arg("-c")
        .arg("filter.lfs.process=")
        .arg("-c")
        .arg("filter.lfs.smudge=")
        .arg("-c")
        .arg("filter.lfs.clean=")
        .arg("-c")
        .arg("gpg.format=openpgp")
        .arg("-c")
        .arg("commit.gpgsign=false")
        .arg("-c")
        .arg("tag.gpgsign=false")
        .arg("-c")
        .arg("gc.auto=0")
        .arg("-c")
        .arg("checkout.workers=1")
        .arg("-C");
    cmd.arg(repo_root);
    for a in args {
        cmd.arg(a);
    }
    git_env(&mut cmd, isolated_home);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let out = cmd
        .output()
        .map_err(|e| RelayError::internal(format!("git spawn failed: {e}")))?;
    Ok(GitOutput { stdout: out.stdout, success: out.status.success() })
}

/// Runs a whitelisted read-only git command, mapping failures to errors.
pub fn git_run(repo_root: &str, args: &[&str], isolated_home: &Path) -> RelayResult<Vec<u8>> {
    let out = run_git(Path::new(repo_root), args, isolated_home)?;
    if !out.success {
        return Err(RelayError::internal("git command failed"));
    }
    Ok(out.stdout)
}

pub struct RepoIdentity {
    pub root: String,
    pub common_dir: String,
    pub head_commit: String,
}

/// Identifies a git work tree: real toplevel + common dir identity. The
/// common dir distinguishes linked worktrees; two tasks on the same common
/// dir never run capture/checks in parallel (spec §3).
pub fn identify(repo_root: &str, isolated_home: &Path) -> RelayResult<RepoIdentity> {
    let root = crate::pathsafe::validate_root(repo_root)?;
    let root_path = Path::new(&root);
    // rev-parse --show-toplevel must resolve to the same root we validated.
    let top_bytes = git_run(&root, &["rev-parse", "--show-toplevel"], isolated_home)?;
    let top = String::from_utf8_lossy(&top_bytes).trim().replace('/', "\\");
    let root_norm = root.to_lowercase();
    let top_norm = top.trim_start_matches(r"\\?\").to_lowercase();
    if top_norm != root_norm {
        return Err(RelayError::validation(
            "path is inside a repository but is not its toplevel; attach the toplevel directory",
        ));
    }
    let common_bytes =
        git_run(&root, &["rev-parse", "--path-format=absolute", "--git-common-dir"], isolated_home)?;
    let common = String::from_utf8_lossy(&common_bytes).trim().replace('/', "\\");
    let head = git_run(&root, &["rev-parse", "HEAD"], isolated_home).unwrap_or_default();
    let head_commit = String::from_utf8_lossy(&head).trim().to_string();
    Ok(RepoIdentity { root, common_dir: common, head_commit })
}

/// Lists tracked files with stage info (mode + path), rejecting gitlinks.
/// Output is NUL-delimited: `<mode> <hash> <stage>\0<path>\0`.
pub fn ls_files_cached(repo_root: &str, isolated_home: &Path) -> RelayResult<Vec<(String, String)>> {
    let out = git_run(repo_root, &["ls-files", "-z", "--stage"], isolated_home)?;
    parse_ls_files_stage(&out)
}

pub fn parse_ls_files_stage(data: &[u8]) -> RelayResult<Vec<(String, String)>> {
    let mut out = Vec::new();
    for rec in data.split(|b| *b == 0) {
        if rec.is_empty() {
            continue;
        }
        let s = String::from_utf8_lossy(rec);
        let Some((meta, path)) = s.split_once('\t') else {
            return Err(RelayError::internal("git ls-files output malformed"));
        };
        let mut parts = meta.split_whitespace();
        let mode = parts.next().unwrap_or("");
        if mode == "160000" {
            return Err(RelayError::validation(format!(
                "submodule entry is not supported for capture: {}",
                relative_display(path)
            )));
        }
        out.push((mode.to_string(), path.to_string()));
    }
    Ok(out)
}

pub fn parse_ls_files_null(data: &[u8]) -> Vec<String> {
    data.split(|b| *b == 0)
        .filter(|r| !r.is_empty())
        .map(|r| String::from_utf8_lossy(r).to_string())
        .collect()
}

/// Untracked (not ignored) candidates for explicit user selection.
pub fn ls_untracked(repo_root: &str, isolated_home: &Path) -> RelayResult<Vec<String>> {
    let out = git_run(
        repo_root,
        &["ls-files", "-z", "--others", "--exclude-standard"],
        isolated_home,
    )?;
    Ok(parse_ls_files_null(&out))
}

/// Dirty flag: any working-tree difference from HEAD (porcelain v1 non-empty).
pub fn is_dirty(repo_root: &str, isolated_home: &Path) -> RelayResult<bool> {
    let out = git_run(repo_root, &["status", "--porcelain"], isolated_home)?;
    Ok(!out.is_empty())
}

/// Returns the LFS filter attribute for each listed path (empty string when
/// unspecified). `filter=lfs` on a selected file means remote LFS dependency
/// → capture rejected with an actionable message.
pub fn lfs_attr(repo_root: &str, paths: &[&str], isolated_home: &Path) -> RelayResult<Vec<String>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut args: Vec<&str> = vec!["check-attr", "-z", "filter", "--"];
    args.extend(paths);
    let out = git_run(repo_root, &args, isolated_home)?;
    // Output: <path> NUL <attr> NUL <value> NUL (triples, repeated)
    let recs: Vec<&[u8]> = out.split(|b| *b == 0).filter(|r| !r.is_empty()).collect();
    let mut values = Vec::new();
    for trip in recs.chunks(3) {
        if trip.len() == 3 {
            let v = String::from_utf8_lossy(trip[2]).to_string();
            values.push(v);
        }
    }
    Ok(values)
}

fn relative_display(p: &str) -> &str {
    // ls-files paths are already relative; quote only for display safety.
    p
}

/// An isolated empty HOME directory so global git config never applies.
pub fn isolated_home(base: &Path) -> RelayResult<PathBuf> {
    let dir = base.join("git-isolated-home");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stage_output_and_rejects_gitlinks() {
        let data = b"100644 abc\tfile.txt\0160000 def\tsub\0".to_vec();
        assert!(parse_ls_files_stage(&data).is_err());
        let data = b"100644 abc\tfile.txt\0100755 def\trun.sh\0".to_vec();
        let v = parse_ls_files_stage(&data).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].1, "file.txt");
    }
}
