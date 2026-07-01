//! Микроядро VOID — bare-metal RISC-V, S-mode.
//!
//! Веха 1: загрузка из OpenSBI, настройка стека, обнуление .bss, вывод в UART.
//! Веха 2: вектор trap'ов, обработка исключений (ebreak) и таймерные прерывания.
//! Веха 3: виртуальная память Sv39 (direct map RAM + W^X), включение трансляции.
//! Веха 4: куча ядра (free-list аллокатор + #[global_allocator]) → работает `alloc`.
//! Веха 5: кооперативная многозадачность (контексты + round-robin планировщик).
//! См. роадмап и ADR в Obsidian (`10-projects/void/`).
#![no_std]
#![no_main]

extern crate alloc;

mod context;
mod csr;
mod frame;
mod heap;
mod paging;
mod sbi;
mod sched;
mod sync;
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
    println!("  ║  VOID — Веха 5                            ║");
    println!("  ║  многозадачность · планировщик · RISC-V   ║");
    println!("  ╚══════════════════════════════════════════╝");
    println!();
    println!("  hart id : {}", hartid);
    println!("  dtb     : {:#x}", dtb);
    println!("  void-abi: v{}", void_abi::VERSION);
    println!();

    // Вектор trap'ов нужен и для page fault'ов, и для таймера.
    trap::init();
    println!("  [trap] вектор установлен");

    // Инфраструктура из прошлых вех (кратко): trap-вектор, Sv39, куча.
    let root = paging::init();
    // SAFETY: таблицы идентично отображают текущие PC/SP/UART.
    unsafe { paging::enable(root) }
    println!("  [vm]   Sv39 включён (direct map + W^X)");
    heap::init();
    println!("  [heap] куча ядра готова (2 МиБ)");
    println!();

    // Веха 5: кооперативная многозадачность.
    sched::init(); // текущее исполнение (kmain) становится задачей «main»
    sched::spawn("A", worker);
    sched::spawn("B", worker);
    sched::spawn("C", worker);
    println!("  [sched] запущены задачи A/B/C, уступаем управление:");
    while sched::other_runnable() {
        sched::yield_now();
    }
    println!("  [sched] все задачи завершились — управление вернулось в main");
    println!();

    // Таймер — фоновые тики в idle-цикле (trap'ы работают и с MMU, и после задач).
    timer::init();
    println!("  [timer] idle-цикл, тики раз в секунду:");

    loop {
        // SAFETY: wfi — ждать прерывания; в S-mode разрешено.
        unsafe { core::arch::asm!("wfi") }
    }
}

/// Тело демонстрационной задачи: печатает своё имя несколько раз, уступая процессор.
/// Все три задачи используют одну функцию — различаются по имени (см. `sched::current_name`).
fn worker() {
    let name = sched::current_name();
    for step in 0..4 {
        println!("    [{}] шаг {}", name, step);
        sched::yield_now();
    }
    println!("    [{}] завершилась", name);
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!();
    println!("  [PANIC] {}", info);
    loop {
        unsafe { core::arch::asm!("wfi") }
    }
}
