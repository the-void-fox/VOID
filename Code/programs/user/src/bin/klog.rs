//! `klog` — показать журнал ядра (Веха 116).
//!
//! ```text
//! klog        весь журнал, какой сохранился
//! klog 40     последние 40 строк
//! ```
//!
//! Зачем это отдельная программа, а не команда шелла: журнал нужен и в спасательном `vsh`, и в
//! `vvsh`, и внутри панели терминала, а копия одного и того же кода в трёх местах — верный способ
//! получить три разных поведения.
//!
//! Что здесь видно такого, чего не видно на экране: всё, что ядро напечатало ДО того, как
//! терминал забрал экран, и всё, что оно печатает потом (на машине без COM-порта этот вывод
//! иначе пропадает совсем — Веха 97 уводит консоль ядра в serial, которого у ноутбука нет).
#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec;
use void_user as sys;

/// Журнал в ядре — 64 КиБ; берём с запасом, чтобы забрать его целиком за один вызов.
const CAP: usize = 96 * 1024;

#[global_allocator]
static ALLOC: sys::heap::Heap<{ 512 * 1024 }> = sys::heap::Heap::new();

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let mut argbuf = [0u8; 128];
    let n = sys::args(&mut argbuf);
    let tail = argbuf[..n]
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .nth(1)
        .and_then(|s| core::str::from_utf8(s).ok())
        .and_then(|s| s.parse::<usize>().ok());

    let mut buf = vec![0u8; CAP];
    let (got, lost) = sys::klog(&mut buf);
    if got == 0 {
        sys::write("журнал ядра пуст\n".as_bytes());
        sys::exit(0);
    }
    let text = core::str::from_utf8(&buf[..got]).unwrap_or("журнал не UTF-8");

    // Потерянное кольцом называется вслух: «журнал начинается не с начала» — это факт, без
    // которого читатель решит, что загрузка началась с середины строки.
    if lost > 0 {
        sys::write(
            alloc::format!("— начало журнала вытеснено ({} Б потеряно) —\n", lost).as_bytes(),
        );
    }

    match tail {
        // Хвост печатаем по строкам: `klog 40` — то, что спрашивают чаще всего.
        Some(k) => {
            let lines: alloc::vec::Vec<&str> = text.lines().collect();
            let from = lines.len().saturating_sub(k);
            for line in &lines[from..] {
                sys::write(line.as_bytes());
                sys::write(b"\n");
            }
        }
        None => sys::write(text.as_bytes()),
    }
    sys::exit(0);
}
