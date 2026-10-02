use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use miette::Diagnostic;

#[derive(Debug, thiserror::Error, Diagnostic)]
pub enum Error {
    #[error("io error: {0}")]
    #[diagnostic(code(docket::io))]
    Io(#[from] std::io::Error),

    #[error("database error: {0}")]
    #[diagnostic(code(docket::db))]
    Db(#[from] rusqlite::Error),

    #[error("no user identity on the request")]
    #[diagnostic(code(docket::unauthenticated))]
    Unauthenticated,

    #[error("no such {0}")]
    #[diagnostic(code(docket::not_found))]
    NotFound(&'static str),

    #[error("{0}")]
    #[diagnostic(code(docket::forbidden))]
    Forbidden(&'static str),

    #[error("{0}")]
    #[diagnostic(code(docket::bad_request))]
    BadRequest(&'static str),
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match self {
            Error::Io(_) | Error::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Error::Unauthenticated => StatusCode::UNAUTHORIZED,
            Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::Forbidden(_) => StatusCode::FORBIDDEN,
            Error::BadRequest(_) => StatusCode::BAD_REQUEST,
        };
        (status, self.to_string()).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses() {
        let cases = [
            (Error::Io(std::io::Error::other("disk")), 500),
            (Error::Db(rusqlite::Error::QueryReturnedNoRows), 500),
            (Error::Unauthenticated, 401),
            (Error::NotFound("thread"), 404),
            (Error::Forbidden("read-only"), 403),
            (Error::BadRequest("empty"), 400),
        ];
        for (err, status) in cases {
            assert_eq!(err.into_response().status().as_u16(), status);
        }
    }
}
