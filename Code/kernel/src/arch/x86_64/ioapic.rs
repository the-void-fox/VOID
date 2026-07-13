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

#[allow(dead_code)]
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
