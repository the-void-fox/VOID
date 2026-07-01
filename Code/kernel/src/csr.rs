//! Тонкие обёртки над CSR (Control and Status Registers) RISC-V для S-mode.
//!
//! CSR — это специальные регистры процессора, через которые ядро управляет
//! прерываниями, видит причину trap'а и т.д. Доступ к ним — только спец-инструкциями
//! `csrr`/`csrw`/`csrs`/`csrc`, поэтому каждую обёртку приходится писать на inline-asm.
//!
//! Нужные нам S-mode CSR:
//! - `stvec`   — адрес обработчика trap'ов (куда CPU прыгает при исключении/прерывании);
//! - `scause`  — причина последнего trap'а (старший бит = прерывание, младшие = код);
//! - `sepc`    — PC, на котором случился trap (адрес возврата для `sret`);
//! - `stval`   — доп. информация (напр. адрес, вызвавший page fault);
//! - `sstatus` — статус S-mode, нас интересует бит SIE (глобальное разрешение прерываний);
//! - `sie`     — маска разрешённых типов прерываний (бит STIE — таймер);
//! - `time`    — счётчик реального времени (читается инструкцией `rdtime`).

use core::arch::asm;

// ─── scause: разбор причины ────────────────────────────────────────────────

/// Старший бит `scause`: 1 — прерывание (асинхронное), 0 — исключение (синхронное).
/// На rv64 это бит 63.
pub const INTERRUPT_BIT: usize = 1 << (usize::BITS - 1);

// Коды причин, на которые мы реально диспетчеризуем (полный список — в `trap::cause_name`).
/// Прерывание таймера S-mode (когда установлен INTERRUPT_BIT).
pub const IRQ_S_TIMER: usize = 5;
/// Исключение breakpoint — инструкция `ebreak` (когда INTERRUPT_BIT снят).
pub const EXC_BREAKPOINT: usize = 3;

// ─── Чтение/запись CSR ──────────────────────────────────────────────────────

/// Установить адрес обработчика trap'ов. Младшие 2 бита = режим: 0 — Direct
/// (все trap'ы идут по одному адресу), 1 — Vectored. Мы используем Direct,
/// поэтому адрес должен быть выровнен по 4 байта (младшие биты = 0).
#[inline]
pub fn write_stvec(addr: usize) {
    unsafe { asm!("csrw stvec, {0}", in(reg) addr, options(nomem, nostack)) }
}

/// Причина последнего trap'а.
#[inline]
pub fn read_scause() -> usize {
    let v: usize;
    unsafe { asm!("csrr {0}, scause", out(reg) v, options(nomem, nostack)) }
    v
}

/// Доп. информация о trap'е (адрес при page fault и т.п.).
#[inline]
pub fn read_stval() -> usize {
    let v: usize;
    unsafe { asm!("csrr {0}, stval", out(reg) v, options(nomem, nostack)) }
    v
}

/// Счётчик времени (тики таймбазы; на QEMU virt — 10 МГц).
#[inline]
pub fn read_time() -> u64 {
    let v: u64;
    unsafe { asm!("rdtime {0}", out(reg) v, options(nomem, nostack)) }
    v
}

/// Разрешить таймерные прерывания: выставить бит STIE (5) в `sie`.
/// (`csrs` атомарно устанавливает биты по маске.)
#[inline]
pub fn enable_timer_interrupt() {
    unsafe { asm!("csrs sie, {0}", in(reg) 1usize << 5, options(nomem, nostack)) }
}

/// Глобально включить прерывания в S-mode: бит SIE (1) в `sstatus`.
/// До этого вызова прерывания не доставляются, даже если `sie` их разрешает.
#[inline]
pub fn enable_interrupts() {
    unsafe { asm!("csrs sstatus, {0}", in(reg) 1usize << 1, options(nomem, nostack)) }
}

/// Выключить прерывания S-mode, вернув прежнее значение бита SIE.
/// Пара к [`irq_restore`] — для критических секций, которые не должны быть вытеснены.
#[inline]
pub fn irq_save_disable() -> bool {
    let prev: usize;
    // csrrc: атомарно прочитать sstatus и сбросить биты по маске (здесь — SIE).
    unsafe {
        asm!("csrrc {0}, sstatus, {1}", out(reg) prev, in(reg) 1usize << 1, options(nomem, nostack))
    }
    prev & (1 << 1) != 0
}

/// Восстановить бит SIE в состояние `enabled` (обычно — из [`irq_save_disable`]).
#[inline]
pub fn irq_restore(enabled: bool) {
    if enabled {
        unsafe { asm!("csrs sstatus, {0}", in(reg) 1usize << 1, options(nomem, nostack)) }
    } else {
        unsafe { asm!("csrc sstatus, {0}", in(reg) 1usize << 1, options(nomem, nostack)) }
    }
}
