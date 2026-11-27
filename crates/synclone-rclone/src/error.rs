//! Errors returned by the rclone remote control API.

use std::fmt;

/// What kind of failure an rc call ended in. The sync engine uses this to
/// decide between retrying, backing off, and treating a path as gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// The object or directory does not exist (HTTP 404 from rc, or an
    /// "object not found" / "directory not found" message).
    NotFound,
    /// The provider told us to slow down (HTTP 429, "rate limit", "too many requests").
    RateLimited,
    /// rclone could not be reached, or the provider could not be reached.
    Network,
    /// The rc server refused our credentials.
    Unauthorized,
    /// rclone rejected the request (bad parameters, unknown remote...).
    BadRequest,
    /// The source changed while it was being copied (rclone's local backend
    /// reports "source file is being updated").
    SourceChanged,
    /// Anything else.
    Other,
}

#[derive(Debug, Clone)]
pub struct RcloneError {
    pub kind: ErrorKind,
    /// The rc path that failed, e.g. `operations/copyfile`.
    pub path: String,
    /// HTTP status from rc, when there was a response.
    pub status: Option<u16>,
    pub message: String,
}

impl RcloneError {
    pub fn new(kind: ErrorKind, path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind,
            path: path.into(),
            status: None,
            message: message.into(),
        }
    }

    /// Classify an rc error response.
    pub fn from_response(path: &str, status: u16, message: &str) -> Self {
        let lower = message.to_ascii_lowercase();
        let kind = if status == 401 || status == 403 && lower.contains("auth") {
            ErrorKind::Unauthorized
        } else if lower.contains("source file is being updated")
            || lower.contains("can't copy - source file is being updated")
        {
            ErrorKind::SourceChanged
        } else if status == 429
            || lower.contains("rate limit")
            || lower.contains("ratelimit")
            || lower.contains("too many requests")
            || lower.contains("user rate limit exceeded")
            || lower.contains("quota exceeded")
        {
            ErrorKind::RateLimited
        } else if status == 404
            || lower.contains("object not found")
            || lower.contains("directory not found")
            || lower.contains("not found")
            || lower.contains("doesn't exist")
            || lower.contains("no such file")
        {
            ErrorKind::NotFound
        } else if lower.contains("connection refused")
            || lower.contains("no such host")
            || lower.contains("i/o timeout")
            || lower.contains("network is unreachable")
            || lower.contains("connection reset")
        {
            ErrorKind::Network
        } else if status == 400 {
            ErrorKind::BadRequest
        } else {
            ErrorKind::Other
        };
        Self {
            kind,
            path: path.to_string(),
            status: Some(status),
            message: message.to_string(),
        }
    }

    pub fn is_not_found(&self) -> bool {
        self.kind == ErrorKind::NotFound
    }

    /// Worth trying again later (with backoff).
    pub fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            ErrorKind::RateLimited | ErrorKind::Network | ErrorKind::SourceChanged | ErrorKind::Other
        )
    }
}

impl fmt::Display for RcloneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.status {
            Some(s) => write!(f, "{} failed ({}): {}", self.path, s, self.message),
            None => write!(f, "{} failed: {}", self.path, self.message),
        }
    }
}

impl std::error::Error for RcloneError {}

pub type Result<T, E = RcloneError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_common_errors() {
        assert_eq!(
            RcloneError::from_response("operations/deletefile", 404, "object not found").kind,
            ErrorKind::NotFound
        );
        assert_eq!(
            RcloneError::from_response("operations/list", 500, "googleapi: Error 403: User Rate Limit Exceeded").kind,
            ErrorKind::RateLimited
        );
        assert_eq!(
            RcloneError::from_response(
                "operations/copyfile",
                500,
                "can't copy - source file is being updated (size changed from 1 to 2)"
            )
            .kind,
            ErrorKind::SourceChanged
        );
        assert_eq!(
            RcloneError::from_response("core/version", 401, "").kind,
            ErrorKind::Unauthorized
        );
        assert!(!RcloneError::from_response("x", 400, "bad").is_retryable());
    }
}
