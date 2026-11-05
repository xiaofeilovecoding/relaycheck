//! Operation manager: the single write-job slot, cancellation registry, and
//! crash recovery. Only one capture/check/import runs at a time (spec §3);
//! cancellation is cooperative via a flag plus process-tree kill; a service
//! restart marks unfinished operations interrupted without rerunning them.

use crate::checks::CancelFlag;
use crate::errors::{Code, RelayError, RelayResult};
use crate::store::Store;
use std::collections::HashMap;
use std::sync::Mutex;

pub struct OpEntry {
    pub cancel: CancelFlag,
    pub kind: String,
}

pub struct OpManager {
    inner: Mutex<OpInner>,
}

struct OpInner {
    active: Option<(String, OpEntry)>,
}

impl OpManager {
    pub fn new() -> OpManager {
        OpManager { inner: Mutex::new(OpInner { active: None }) }
    }

    /// Tries to claim the single write slot. Returns the cancel flag.
    pub fn begin(&self, op_id: &str, kind: &str) -> RelayResult<CancelFlag> {
        let mut g = self.inner.lock().unwrap();
        if g.active.is_some() {
            return Err(RelayError::new(
                Code::ELimit,
                "another capture/check/import job is active; it must finish or be cancelled first",
            )
            .with_details(vec![format!("kind={kind}")]));
        }
        let cancel = CancelFlag::new();
        g.active =
            Some((op_id.to_string(), OpEntry { cancel: cancel.clone(), kind: kind.to_string() }));
        Ok(cancel)
    }

    pub fn finish(&self, op_id: &str) {
        let mut g = self.inner.lock().unwrap();
        if let Some((id, _)) = g.active.as_ref() {
            if id == op_id {
                g.active = None;
            }
        }
    }

    /// Requests cancellation of an active operation. Returns false when the
    /// op is unknown or already finished. The caller still needs to wait for
    /// the worker to observe the flag (stop confirmation lives in checks.rs).
    pub fn cancel(&self, op_id: &str) -> RelayResult<bool> {
        let g = self.inner.lock().unwrap();
        if let Some((id, entry)) = g.active.as_ref() {
            if id == op_id {
                entry.cancel.cancel();
                return Ok(true);
            }
        }
        let _ = RelayError::not_found("operation is not active");
        Ok(false)
    }

    pub fn is_active(&self, op_id: &str) -> bool {
        let g = self.inner.lock().unwrap();
        matches!(g.active.as_ref(), Some((id, _)) if id == op_id)
    }

    /// True when any write job is currently active (blocks acceptance).
    pub fn any_active(&self) -> bool {
        let g = self.inner.lock().unwrap();
        g.active.is_some()
    }

    /// Startup recovery: any DB operation still pending/running is marked
    /// interrupted — we never re-dispatch a check from logs or state (§5).
    pub fn recover_stale(&self, store: &Store) -> RelayResult<usize> {
        store.mark_stale_running_interrupted()
    }
}
