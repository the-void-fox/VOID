//! vvsh — вычислитель системного конфига VOID (ADR 0006, [[vvsh-config-layout]], M1a).
//!
//! Пока одна подкоманда:
//!   `run vvsh eval FILE`  — прочитать `.vv` из posixfs, вычислить НА VOID и напечатать
//!                           нормализованный системный конфиг (строки `service …`/`shell …` —
//!                           тот же формат, что сегодня даёт `nix/system.nix` и читает init).
//!
//! A2: вся логика языка — в общем крейте `vvsh-core` (host-тестируем, без QEMU). Права
//! наследуются от vsh через `run` (start-caps, как в install.rs): у `SYS_EXEC`-ребёнка a0/a1 НЕ
//! несут права — читаем `SYS_STARTCAP`. Порядок vsh (`endpoint:posixfs store:xw endpoint:net-srv`):
//! slot 0 = **posixfs-endpoint** (им читаем `/etc/system/*.vv`).
#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering};

use void_user as sys;
use void_user::posix as px;

// ── глобальный аллокатор: bump поверх ленивой кучи процесса (heap_map) ──────────
// vvsh короткоживущий (eval конфига → печать → exit) → dealloc no-op, арены хватает.
struct Bump;
const ARENA: usize = 4 * 1024 * 1024;
static BASE: AtomicUsize = AtomicUsize::new(0);
static NEXT: AtomicUsize = AtomicUsize::new(0);
static END: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Bump {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if BASE.load(Ordering::Relaxed) == 0 {
            let base = sys::heap_map(ARENA);
            if base == 0 || base == usize::MAX {
                return core::ptr::null_mut();
            }
            BASE.store(base, Ordering::Relaxed);
            NEXT.store(base, Ordering::Relaxed);
            END.store(base + ARENA, Ordering::Relaxed);
        }
        let align = layout.align();
        let aligned = (NEXT.load(Ordering::Relaxed) + align - 1) & !(align - 1);
        let new_next = aligned + layout.size();
        if new_next > END.load(Ordering::Relaxed) {
            return core::ptr::null_mut();
        }
        NEXT.store(new_next, Ordering::Relaxed);
        aligned as *mut u8
    }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[global_allocator]
static ALLOC: Bump = Bump;

// ── программа ───────────────────────────────────────────────────────────────
#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    // argv: [0]=vvsh, [1]=подкоманда, [2..]=её аргументы.
    let mut abuf = [0u8; 256];
    let n = sys::args(&mut abuf).min(abuf.len());
    let mut argv = abuf[..n].split(|&b| b == 0).filter(|s| !s.is_empty());
    let _name = argv.next();
    let sub = argv.next().unwrap_or(&[]);

    if sub == b"eval" {
        match argv.next() {
            Some(path) => cmd_eval(path),
            None => {
                sys::write("vvsh: eval: нужен путь к .vv-файлу\n".as_bytes());
                sys::exit(2);
            }
        }
    } else if sub.is_empty() {
        sys::write("vvsh - вычислитель конфига VOID (ADR 0006)\n".as_bytes());
        sys::write("  vvsh eval FILE   вычислить .vv и напечатать нормализованный конфиг\n".as_bytes());
        sys::exit(0);
    } else {
        sys::write("vvsh: неизвестная подкоманда: ".as_bytes());
        sys::write(sub);
        sys::write(b"\n");
        sys::exit(2);
    }
}

fn cmd_eval(path: &[u8]) -> ! {
    let ep = sys::start_cap(0); // posixfs-endpoint (унаследован от vsh)
    let src = match read_file(ep, path) {
        Some(s) => s,
        None => {
            sys::write("vvsh: не удалось прочитать файл: ".as_bytes());
            sys::write(path);
            sys::write(b"\n");
            sys::exit(1);
        }
    };
    let text = match core::str::from_utf8(&src) {
        Ok(t) => t,
        Err(_) => {
            sys::write("vvsh: файл не UTF-8\n".as_bytes());
            sys::exit(1);
        }
    };
    match vvsh_core::build_config(text) {
        Ok(out) => {
            sys::write(out.as_bytes());
            sys::exit(0);
        }
        Err(e) => {
            sys::write("vvsh: ошибка: ".as_bytes());
            sys::write(e.as_bytes());
            sys::write(b"\n");
            sys::exit(1);
        }
    }
}

/// Прочитать файл posixfs целиком в `Vec<u8>` (по 512-байтовым порциям). `None` — файла нет или
/// это каталог. Проверяем `stat` ДО `open`: у posixfs `open` создаёт файл, а нам от опечатки в
/// пути пустышки плодить незачем.
fn read_file(ep: usize, path: &[u8]) -> Option<Vec<u8>> {
    match px::stat(ep, path) {
        Some((is_dir, _)) if !is_dir => {}
        _ => return None,
    }
    let fd = px::open(ep, path, 0);
    if fd == usize::MAX {
        return None;
    }
    let mut out = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let k = px::read(ep, fd, &mut chunk);
        if k == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..k]);
    }
    px::close(ep, fd);
    Some(out)
}
