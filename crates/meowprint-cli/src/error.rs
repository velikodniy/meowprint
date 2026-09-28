//! Errors from content rendering. Printer errors remain separate in the library.
#[derive(Debug, thiserror::Error)]
/// Rendering failures, separate from printer operations.
pub enum Error {
    #[error("{0}")]
    InvalidInput(String),
    #[error("{0}")]
    Image(#[from] image::ImageError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}
/// A rendering result.
pub type Result<T> = std::result::Result<T, Error>;
/// Build a content validation error.
pub fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidInput(message.into())
}
