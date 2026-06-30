// Передаём линкеру наш скрипт раскладки памяти (абсолютным путём, чтобы не зависеть
// от рабочей директории cargo).
use std::path::PathBuf;

fn main() {
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let linker = dir.join("linker.ld");
    println!("cargo:rustc-link-arg=-T{}", linker.display());
    println!("cargo:rerun-if-changed={}", linker.display());
}
