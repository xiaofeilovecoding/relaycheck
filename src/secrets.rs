//! High-confidence secret canary scanning (spec §5, R11).
//!
//! Rules:
//! - capture: any hit blocks the snapshot (never silently drops the file)
//! - check output: matches are redacted in stored summaries with a count;
//!   the secret is never pushed raw to the browser and never returned as a
//!   "whole log" error
//! - handoff export: a hit in narrative fields blocks the export
//!
//! Uses the official `regex` crate (linear-time engine, no catastrophic
//! backtracking) so the security control itself is well-tested code, not a
//! hand-rolled matcher. Scanning works across streamed chunks because the
//! caller keeps an overlap buffer (see checks.rs).

use regex::Regex;

struct Matcher {
    name: &'static str,
    re: Regex,
}

fn matchers() -> &'static [Matcher] {
    use std::sync::OnceLock;
    static MATCHERS: OnceLock<Vec<Matcher>> = OnceLock::new();
    MATCHERS.get_or_init(|| {
        let mut v = Vec::new();
        let add = |name: &'static str, pat: &str, v: &mut Vec<Matcher>| {
            if let Ok(re) = Regex::new(pat) {
                v.push(Matcher { name, re });
            }
        };
        // AWS access key ids
        add("aws_access_key", r"AKIA[0-9A-Z]{16}", &mut v);
        add("aws_session_key", r"ASIA[0-9A-Z]{16}", &mut v);
        // OpenAI / Anthropic style keys
        add("openai_key", r"sk-[0-9A-Za-z_-]{20,}", &mut v);
        // GitHub tokens
        add("github_token", r"gh[pousr]_[0-9A-Za-z]{36,}", &mut v);
        add("github_pat", r"github_pat_[0-9A-Za-z_]{22,}", &mut v);
        // Slack
        add("slack_token", r"xox[baprs]-[0-9A-Za-z-]{10,}", &mut v);
        // Google API key
        add("google_api_key", r"AIza[0-9A-Za-z_-]{35}", &mut v);
        // Private key blocks
        add(
            "private_key_block",
            r"-----BEGIN (RSA |EC |OPENSSH |PGP |DSA |ENCRYPTED )?PRIVATE KEY-----",
            &mut v,
        );
        // Authorization headers carrying material
        add("bearer_header", r"Authorization[:=][ ]*Bearer[ ][0-9A-Za-z._-]{16,}", &mut v);
        // Generic assignments of key-like names to literal values
        add(
            "key_assignment",
            r#"(?i)(api[_-]?key|secret[_-]?key|access[_-]?key|auth[_-]?token|client[_-]?secret)["']?[ ]*[=:][ ]*["']?[0-9A-Za-z_-]{16,}["']?"#,
            &mut v,
        );
        // RelayCheck deterministic test canaries
        add("relaycheck_canary", r"RC_CANARY[_0-9A-Za-z]{12,}", &mut v);
        v
    })
}

pub struct SecretMatch {
    pub pattern: &'static str,
    pub start: usize,
    pub end: usize,
}

/// Scans a byte buffer (lossy UTF-8) for high-confidence secrets.
pub fn scan_bytes(data: &[u8]) -> Option<SecretMatch> {
    let text = String::from_utf8_lossy(data);
    for m in matchers() {
        if let Some(mo) = m.re.find(&text) {
            return Some(SecretMatch {
                pattern: m.name,
                start: mo.start(),
                end: mo.end(),
            });
        }
    }
    None
}

/// Redacts every high-confidence match. Returns the redacted text plus the
/// number of redactions (the secret itself never survives).
pub fn redact_bytes(data: &[u8]) -> (String, usize) {
    let mut text = String::from_utf8_lossy(data).into_owned();
    let mut count = 0usize;
    for m in matchers() {
        let result = m.re.replace_all(&text, format!("[REDACTED:{}]", m.name));
        if result.len() != text.len() || result != text {
            let before = text.clone();
            text = result.into_owned();
            // Count replaced occurrences by diffing matches on the original.
            count += m.re.find_iter(&before).count();
        }
    }
    (text, count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_aws_key() {
        assert!(scan_bytes(b"id = AKIAIOSFODNN7EXAMPLE and more").is_some());
    }

    #[test]
    fn detects_private_key_header() {
        assert!(scan_bytes(b"-----BEGIN OPENSSH PRIVATE KEY-----\nabc").is_some());
        assert!(scan_bytes(b"-----BEGIN RSA PRIVATE KEY-----").is_some());
    }

    #[test]
    fn detects_sk_and_canary() {
        assert!(scan_bytes(b"Authorization: Bearer sk-1234567890abcdef1234567890").is_some());
        assert!(scan_bytes(b"token: RC_CANARY_LIVE_TOKEN_0123456789abcdef").is_some());
    }

    #[test]
    fn detects_key_assignment() {
        assert!(scan_bytes(b"apiKey = 'abcdefgh12345678abcdefgh'").is_some());
        assert!(scan_bytes(b"CLIENT_SECRET: \"abcdefgh12345678xyz\"").is_some());
    }

    #[test]
    fn clean_text_passes() {
        assert!(scan_bytes(b"just ordinary source code with no secrets 1234").is_none());
        assert!(scan_bytes(b"let x = compute_hash(input); // discussion").is_none());
    }

    #[test]
    fn redaction_removes_secret() {
        let (red, n) = redact_bytes(b"key AKIAIOSFODNN7EXAMPLE here");
        assert_eq!(n, 1);
        assert!(!red.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(red.contains("[REDACTED:"));
    }

    #[test]
    fn redaction_multiple_patterns() {
        let (red, n) = redact_bytes(b"a AKIAIOSFODNN7EXAMPLE b RC_CANARY_TEST_TOKEN_123456 c");
        assert!(n >= 2, "n={n}");
        assert!(!red.contains("AKIA"));
        assert!(!red.contains("RC_CANARY"));
    }
}
