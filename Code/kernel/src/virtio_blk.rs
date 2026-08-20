//! Драйвер блочного устройства virtio-blk (Веха 7.1; Веха 27 — два транспорта).
//!
//! Это «руки» для Вехи 7.2: умеет читать и писать 512-байтные секторы виртуального диска.
//! Устройство ищет АРХ ([`arch::probe_virtio_blk`]): на QEMU `virt` (riscv) это слот
//! **virtio-mmio**, на q35 (x86) — **virtio-pci** (общая cfg-структура из vendor-capability;
//! PCI-обвязку и MSI-X делает арх, сюда приходят готовые MMIO-адреса). Сам протокол
//! одинаков — **split virtqueue**, три общих с устройством кольца в RAM:
//!   - **desc** (таблица дескрипторов): куски буферов [адрес, длина, флаги, next];
//!   - **avail** (кольцо доступных): мы кладём сюда индексы готовых запросов;
//!   - **used** (кольцо использованных): устройство кладёт сюда завершённые.
//!
//! Один запрос к диску — цепочка из 3 дескрипторов: заголовок (тип+сектор), буфер данных
//! (512 Б), байт статуса. Мы публикуем цепочку в avail и «дёргаем» устройство (notify —
//! у каждого транспорта свой адрес).
//!
//! Целимся в virtio **modern** (VIRTIO 1.0): mmio версии 2 (`force-legacy=false`),
//! pci с `disable-legacy=on` (см. .cargo/config.toml).
//!
//! Два пути завершения запроса:
//! - **Синхронный** ([`read`]/[`write`]) — опрос кольца used. Используется на ранней загрузке
//!   (прерывания ещё выключены) — там всё равно делать нечего, кроме ожидания диска.
//! - **Асинхронный** ([`read_async`]) — прерывание+пробуждение: запрос публикуется, future
//!   паркуется; по завершении устройство шлёт IRQ (PLIC на riscv, MSI-X на x86) →
//!   [`on_irq`] будит future. Это «диск без опроса», настоящий async I/O над
//!   [[async-executor]].

use core::future::Future;
use core::pin::Pin;
use core::ptr::read_volatile;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use core::task::{Context, Poll, Waker};

use alloc::boxed::Box;

use crate::sync::SpinLock;
use crate::virtio::{self, Cfg, Queue, DESC_F_NEXT, DESC_F_WRITE};
use crate::arch;

/// Размер сектора virtio-blk.
pub const SECTOR_SIZE: usize = 512;

/// Размер очереди (число дескрипторов). Нам хватает 8 (запрос = 3 дескриптора).
const QSIZE: usize = 8;

// Типы запроса virtio-blk.
const BLK_T_IN: u32 = 0; // чтение с диска
const BLK_T_OUT: u32 = 1; // запись на диск

/// Заголовок запроса virtio-blk (16 байт).
#[allow(dead_code)] // layout для DMA: часть полей читает/пишет только устройство
#[repr(C)]
struct ReqHeader {
    kind: u32,
    reserved: u32,
    sector: u64,
}

/// Транспорт устройства ПОСЛЕ инициализации — только для подтверждения прерывания: «дёргать»
/// очередь умеет [`Queue::kick`], а вот сброс статуса прерывания у транспортов свой.
enum Transport {
    Mmio { base: usize },
    /// `isr` — байт INTx-статуса (читается-и-сбрасывается; при MSI-X не обязателен, но безвреден).
    Pci { isr: usize },
}

impl Transport {
    /// Подтвердить прерывание устройства (чтобы линия/статус не залипли).
    fn irq_ack(&self) {
        unsafe {
            match self {
                Transport::Mmio { base } => {
                    let is = virtio::r32(*base, virtio::REG_INTERRUPT_STATUS);
                    if is != 0 {
                        virtio::w32(*base, virtio::REG_INTERRUPT_ACK, is);
                    }
                }
                Transport::Pci { isr, .. } => {
                    read_volatile(*isr as *const u8); // чтение = сброс
                }
            }
        }
    }
}

/// Состояние инициализированного устройства. Кольца хранятся как адреса (usize),
/// а не как сырые указатели, — чтобы структура была `Send` и жила под `SpinLock`.
struct VirtioBlk {
    t: Transport,
    q: Queue,
    capacity_sectors: u64,
}

impl VirtioBlk {
    /// Выполнить один запрос: сектор `sector`, буфер по адресу `buf` (512 Б), чтение/запись.
    /// Возвращает true при статусе OK. Синхронно: публикуем цепочку и опрашиваем used.
    fn request(&mut self, sector: u64, buf: usize, write: bool) -> bool {
        let hdr = ReqHeader {
            kind: if write { BLK_T_OUT } else { BLK_T_IN },
            reserved: 0,
            sector,
        };
        let mut status: u8 = 0xff;

        unsafe {
            // Веха 87: в полях дескрипторов — ФИЗИЧЕСКИЕ адреса (их знает устройство).
            //
            // Дескриптор 0 — заголовок (устройство читает).
            let q = &mut self.q;
            q.set_desc(0, arch::virt_to_phys(&hdr as *const _ as usize) as u64, 16, DESC_F_NEXT, 1);
            // Дескриптор 1 — данные. Для чтения устройство ПИШЕТ в буфер (DESC_F_WRITE).
            let data_flags = DESC_F_NEXT | if write { 0 } else { DESC_F_WRITE };
            q.set_desc(1, arch::virt_to_phys(buf) as u64, SECTOR_SIZE as u32, data_flags, 2);
            // Дескриптор 2 — байт статуса (устройство пишет).
            q.set_desc(2, arch::virt_to_phys(&mut status as *mut _ as usize) as u64, 1, DESC_F_WRITE, 0);

            // Опубликовать голову цепочки (дескриптор 0) и разбудить устройство.
            q.offer(0);
            q.kick();

            // Опрашиваем used, пока устройство не завершит наш запрос.
            while !q.has_used() {
                core::hint::spin_loop();
            }
            q.take_used();

            // Мы опрашиваем, но подтвердим прерывание устройства, чтобы оно не залипло.
            self.t.irq_ack();

            read_volatile(&status) == 0
        }
    }
}

impl VirtioBlk {
    /// Опубликовать цепочку чтения (3 дескриптора) и дёрнуть устройство. НЕ ждёт завершения —
    /// его сообщит прерывание. Адреса hdr/buf/status должны жить до завершения (у future — в Box).
    fn submit_read(&mut self, hdr: u64, buf: u64, status: u64) {
        unsafe {
            self.q.set_desc(0, hdr, 16, DESC_F_NEXT, 1);
            self.q.set_desc(1, buf, SECTOR_SIZE as u32, DESC_F_NEXT | DESC_F_WRITE, 2);
            self.q.set_desc(2, status, 1, DESC_F_WRITE, 0);
            self.q.offer(0);
            self.q.kick();
        }
    }

    /// Отметить, что одна запись used-кольца обработана (после завершения запроса).
    fn complete(&mut self) {
        unsafe { self.q.take_used() };
    }
}

// Безопасно: все поля — адреса/числа; доступ сериализуется внешним SpinLock.
unsafe impl Send for VirtioBlk {}

static BLK: SpinLock<Option<VirtioBlk>> = SpinLock::new(None);

// ─── состояние прерываний / async ────────────────────────────────────────────

/// Как подтверждать прерывание из [`on_irq`] БЕЗ замка BLK (его может держать
/// синхронный путь): mmio-база ИЛИ адрес pci-ISR-байта; 0 — транспорта нет.
static IRQ_ACK_MMIO: AtomicUsize = AtomicUsize::new(0);
static IRQ_ACK_ISR: AtomicUsize = AtomicUsize::new(0);
static IRQ_NUM: AtomicU32 = AtomicU32::new(0);
/// Счётчик обработанных прерываний устройства (для наглядности).
static IRQ_COUNT: AtomicU32 = AtomicU32::new(0);

/// Состояние единственной async-операции (в демо больше одной за раз не бывает).
struct AsyncIo {
    active: bool,
    done: bool,
    waker: Option<Waker>,
}
static ASYNC: SpinLock<AsyncIo> = SpinLock::new(AsyncIo { active: false, done: false, waker: None });

/// Номер прерывания устройства (riscv: источник PLIC, x86: вектор MSI-X; 0 — нет).
pub fn irq() -> u32 {
    IRQ_NUM.load(Ordering::Relaxed)
}

/// Сколько прерываний устройства обработано.
pub fn irq_count() -> u32 {
    IRQ_COUNT.load(Ordering::Relaxed)
}

/// Обработчик прерывания устройства (riscv: `plic::handle_external`, x86: вектор MSI-X).
/// Выполняется в trap-контексте (прерывания выключены), поэтому замки берёт без доп.
/// отключения. Замок BLK НЕ трогает (его может держать синхронный путь) — только
/// подтверждает прерывание и будит future.
pub fn on_irq() {
    let base = IRQ_ACK_MMIO.load(Ordering::Relaxed);
    if base != 0 {
        unsafe {
            let is = virtio::r32(base, virtio::REG_INTERRUPT_STATUS);
            if is != 0 {
                virtio::w32(base, virtio::REG_INTERRUPT_ACK, is);
            }
        }
    }
    let isr = IRQ_ACK_ISR.load(Ordering::Relaxed);
    if isr != 0 {
        unsafe { read_volatile(isr as *const u8) }; // чтение = сброс INTx-статуса
    }
    IRQ_COUNT.fetch_add(1, Ordering::Relaxed);
    // Веха 86 — подмешать джиттер завершения дисковой операции в пул энтропии ядра.
    // Только атомарные операции: замок в контексте прерывания = дедлок (см. [[known-gaps]]).
    crate::random::stir(2);

    let waker = {
        let mut a = ASYNC.lock_irq();
        if a.active {
            a.done = true;
            a.waker.take()
        } else {
            None
        }
    };
    if let Some(w) = waker {
        w.wake(); // разбудить future (executor-очередь irq-safe)
    }
}

/// Найти (через арх) и инициализировать первое virtio-blk устройство.
///
/// Рукопожатие — общее с сетью и энтропией ([`crate::virtio`]): сброс, фичи, очередь, DRIVER_OK.
/// Своих feature-битов у нас нет; очередь одна, и ей НУЖЕН MSI-X-вектор 0 — на нём держится
/// асинхронное чтение ([`read_async`]).
pub fn init() -> bool {
    let Some(dev) = arch::probe_virtio_blk() else {
        return false;
    };
    let cfg = Cfg::new(dev.transport);
    cfg.begin();
    if !cfg.accept_features(0) {
        return false; // устройство не приняло наш набор фич
    }
    cfg.no_config_msix();
    // Вектор просим только у PCI: у mmio прерывание одно на устройство и назначает его арх.
    let msix = matches!(dev.transport, arch::BlkTransport::Pci { .. }).then_some(0u16);
    let Some(q) = cfg.queue("virtio-blk", 0, QSIZE, msix) else {
        return false;
    };
    cfg.ready();

    // Ёмкость (в секторах) — первые 8 байт конфиг-области устройства.
    let conf = cfg.config();
    let capacity = unsafe {
        read_volatile(conf as *const u32) as u64
            | (read_volatile((conf + 4) as *const u32) as u64) << 32
    };

    let t = match dev.transport {
        arch::BlkTransport::Mmio { base } => {
            IRQ_ACK_MMIO.store(base, Ordering::Relaxed);
            Transport::Mmio { base }
        }
        arch::BlkTransport::Pci { isr, .. } => {
            IRQ_ACK_ISR.store(isr, Ordering::Relaxed);
            Transport::Pci { isr }
        }
    };
    *BLK.lock() = Some(VirtioBlk { t, q, capacity_sectors: capacity });
    IRQ_NUM.store(dev.irq, Ordering::Relaxed);
    true
}

/// Ёмкость диска в секторах (0, если не инициализирован).
pub fn capacity_sectors() -> u64 {
    BLK.lock().as_ref().map_or(0, |b| b.capacity_sectors)
}

/// Прочитать сектор `sector` в `buf`. Возвращает true при успехе.
pub fn read(sector: u64, buf: &mut [u8; SECTOR_SIZE]) -> bool {
    let mut guard = BLK.lock();
    match guard.as_mut() {
        Some(blk) => blk.request(sector, buf.as_mut_ptr() as usize, false),
        None => false,
    }
}

/// Записать `buf` в сектор `sector`. Возвращает true при успехе.
pub fn write(sector: u64, buf: &[u8; SECTOR_SIZE]) -> bool {
    let mut guard = BLK.lock();
    match guard.as_mut() {
        Some(blk) => blk.request(sector, buf.as_ptr() as usize, true),
        None => false,
    }
}

/// Асинхронно прочитать сектор `sector`. Возвращает future: `Some(байты)` при успехе,
/// `None` при ошибке. Future публикует запрос и **паркуется** до прерывания от устройства —
/// без опроса. Буферы (заголовок/данные/статус) живут в `Box` (стабильные адреса для DMA).
pub fn read_async(sector: u64) -> ReadFuture {
    ReadFuture {
        hdr: Box::new(ReqHeader { kind: BLK_T_IN, reserved: 0, sector }),
        buf: Box::new([0u8; SECTOR_SIZE]),
        status: Box::new(0xff),
        submitted: false,
    }
}

/// Future чтения сектора (см. [`read_async`]).
pub struct ReadFuture {
    hdr: Box<ReqHeader>,
    buf: Box<[u8; SECTOR_SIZE]>,
    status: Box<u8>,
    submitted: bool,
}

impl Future for ReadFuture {
    type Output = Option<[u8; SECTOR_SIZE]>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut(); // ReadFuture: Unpin (поля — Box/скаляры)

        if !this.submitted {
            // Веха 87: устройство читает/пишет по ФИЗИЧЕСКИМ адресам, а `Box` живёт в куче ядра
            // (в direct-map) — переводим, иначе DMA уйдёт по адресу верхней половины.
            let hp = arch::virt_to_phys(&*this.hdr as *const ReqHeader as usize) as u64;
            let bp = arch::virt_to_phys(this.buf.as_ptr() as usize) as u64;
            let sp = arch::virt_to_phys(&*this.status as *const u8 as usize) as u64;
            let mut g = BLK.lock();
            let Some(blk) = g.as_mut() else {
                return Poll::Ready(None); // диск не инициализирован
            };
            // Зарегистрировать ожидание ДО notify, чтобы не разминуться с прерыванием.
            // Веха 89: `lock_irq` — замок берёт и `on_irq` (см. [[sync]]).
            {
                let mut a = ASYNC.lock_irq();
                a.active = true;
                a.done = false;
                a.waker = Some(cx.waker().clone());
            }
            blk.submit_read(hp, bp, sp);
            this.submitted = true;
            return Poll::Pending;
        }

        // Уже отправлено — проверить завершение (флаг выставляет on_irq).
        let done = {
            let mut a = ASYNC.lock_irq();
            if a.done {
                a.active = false;
                true
            } else {
                a.waker = Some(cx.waker().clone()); // обновить waker
                false
            }
        };

        if done {
            if let Some(blk) = BLK.lock().as_mut() {
                blk.complete();
            }
            if *this.status == 0 {
                Poll::Ready(Some(*this.buf))
            } else {
                Poll::Ready(None)
            }
        } else {
            Poll::Pending
        }
    }
}
