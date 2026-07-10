//! Первая userspace-программа VOID как отдельный ELF-крейт (Веха 19.1).
//!
//! В отличие от программ в `kernel/src/user.rs` (вшиты в секцию `.user` образа ядра, общий код
//! для всех процессов), эта — самостоятельный статический ELF64/RISC-V со своей точкой входа и
//! линковкой на фиксированную базу (см. `linker.ld`). `kernel/build.rs` собирает её ОТДЕЛЬНЫМ
//! `cargo build` (свой `--target-dir`, иначе deadlock на локе target ядра) и включает готовый
//! бинарник в образ ядра как байты (`include_bytes!`) — но лишь как *семя*: на первом запуске
//! ядро кладёт эти байты в объектный store и вешает корень `bin/hello`, а исполняет их оттуда —
//! по content-id, а не по адресу в своём образе (см. `kernel/src/elf.rs`, `proc::spawn_elf`,
//! [[exec-from-store]]).
//!
//! Никакого libc/std — только `core` и два ecall'а (ABI см. в шапке `kernel/src/user.rs`):
//! `1 = WRITE(ptr, len)`, `2 = EXIT(code)`.
#![no_std]
#![no_main]

use core::arch::asm;
use core::panic::PanicInfo;

const SYS_WRITE: usize = 1;
const SYS_EXIT: usize = 2;

static MSG: &[u8] =
    b"[hello] I'm an ELF loaded by content-id from the object store, not baked into the kernel image!\n";

/// Точка входа. Адрес берётся из `e_entry` ELF-заголовка загрузчиком ядра (kernel/src/elf.rs),
/// а НЕ из позиции `_start` в файле — поэтому обычная `extern "C" fn`, без асм-трамплина. Стек
/// (`sp`) и аргумент (`a0`) уже выставлены ядром в стартовом trap-кадре процесса (как для любого
/// процесса VOID, см. `proc::spawn`/`spawn_elf`) — единообразный ABI, хоть аргумент пока не нужен.
#[no_mangle]
pub extern "C" fn _start(_arg: usize) -> ! {
    unsafe {
        // SYS_WRITE(MSG, len) — ядро читает буфер процесса и печатает в консоль.
        asm!(
            "ecall",
            in("a7") SYS_WRITE,
            inout("a0") MSG.as_ptr() as usize => _,
            in("a1") MSG.len(),
            options(nostack),
        );
        // SYS_EXIT(0) — завершиться штатно.
        asm!("ecall", in("a7") SYS_EXIT, in("a0") 0usize, options(nostack, noreturn));
    }
}

/// Паника внутри процесса — завершиться ненулевым кодом через SYS_EXIT, а не уронить ядро
/// (у процесса нет собственного обработчика вывода стека — это не задача userspace-программы).
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    unsafe { asm!("ecall", in("a7") SYS_EXIT, in("a0") 1usize, options(nostack, noreturn)) }
}
