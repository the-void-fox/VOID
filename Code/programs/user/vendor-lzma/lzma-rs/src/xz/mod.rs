//! Logic for handling `.xz` file format.
//!
//! Format specifications are at [https://tukaani.org/xz/xz-file-format.txt](spec).
//!
//! [spec]: https://tukaani.org/xz/xz-file-format.txt

use alloc::format;
use crate::error;

pub(crate) mod crc;
pub(crate) mod footer;
pub(crate) mod header;

/// Stream flags, see sect. 2.1.1.2.
///
/// This does not store the leading null byte, which is currently unused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StreamFlags {
    pub(crate) check_method: CheckMethod,
}

impl StreamFlags {
    /// Parse Stream Flags from a 16bits value.
    pub(crate) fn parse(input: u16) -> error::Result<Self> {
        let flags_bytes = input.to_be_bytes();

        if flags_bytes[0] != 0x00 {
            return Err(error::Error::XzError(format!(
                "Invalid null byte in Stream Flags: {:x}",
                flags_bytes[0]
            )));
        }

        let flags = Self {
            check_method: CheckMethod::try_from(flags_bytes[1])?,
        };
        Ok(flags)
    }

    // `serialize` из апстрима убран: это сторона ЗАПИСИ, а копия только распаковывает.
}

/// Stream check type, see sect. 2.1.1.2.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CheckMethod {
    None = 0x00,
    Crc32 = 0x01,
    Crc64 = 0x04,
    Sha256 = 0x0A,
}

impl CheckMethod {
    /// Parse Check ID (second byte in Stream Flags).
    pub fn try_from(id: u8) -> error::Result<CheckMethod> {
        match id {
            0x00 => Ok(CheckMethod::None),
            0x01 => Ok(CheckMethod::Crc32),
            0x04 => Ok(CheckMethod::Crc64),
            0x0A => Ok(CheckMethod::Sha256),
            _ => Err(error::Error::XzError(format!(
                "Invalid check method {:x}, expected one of [0x00, 0x01, 0x04, 0x0A]",
                id
            ))),
        }
    }
}

impl From<CheckMethod> for u8 {
    fn from(method: CheckMethod) -> u8 {
        method as u8
    }
}

#[cfg(test)]
mod test {
    use super::*;

    // Апстримный `test_streamflags_roundtrip` убран вместе с `serialize` (сторона записи) —
    // он же был единственным местом, требовавшим `Seek`.
    #[test]
    fn test_checkmethod_roundtrip() {
        let mut count_valid = 0;
        for input in 0..u8::MAX {
            if let Ok(check) = CheckMethod::try_from(input) {
                let output: u8 = check.into();
                assert_eq!(input, output);
                count_valid += 1;
            }
        }
        assert_eq!(count_valid, 4);
    }
}
