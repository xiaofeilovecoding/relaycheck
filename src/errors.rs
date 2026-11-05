//! Error codes and the shared error type.
//!
//! Domain codes are the frozen set from the spec; HTTP plumbing errors reuse
//! the same envelope. `http_status` maps codes to response status codes and
//! the CLI maps them to exit codes (0 ok / 1 check failure / 2 bad args /
//! 3 blocked or conflict / 4 internal / 5 stop unknown).

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    // Frozen domain codes (spec §7)
    ERevisionConflict,
    EEvidenceStale,
    EProfileChanged,
    EUnverifiedImport,
    ECaptureUnstable,
    EPathBoundary,
    ESecretDetected,
    ELimit,
    EStopUnconfirmed,
    EEventGap,
    ECorrupt,
    EProfileUntrusted,
    EMissingDependency,
    // Protocol / plumbing codes
    EValidation,
    EUnauthorized,
    EForbidden,
    ENotFound,
    EState,
    ERequestConflict,
    EInternal,
}

impl Code {
    pub fn as_str(self) -> &'static str {
        match self {
            Code::ERevisionConflict => "E_REVISION_CONFLICT",
            Code::EEvidenceStale => "E_EVIDENCE_STALE",
            Code::EProfileChanged => "E_PROFILE_CHANGED",
            Code::EUnverifiedImport => "E_UNVERIFIED_IMPORT",
            Code::ECaptureUnstable => "E_CAPTURE_UNSTABLE",
            Code::EPathBoundary => "E_PATH_BOUNDARY",
            Code::ESecretDetected => "E_SECRET_DETECTED",
            Code::ELimit => "E_LIMIT",
            Code::EStopUnconfirmed => "E_STOP_UNCONFIRMED",
            Code::EEventGap => "E_EVENT_GAP",
            Code::ECorrupt => "E_CORRUPT",
            Code::EProfileUntrusted => "E_PROFILE_UNTRUSTED",
            Code::EMissingDependency => "E_MISSING_DEPENDENCY",
            Code::EValidation => "E_VALIDATION",
            Code::EUnauthorized => "E_UNAUTHORIZED",
            Code::EForbidden => "E_FORBIDDEN",
            Code::ENotFound => "E_NOT_FOUND",
            Code::EState => "E_STATE",
            Code::ERequestConflict => "E_REQUEST_CONFLICT",
            Code::EInternal => "E_INTERNAL",
        }
    }

    pub fn http_status(self) -> u16 {
        match self {
            Code::ERevisionConflict
            | Code::EEvidenceStale
            | Code::EProfileChanged
            | Code::EStopUnconfirmed
            | Code::ERequestConflict => 409,
            Code::EUnverifiedImport => 422,
            Code::ECaptureUnstable => 409,
            Code::EPathBoundary => 400,
            Code::ESecretDetected => 422,
            Code::ELimit => 429,
            Code::EEventGap => 409,
            Code::ECorrupt => 500,
            Code::EProfileUntrusted => 403,
            Code::EMissingDependency => 424,
            Code::EValidation => 400,
            Code::EUnauthorized => 401,
            Code::EForbidden => 403,
            Code::ENotFound => 404,
            Code::EState => 409,
            Code::EInternal => 500,
        }
    }

    /// CLI exit code per the frozen contract (spec §5).
    pub fn cli_exit(self) -> i32 {
        match self {
            Code::EMissingDependency | Code::EProfileUntrusted => 3,
            Code::ERevisionConflict
            | Code::EEvidenceStale
            | Code::EProfileChanged
            | Code::EUnverifiedImport
            | Code::ECaptureUnstable
            | Code::EStopUnconfirmed
            | Code::ERequestConflict
            | Code::EState
            | Code::EEventGap => 3,
            Code::EPathBoundary | Code::EValidation | Code::ELimit | Code::ENotFound => 2,
            Code::ESecretDetected => 1, // a blocked capture/verify result is a real failure signal
            Code::EForbidden | Code::EUnauthorized => 3,
            Code::ECorrupt | Code::EInternal => 4,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RelayError {
    pub code: Code,
    pub message: String,
    /// Safe, relative detail identifiers only. Never raw exceptions, never
    /// absolute paths, never secret material (spec §7).
    pub details: Vec<String>,
}

impl RelayError {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        RelayError { code, message: message.into(), details: Vec::new() }
    }

    pub fn with_details(mut self, details: Vec<String>) -> Self {
        self.details = details;
        self
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(Code::EValidation, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(Code::EInternal, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(Code::ENotFound, message)
    }

    pub fn state(message: impl Into<String>) -> Self {
        Self::new(Code::EState, message)
    }
}

impl fmt::Display for RelayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for RelayError {}

impl From<rusqlite::Error> for RelayError {
    fn from(e: rusqlite::Error) -> Self {
        RelayError::new(Code::EInternal, format!("database error: {e}"))
    }
}

impl From<std::io::Error> for RelayError {
    fn from(e: std::io::Error) -> Self {
        RelayError::new(Code::EInternal, format!("io error: {e}"))
    }
}

impl From<serde_json::Error> for RelayError {
    fn from(e: serde_json::Error) -> Self {
        RelayError::new(Code::EValidation, format!("json error: {e}"))
    }
}

pub type RelayResult<T> = Result<T, RelayError>;
