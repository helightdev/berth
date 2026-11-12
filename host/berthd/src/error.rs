use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use berth_api::{ErrorBody, ErrorDetail, code};

/// An error as it goes over the wire.
#[derive(Debug)]
pub struct E(pub (u16, &'static str), pub String);

pub type R<T> = Result<T, E>;

impl E {
    pub fn bad(message: impl Into<String>) -> E {
        E(code::BAD_REQUEST, message.into())
    }
    pub fn forbidden(message: impl Into<String>) -> E {
        E(code::FORBIDDEN, message.into())
    }
    pub fn missing(what: impl std::fmt::Display) -> E {
        E(code::NOT_FOUND, format!("No {what}"))
    }
    pub fn internal(error: impl std::fmt::Display) -> E {
        E(code::INTERNAL, error.to_string())
    }
    pub fn unavailable(message: impl Into<String>) -> E {
        E(code::NOT_AVAILABLE, message.into())
    }
}

impl From<std::io::Error> for E {
    fn from(error: std::io::Error) -> E {
        E::internal(error)
    }
}

impl IntoResponse for E {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.0.0).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = ErrorBody {
            error: ErrorDetail {
                code: self.0.1.into(),
                message: self.1,
            },
        };
        (status, Json(body)).into_response()
    }
}
