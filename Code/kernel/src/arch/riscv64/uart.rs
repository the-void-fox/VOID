//! Минимальный драйвер UART (NS16550A) для QEMU virt.
//!
//! База UART0 на машине `virt` — 0x1000_0000. Для вывода в QEMU достаточно писать
//! байт в регистр THR (offset 0); реальное железо потребовало бы инициализации
//! делителя/линии — добавим, когда дойдём до настоящих драйверов.
//!
//! Веха 20.1 — **приём (RX) по прерыванию**: разрешаем в IER прерывание «данные готовы»,
//! обработчик [`on_irq`] вычерпывает FIFO в кольцевой буфер ядра. Так ввод не теряется,
//! даже если пришёл, пока никто не читает (ранний ввод из pipe, набор во время работы
//! процесса): QEMU придерживает байты, пока гость не заберёт из FIFO, а мы забираем их
//! в момент прерывания. Потребитель — `SYS_READ` ([[processes|proc]]): процесс блокируется,
//! пока [`has_input`] пуст, и ядро будит его, когда буфер наполнится.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::sync::SpinLock;

const UART0_BASE: usize = 0x1000_0000;
const UART0_THR: *mut u8 = UART0_BASE as *mut u8; // запись: Transmitter Holding
const UART0_RBR: *const u8 = UART0_BASE as *const u8; // чтение: Receiver Buffer
const UART0_IER: *mut u8 = (UART0_BASE + 1) as *mut u8; // Interrupt Enable
const UART0_FCR: *mut u8 = (UART0_BASE + 2) as *mut u8; // FIFO Control
const UART0_LSR: *const u8 = (UART0_BASE + 5) as *const u8; // Line Status
const LSR_DATA_READY: u8 = 1 << 0;

/// Номер IRQ UART0 в PLIC на QEMU `virt`.
pub const IRQ: u32 = 10;

/// Кольцевой буфер принятых байт. 256 — с запасом и для набора с клавиатуры, и для
/// сценария, поданного через pipe (QEMU дошлёт остальное по мере вычерпывания FIFO).
/// Веха 101 — 1 КиБ, а не 256 Б: столько же, сколько строка команды в шелле. Прежние 256
/// совпадали со СТАРЫМ пределом строки, и стоило вставить в консоль длинный модуль `.vv`, как
/// байты пропадали ИЗ СЕРЕДИНЫ (кольцо переполнялось быстрее, чем шелл его вычерпывал).
const RING_SIZE: usize = 1024;

struct Ring {
    buf: [u8; RING_SIZE],
    head: usize, // куда пишет обработчик прерывания
    tail: usize, // откуда читает потребитель
}

static RING: SpinLock<Ring> = SpinLock::new(Ring { buf: [0; RING_SIZE], head: 0, tail: 0 });

/// Сколько байт ввода потеряно переполнением кольца (забирается и обнуляется [`take_lost`]).
static LOST: AtomicUsize = AtomicUsize::new(0);

/// Забрать и обнулить счётчик потерянного ввода.
pub fn take_lost() -> usize {
    LOST.swap(0, Ordering::Relaxed)
}

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

/// Включить приём: FIFO + прерывание «принятые данные готовы» (IER bit 0).
/// Само прерывание дойдёт, когда его разрешат в PLIC ([[traps-and-timer|plic]]) и в `sie`.
pub fn init_rx() {
    unsafe {
        core::ptr::write_volatile(UART0_FCR, 0b0000_0111); // FIFO вкл + сброс RX/TX FIFO
        core::ptr::write_volatile(UART0_IER, 0b0000_0001); // прерывание приёма
    }
}

/// Обработчик прерывания UART: вычерпать все готовые байты из FIFO в кольцевой буфер.
/// Вызывается из [`crate::plic::handle_external`]; при переполнении кольца новые байты
/// отбрасываются (лучше потерять хвост ввода, чем блокировать обработчик).
pub fn on_irq() {
    drain_rx();
}

/// Вычерпать FIFO приёмника опросом (LSR.DR). Кроме обработчика прерывания это зовут
/// таймерные тики ([`crate::timer`]): во время сессий процессов внешние прерывания выключены
/// (SEIE=0, см. [[process-preemption|proc::run]]), и без регулярного вычерпывания FIFO UART
/// (16 байт) и буфер mux'а QEMU переполняются — набранное на консоли терялось бы.
pub fn drain_rx() {
    let mut ring = RING.lock_irq();
    // SAFETY: MMIO-регистры UART0; LSR.DR гарантирует, что RBR держит принятый байт.
    unsafe {
        while core::ptr::read_volatile(UART0_LSR) & LSR_DATA_READY != 0 {
            let b = core::ptr::read_volatile(UART0_RBR);
            let next = (ring.head + 1) % RING_SIZE;
            if next != ring.tail {
                let at = ring.head;
                ring.buf[at] = b;
                ring.head = next;
            } else {
                // Переполнение больше не молчит: счётчик заберёт и покажет чтение ввода
                // (`SYS_READ`). Молча терять набранное — худший из вариантов: человек видит
                // испорченную строку и не знает, он ошибся или система.
                LOST.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Есть ли непрочитанный ввод.
pub fn has_input() -> bool {
    let ring = RING.lock_irq();
    ring.head != ring.tail
}

/// Забрать один принятый байт (None — буфер пуст).
pub fn getc() -> Option<u8> {
    let mut ring = RING.lock_irq();
    if ring.head == ring.tail {
        return None;
    }
    let b = ring.buf[ring.tail];
    ring.tail = (ring.tail + 1) % RING_SIZE;
    Some(b)
}
