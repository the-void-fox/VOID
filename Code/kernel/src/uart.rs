//! Минимальный драйвер UART (NS16550A) для QEMU virt.
//!
//! База UART0 на машине `virt` — 0x1000_0000. Для вывода в QEMU достаточно писать
//! байт в регистр THR (offset 0); реальное железо потребовало бы инициализации
//! делителя/линии — добавим, когда дойдём до настоящих драйверов.

use core::fmt::{self, Write};

const UART0_THR: *mut u8 = 0x1000_0000 as *mut u8;

/// Zero-sized хэндл UART0.
pub struct Uart;

impl Uart {
    #[inline]
    fn putc(c: u8) {
        // SAFETY: фиксированный MMIO-адрес UART0 на QEMU virt.
        unsafe { core::ptr::write_volatile(UART0_THR, c) }
    }
}

impl Write for Uart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            if b == b'\n' {
                Uart::putc(b'\r'); // CRLF для последовательного терминала
            }
            Uart::putc(b);
        }
        Ok(())
    }
}
