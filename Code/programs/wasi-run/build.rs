// Раскладка памяти — общий скрипт всех userspace-программ VOID (база 0x4000_0000,
// W^X-сегменты): один источник правды в programs/user/linker.ld.
use std::path::PathBuf;

fn main() {
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let linker = dir.join("../user/linker.ld");
    println!("cargo:rustc-link-arg-bins=-T{}", linker.display());
    println!("cargo:rerun-if-changed={}", linker.display());
}
