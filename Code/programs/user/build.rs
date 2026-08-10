// Передаём линкеру свой скрипт раскладки памяти (см. linker.ld) — программы линкуются на
// фиксированную базу в регионе VPN[2]=1 (см. `proc::USER_REGION_START` в ядре), а НЕ на адрес
// 0x8020_0000, где живёт само ядро. `-link-arg-bins`: скрипт нужен только бинарям, не rlib'у
// библиотеки шимов.
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let linker = dir.join("linker.ld");
    println!("cargo:rustc-link-arg-bins=-T{}", linker.display());
    println!("cargo:rerun-if-changed={}", linker.display());
    println!("cargo:rustc-env=VOID_BUILD={}", stamp(&dir));
}

/// Отметка сборки: `<хэш коммита>[+] <дата>`. Печатается программами на старте.
///
/// Веха 122.1 — раньше это была КОНСТАНТА В КОДЕ («Веха 121.2»), и она соврала при первой же
/// возможности: следующая веха её не подняла, а владелец два раза перезаписал флешку, пытаясь
/// понять, почему система «старая». Она была новая — врала строка.
///
/// Поэтому теперь отметка берётся у git и не может отстать от кода. `+` значит «в рабочем дереве
/// есть несохранённые правки»: собранное из грязного дерева воспроизвести по хэшу нельзя, и знать
/// об этом важнее, чем красиво выглядеть.
fn stamp(dir: &PathBuf) -> String {
    // Пересобираться при смене коммита: без этого отметка застынет на той, что была при первой
    // сборке (исходники-то не менялись).
    if let Some(git) = dir.ancestors().map(|p| p.join(".git")).find(|p| p.exists()) {
        println!("cargo:rerun-if-changed={}", git.join("HEAD").display());
        if let Ok(head) = std::fs::read_to_string(git.join("HEAD")) {
            if let Some(r) = head.strip_prefix("ref: ") {
                println!("cargo:rerun-if-changed={}", git.join(r.trim()).display());
            }
        }
    }
    let run = |args: &[&str]| -> Option<String> {
        let out = Command::new("git").args(args).current_dir(dir).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let hash = run(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "без-git".into());
    let dirty = match run(&["status", "--porcelain"]) {
        Some(s) if !s.is_empty() => "+",
        _ => "",
    };
    let date = run(&["log", "-1", "--format=%cs"]).unwrap_or_default();
    format!("{hash}{dirty} {date}").trim().to_string()
}
