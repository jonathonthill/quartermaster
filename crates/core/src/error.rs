use std::io;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("already exists: {0}")]
    AlreadyExists(String),
    #[error("not a folder: {0}")]
    NotADirectory(String),
    #[error("invalid path: {0}")]
    InvalidPath(String),
    /// Data did not match its checksum. Nothing was committed.
    #[error("verification failed: {0}")]
    Verify(String),
    /// Stored data is damaged or structurally invalid.
    #[error("corrupt data: {0}")]
    Corrupt(String),
    #[error("par2: {0}")]
    Par2(String),
    /// A file server's signed-in connection to an archive (see [`crate::link`]) is closed.
    #[error("not signed in to {0}")]
    SignedOut(String),
    #[error("{0}")]
    Other(String),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Db(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn other(msg: impl Into<String>) -> Self {
        Error::Other(msg.into())
    }

    /// The connection to a remote archive or file server dropped.
    pub fn is_lost(&self) -> bool {
        matches!(self, Error::Other(m) if m.starts_with("lost the connection"))
    }
}
