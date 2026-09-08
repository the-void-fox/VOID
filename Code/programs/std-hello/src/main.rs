//! Первая std-программа VOID (Веха 31, fs/alloc — Веха 32) — обычный Rust:
//! без `#![no_std]`, без `#![no_main]`, без единого syscall-шима в исходнике.
//! Всё, что здесь видно, идёт через порт std (vendor/rust): println →
//! SYS_WRITE, куча → free-list над SYS_MAP, args/env → SYS_ARGS, Instant →
//! rdtime/rdtsc, std::fs → IPC к posixfs (стартовая capability, слот 0),
//! exit → SYS_EXIT.

use std::collections::HashMap;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

fn main() {
    println!("[hello-std] Привет от НАСТОЯЩЕЙ std на VOID!");

    // Веха 86 — настоящие часы: `SystemTime` больше не идёт от выдуманной базы, а приходит
    // из ядра (`SYS_TIME`), которое прочитало RTC платформы на загрузке.
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => println!("[hello-std] SystemTime: {} с Unix (часы ядра)", d.as_secs()),
        Err(_) => println!("[hello-std] SystemTime: раньше эпохи?!"),
    }

    let args: Vec<String> = std::env::args().collect();
    println!("[hello-std] argv: {args:?}");
    println!(
        "[hello-std] env ARCH={} SYSTEM={}",
        std::env::var("ARCH").unwrap_or_else(|_| "?".into()),
        std::env::var("SYSTEM").unwrap_or_else(|_| "?".into()),
    );

    // Куча, итераторы, сортировка — рабочая нагрузка на аллокатор и стек.
    let t = Instant::now();
    let mut v: Vec<u64> = (1..=100_000).rev().collect();
    v.sort_unstable();
    let sum: u64 = v.iter().sum();
    println!("[hello-std] sort 100k + сумма = {sum} за {:?}", t.elapsed());

    // Веха 32: аллокатор теперь free-list — освобождённое ПЕРЕиспользуется.
    // Сто кругов «выделил мегабайт — отпустил»: bump-арена (16 МиБ) умерла бы
    // на 17-м, free-list живёт в одном и том же блоке.
    let t = Instant::now();
    for _ in 0..100 {
        let big: Vec<u8> = vec![0xAA; 1024 * 1024];
        std::hint::black_box(&big);
    }
    println!("[hello-std] alloc: 100 × (1 МиБ alloc+free) в арене 16 МиБ за {:?}", t.elapsed());

    // HashMap упражняет ГСЧ сидов (sys/random) и хэширование.
    let mut m = HashMap::new();
    m.insert("ядро", "VOID");
    m.insert("std", "родная");
    println!("[hello-std] HashMap работает: {} записи", m.len());

    // Веха 32: std::fs через IPC к посикс-персоналии. Файл переживает
    // перезагрузку и виден с другой архитектуры (и в vsh: cat std.txt).
    let arch = std::env::var("ARCH").unwrap_or_else(|_| "?".into());
    if let Err(e) = fs_demo(&arch) {
        println!("[hello-std] fs: ОШИБКА: {e}");
        std::process::exit(1);
    }

    println!("[hello-std] выходим с кодом 7 — проверка exit-кода");
    std::process::exit(7);
}

/// std::fs без единого шима: write → read_to_string → metadata → read_dir.
fn fs_demo(arch: &str) -> std::io::Result<()> {
    std::fs::write("std.txt", format!("привет из std::fs (писал {arch})\n"))?;
    let back = std::fs::read_to_string("std.txt")?;
    println!("[hello-std] fs: записал и перечитал std.txt: {:?}", back.trim_end());
    let meta = std::fs::metadata("std.txt")?;
    println!("[hello-std] fs: metadata: {} байт", meta.len());
    // Веха 177 — ВРЕМЯ файла из std. Оно только что записано, значит и разница с `now` должна
    // быть секундами, а не десятилетиями: печатаем именно её, потому что абсолютная дата на
    // машине без RTC ничего не доказывает, а «сколько прошло» доказывает.
    match meta.modified() {
        Ok(t) => println!(
            "[hello-std] fs: modified: {} с назад",
            std::time::SystemTime::now().duration_since(t).map(|d| d.as_secs()).unwrap_or(0)
        ),
        Err(e) => println!("[hello-std] fs: modified: НЕТ ({e})"),
    }
    // Веха 177 попутно починила и это: шестой байт ответа `stat` здесь не читали вовсе, и для
    // std каталогов не существовало — `metadata("/etc").is_dir()` отвечал «нет».
    println!(
        "[hello-std] fs: metadata(\"/etc\").is_dir() = {:?}",
        std::fs::metadata("/etc").map(|m| m.is_dir())
    );
    let names: Vec<String> = std::fs::read_dir(".")?
        .filter_map(|e| Some(e.ok()?.file_name().into_string().ok()?))
        .collect();
    println!("[hello-std] fs: в каталоге {} файлов: {names:?}", names.len());

    // Веха 176 — КАТАЛОГИ из std. До неё `create_dir` был `unsupported`, хотя каталоги у
    // персоналии есть с Вехи 44: просто никто не связал одно с другим. Здесь это и проверяется —
    // создать, положить внутрь, снести вместе с содержимым.
    let _ = std::fs::remove_dir_all("std-dir"); // от прошлого запуска
    std::fs::create_dir("std-dir")?;
    std::fs::create_dir("std-dir/sub")?;
    std::fs::write("std-dir/sub/deep.txt", "глубже\n")?;
    let deep = std::fs::read_to_string("std-dir/sub/deep.txt")?;
    println!("[hello-std] fs: каталоги: std-dir/sub/deep.txt = {:?}", deep.trim_end());
    std::fs::remove_dir_all("std-dir")?;
    println!(
        "[hello-std] fs: remove_dir_all снёс дерево: {}",
        if std::fs::metadata("std-dir").is_err() { "да" } else { "НЕТ" }
    );
    Ok(())
}
