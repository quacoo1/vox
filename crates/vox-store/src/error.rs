//! Store errors.

/// Errors surfaced by vox-store (most store operations log and continue instead of failing).
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("invalid JSON: {0}")]
    Json(String),
    #[error("invalid read id {0:?}")]
    InvalidId(String),
    #[error("secure storage failed: {0}")]
    Secret(#[from] crate::secrets::SecretError),
    #[error("{0}")]
    Other(String),
}
