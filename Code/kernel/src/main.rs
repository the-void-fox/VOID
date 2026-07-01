//! Микроядро VOID — bare-metal RISC-V, S-mode.
//!
//! Веха 1: загрузка из OpenSBI, настройка стека, обнуление .bss, вывод в UART.
//! Веха 2: вектор trap'ов, обработка исключений (ebreak) и таймерные прерывания.
//! Веха 3: виртуальная память Sv39 (direct map RAM + W^X), включение трансляции.
//! Веха 4: куча ядра (free-list аллокатор + #[global_allocator]) → работает `alloc`.
//! Веха 5: кооперативная многозадачность (контексты + round-robin планировщик).
//! Веха 5.5: вытеснение по таймеру (preemption) — задачи чередуются без явного yield.
//! Веха 6: объектная модель — контент-адресуемые значения + изменяемые корни (стержень).
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
    println!("  ║  VOID — Веха 6                            ║");
    println!("  ║  объектная модель · контент-адреса        ║");
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

    // Веха 6: объектная модель (контент-адресация, дедупликация, корни, история версий).
    object_demo();
    println!();

    // Многозадачность (Вехи 5–5.5) по-прежнему работает: две задачи параллельно пишут в
    // ОБЩЕЕ объектное хранилище, таймер их вытесняет.
    sched::init();
    sched::spawn("X", writer);
    sched::spawn("Y", writer);
    println!("  [sched] задачи X/Y пишут в общий store (вытесняются таймером):");
    timer::init();
    while sched::other_runnable() {
        // SAFETY: wfi — спать до следующего прерывания (таймера).
        unsafe { core::arch::asm!("wfi") }
    }
    println!(
        "  [obj] после задач уникальных объектов: {} (вытеснений таймером: {})",
        object::len(),
        timer::ticks(),
    );
    println!();
    println!("  [idle] ядро простаивает (wfi).");

    loop {
        // SAFETY: wfi — ждать прерывания; в S-mode разрешено.
        unsafe { core::arch::asm!("wfi") }
    }
}

/// Демонстрация объектной модели: контент-адресация, дедупликация, корни, история версий.
fn object_demo() {
    // Неизменяемые значения, адресуемые по хэшу содержимого.
    let a = object::put(b"hello");
    let a2 = object::put(b"hello"); // те же байты
    let b = object::put(b"world");
    println!("  [obj] put \"hello\"       -> {}", id_short(&a));
    println!("  [obj] put \"hello\" снова -> {} (тот же адрес: {})", id_short(&a2), a == a2);
    println!("  [obj] put \"world\"       -> {}", id_short(&b));
    println!("  [obj] уникальных объектов: {} (не 3 — \"hello\" дедуплицирован)", object::len());

    // Изменяемый корень указывает на неизменяемое значение. Мутация = новое значение.
    let v0 = object::put(b"state=0");
    object::set_root("system", v0);
    let v1 = object::put(b"state=1");
    object::set_root("system", v1); // переключаем корень на новую версию

    let cur = object::root("system").unwrap();
    println!("  [obj] корень 'system' -> {} (после двух записей)", id_short(&cur));
    // Старая версия НЕ исчезла — неизменяемость сохраняет историю.
    object::with(&v0, |bytes| {
        let text = bytes.and_then(|b| core::str::from_utf8(b).ok()).unwrap_or("?");
        println!("  [obj] старая версия v0 всё ещё в store: \"{}\"", text);
    });
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
