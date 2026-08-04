//! Проверка копии на НАСТОЯЩИХ архивах `xz` — тестов апстрима здесь нет намеренно: они опирались
//! на `std::io::Seek` и на сравнение с C-библиотекой, а нам важно другое — что после перевода в
//! `no_std` и подмены `std::io`/`byteorder`/`crc` шимами распаковка даёт БАЙТ В БАЙТ то же, что
//! `xz -d`, и что порча архива приводит к ошибке, а не к панике.
//!
//! Фикстуры собраны так (эталон восстанавливается функцией `expected()` ниже):
//! ```sh
//! xz -9 -c lines.txt > tests/fixtures/lines.xz              # проверка CRC64 (умолчание xz)
//! xz -0 --check=crc32 -c lines.txt > tests/fixtures/lines-crc32.xz
//! ```

use std::fmt::Write as _;

const LINES_XZ: &[u8] = include_bytes!("fixtures/lines.xz");
const LINES_CRC32_XZ: &[u8] = include_bytes!("fixtures/lines-crc32.xz");

/// Эталон — ровно то, что было сжато.
fn expected() -> Vec<u8> {
    let mut s = String::new();
    for i in 0..2000 {
        let _ = writeln!(s, "строка {}: VOID кладёт NAR в контент-адресуемый store", i);
    }
    s.into_bytes()
}

fn unxz(input: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    lzma_rs::xz_decompress(&mut { input }, &mut out).map_err(|e| format!("{}", e))?;
    Ok(out)
}

/// Основной случай: `xz -9`, проверка CRC64 — то, чем сжат бинарный кэш nixpkgs.
#[test]
fn xz9_crc64_matches_original() {
    let out = unxz(LINES_XZ).expect("распаковка");
    assert_eq!(out, expected());
}

/// Второй вид проверки в контейнере xz — CRC32. Считается другой таблицей, поэтому проверяется
/// отдельно: ошибка в одной из них иначе осталась бы незамеченной.
#[test]
fn xz0_crc32_matches_original() {
    let out = unxz(LINES_CRC32_XZ).expect("распаковка");
    assert_eq!(out, expected());
}

/// Обрезанный архив — ошибка, а не паника: вход приезжает из сети.
#[test]
fn truncated_is_error() {
    let err = unxz(&LINES_XZ[..LINES_XZ.len() / 2]).unwrap_err();
    assert!(!err.is_empty(), "ошибка должна иметь текст");
}

/// Испорченный байт внутри сжатых данных обязан не пройти проверку CRC — ради этого она и
/// оставлена в копии.
#[test]
fn corrupted_payload_fails_check() {
    let mut bad = LINES_XZ.to_vec();
    let mid = bad.len() / 2;
    bad[mid] ^= 0xFF;
    assert!(unxz(&bad).is_err(), "порча данных проехала молча");
}

/// Не архив вовсе — понятная ошибка про магию xz.
#[test]
fn not_xz_is_error() {
    let err = unxz(b"nix-archive-1 and nothing else").unwrap_err();
    assert!(err.contains("magic"), "получили: {}", err);
}
