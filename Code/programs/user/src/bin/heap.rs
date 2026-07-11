//! Демо кучи процесса (Веха 22): маппит 4 страницы через SYS_MAP (ядро НЕ выделяет ни одного
//! фрейма — ленивый резерв), пишет байт в КАЖДУЮ (каждая первая запись — page fault, по которому
//! ядро выделяет обнулённый фрейм и повторяет инструкцию), проверяет написанное и то, что
//! нетронутое обнулено.
#![no_std]
#![no_main]

use void_user as sys;

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let base = sys::heap_map(4 * 4096);
    if base == usize::MAX {
        sys::exit(1);
    }
    let heap = unsafe { core::slice::from_raw_parts_mut(base as *mut u8, 4 * 4096) };
    for i in 0..4 {
        heap[i * 4096 + i] = 0xA0 + i as u8; // разные страницы — 4 ленивых фолта
    }
    let mut ok = true;
    for i in 0..4 {
        if heap[i * 4096 + i] != 0xA0 + i as u8 {
            ok = false; // записанное читается
        }
        if heap[i * 4096 + 100] != 0 {
            ok = false; // нетронутый хвост страницы — нули (свежий фрейм)
        }
    }
    if ok {
        sys::write(b"[heap] 4 lazy pages: written, verified, rest is zeroed - OK\n");
    } else {
        sys::write(b"[heap] heap verification FAILED\n");
    }
    sys::exit(0);
}
