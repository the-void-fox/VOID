//! Микроядро VOID — bare-metal RISC-V, S-mode.
//!
//! Веха 1: загрузка из OpenSBI, настройка стека, обнуление .bss, вывод в UART.
//! Веха 2: вектор trap'ов, обработка исключений (ebreak) и таймерные прерывания.
//! Веха 3: виртуальная память Sv39 (direct map RAM + W^X), включение трансляции.
//! Веха 4: куча ядра (free-list аллокатор + #[global_allocator]) → работает `alloc`.
//! Веха 5: кооперативная многозадачность (контексты + round-robin планировщик).
//! Веха 5.5: вытеснение по таймеру (preemption) — задачи чередуются без явного yield.
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
/// Выключаем прерывания на время строки: иначе вытеснение по таймеру могло бы
/// переключить задачу прямо посреди вывода, и строки разных задач перемешались бы.
pub fn _print(args: core::fmt::Arguments) {
    let sie = csr::irq_save_disable();
    let _ = uart::Uart.write_fmt(args);
    csr::irq_restore(sie);
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
    println!("  ║  VOID — Веха 5.5                          ║");
    println!("  ║  вытеснение по таймеру · RISC-V           ║");
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

    // Веха 5.5: вытесняющая многозадачность.
    sched::init(); // текущее исполнение (kmain) становится задачей «main»
    sched::spawn("A", busy_worker);
    sched::spawn("B", busy_worker);
    sched::spawn("C", busy_worker);
    println!("  [sched] A/B/C запущены. Они НЕ вызывают yield — чередует только таймер.");
    println!();

    // Запускаем таймер → начинается вытеснение. main спит на wfi; таймер будит ядро,
    // вытесняет к задачам и по кругу возвращает управление сюда.
    timer::init();
    while sched::other_runnable() {
        // SAFETY: wfi — спать до следующего прерывания (таймера).
        unsafe { core::arch::asm!("wfi") }
    }
    println!();
    println!("  [sched] все задачи завершились. Вытеснений (тиков таймера): {}", timer::ticks());
    println!("  [idle] ядро простаивает (wfi).");

    loop {
        // SAFETY: wfi — ждать прерывания; в S-mode разрешено.
        unsafe { core::arch::asm!("wfi") }
    }
}

/// Тело демонстрационной задачи: намеренно НЕ вызывает yield. Делает «работу» раундами,
/// печатая прогресс. Переключить её на другую задачу может только вытеснение по таймеру —
/// поэтому чередование A/B/C в выводе и есть доказательство, что вытеснение работает.
fn busy_worker() {
    let name = sched::current_name();
    println!("    [{}] стартовала", name);
    for round in 0..5 {
        // Занятый цикл — имитация полезной работы. acc печатаем, чтобы цикл не выкинули.
        let mut acc: u64 = 0;
        for i in 0..300_000u64 {
            acc = acc.wrapping_add(i ^ (i << 1));
        }
        println!("    [{}] раунд {} (acc={:#x})", name, round, acc);
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
