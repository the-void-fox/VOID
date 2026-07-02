//! Обработка trap'ов: установка вектора и Rust-диспетчер.
//!
//! Низкоуровневое сохранение/восстановление регистров делает ассемблерный трамплин
//! `trap_entry` (см. trap_entry.s). Он зовёт отсюда [`trap_handler`] с указателем на
//! [`TrapFrame`] — снимок всех регистров на момент trap'а. Диспетчер смотрит на
//! `scause` и решает, что это было: таймер, breakpoint или что-то фатальное.

use crate::{csr, plic, println, proc, timer};

core::arch::global_asm!(include_str!("trap_entry.s"));

extern "C" {
    /// Ассемблерная точка входа в обработчик (адрес кладём в `stvec`).
    fn trap_entry();
}

/// Снимок состояния процессора на момент trap'а. Раскладка строго совпадает с
/// порядком сохранения в trap_entry.s (поэтому `repr(C)` и фиксированный порядок).
/// `Copy`/`Default` — чтобы сохранять его как состояние процесса ([[user-mode|proc]]).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct TrapFrame {
    /// Регистры x0..x31 (x0 всегда 0; x2 — исходный sp).
    pub regs: [usize; 32],
    /// Адрес инструкции, вызвавшей trap (он же адрес возврата).
    pub sepc: usize,
    /// Снимок sstatus.
    pub sstatus: usize,
}

/// ABI-имена регистров для читаемого дампа.
const REG_NAMES: [&str; 32] = [
    "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "s0", "s1", "a0", "a1", "a2", "a3", "a4",
    "a5", "a6", "a7", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "t3", "t4",
    "t5", "t6",
];

/// Установить вектор обработки trap'ов (режим Direct). Вызвать один раз при старте.
pub fn init() {
    // Адрес функции -> сначала в указатель, потом в usize (так требует линт).
    csr::write_stvec(trap_entry as *const () as usize);
    // Инвариант переключения стека (Веха 10): в ядре sscratch = 0. trap_entry.s опирается
    // на это, чтобы отличить trap из ядра (S) от trap'а из пользователя (U).
    csr::write_sscratch(0);
}

/// Rust-сторона обработчика. Вызывается из trap_entry.s; `frame` указывает на
/// сохранённые регистры на стеке. Менять `frame.sepc` здесь = менять адрес возврата.
#[no_mangle]
pub extern "C" fn trap_handler(frame: &mut TrapFrame) {
    let scause = csr::read_scause();

    // trap из U-mode (SPP=0): системный вызов (ecall) ЛИБО таймерное вытеснение (Веха 16).
    // Управление уходит в планировщик процессов и сюда НЕ возвращается (возобновляется процесс).
    if frame.sstatus & (1 << 8) == 0 {
        proc::handle_user_trap(frame, scause);
    }

    // trap из ядра (S-mode) — как раньше.
    let is_interrupt = scause & csr::INTERRUPT_BIT != 0;
    let code = scause & !csr::INTERRUPT_BIT;

    if is_interrupt {
        match code {
            csr::IRQ_S_TIMER => timer::on_tick(),
            csr::IRQ_S_EXTERNAL => plic::handle_external(), // устройства (диск)
            other => println!("  [trap] неизвестное прерывание, код={}", other),
        }
    } else {
        match code {
            csr::EXC_BREAKPOINT => {
                // `ebreak`: сообщаем и перешагиваем инструкцию, иначе зациклимся на ней.
                // Длина зависит от сжатия (расширение C): 2 байта (c.ebreak) или 4.
                let insn_lo = unsafe { core::ptr::read(frame.sepc as *const u16) };
                let len = if insn_lo & 0b11 == 0b11 { 4 } else { 2 };
                println!("  [trap] breakpoint @ {:#x} → перешагиваем {} байт", frame.sepc, len);
                frame.sepc += len;
            }
            _ => fatal(frame, scause),
        }
    }
}

/// Необработанный (фатальный) trap: печатаем полный контекст и останавливаемся.
/// Это наша «трассировка» Вехи 2 — дамп scause/sepc/stval и всех регистров.
/// (Полный backtrace по кадрам стека добавим, когда появится раскрутка.)
fn fatal(frame: &TrapFrame, scause: usize) -> ! {
    println!();
    println!("  ╔═ FATAL TRAP ═══════════════════════════════════");
    println!("  ║ scause : {:#x}  ({})", scause, cause_name(scause));
    println!("  ║ sepc   : {:#018x}", frame.sepc);
    println!("  ║ stval  : {:#018x}", csr::read_stval());
    println!("  ║ sstatus: {:#018x}", frame.sstatus);
    println!("  ╟─ регистры ─────────────────────────────────────");
    let mut i = 0;
    while i < 32 {
        println!(
            "  ║ {:>4}={:#018x}   {:>4}={:#018x}",
            REG_NAMES[i], frame.regs[i],
            REG_NAMES[i + 1], frame.regs[i + 1],
        );
        i += 2;
    }
    println!("  ╚════════════════════════════════════════════════");
    loop {
        unsafe { core::arch::asm!("wfi") }
    }
}

/// Человекочитаемое имя причины trap'а.
fn cause_name(scause: usize) -> &'static str {
    let code = scause & !csr::INTERRUPT_BIT;
    if scause & csr::INTERRUPT_BIT != 0 {
        match code {
            1 => "S-software interrupt",
            5 => "S-timer interrupt",
            9 => "S-external interrupt",
            _ => "неизвестное прерывание",
        }
    } else {
        match code {
            0 => "instruction address misaligned",
            1 => "instruction access fault",
            2 => "illegal instruction",
            3 => "breakpoint",
            4 => "load address misaligned",
            5 => "load access fault",
            6 => "store address misaligned",
            7 => "store access fault",
            8 => "ecall from U-mode",
            9 => "ecall from S-mode",
            12 => "instruction page fault",
            13 => "load page fault",
            15 => "store page fault",
            _ => "неизвестное исключение",
        }
    }
}
