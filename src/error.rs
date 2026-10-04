//! One error type for the whole API. Every failure leaves as the same JSON shape,
//! `{ "error": { "code": "...", "message": "..." } }`: `code` is stable for the websites to react
//! to, `message` is safe to show. Database and internal details are logged, never sent.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("sign in required")]
    Unauthorized,
    #[error("not allowed")]
    Forbidden,
    #[error("not found")]
    NotFound,
    #[error("conflict")]
    Conflict,
    /// Wrong or expired one-time code.
    #[error("that code didn't work")]
    OtpInvalid,
    /// Wrong email or password. One answer for both, so it can't be used to find staff emails.
    #[error("email or password is wrong")]
    InvalidCredentials,
    #[error("{0}")]
    RateLimited(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl AppError {
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::InvalidInput(msg.into())
    }

    fn parts(&self) -> (StatusCode, &'static str, String) {
        match self {
            Self::InvalidInput(m) => (StatusCode::BAD_REQUEST, "invalid_input", m.clone()),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized", self.to_string()),
            Self::Forbidden => (StatusCode::FORBIDDEN, "forbidden", self.to_string()),
            Self::NotFound | Self::Db(sqlx::Error::RowNotFound) => {
                (StatusCode::NOT_FOUND, "not_found", "not found".into())
            }
            Self::Conflict => (StatusCode::CONFLICT, "conflict", self.to_string()),
            Self::OtpInvalid => (StatusCode::UNAUTHORIZED, "otp_invalid", self.to_string()),
            Self::InvalidCredentials => (
                StatusCode::UNAUTHORIZED,
                "invalid_credentials",
                self.to_string(),
            ),
            Self::RateLimited(m) => (StatusCode::TOO_MANY_REQUESTS, "rate_limited", m.clone()),
            Self::Db(_) | Self::Other(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "something went wrong on our side".into(),
            ),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, message) = self.parts();
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = ?self, "internal error");
        }
        (
            status,
            Json(json!({ "error": { "code": code, "message": message } })),
        )
            .into_response()
    }
}

pub type Result<T> = std::result::Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_errors_never_leak_details() {
        let (status, code, msg) =
            AppError::Other(anyhow::anyhow!("password=hunter2 at db.internal:5432")).parts();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(code, "internal");
        assert!(!msg.contains("hunter2") && !msg.contains("5432"));
    }

    #[test]
    fn codes_and_statuses() {
        assert_eq!(AppError::OtpInvalid.parts().0, StatusCode::UNAUTHORIZED);
        assert_eq!(
            AppError::InvalidCredentials.parts().1,
            "invalid_credentials"
        );
        assert_eq!(
            AppError::RateLimited("slow down".into()).parts().0,
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            AppError::Db(sqlx::Error::RowNotFound).parts().0,
            StatusCode::NOT_FOUND
        );
    }
}
