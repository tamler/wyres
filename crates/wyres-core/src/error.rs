use serde::Serialize;

/// Errors returned by the Wyres core. Each variant maps to a stable, machine-readable code.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Ambiguous(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Serializable form of an [`Error`] for CLI `--json` output and MCP tool errors.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorBody {
    pub code: &'static str,
    pub message: String,
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Error::NotFound(_) => "not_found",
            Error::Invalid(_) => "invalid",
            Error::Conflict(_) => "conflict",
            Error::Ambiguous(_) => "ambiguous",
            Error::Forbidden(_) => "forbidden",
            Error::Db(_) => "db",
            Error::Io(_) => "io",
            Error::Json(_) => "json",
        }
    }

    pub fn body(&self) -> ErrorBody {
        ErrorBody {
            code: self.code(),
            message: self.to_string(),
        }
    }
}
