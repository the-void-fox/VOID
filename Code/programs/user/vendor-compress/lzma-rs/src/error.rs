//! Error handling.

use crate::io;
use alloc::string::String;
use core::fmt::Display;
use core::result;

/// Library errors.
#[derive(Debug)]
pub enum Error {
    /// I/O error.
    IoError(io::Error),
    /// Not enough bytes to complete header
    HeaderTooShort(io::Error),
    /// LZMA error.
    LzmaError(String),
    /// XZ error.
    XzError(String),
}

/// Library result alias.
pub type Result<T> = result::Result<T, Error>;

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::IoError(e)
    }
}

impl Display for Error {
    fn fmt(&self, fmt: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::IoError(e) => write!(fmt, "io error: {}", e),
            Error::HeaderTooShort(e) => write!(fmt, "header too short: {}", e),
            Error::LzmaError(e) => write!(fmt, "lzma error: {}", e),
            Error::XzError(e) => write!(fmt, "xz error: {}", e),
        }
    }
}

// `impl std::error::Error` из апстрима убран: в no_std такого трейта нет, а нужен он был только
// для цепочки `source()`, которой в VOID некому пользоваться — ошибка доезжает текстом.

#[cfg(test)]
mod test {
    use super::Error;
    use alloc::string::ToString;

    #[test]
    fn test_display() {
        assert_eq!(
            Error::IoError(crate::io::Error::new(
                crate::io::ErrorKind::Other,
                "this is an error"
            ))
            .to_string(),
            "io error: this is an error"
        );
        assert_eq!(
            Error::LzmaError("this is an error".to_string()).to_string(),
            "lzma error: this is an error"
        );
        assert_eq!(
            Error::XzError("this is an error".to_string()).to_string(),
            "xz error: this is an error"
        );
    }
}
