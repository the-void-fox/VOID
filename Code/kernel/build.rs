// Передаём линкеру наш скрипт раскладки памяти (абсолютным путём, чтобы не зависеть
// от рабочей директории cargo).
use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()); // .../Code/kernel
    let linker = dir.join("linker.ld");
    println!("cargo:rustc-link-arg=-T{}", linker.display());
    println!("cargo:rerun-if-changed={}", linker.display());

    build_hello_program(&dir);
}

/// Веха 19.1 — собрать userspace-программу `programs/hello` (отдельный ELF-крейт, НЕ член
/// workspace ядра, см. `exclude` в ../Cargo.toml) и передать путь к готовому ELF через
/// `cargo:rustc-env`, чтобы `main.rs` мог включить его байты через `include_bytes!(env!(...))`.
///
/// ВАЖНО: собираем ОТДЕЛЬНЫМ вызовом `cargo build` с СОБСТВЕННЫМ `--target-dir` внутри `OUT_DIR`
/// ядра. Если бы программа делила `target/` с ядром, этот дочерний `cargo build` попытался бы
/// взять тот же файловый лок каталога target, который уже держит ВНЕШНИЙ cargo, собирающий
/// ядро (мы вызваны из его build-скрипта) — гарантированный deadlock.
fn build_hello_program(kernel_dir: &PathBuf) {
    let workspace_dir = kernel_dir.parent().expect("kernel/.. должен существовать (Code/)");
    let program_dir = workspace_dir.join("programs").join("hello");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let program_target_dir = out_dir.join("hello-target");

    // Пересобрать программу, если поменялись её исходники/раскладка/манифест.
    println!("cargo:rerun-if-changed={}", program_dir.join("src").display());
    println!("cargo:rerun-if-changed={}", program_dir.join("Cargo.toml").display());
    println!("cargo:rerun-if-changed={}", program_dir.join("linker.ld").display());
    println!("cargo:rerun-if-changed={}", program_dir.join("build.rs").display());

    // CARGO — путь к бинарнику cargo, которым нас сейчас собирают (переменная окружения build-
    // скрипта); используем ЕГО ЖЕ, а не «cargo» из PATH, чтобы гарантированно совпал toolchain.
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(&cargo)
        .arg("build")
        .arg("--release")
        .arg("--target")
        .arg("riscv64gc-unknown-none-elf")
        .arg("--manifest-path")
        .arg(program_dir.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&program_target_dir)
        .status()
        .expect("не удалось запустить cargo для сборки programs/hello");
    assert!(status.success(), "сборка programs/hello (userspace ELF-программа) провалилась");

    let elf_path = program_target_dir
        .join("riscv64gc-unknown-none-elf")
        .join("release")
        .join("hello");
    assert!(elf_path.is_file(), "ожидался готовый ELF по пути {}", elf_path.display());
    println!("cargo:rustc-env=HELLO_ELF={}", elf_path.display());
}
