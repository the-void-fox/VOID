//! Микроядро VOID — bare-metal RISC-V, S-mode.
//!
//! Веха 1: загрузка из OpenSBI, настройка стека, обнуление .bss, вывод в UART.
//! Веха 2: вектор trap'ов, обработка исключений (ebreak) и таймерные прерывания.
//! См. роадмап и ADR в Obsidian (`10-projects/void/`).
#![no_std]
#![no_main]

mod csr;
mod sbi;
mod timer;
mod trap;
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
    println!("  ║  VOID — Веха 2                            ║");
    println!("  ║  trap'ы, исключения и таймер · RISC-V     ║");
    println!("  ╚══════════════════════════════════════════╝");
    println!();
    println!("  hart id : {}", hartid);
    println!("  dtb     : {:#x}", dtb);
    println!("  void-abi: v{}", void_abi::VERSION);
    println!();

    // 1) Поставить вектор обработки trap'ов (stvec → trap_entry).
    trap::init();
    println!("  [trap] вектор установлен (stvec → trap_entry)");

    // 2) Проверка обработки исключений: намеренно выполняем ebreak.
    //    Диспетчер поймает breakpoint, перешагнёт инструкцию и вернёт управление сюда.
    println!("  [test] выполняем ebreak ...");
    unsafe { core::arch::asm!("ebreak") }
    println!("  [test] вернулись из ebreak → обработчик исключений работает");
    println!();

    // 3) Запустить периодический таймер и глобально включить прерывания.
    timer::init();
    println!("  [timer] таймер вооружён, прерывания включены — ждём тики (раз в секунду):");

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
