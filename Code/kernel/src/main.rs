//! Микроядро VOID — bare-metal RISC-V, S-mode.
//!
//! Веха 1: загрузка из OpenSBI, настройка стека, обнуление .bss, вывод в UART.
//! См. роадмап и ADR в Obsidian (`10-projects/void/`).
#![no_std]
#![no_main]

mod uart;

use core::fmt::Write;
use core::panic::PanicInfo;

// Ассемблерная точка входа `_start` (см. entry.s) → вызывает `kmain`.
core::arch::global_asm!(include_str!("entry.s"));

/// Печать в UART0. Внутреннее API макросов `print!`/`println!`.
pub fn _print(args: core::fmt::Arguments) {
    let _ = uart::Uart.write_fmt(args);
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::_print(format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! println {
    ()              => { $crate::print!("\n") };
    ($($arg:tt)*)   => { $crate::print!("{}\n", format_args!($($arg)*)) };
}

/// Главная функция ядра. Вызывается из `_start` с a0=hartid, a1=dtb.
#[no_mangle]
pub extern "C" fn kmain(hartid: usize, dtb: usize) -> ! {
    println!();
    println!("  ╔══════════════════════════════════════════╗");
    println!("  ║  VOID — Веха 1                            ║");
    println!("  ║  персистентное контент-адресуемое        ║");
    println!("  ║  capability-ядро · RISC-V · Rust          ║");
    println!("  ╚══════════════════════════════════════════╝");
    println!();
    println!("  hart id : {}", hartid);
    println!("  dtb     : {:#x}", dtb);
    println!("  void-abi: v{}", void_abi::VERSION);
    println!();
    println!("  [ok] boot + UART работают. Парковка в wfi.");

    loop {
        // SAFETY: wfi — ждать прерывания; в S-mode разрешено.
        unsafe { core::arch::asm!("wfi") }
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!();
    println!("  [PANIC] {}", info);
    loop {
        unsafe { core::arch::asm!("wfi") }
    }
}
