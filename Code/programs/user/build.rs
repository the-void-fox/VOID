// Передаём линкеру свой скрипт раскладки памяти (см. linker.ld) — программы линкуются на
// фиксированную базу в регионе VPN[2]=1 (см. `proc::USER_REGION_START` в ядре), а НЕ на адрес
// 0x8020_0000, где живёт само ядро. `-link-arg-bins`: скрипт нужен только бинарям, не rlib'у
// библиотеки шимов.
use std::path::PathBuf;

fn main() {
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let linker = dir.join("linker.ld");
    println!("cargo:rustc-link-arg-bins=-T{}", linker.display());
    println!("cargo:rerun-if-changed={}", linker.display());
}
