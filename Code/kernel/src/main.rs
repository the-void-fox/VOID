//! Микроядро VOID — bare-metal RISC-V, S-mode.
//!
//! Веха 1: загрузка из OpenSBI, настройка стека, обнуление .bss, вывод в UART.
//! Веха 2: вектор trap'ов, обработка исключений (ebreak) и таймерные прерывания.
//! Веха 3: виртуальная память Sv39 (direct map RAM + W^X), включение трансляции.
//! Веха 4: куча ядра (free-list аллокатор + #[global_allocator]) → работает `alloc`.
//! См. роадмап и ADR в Obsidian (`10-projects/void/`).
#![no_std]
#![no_main]

extern crate alloc;

mod csr;
mod frame;
mod heap;
mod paging;
mod sbi;
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
    println!("  ║  VOID — Веха 4                            ║");
    println!("  ║  куча ядра · alloc/Box/Vec/String         ║");
    println!("  ╚══════════════════════════════════════════╝");
    println!();
    println!("  hart id : {}", hartid);
    println!("  dtb     : {:#x}", dtb);
    println!("  void-abi: v{}", void_abi::VERSION);
    println!();

    // Вектор trap'ов нужен и для page fault'ов, и для таймера.
    trap::init();
    println!("  [trap] вектор установлен");

    // 1) Построить таблицы Sv39: direct map RAM (RW) + W^X ядра + страница UART.
    let root = paging::init();
    println!("  [vm] таблицы Sv39 построены, корень @ {:#x}", root);

    // 2) Подготовить демонстрацию ДО включения трансляции: кладём магию в свежий
    //    фрейм и отображаем его на «высокий» VA, которого нет в физической RAM.
    //    Если после включения чтение по этому VA вернёт магию — MMU реально транслирует.
    const HIGH_VA: usize = 0x30_0000_0000; // ~192 ГиБ, заведомо вне RAM
    const MAGIC: usize = 0xDEAD_BEEF_CAFE_F00D;
    let demo = frame::alloc().expect("демо-фрейм");
    unsafe {
        core::ptr::write_volatile(demo as *mut usize, MAGIC);
        paging::map(root, HIGH_VA, demo, paging::PTE_R | paging::PTE_W);
    }
    println!("  [vm] демо-фрейм @ {:#x} → отображён на VA {:#x}", demo, HIGH_VA);

    // 3) Включить paging.
    // SAFETY: таблицы построены и идентично отображают текущие PC/SP/UART.
    unsafe { paging::enable(root) }
    println!("  [vm] paging включён (satp = Sv39)");

    // 4) Проверки — уже под трансляцией.
    let pc = kmain as *const () as usize;
    println!("  [vm] translate(код  {:#x}) = {:x?}", pc, paging::translate(root, pc));
    println!("  [vm] translate(UART 0x10000000) = {:x?}", paging::translate(root, 0x1000_0000));
    let got = unsafe { core::ptr::read_volatile(HIGH_VA as *const usize) };
    println!(
        "  [vm] чтение по VA {:#x} = {:#x} → {}",
        HIGH_VA,
        got,
        if got == MAGIC { "OK ✓ (трансляция работает)" } else { "FAIL ✗" },
    );
    println!();

    // 5) Куча ядра. После init работают Box/Vec/String из `alloc`.
    heap::init();
    println!("  [heap] куча ядра готова (2 МиБ, free-list)");
    heap_demo();
    println!();

    // 6) Таймер — доказывает, что trap'ы/прерывания живут и с включённым MMU.
    timer::init();
    println!("  [timer] идём под трансляцией, ждём тики:");

    loop {
        // SAFETY: wfi — ждать прерывания; в S-mode разрешено.
        unsafe { core::arch::asm!("wfi") }
    }
}

/// Демонстрация кучи: динамические коллекции + освобождение/переиспользование.
fn heap_demo() {
    use alloc::boxed::Box;
    use alloc::string::String;
    use alloc::vec::Vec;

    // Vec растёт динамически (несколько реаллокаций по мере push).
    let mut v: Vec<u64> = Vec::new();
    for i in 0..16 {
        v.push(i * i);
    }
    println!("  [heap] Vec len={} cap={}: {:?} …", v.len(), v.capacity(), &v[..6]);

    // String тоже живёт на куче.
    let s = String::from("строки на куче — ок");
    println!("  [heap] String: \"{}\"", s);

    // Box кладёт значение на кучу.
    let b = Box::new([0xABu8; 32]);
    println!("  [heap] Box<[u8;32]> @ {:p}, [0]={:#x}", &*b, b[0]);

    // Освобождение и переиспользование: адрес после free должен вернуться.
    let a1 = Box::new(0xAAAA_u64);
    let p1 = &*a1 as *const u64 as usize;
    drop(a1); // блок уходит обратно в свободный список
    let a2 = Box::new(0xBBBB_u64);
    let p2 = &*a2 as *const u64 as usize;
    println!(
        "  [heap] free+reuse: {:#x} → {:#x} → {}",
        p1,
        p2,
        if p1 == p2 { "тот же адрес ✓" } else { "другой" },
    );
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!();
    println!("  [PANIC] {}", info);
    loop {
        unsafe { core::arch::asm!("wfi") }
    }
}
