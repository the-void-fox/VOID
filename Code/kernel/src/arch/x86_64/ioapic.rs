//! IOAPIC (Веха 27) — маршрутизация ISA-линий в вектора LAPIC.
//!
//! Нужен нам ровно для одного: COM1 (ISA IRQ4 → GSI4) должен приходить вектором
//! [`super::trap::VEC_CONSOLE`], чтобы ввод будил спящее ядро честным прерыванием,
//! а не опросом на тиках. PCI-устройства IOAPIC не трогают — их прерывания ходят
//! MSI-X прямо в LAPIC (см. pci.rs).
//!
//! Доступ — пара регистров: индекс (IOREGSEL) + окно данных (IOWIN). Записи
//! перенаправления (RTE) — по два 32-битных регистра на GSI, начиная с 0x10.

use core::ptr::{read_volatile, write_volatile};

/// База IOAPIC на q35 (стандартная); страница отображается в paging::init.
pub const IOAPIC_BASE: usize = 0xfec0_0000;

const IOREGSEL: usize = 0x00;
const IOWIN: usize = 0x10;

fn write(reg: u32, val: u32) {
    unsafe {
        write_volatile((IOAPIC_BASE + IOREGSEL) as *mut u32, reg);
        write_volatile((IOAPIC_BASE + IOWIN) as *mut u32, val);
    }
}

fn read(reg: u32) -> u32 {
    unsafe {
        write_volatile((IOAPIC_BASE + IOREGSEL) as *mut u32, reg);
        read_volatile((IOAPIC_BASE + IOWIN) as *const u32)
    }
}

/// Направить GSI на вектор: fixed, physical, edge, active-high, без маски, CPU 0.
pub fn route(gsi: u32, vector: u8) {
    write(0x10 + 2 * gsi, vector as u32);
    write(0x11 + 2 * gsi, 0); // destination: APIC ID 0
}

/// Направить PCI INTx-линию (Веха 52): level-triggered, active-low — как требуют
/// разделяемые PCI-прерывания (INTA#..INTD#, GSI 16..23 на q35). Иначе edge-режим
/// не ловит удержание линии картой (e1000 держит INTx, пока не прочитан ICR).
/// Стартуем ЗАМАСКИРОВАННЫМИ (бит 16): взводит `SYS_IRQ_WAIT` через [`set_userdrv_masked`].
pub fn route_level_low(gsi: u32, vector: u8) {
    // low: вектор | mask=1 (1<<16) | polarity=active-low (1<<13) | trigger=level (1<<15).
    write(0x10 + 2 * gsi, vector as u32 | (1 << 16) | (1 << 13) | (1 << 15));
    write(0x11 + 2 * gsi, 0); // destination: APIC ID 0
}

/// Веха 52 — за/раз-маскировать все PCI INTx-линии (GSI 16..23), сведённые на вектор
/// userspace-драйвера. Модель «oneshot»: `SYS_IRQ_WAIT` размаскирует (взвод перед сном),
/// обработчик VEC_USERDRV маскирует (иначе level-линию, которую карта держит до чтения ICR,
/// IOAPIC переотправлял бы штормом). Бит 16 RTE — маска.
pub fn set_userdrv_masked(masked: bool) {
    for gsi in 16..24u32 {
        let lo = read(0x10 + 2 * gsi);
        let lo = if masked { lo | (1 << 16) } else { lo & !(1 << 16) };
        write(0x10 + 2 * gsi, lo);
    }
}
