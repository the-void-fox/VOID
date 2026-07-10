// Передаём линкеру свой скрипт раскладки памяти (см. linker.ld) — программа линкуется на
// фиксированную базу в регионе VPN[2]=1 (см. `proc::USER_REGION_START` в ядре), а НЕ на адрес
// 0x8020_0000, где живёт само ядро. Тот же трюк, что и в `kernel/build.rs`.
use std::path::PathBuf;

fn main() {
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let linker = dir.join("linker.ld");
    println!("cargo:rustc-link-arg=-T{}", linker.display());
    println!("cargo:rerun-if-changed={}", linker.display());
}
