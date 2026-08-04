//! Pure-Rust codecs for LZMA, LZMA2, and XZ — **закреплённая копия для VOID**.
//!
//! Апстрим — `lzma-rs` 0.3.0 (Guillaume Endignoux, MIT). Что изменено, целиком:
//!
//! - крейт переведён в `no_std` + `alloc` — у программ VOID нет ни std, ни libc;
//! - оставлена только РАСПАКОВКА: `encode/` и потоковый `decode/stream.rs` выброшены. Сжимать нам
//!   нечего (архивы приходят из бинарного кэша nixpkgs), а `stream` тянул `std::io` глубже всех;
//! - три зависимости заменены шимами внутри копии: [`io`] (часть `std::io`), [`byteorder`] и
//!   [`crc`]. Ни одной внешней зависимости у крейта теперь нет — сознательно: унификация фич в
//!   общем дереве однажды включила бы какому-нибудь из них `std`, которого здесь не существует.
//!
//! Тела декодеров (LZMA, LZMA2, разбор контейнера xz) НЕ тронуты — правился только их контакт с
//! внешним миром. Это то же правило, что со smoltcp, rustls и ereb: подменять границу, а не тело,
//! иначе обновление апстрима превращается в переписывание.

#![cfg_attr(not(test), no_std)]
#![deny(missing_debug_implementations)]
#![forbid(unsafe_code)]

extern crate alloc;

#[macro_use]
mod macros;

pub mod byteorder;
pub mod crc;
pub mod io;

mod decode;

pub mod error;

mod util;
mod xz;

/// Decompression helpers.
pub mod decompress {
    pub use crate::decode::options::*;
}

/// Decompress LZMA data with default [`Options`](decompress/struct.Options.html).
pub fn lzma_decompress<R: io::BufRead, W: io::Write>(
    input: &mut R,
    output: &mut W,
) -> error::Result<()> {
    lzma_decompress_with_options(input, output, &decompress::Options::default())
}

/// Decompress LZMA data with the provided options.
pub fn lzma_decompress_with_options<R: io::BufRead, W: io::Write>(
    input: &mut R,
    output: &mut W,
    options: &decompress::Options,
) -> error::Result<()> {
    let params = decode::lzma::LzmaParams::read_header(input, options)?;
    let mut decoder = decode::lzma::LzmaDecoder::new(params, options.memlimit)?;
    decoder.decompress(input, output)
}

/// Decompress LZMA2 data with default [`Options`](decompress/struct.Options.html).
pub fn lzma2_decompress<R: io::BufRead, W: io::Write>(
    input: &mut R,
    output: &mut W,
) -> error::Result<()> {
    decode::lzma2::Lzma2Decoder::new().decompress(input, output)
}

/// Decompress XZ data with default [`Options`](decompress/struct.Options.html).
pub fn xz_decompress<R: io::BufRead, W: io::Write>(
    input: &mut R,
    output: &mut W,
) -> error::Result<()> {
    decode::xz::decode_stream(input, output)
}
