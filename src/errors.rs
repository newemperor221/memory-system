//! MemorySystem v2 Error Types

use thiserror::Error;
use axum::Json;

#[derive(Error, Debug)]
pub enum MemoryError {
    #[error("L1 write failed: {0}")]
    L1WriteFailed(String),

    #[error("L1 entry not found: {0}")]
    L1NotFound(String),

    #[error("L2 write failed: {0}")]
    L2WriteFailed(String),

    #[error("L2 embedding failed: {0}")]
    L2EmbedFailed(String),

    #[error("L2 invalid vector (NaN/Inf): {0}")]
    L2InvalidVector(String),

    #[error("L2 is rebuilding, temporarily unavailable")]
    L2Rebuilding,

    #[error("L3 archive failed: {0}")]
    L3ArchiveFailed(String),

    #[error("L3 import failed: {0}")]
    L3ImportFailed(String),

    #[error("API key missing")]
    ApiKeyMissing,

    #[error("API key invalid")]
    ApiKeyInvalid,

    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("entry too large: {size} bytes (max {max}): {key}")]
    EntryTooLarge { key: String, size: usize, max: usize },

    #[error("invalid key: {0}")]
    InvalidKey(String),

    #[error("unauthorized: {0}")]
    Unauthorized(String),

    #[error("rate limited: {0}")]
    RateLimited(String),

    #[error("service unavailable: {0}")]
    ServiceUnavailable(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl serde::Serialize for MemoryError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl axum::response::IntoResponse for MemoryError {
    fn into_response(self) -> axum::response::Response {
        let status = match &self {
            MemoryError::L1NotFound(_) => axum::http::StatusCode::NOT_FOUND,
            MemoryError::InvalidRequest(_) => axum::http::StatusCode::BAD_REQUEST,
            MemoryError::Unauthorized(_) => axum::http::StatusCode::UNAUTHORIZED,
            MemoryError::RateLimited(_) => axum::http::StatusCode::TOO_MANY_REQUESTS,
            MemoryError::EntryTooLarge { .. } => axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            MemoryError::ServiceUnavailable(_) => axum::http::StatusCode::SERVICE_UNAVAILABLE,
            _ => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(serde_json::json!({
            "ok": false,
            "error": self.to_string()
        }))).into_response()
    }
}
