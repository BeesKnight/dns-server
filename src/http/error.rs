use std::time::Duration;

use axum::response::{IntoResponse, Response};
use axum::Json;
use http::header::RETRY_AFTER;
use http::{HeaderValue, StatusCode};
use serde::{Deserialize, Serialize};
use tracing::error;

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    pub error: String,
    pub message: String,
    pub retryable: bool,
}

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
    pub retry_after: Option<Duration>,
}

impl ApiError {
    pub fn new(
        status: StatusCode,
        code: &'static str,
        message: impl Into<String>,
        retryable: bool,
    ) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            retryable,
            retry_after: None,
        }
    }

    pub fn rate_limited(endpoint: &'static str, retry_after: Duration) -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            code: "ERR_RATE_LIMITED",
            message: format!("rate limit exceeded for {}", endpoint),
            retryable: true,
            retry_after: Some(retry_after),
        }
    }

    pub fn with_retry_after(mut self, delay: Duration) -> Self {
        self.retry_after = Some(delay);
        self
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let payload = ErrorEnvelope {
            error: self.code.to_string(),
            message: self.message,
            retryable: self.retryable,
        };
        let mut response = (self.status, Json(payload)).into_response();
        if let Some(delay) = self.retry_after {
            if let Ok(header) = HeaderValue::from_str(&delay.as_secs().to_string()) {
                response.headers_mut().insert(RETRY_AFTER, header);
            }
        }
        response
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

impl From<crate::workers::storage::StorageError> for ApiError {
    fn from(value: crate::workers::storage::StorageError) -> Self {
        use crate::workers::storage::StorageError;
        match value {
            StorageError::AgentNotFound
            | StorageError::LeaseNotFound
            | StorageError::TaskNotFound => ApiError::new(
                StatusCode::NOT_FOUND,
                "ERR_NOT_FOUND",
                "resource not found",
                false,
            ),
            StorageError::AgentRevoked => ApiError::new(
                StatusCode::FORBIDDEN,
                "ERR_FORBIDDEN",
                "agent has been revoked",
                false,
            ),
            StorageError::AgentDisabled => ApiError::new(
                StatusCode::FORBIDDEN,
                "ERR_FORBIDDEN",
                "agent is disabled",
                false,
            ),
            StorageError::Forbidden(_) => ApiError::new(
                StatusCode::UNAUTHORIZED,
                "ERR_UNAUTHORIZED",
                "invalid credentials",
                false,
            ),
            StorageError::Database(err) => {
                error!(error = %err, "database error");
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "ERR_BACKEND",
                    "database operation failed",
                    true,
                )
            }
            StorageError::Serialization(err) => {
                error!(error = %err, "storage serialization error");
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "ERR_BACKEND",
                    "storage serialization error",
                    true,
                )
            }
            StorageError::Other(err) => {
                error!(error = %err, "storage backend error");
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "ERR_BACKEND",
                    "storage backend error",
                    true,
                )
            }
        }
    }
}
