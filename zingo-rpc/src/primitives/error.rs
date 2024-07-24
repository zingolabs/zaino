//! Holds Zingo-Proxy error types.

/// A serialization error.
#[derive(thiserror::Error, Debug)]
pub enum SerializationError {
    /// An io error that prevented deserialization
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The data to be deserialized was malformed.
    // TODO: refine errors
    #[error("parse error: {0}")]
    Parse(&'static str),

    /// A string was not UTF-8.
    ///
    /// Note: Rust `String` and `str` are always UTF-8.
    #[error("string was not UTF-8: {0}")]
    Utf8Error(#[from] std::str::Utf8Error),

    /// A slice was an unexpected length during deserialization.
    #[error("slice was the wrong length: {0}")]
    TryFromSliceError(#[from] std::array::TryFromSliceError),

    /// The length of a vec is too large to convert to a usize (and thus, too large to allocate on this platform)
    #[error("CompactSize too large: {0}")]
    TryFromIntError(#[from] std::num::TryFromIntError),

    /// A string was not valid hexadecimal.
    #[error("string was not hex: {0}")]
    FromHexError(#[from] hex::FromHexError),
}
