// Передаём линкеру наш скрипт раскладки памяти (абсолютным путём, чтобы не зависеть
// от рабочей директории cargo).
use std::env;
use std::path::PathBuf;
use std::process::Command;

/// Все программы системы (бинари крейта `programs/user`). Ядро включает их байты как СЕМЕНА
/// (`include_bytes!` в main.rs) и на загрузке сеет в объектный store под корни `bin/<имя>` —
/// исполняются они всегда ИЗ store по content-id, никогда из образа ядра (Веха 23).
const PROGRAMS: &[&str] = &[
    "hello", "vsh", "posixfs", "mini-sh", "blk-srv", "blk-cli", "obj-srv", "obj-cli", "cap-srv",
    "cap-cli", "busy", "heap", "crash",
];

fn main() {
    let dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()); // .../Code/kernel

    // Веха 24: скрипт линковки — по архитектуре таргета (один проект, N образов).
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let (linker, prog_target) = match arch.as_str() {
        "riscv64" => (dir.join("linker.ld"), "riscv64gc-unknown-none-elf"),
        "x86_64" => (dir.join("linker-x86_64.ld"), "x86_64-unknown-none"),
        other => panic!("нет скрипта линковки для target_arch={other}"),
    };
    println!("cargo:rustc-link-arg=-T{}", linker.display());
    println!("cargo:rerun-if-changed={}", linker.display());

    build_user_programs(&dir, prog_target);
}

/// Веха 19.1/23 — собрать userspace-программы (крейт `programs/user`: библиотека шимов + все
/// бинари, НЕ член workspace ядра, см. `exclude` в ../Cargo.toml) ПОД АРХИТЕКТУРУ ЯДРА
/// (`target` — Веха 26: у каждой архитектуры свои семена) и передать пути к готовым ELF
/// через `cargo:rustc-env` (`PROG_<ИМЯ>`), чтобы `main.rs` мог включить их байты через
/// `include_bytes!(env!(...))`. Дочерний cargo наследует `Code/.cargo/config.toml` (CWD —
/// kernel/): оттуда x86-программы получают `relocation-model=static`, как и ядро.
///
/// ВАЖНО: собираем ОТДЕЛЬНЫМ вызовом `cargo build` с СОБСТВЕННЫМ `--target-dir` внутри `OUT_DIR`
/// ядра. Если бы программы делили `target/` с ядром, этот дочерний `cargo build` попытался бы
/// взять тот же файловый лок каталога target, который уже держит ВНЕШНИЙ cargo, собирающий
/// ядро (мы вызваны из его build-скрипта) — гарантированный deadlock.
fn build_user_programs(kernel_dir: &PathBuf, target: &str) {
    let workspace_dir = kernel_dir.parent().expect("kernel/.. должен существовать (Code/)");
    let program_dir = workspace_dir.join("programs").join("user");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let program_target_dir = out_dir.join("user-target");

    // Пересобрать программы, если поменялись их исходники/раскладка/манифест.
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
        .arg(target)
        .arg("--manifest-path")
        .arg(program_dir.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&program_target_dir)
        .status()
        .expect("не удалось запустить cargo для сборки programs/user");
    assert!(status.success(), "сборка programs/user (userspace ELF-программы) провалилась");

    let bin_dir = program_target_dir.join(target).join("release");
    for name in PROGRAMS {
        let elf_path = bin_dir.join(name);
        assert!(elf_path.is_file(), "ожидался готовый ELF по пути {}", elf_path.display());
        // `PROG_MINI_SH=...` и т.п. — имена env-переменных не терпят дефисов.
        let var = name.to_uppercase().replace('-', "_");
        println!("cargo:rustc-env=PROG_{}={}", var, elf_path.display());
    }
}
