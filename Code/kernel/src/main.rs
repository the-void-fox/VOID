//! Микроядро VOID — bare-metal RISC-V, S-mode.
//!
//! Веха 1: загрузка из OpenSBI, настройка стека, обнуление .bss, вывод в UART.
//! Веха 2: вектор trap'ов, обработка исключений (ebreak) и таймерные прерывания.
//! Веха 3: виртуальная память Sv39 (direct map RAM + W^X), включение трансляции.
//! Веха 4: куча ядра (free-list аллокатор + #[global_allocator]) → работает `alloc`.
//! Веха 5: кооперативная многозадачность (контексты + round-robin планировщик).
//! Веха 5.5: вытеснение по таймеру (preemption) — задачи чередуются без явного yield.
//! Веха 6: объектная модель — контент-адресуемые значения + изменяемые корни (стержень).
//! Веха 7.1: драйвер virtio-blk — чтение/запись секторов виртуального диска.
//! Веха 7.2: персистентный контент-адресуемый store — состояние переживает перезагрузку.
//! См. роадмап и ADR в Obsidian (`10-projects/void/`).
#![no_std]
#![no_main]

extern crate alloc;

mod context;
mod csr;
mod frame;
mod heap;
mod object;
mod paging;
mod sbi;
mod sched;
mod sync;
mod timer;
mod trap;
mod uart;
mod virtio_blk;

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
    println!("  ║  VOID — Веха 7.2                          ║");
    println!("  ║  персистентный store · диск               ║");
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

    // Веха 7.1: подключить диск (нужен для персистентности).
    if virtio_blk::init() {
        println!("  [blk]  virtio-blk: {} секторов", virtio_blk::capacity_sectors());
    } else {
        println!("  [blk]  virtio-blk не найден — персистентность недоступна!");
    }

    // Веха 7.2: загрузить состояние с диска (RAM — кэш, диск — истина).
    if object::load() {
        println!(
            "  [store] загружено с диска: {} объектов, поколение {}",
            object::len(),
            object::generation(),
        );
        if let Some(rid) = object::root("system") {
            object::with(&rid, |b| {
                let text = b.and_then(|x| core::str::from_utf8(x).ok()).unwrap_or("?");
                println!("  [store] system root С ПРОШЛОГО запуска: \"{}\"", text);
            });
        }
    } else {
        println!("  [store] диск пуст — это ПЕРВЫЙ запуск");
    }
    println!();

    // Многозадачность: задачи пишут объекты в общий store (одинаковый вклад → дедуп).
    sched::init();
    sched::spawn("X", writer);
    sched::spawn("Y", writer);
    println!("  [sched] X/Y пишут объекты (вытесняются таймером):");
    timer::init();
    while sched::other_runnable() {
        // SAFETY: wfi — спать до следующего прерывания (таймера).
        unsafe { core::arch::asm!("wfi") }
    }
    println!("  [sched] задачи завершились (вытеснений таймером: {})", timer::ticks());
    println!();

    // Новый корень этого запуска и фиксация на диск — переживёт перезагрузку QEMU.
    let marker = alloc::format!("boot #{} — VOID помнит своё состояние", object::generation() + 1);
    let id = object::put(marker.as_bytes());
    object::set_root("system", id);
    object::commit();
    println!("  [store] новый system root: \"{}\"", marker);
    println!(
        "  [store] commit → {} объектов, поколение {} (записано на диск)",
        object::len(),
        object::generation(),
    );
    println!();
    println!("  [idle] перезагрузи QEMU — состояние вернётся. Простаиваем (wfi).");

    loop {
        // SAFETY: wfi — ждать прерывания; в S-mode разрешено.
        unsafe { core::arch::asm!("wfi") }
    }
}

/// Задача-писатель: кладёт несколько значений в общий объектный store.
fn writer() {
    let name = sched::current_name();
    for i in 0..3 {
        let data = alloc::format!("{}-value-{}", name, i);
        let id = object::put(data.as_bytes());
        println!("    [{}] put \"{}\" -> {}", name, data, id_short(&id));
        // Немного занятости, чтобы дать таймеру шанс вытеснить между записями.
        let mut acc = 0u64;
        for k in 0..250_000u64 {
            acc = acc.wrapping_add(k);
        }
        let _ = acc;
    }
}

/// Короткое hex-представление контент-адреса (первые 5 байт) — для вывода.
fn id_short(id: &void_abi::ContentId) -> alloc::string::String {
    use core::fmt::Write as _;
    let mut s = alloc::string::String::new();
    for byte in &id.0[..5] {
        let _ = write!(s, "{:02x}", byte);
    }
    s.push('…');
    s
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!();
    println!("  [PANIC] {}", info);
    loop {
        unsafe { core::arch::asm!("wfi") }
    }
}
