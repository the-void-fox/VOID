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
    "cap-cli", "busy", "heap", "crash", "bench", "net-srv", "threads", "freeze", "e1000d",
    "lx_e1000", "install", "vvsh", "httpsc", "spawn-demo", "stdio-demo", "pkg", "klog", "ved",
    "hostile",
];

/// Программы ТОЛЬКО ДЛЯ x86_64 (Веха 97). `term` рисует в пиксельный фреймбуфер, а на riscv его
/// нет вовсе (в QEMU `virt` нет дисплея) — сеять туда нечего. Дело не только в бесполезности:
/// с вшитым шрифтом бинарь весит 2.7 МБ, и на riscv его посев ПАДАЛ — куча ядра (16 МиБ) не
/// давала такой кусок поверх кэша store. Это же и есть довод перенести шрифт в store отдельным
/// объектом-деревом (Веха 94 умеет), а не носить его в ELF.
const PROGRAMS_X86: &[&str] = &["term", "wm", "winbox"];

/// Веха 132 — C-драйверы (портированный код Linux, сборка nix'ом). Едут семенами В ЯДРЕ, как и
/// программы на Rust: до store целевой машины иначе не добраться (см. `stage_c_drivers`).
const C_DRIVERS: &[&str] = &["lx-atl1c-hw", "lx-atl1c-full"];

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

    build_user_programs(&dir, prog_target, arch == "x86_64");
    stage_c_drivers(&dir, &arch);
}

/// Веха 132 — C-ДРАЙВЕРЫ, собранные nix'ом (портированный код Linux), в семена ядра.
///
/// Зачем это здесь, а не мостом с хоста, как раньше. Мост (`void-store-import`) пишет в store
/// ОБРАЗА, и на машине разработчика этого хватает. Но на ноутбуке store живёт на внутреннем
/// SATA-диске, а грузится система с флешки: USB для VOID вообще не блочное устройство (xHCI
/// поднят ради клавиатуры). То есть всё, что мост положил в образ на флешке, на этой машине
/// недостижимо в принципе — драйвер туда не доставить никак, кроме как ВНУТРИ ЯДРА.
///
/// Артефакт nix'а сюда не собирается: `cargo build` не должен зависеть от того, доступен ли
/// nix. Драйвер выкладывается заранее (`Code/tools/stage-drivers.sh`), а если его нет —
/// подставляется ПУСТОЕ семя, и об этом говорят и сборка, и загрузка. Молчаливое отсутствие
/// драйвера выглядело бы как «карта не работает», и искать причину пришлось бы на железе.
fn stage_c_drivers(kernel_dir: &PathBuf, arch: &str) {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let staged = kernel_dir
        .parent()
        .expect("kernel/.. — Code/")
        .join("programs/lx-linux/prebuilt")
        .join(arch);
    println!("cargo:rerun-if-changed={}", staged.display());

    for name in C_DRIVERS {
        let var = name.to_uppercase().replace('-', "_");
        let src = staged.join(name);
        let path = if src.is_file() {
            src
        } else {
            // Пустой файл-заглушка: `include_bytes!` нужен путь, существующий во время сборки,
            // а нулевая длина — признак «драйвера нет», который ядро проверяет при посеве.
            let stub = out_dir.join(format!("{name}.absent"));
            std::fs::write(&stub, b"").expect("не записать заглушку драйвера");
            println!(
                "cargo:warning=C-драйвер {name} ({arch}) не выложен — ядро соберётся БЕЗ него; \
                 собрать: Code/tools/stage-drivers.sh"
            );
            stub
        };
        println!("cargo:rustc-env=DRV_{}={}", var, path.display());
    }
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
fn build_user_programs(kernel_dir: &PathBuf, target: &str, x86: bool) {
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
    // x86-only программы участвуют только в x86-сборке (см. PROGRAMS_X86).
    let all = PROGRAMS.iter().chain(if x86 { PROGRAMS_X86 } else { &[] });
    for name in all {
        let elf_path = bin_dir.join(name);
        assert!(elf_path.is_file(), "ожидался готовый ELF по пути {}", elf_path.display());
        // `PROG_MINI_SH=...` и т.п. — имена env-переменных не терпят дефисов.
        let var = name.to_uppercase().replace('-', "_");
        println!("cargo:rustc-env=PROG_{}={}", var, elf_path.display());
    }
}
