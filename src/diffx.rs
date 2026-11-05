//! A→B change list (spec §1.2, R20).
//!
//! The baseline is always the snapshot the parent handoff points at — never
//! "latest HEAD", never another candidate. Comparison is by captured bytes.
//! Text files (no NUL in the first 8 KiB) get a bounded unified diff;
//! binary files show size/hash pairs; nothing here is a quality score.

use crate::model::ManifestFile;

pub const MAX_DIFF_LINES: usize = 2000;
const TEXT_SCAN_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone)]
pub struct ChangeEntry {
    pub path: String,
    pub kind: ChangeKind,
    pub plus: usize,
    pub minus: usize,
    pub old_size: Option<i64>,
    pub new_size: Option<i64>,
    pub old_sha: Option<String>,
    pub new_sha: Option<String>,
    pub unified: Vec<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

pub fn is_probably_text(data: &[u8]) -> bool {
    let scan = &data[..data.len().min(TEXT_SCAN_BYTES)];
    !scan.contains(&0)
}

/// Computes the change list from two manifests. `files_a` may be empty when
/// the baseline is unavailable (caller handles that case before calling).
pub fn compare(files_a: &[ManifestFile], files_b: &[ManifestFile]) -> Vec<ChangeEntry> {
    use std::collections::HashMap;
    let mut map_a: HashMap<&str, &ManifestFile> =
        files_a.iter().map(|f| (f.path.as_str(), f)).collect();
    let mut map_b: HashMap<&str, &ManifestFile> =
        files_b.iter().map(|f| (f.path.as_str(), f)).collect();
    let mut out = Vec::new();
    // Deleted: in A, not in B.
    for (path, fa) in files_a.iter().map(|f| (f.path.as_str(), f)) {
        if !map_b.contains_key(path) {
            out.push(ChangeEntry {
                path: path.to_string(),
                kind: ChangeKind::Deleted,
                plus: 0,
                minus: 0,
                old_size: Some(fa.size),
                new_size: None,
                old_sha: Some(fa.sha256.clone()),
                new_sha: None,
                unified: Vec::new(),
                truncated: false,
            });
        }
    }
    // Added / Modified.
    for fb in files_b {
        match map_a.get(fb.path.as_str()) {
            None => out.push(ChangeEntry {
                path: fb.path.clone(),
                kind: ChangeKind::Added,
                plus: 0,
                minus: 0,
                old_size: None,
                new_size: Some(fb.size),
                old_sha: None,
                new_sha: Some(fb.sha256.clone()),
                unified: Vec::new(),
                truncated: false,
            }),
            Some(fa) => {
                if fa.sha256 != fb.sha256 {
                    let (plus, minus, unified, truncated) = (0, 0, Vec::new(), false);
                    out.push(ChangeEntry {
                        path: fb.path.clone(),
                        kind: ChangeKind::Modified,
                        plus,
                        minus,
                        old_size: Some(fa.size),
                        new_size: Some(fb.size),
                        old_sha: Some(fa.sha256.clone()),
                        new_sha: Some(fb.sha256.clone()),
                        unified,
                        truncated,
                    });
                }
            }
        }
    }
    out.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    out
}

/// Fills line-count and unified-diff details for a modified entry, given the
/// two byte contents. Line detail is bounded (MAX_DIFF_LINES); beyond that
/// the entry keeps hash/size info and a truncated marker.
pub fn fill_text_detail(entry: &mut ChangeEntry, old_bytes: &[u8], new_bytes: &[u8]) {
    let text = is_probably_text(old_bytes) && is_probably_text(new_bytes);
    if !text {
        return; // binary: hash/size only
    }
    let old_lines = split_lines(old_bytes);
    let new_lines = split_lines(new_bytes);
    if old_lines.len() + new_lines.len() > MAX_DIFF_LINES * 4 {
        entry.truncated = true;
        return;
    }
    let ops = myers_diff(&old_lines, &new_lines);
    let mut unified = vec![format!("--- a/{}", entry.path), format!("+++ b/{}", entry.path)];
    let mut plus = 0usize;
    let mut minus = 0usize;
    let mut emitted = 0usize;
    let mut truncated = false;
    // Group into hunks with at most 3 context lines around changes.
    let mut i = 0usize;
    let n = ops.len();
    while i < n {
        let (op, line) = &ops[i];
        if *op == Op::Keep {
            i += 1;
            continue;
        }
        // Hunk start: back up 3 context lines.
        let start = i.saturating_sub(3);
        let mut hunk: Vec<String> = Vec::new();
        let mut j = start;
        let mut changes_ahead = false;
        while j < n {
            let (op2, line2) = &ops[j];
            match op2 {
                Op::Keep => {
                    // Look ahead: keep only if within 3 lines of a change.
                    let has_change = ops[j..(j + 4).min(n)]
                        .iter()
                        .any(|(o, _)| *o != Op::Keep);
                    if !has_change {
                        break;
                    }
                    hunk.push(format!(" {line2}"));
                }
                Op::Del => {
                    minus += 1;
                    hunk.push(format!("-{line2}"));
                    changes_ahead = true;
                }
                Op::Add => {
                    plus += 1;
                    hunk.push(format!("+{line2}"));
                    changes_ahead = true;
                }
            }
            j += 1;
            if emitted + hunk.len() > MAX_DIFF_LINES {
                truncated = true;
                break;
            }
        }
        if !changes_ahead && hunk.is_empty() {
            i = j;
            continue;
        }
        unified.push(format!(
            "@@ -{},+{} @@",
            hunk.iter().filter(|l| !l.starts_with('+')).count(),
            hunk.iter().filter(|l| !l.starts_with('-')).count()
        ));
        let take = hunk.len().min(MAX_DIFF_LINES.saturating_sub(emitted));
        unified.extend(hunk.into_iter().take(take));
        emitted = unified.len() - 2;
        if truncated {
            break;
        }
        i = j;
    }
    entry.plus = plus;
    entry.minus = minus;
    entry.unified = unified;
    entry.truncated = truncated;
}

fn split_lines(data: &[u8]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (idx, b) in data.iter().enumerate() {
        if *b == b'\n' {
            let mut end = idx;
            if end > start && data[end - 1] == b'\r' {
                end -= 1;
            }
            lines.push(String::from_utf8_lossy(&data[start..end]).into_owned());
            start = idx + 1;
        }
    }
    if start < data.len() {
        lines.push(String::from_utf8_lossy(&data[start..]).into_owned());
    }
    lines
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Keep,
    Del,
    Add,
}

/// Myers O(ND) diff on lines. Returns the edit script.
fn myers_diff(a: &[String], b: &[String]) -> Vec<(Op, String)> {
    let n = a.len();
    let m = b.len();
    let max_d = n + m;
    let mut v = vec![0i64; 2 * max_d + 2];
    let offset = max_d as i64;
    let mut trace: Vec<Vec<i64>> = Vec::new();
    let mut found_d = None;
    'outer: for d in 0..=max_d as i64 {
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            let idx = (k + offset) as usize;
            let mut x = if k == -d || (k != d && v[idx - 1] < v[idx + 1]) {
                v[idx + 1]
            } else {
                v[idx - 1] + 1
            };
            let mut y = x - k;
            while (x as usize) < n && (y as usize) < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[idx] = x;
            if x as usize >= n && y as usize >= m {
                found_d = Some(d);
                break 'outer;
            }
            k += 2;
        }
    }
    let Some(d_final) = found_d else {
        // Unreachable: max_d always suffices.
        return a.iter().map(|l| (Op::Del, l.clone())).chain(b.iter().map(|l| (Op::Add, l.clone()))).collect();
    };
    // Backtrack.
    let mut path: Vec<(Op, String)> = Vec::new();
    let mut x = n as i64;
    let mut y = m as i64;
    for d in (0..=d_final).rev() {
        let v = &trace[d as usize];
        let k = x - y;
        let idx = (k + offset) as usize;
        let prev_k = if k == -d || (k != d && v[idx - 1] < v[idx + 1]) {
            k + 1
        } else {
            k - 1
        };
        let prev_x = v[(prev_k + offset) as usize];
        let prev_y = prev_x - prev_k;
        // Snake first (walking backwards: consumes matching lines).
        while x > prev_x && y > prev_y {
            path.push((Op::Keep, a[(x - 1) as usize].clone()));
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            if x == prev_x {
                path.push((Op::Add, b[(prev_y) as usize].clone()));
                y -= 1;
            } else {
                path.push((Op::Del, a[(prev_x) as usize].clone()));
                x -= 1;
            }
        }
    }
    path.reverse();
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_added_modified_deleted() {
        let fa = |p: &str, s: &str| ManifestFile {
            path: p.into(),
            mode: "100644".into(),
            size: s.len() as i64,
            sha256: crate::ids::sha256_hex(s.as_bytes()),
        };
        let a = vec![fa("same.txt", "x"), fa("mod.txt", "old"), fa("gone.txt", "g")];
        let b = vec![fa("same.txt", "x"), fa("mod.txt", "new"), fa("new.txt", "n")];
        let changes = compare(&a, &b);
        let kinds: Vec<(String, ChangeKind)> =
            changes.iter().map(|c| (c.path.clone(), c.kind.clone())).collect();
        assert_eq!(
            kinds,
            vec![
                ("gone.txt".into(), ChangeKind::Deleted),
                ("mod.txt".into(), ChangeKind::Modified),
                ("new.txt".into(), ChangeKind::Added),
            ]
        );
    }

    #[test]
    fn unified_diff_counts() {
        let mut e = ChangeEntry {
            path: "f.txt".into(),
            kind: ChangeKind::Modified,
            plus: 0,
            minus: 0,
            old_size: None,
            new_size: None,
            old_sha: None,
            new_sha: None,
            unified: vec![],
            truncated: false,
        };
        let old = b"a\nb\nc\n";
        let new = b"a\nB\nc\nd\n";
        fill_text_detail(&mut e, old, new);
        assert_eq!(e.plus, 2, "B and d added: {e:?}");
        assert_eq!(e.minus, 1);
        assert!(e.unified.iter().any(|l| l == "-b"));
        assert!(e.unified.iter().any(|l| l == "+B"));
        assert!(e.unified.iter().any(|l| l == "+d"));
    }

    #[test]
    fn binary_files_get_no_line_diff() {
        let mut e = ChangeEntry {
            path: "f.bin".into(),
            kind: ChangeKind::Modified,
            plus: 0,
            minus: 0,
            old_size: None,
            new_size: None,
            old_sha: None,
            new_sha: None,
            unified: vec![],
            truncated: false,
        };
        fill_text_detail(&mut e, b"\0\x01\x02", b"\0\x01\x03");
        assert!(e.unified.is_empty());
    }

    #[test]
    fn crlf_and_no_eol_handled() {
        let lines = split_lines(b"p\r\nq\r\nr");
        assert_eq!(lines, vec!["p", "q", "r"]);
    }
}
