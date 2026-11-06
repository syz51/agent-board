use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use board_proto::{ApiError, ErrorCode};

/// Every failure of this crate. `Api` errors are answers to the caller; the others are
/// server faults, reported to callers as `internal` and logged.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}", .0.message)]
    Api(Box<ApiError>),
    #[error("database: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Config(String),
    #[error("{0}")]
    Internal(String),
}

pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub(crate) fn api(
        code: ErrorCode,
        message: impl Into<String>,
        next: impl Into<String>,
    ) -> Self {
        Self::Api(Box::new(ApiError {
            code,
            message: message.into(),
            next: Some(next.into()),
        }))
    }

    /// The API error code, if this is an answer to the caller.
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Api(e) => Some(e.code),
            _ => None,
        }
    }
}

pub(crate) fn invalid(message: impl Into<String>, next: impl Into<String>) -> Error {
    Error::api(ErrorCode::Invalid, message, next)
}

pub(crate) fn not_found(message: impl Into<String>, next: impl Into<String>) -> Error {
    Error::api(ErrorCode::NotFound, message, next)
}

pub(crate) fn forbidden(message: impl Into<String>, next: impl Into<String>) -> Error {
    Error::api(ErrorCode::Forbidden, message, next)
}

pub(crate) fn unauthorized(message: impl Into<String>, next: impl Into<String>) -> Error {
    Error::api(ErrorCode::Unauthorized, message, next)
}

pub(crate) fn conflict(message: impl Into<String>, next: impl Into<String>) -> Error {
    Error::api(ErrorCode::VersionConflict, message, next)
}

pub(crate) fn refused(message: impl Into<String>, next: impl Into<String>) -> Error {
    Error::api(ErrorCode::ClaimRefused, message, next)
}

pub(crate) fn bad_transition(message: impl Into<String>, next: impl Into<String>) -> Error {
    Error::api(ErrorCode::InvalidTransition, message, next)
}

pub(crate) fn status_of(code: ErrorCode) -> StatusCode {
    match code {
        ErrorCode::Invalid | ErrorCode::EvidenceRequired | ErrorCode::InvalidTransition => {
            StatusCode::BAD_REQUEST
        }
        ErrorCode::Unauthorized => StatusCode::UNAUTHORIZED,
        ErrorCode::Forbidden => StatusCode::FORBIDDEN,
        ErrorCode::NotFound => StatusCode::NOT_FOUND,
        ErrorCode::VersionConflict | ErrorCode::ClaimRefused => StatusCode::CONFLICT,
        ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let body = match self {
            Self::Api(e) => *e,
            other => {
                tracing::error!(error = %other, "request failed");
                ApiError {
                    code: ErrorCode::Internal,
                    message: "internal server error".into(),
                    next: Some(
                        "retry shortly; if it keeps failing, the server log has details".into(),
                    ),
                }
            }
        };
        (status_of(body.code), axum::Json(body)).into_response()
    }
}

macro_rules! rejection {
    ($($ty:ty),*) => {$(
        impl From<$ty> for Error {
            fn from(rejection: $ty) -> Self {
                invalid(rejection.body_text(), "fix the request to match docs/API.md and retry")
            }
        }
    )*};
}

rejection!(
    axum::extract::rejection::JsonRejection,
    axum::extract::rejection::QueryRejection,
    axum::extract::rejection::PathRejection
);
