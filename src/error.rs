use axum::{
    http::{self, StatusCode},
    response::IntoResponse,
};

#[derive(thiserror::Error, Debug)]
pub enum RequestError {
    #[error("Failed to presign request for object {object_key:?}")]
    PresignFailure { object_key: String },
    #[error("Failed to create presign configuration")]
    PresignConfigFailure,
    #[error("There is no such channel: {file_name:?}")]
    NoSuchChannel { file_name: String },

    #[error("Invalid token: {reason}")]
    InvalidToken { reason: String },
    #[error("Unsupported HTTP method: {method}")]
    UnsupportedMethod { method: http::Method },
    #[error("Unknown error")]
    Unknown,
}

impl IntoResponse for RequestError {
    fn into_response(self) -> axum::response::Response {
        (
            match self {
                RequestError::NoSuchChannel { file_name: _ } => StatusCode::NOT_FOUND,
                RequestError::InvalidToken { reason: _ } => StatusCode::FORBIDDEN,
                RequestError::UnsupportedMethod { method: _ } => StatusCode::METHOD_NOT_ALLOWED,
                RequestError::PresignConfigFailure
                | RequestError::PresignFailure { object_key: _ }
                | RequestError::Unknown => StatusCode::INTERNAL_SERVER_ERROR,
            },
            format!("{}", self),
        )
            .into_response()
    }
}
