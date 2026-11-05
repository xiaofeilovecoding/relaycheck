//! ID and hash helpers.

use crate::errors::{RelayError, RelayResult};
use sha2::{Digest, Sha256};

pub fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub fn sha256_file(path: &std::path::Path) -> RelayResult<String> {
    let file = std::fs::File::open(path)
        .map_err(|e| RelayError::internal(format!("open failed: {e}")))?;
    let mut reader = std::io::BufReader::with_capacity(1024 * 1024, file);
    let mut hasher = Sha256::new();
    std::io::copy(&mut reader, &mut hasher)
        .map_err(|e| RelayError::internal(format!("read failed: {e}")))?;
    Ok(hex::encode(hasher.finalize()))
}

pub fn is_valid_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Validates a `sha256:<64hex>` snapshot/blob identifier.
pub fn parse_hash_id(s: &str) -> RelayResult<&str> {
    let rest = s
        .strip_prefix("sha256:")
        .ok_or_else(|| RelayError::validation("identifier must be sha256:<64hex>"))?;
    if !is_valid_sha256(rest) {
        return Err(RelayError::validation("identifier must be sha256:<64hex>"));
    }
    Ok(rest)
}

pub fn short_hash(s: &str) -> String {
    let bare = s.strip_prefix("sha256:").unwrap_or(s);
    let n = bare.chars().count().min(7);
    bare.chars().take(n).collect()
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn new_token(n_bytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; n_bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}
