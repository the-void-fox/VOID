//! PLIC — Platform-Level Interrupt Controller (RISC-V) на QEMU `virt`.
//!
//! PLIC маршрутизирует прерывания *устройств* (в отличие от таймерных/программных, что идут
//! напрямую через `sie`). Каждому источнику — номер (IRQ) и приоритет; каждому «контексту»
//! (hart×режим) — своя маска разрешённых источников, порог и регистр claim/complete.
//!
//! На QEMU `virt`: hart 0 S-mode = **контекст 1**. Обработка внешнего прерывания:
//! `claim` (узнать номер сработавшего) → обслужить → `complete` (сообщить PLIC, что готово).

use core::ptr::{read_volatile, write_volatile};

// База PLIC на QEMU virt и раскладка регистров.
const PLIC_BASE: usize = 0x0c00_0000;
const PRIORITY: usize = PLIC_BASE; // priority[irq] = base + 4*irq
const CTX1_ENABLE: usize = PLIC_BASE + 0x2080; // маска источников для контекста 1 (hart0-S)
const CTX1_THRESHOLD: usize = PLIC_BASE + 0x20_1000; // порог контекста 1
const CTX1_CLAIM: usize = PLIC_BASE + 0x20_1004; // claim/complete контекста 1

/// Настроить PLIC и разрешить первый источник `irq`: порог 0, приоритет > 0, маска контекста 1.
/// Дополнительные источники (Веха 20: UART) добавляются через [`enable`].
pub fn init(irq: u32) {
    unsafe {
        // Порог 0 — пропускать любые приоритеты > 0.
        write_volatile(CTX1_THRESHOLD as *mut u32, 0);
    }
    enable(irq);
}

/// Разрешить ещё один источник `irq` в контексте 1 (приоритет 1 + бит маски).
pub fn enable(irq: u32) {
    unsafe {
        // Приоритет источника (0 = выключен; берём 1).
        write_volatile((PRIORITY + 4 * irq as usize) as *mut u32, 1);
        // Разрешить источник в битовой маске контекста 1.
        let reg = (CTX1_ENABLE + (irq as usize / 32) * 4) as *mut u32;
        write_volatile(reg, read_volatile(reg) | (1 << (irq % 32)));
    }
}

/// Забрать номер сработавшего источника (0 — ничего). Пока не сделан [`complete`],
/// это же прерывание повторно не придёт.
pub fn claim() -> u32 {
    unsafe { read_volatile(CTX1_CLAIM as *const u32) }
}

/// Сообщить PLIC, что обработка `irq` завершена.
pub fn complete(irq: u32) {
    unsafe { write_volatile(CTX1_CLAIM as *mut u32, irq) }
}

/// Обработать внешнее прерывание: claim → диспетчеризовать по номеру → complete.
/// Вызывается из [`crate::trap`] на `IRQ_S_EXTERNAL`.
pub fn handle_external() {
    let irq = claim();
    if irq == 0 {
        return; // ложное срабатывание
    }
    if irq == crate::virtio_blk::irq() {
        crate::virtio_blk::on_irq();
    } else if irq == super::uart::IRQ {
        super::uart::on_irq(); // Веха 20.1: принятые байты → кольцевой буфер
    } else if irq == crate::virtio_net::irq() {
        crate::virtio_net::on_irq(); // Веха 91: приехал кадр — разбудить сетевой сервер
    }
    complete(irq);
}
