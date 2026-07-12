//! Local APIC (xAPIC, MMIO 0xfee0_0000) — таймер вытеснения x86_64 (Веха 25).
//!
//! Аналог пары SBI TIME + sie.STIE на RISC-V: одноразовый (one-shot) счётчик, который
//! перевзводится на каждом тике ([`arm`]). Страница LAPIC отображается UC в таблицах
//! ядра ([`super::paging::init`]); работаем до IOAPIC — линии устройств придут с
//! virtio-pci (Веха 27+).

use core::ptr::{read_volatile, write_volatile};

use super::trap::{VEC_SPURIOUS, VEC_TIMER};

pub const LAPIC_BASE: usize = 0xfee0_0000;

// Смещения регистров (спецификация Intel SDM, том 3).
const REG_EOI: usize = 0x0b0;
const REG_SVR: usize = 0x0f0; // Spurious Interrupt Vector Register
const REG_LVT_TIMER: usize = 0x320;
const REG_TIMER_INIT: usize = 0x380; // initial count (запись = старт отсчёта)
const REG_TIMER_DIV: usize = 0x3e0;

/// Квант вытеснения в тиках LAPIC-таймера. Шина таймера в QEMU ходит на ~1 ГГц
/// (делитель 1): 20_000_000 ≈ 20 мс — тот же квант, что на RISC-V.
const INTERVAL: u32 = 20_000_000;

#[inline]
fn w(reg: usize, v: u32) {
    unsafe { write_volatile((LAPIC_BASE + reg) as *mut u32, v) }
}

#[inline]
fn r(reg: usize) -> u32 {
    unsafe { read_volatile((LAPIC_BASE + reg) as *const u32) }
}

/// Включить LAPIC (SVR: enable + spurious-вектор), делитель 1, LVT-таймер в one-shot
/// на вектор [`VEC_TIMER`]. Первое срабатывание взводит [`arm`].
pub fn init() {
    w(REG_SVR, (1 << 8) | VEC_SPURIOUS as u32); // APIC enable + spurious vector
    w(REG_TIMER_DIV, 0b1011); // делитель 1
    w(REG_LVT_TIMER, VEC_TIMER as u32); // one-shot (бит 17 = 0), не маскирован
    let _ = r(REG_SVR); // сериализовать записи
}

/// Перевзвести one-shot: запись initial count запускает отсчёт заново.
pub fn arm() {
    w(REG_TIMER_INIT, INTERVAL);
}

/// End-of-interrupt — сообщить LAPIC, что вектор обслужен (иначе следующий не придёт).
pub fn eoi() {
    w(REG_EOI, 0);
}

/// Замаскирован ли LVT-таймер (бит 16) — снимок для масок сессий (Веха 26).
pub fn timer_masked() -> bool {
    r(REG_LVT_TIMER) & (1 << 16) != 0
}

/// Маскировать/размаскировать LVT-таймер, не трогая вектор/режим.
pub fn set_timer_masked(masked: bool) {
    let v = r(REG_LVT_TIMER);
    w(REG_LVT_TIMER, if masked { v | (1 << 16) } else { v & !(1 << 16) });
}
