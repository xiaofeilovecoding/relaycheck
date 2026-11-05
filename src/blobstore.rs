//! Content-addressed blob storage under <data>/blobs.
//!
//! Writes go to a same-volume temp file, are flushed, then atomically
//! renamed to their final `blobs/<aa>/<sha256>` path. Integrity is verified
//! by re-hashing on read when requested (corruption → E_CORRUPT and the
//! snapshot is barred from checks/acceptance).

use crate::errors::{Code, RelayError, RelayResult};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    pub fn new(data_dir: &Path) -> RelayResult<BlobStore> {
        let root = data_dir.join("blobs");
        std::fs::create_dir_all(&root)?;
        Ok(BlobStore { root })
    }

    fn path_for(&self, sha256: &str) -> PathBuf {
        self.root.join(&sha256[..2]).join(sha256)
    }

    /// Stores bytes, verifying the caller-supplied hash. Returns the hash.
    pub fn put_bytes(&self, data: &[u8], expected_sha256: &str) -> RelayResult<String> {
        let actual = crate::ids::sha256_hex(data);
        if actual != expected_sha256 {
            return Err(RelayError::new(
                Code::ECaptureUnstable,
                "content hash mismatch while storing blob",
            ));
        }
        let final_path = self.path_for(&actual);
        if final_path.exists() {
            // Content-addressed: same hash means same content; verify size.
            let md = std::fs::metadata(&final_path)?;
            if md.len() as usize != data.len() {
                return Err(RelayError::new(Code::ECorrupt, "existing blob size mismatch"));
            }
            return Ok(actual);
        }
        std::fs::create_dir_all(self.root.join(&actual[..2]))?;
        let tmp = self.root.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            crate::ids::new_token(8)
        ));
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(data)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &final_path)?;
        Ok(actual)
    }

    /// Copies a file into the blob store by streaming (bounded by max_bytes).
    /// Returns (hash, size). Hashes while streaming, then re-opens the temp
    /// copy to verify stability before the final rename.
    pub fn put_file_streaming(
        &self,
        src: &Path,
        max_bytes: i64,
    ) -> RelayResult<(String, i64)> {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        let file = std::fs::File::open(src)
            .map_err(|e| RelayError::internal(format!("open failed: {e}")))?;
        let mut reader = std::io::BufReader::with_capacity(1024 * 1024, file);
        let mut size: i64 = 0;
        let mut buf = [0u8; 1024 * 256];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            size += n as i64;
            if size > max_bytes {
                return Err(RelayError::new(Code::ELimit, "file exceeds per-file size limit"));
            }
            hasher.update(&buf[..n]);
        }
        let hash = hex::encode(hasher.finalize());
        let final_path = self.path_for(&hash);
        if final_path.exists() {
            let md = std::fs::metadata(&final_path)?;
            if md.len() != size as u64 {
                return Err(RelayError::new(Code::ECorrupt, "existing blob size mismatch"));
            }
            return Ok((hash, size));
        }
        std::fs::create_dir_all(self.root.join(&hash[..2]))?;
        // Copy through a temp file, then re-hash the temp copy to confirm
        // the source stayed stable during the copy.
        let tmp = self.root.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            crate::ids::new_token(8)
        ));
        std::fs::copy(src, &tmp)
            .map_err(|e| RelayError::internal(format!("copy failed: {e}")))?;
        let copied_hash = crate::ids::sha256_file(&tmp)?;
        if copied_hash != hash {
            std::fs::remove_file(&tmp).ok();
            return Err(RelayError::new(
                Code::ECaptureUnstable,
                "file changed during capture; snapshot aborted",
            ));
        }
        std::fs::rename(&tmp, &final_path)?;
        Ok((hash, size))
    }

    /// Reads a blob with integrity verification.
    pub fn get(&self, sha256: &str) -> RelayResult<Vec<u8>> {
        let p = self.path_for(sha256);
        let data = std::fs::read(&p).map_err(|_| {
            RelayError::new(Code::ECorrupt, format!("blob missing: {}", &sha256[..12]))
        })?;
        let actual = crate::ids::sha256_hex(&data);
        if actual != sha256 {
            return Err(RelayError::new(Code::ECorrupt, "blob hash mismatch"));
        }
        Ok(data)
    }

    pub fn exists(&self, sha256: &str) -> bool {
        self.path_for(sha256).exists()
    }

    pub fn size_of(&self, sha256: &str) -> RelayResult<i64> {
        Ok(std::fs::metadata(self.path_for(sha256))?.len() as i64)
    }

    /// Extracts a blob to a destination path (streaming copy, creates dirs).
    pub fn copy_to(&self, sha256: &str, dest: &Path) -> RelayResult<()> {
        let src = self.path_for(sha256);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&src, dest)
            .map_err(|e| RelayError::new(Code::ECorrupt, format!("blob read failed: {e}")))?;
        // Verify the destination content hash (source-of-truth check).
        let h = crate::ids::sha256_file(dest)?;
        if h != sha256 {
            return Err(RelayError::new(Code::ECorrupt, "blob copy hash mismatch"));
        }
        Ok(())
    }
}
