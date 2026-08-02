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
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, AtomicU32, AtomicUsize, Ordering};
use core::task::{Context, Poll, Waker};

use alloc::boxed::Box;

use crate::sync::SpinLock;
use crate::{arch, frame};

/// Размер сектора virtio-blk.
pub const SECTOR_SIZE: usize = 512;

/// Размер очереди (число дескрипторов). Нам хватает 8 (запрос = 3 дескриптора).
const QSIZE: usize = 8;

// Регистры virtio-mmio (смещения от базы слота; magic/version/device-id проверяет
// арх-поиск — arch::probe_virtio_blk).
const REG_DRIVER_FEATURES: usize = 0x020;
const REG_DRIVER_FEATURES_SEL: usize = 0x024;
const REG_QUEUE_SEL: usize = 0x030;
const REG_QUEUE_NUM_MAX: usize = 0x034;
const REG_QUEUE_NUM: usize = 0x038;
const REG_QUEUE_READY: usize = 0x044;
const REG_QUEUE_NOTIFY: usize = 0x050;
const REG_INTERRUPT_STATUS: usize = 0x060;
const REG_INTERRUPT_ACK: usize = 0x064;
const REG_STATUS: usize = 0x070;
const REG_QUEUE_DESC_LOW: usize = 0x080;
const REG_QUEUE_DESC_HIGH: usize = 0x084;
const REG_QUEUE_DRIVER_LOW: usize = 0x090;
const REG_QUEUE_DRIVER_HIGH: usize = 0x094;
const REG_QUEUE_DEVICE_LOW: usize = 0x0a0;
const REG_QUEUE_DEVICE_HIGH: usize = 0x0a4;
const REG_CONFIG: usize = 0x100; // конфиг устройства: capacity (u64) в секторах

// Поля структуры common_cfg virtio-pci modern (смещения; все поля LE).
const PCI_DRIVER_FEATURE_SEL: usize = 0x08;
const PCI_DRIVER_FEATURE: usize = 0x0c;
const PCI_MSIX_CONFIG: usize = 0x10; // u16: вектор конфиг-событий (0xffff — нет)
const PCI_DEVICE_STATUS: usize = 0x14; // u8: те же биты STATUS_*, что и в mmio
const PCI_QUEUE_SEL: usize = 0x16; // u16
const PCI_QUEUE_SIZE: usize = 0x18; // u16 (чтение — максимум, запись — наш размер)
const PCI_QUEUE_MSIX_VECTOR: usize = 0x1a; // u16: запись MSI-X-вектора очереди
const PCI_QUEUE_ENABLE: usize = 0x1c; // u16
const PCI_QUEUE_NOTIFY_OFF: usize = 0x1e; // u16: слагаемое notify-адреса
const PCI_QUEUE_DESC: usize = 0x20; // u64
const PCI_QUEUE_DRIVER: usize = 0x28; // u64
const PCI_QUEUE_DEVICE: usize = 0x30; // u64
/// «Вектора нет» в полях msix_vector.
const PCI_NO_VECTOR: u16 = 0xffff;

// Биты регистра Status.
const STATUS_ACKNOWLEDGE: u32 = 1;
const STATUS_DRIVER: u32 = 2;
const STATUS_DRIVER_OK: u32 = 4;
const STATUS_FEATURES_OK: u32 = 8;

// Флаги дескриптора.
const DESC_F_NEXT: u16 = 1; // есть следующий в цепочке
const DESC_F_WRITE: u16 = 2; // устройство ПИШЕТ в буфер (для нас — чтение с диска)

// Feature bit 32 (в старшем dword — бит 0): VIRTIO_F_VERSION_1 — обязателен для modern.
const DRIVER_FEATURE_HI_VERSION_1: u32 = 1;

// Типы запроса virtio-blk.
const BLK_T_IN: u32 = 0; // чтение с диска
const BLK_T_OUT: u32 = 1; // запись на диск

/// Дескриптор split virtqueue (16 байт).
#[allow(dead_code)] // layout для DMA: часть полей читает/пишет только устройство
#[repr(C)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

/// Кольцо доступных: сюда мы кладём индексы голов цепочек.
#[allow(dead_code)] // layout для DMA: часть полей читает/пишет только устройство
#[repr(C)]
struct Avail {
    flags: u16,
    idx: u16,
    ring: [u16; QSIZE],
    used_event: u16,
}

#[allow(dead_code)] // layout для DMA: часть полей читает/пишет только устройство
#[repr(C)]
struct UsedElem {
    id: u32,
    len: u32,
}

/// Кольцо использованных: сюда устройство кладёт завершённые запросы.
#[allow(dead_code)] // layout для DMA: часть полей читает/пишет только устройство
#[repr(C)]
struct Used {
    flags: u16,
    idx: u16,
    ring: [UsedElem; QSIZE],
    avail_event: u16,
}

/// Заголовок запроса virtio-blk (16 байт).
#[allow(dead_code)] // layout для DMA: часть полей читает/пишет только устройство
#[repr(C)]
struct ReqHeader {
    kind: u32,
    reserved: u32,
    sector: u64,
}

/// Транспорт устройства ПОСЛЕ инициализации: что нужно в горячем пути —
/// куда «дёргать» (notify) и как подтверждать прерывание (ack).
enum Transport {
    Mmio { base: usize },
    /// notify — уже вычисленный адрес очереди 0; isr — байт INTx-статуса
    /// (читается-и-сбрасывается; при MSI-X не обязателен, но безвреден).
    Pci { notify: usize, isr: usize },
}

impl Transport {
    /// Разбудить устройство: очередь 0 готова к работе.
    fn notify_queue0(&self) {
        unsafe {
            match self {
                Transport::Mmio { base } => w32(*base, REG_QUEUE_NOTIFY, 0),
                Transport::Pci { notify, .. } => write_volatile(*notify as *mut u16, 0),
            }
        }
    }

    /// Подтвердить прерывание устройства (чтобы линия/статус не залипли).
    fn irq_ack(&self) {
        unsafe {
            match self {
                Transport::Mmio { base } => {
                    let is = r32(*base, REG_INTERRUPT_STATUS);
                    if is != 0 {
                        w32(*base, REG_INTERRUPT_ACK, is);
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
    desc: usize,
    avail: usize,
    used: usize,
    used_idx: u16,
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

        // Веха 87: в полях — физические адреса (их знает устройство); ядру нужен direct-map.
        let desc = frame::ptr(self.desc) as *mut Desc;
        let avail = frame::ptr(self.avail) as *mut Avail;
        let used = frame::ptr(self.used) as *const Used;

        unsafe {
            // Дескриптор 0 — заголовок (устройство читает).
            set_desc(desc, 0, arch::virt_to_phys(&hdr as *const _ as usize) as u64, 16, DESC_F_NEXT, 1);
            // Дескриптор 1 — данные. Для чтения устройство ПИШЕТ в буфер (DESC_F_WRITE).
            let data_flags = DESC_F_NEXT | if write { 0 } else { DESC_F_WRITE };
            set_desc(desc, 1, arch::virt_to_phys(buf) as u64, SECTOR_SIZE as u32, data_flags, 2);
            // Дескриптор 2 — байт статуса (устройство пишет).
            set_desc(desc, 2, arch::virt_to_phys(&mut status as *mut _ as usize) as u64, 1, DESC_F_WRITE, 0);

            // Опубликовать голову цепочки (дескриптор 0) в avail.
            fence(Ordering::SeqCst);
            let ai = read_volatile(&(*avail).idx);
            write_volatile(&mut (*avail).ring[(ai as usize) % QSIZE], 0);
            fence(Ordering::SeqCst);
            write_volatile(&mut (*avail).idx, ai.wrapping_add(1));
            fence(Ordering::SeqCst);

            // Разбудить устройство.
            self.t.notify_queue0();

            // Опрашиваем used, пока устройство не завершит наш запрос.
            while read_volatile(&(*used).idx) == self.used_idx {
                core::hint::spin_loop();
            }
            fence(Ordering::SeqCst);
            self.used_idx = self.used_idx.wrapping_add(1);

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
        let desc = frame::ptr(self.desc) as *mut Desc;
        let avail = frame::ptr(self.avail) as *mut Avail;
        unsafe {
            set_desc(desc, 0, hdr, 16, DESC_F_NEXT, 1);
            set_desc(desc, 1, buf, SECTOR_SIZE as u32, DESC_F_NEXT | DESC_F_WRITE, 2);
            set_desc(desc, 2, status, 1, DESC_F_WRITE, 0);
            fence(Ordering::SeqCst);
            let ai = read_volatile(&(*avail).idx);
            write_volatile(&mut (*avail).ring[(ai as usize) % QSIZE], 0);
            fence(Ordering::SeqCst);
            write_volatile(&mut (*avail).idx, ai.wrapping_add(1));
            fence(Ordering::SeqCst);
            self.t.notify_queue0();
        }
    }

    /// Отметить, что одна запись used-кольца обработана (после завершения запроса).
    fn complete(&mut self) {
        fence(Ordering::SeqCst);
        self.used_idx = self.used_idx.wrapping_add(1);
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
            let is = r32(base, REG_INTERRUPT_STATUS);
            if is != 0 {
                w32(base, REG_INTERRUPT_ACK, is);
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
pub fn init() -> bool {
    let Some(dev) = arch::probe_virtio_blk() else {
        return false;
    };
    let ok = match dev.transport {
        arch::BlkTransport::Mmio { base } => init_mmio(base),
        arch::BlkTransport::Pci { common, notify_base, notify_mult, isr, device } => {
            init_pci(common, notify_base, notify_mult, isr, device)
        }
    };
    if ok {
        IRQ_NUM.store(dev.irq, Ordering::Relaxed);
    }
    ok
}

/// Инициализация по virtio-mmio (QEMU virt): рукопожатие статуса → фичи → очередь 0.
fn init_mmio(base: usize) -> bool {
    unsafe {
        // Сброс и рукопожатие статуса.
        w32(base, REG_STATUS, 0);
        let mut status = STATUS_ACKNOWLEDGE;
        w32(base, REG_STATUS, status);
        status |= STATUS_DRIVER;
        w32(base, REG_STATUS, status);

        // Согласование фич: принимаем только VIRTIO_F_VERSION_1 (обязателен для modern).
        w32(base, REG_DRIVER_FEATURES_SEL, 1);
        w32(base, REG_DRIVER_FEATURES, DRIVER_FEATURE_HI_VERSION_1);
        w32(base, REG_DRIVER_FEATURES_SEL, 0);
        w32(base, REG_DRIVER_FEATURES, 0);
        status |= STATUS_FEATURES_OK;
        w32(base, REG_STATUS, status);
        if r32(base, REG_STATUS) & STATUS_FEATURES_OK == 0 {
            return false; // устройство не приняло наш набор фич
        }

        // Настройка очереди 0.
        w32(base, REG_QUEUE_SEL, 0);
        let num_max = r32(base, REG_QUEUE_NUM_MAX);
        if num_max == 0 {
            return false;
        }
        w32(base, REG_QUEUE_NUM, QSIZE as u32);

        // Кольца — в обнулённых фреймах (RAM отображена идентично: адрес = физический).
        let (desc, avail, used) = alloc_rings();
        write_addr(base, REG_QUEUE_DESC_LOW, REG_QUEUE_DESC_HIGH, desc);
        write_addr(base, REG_QUEUE_DRIVER_LOW, REG_QUEUE_DRIVER_HIGH, avail);
        write_addr(base, REG_QUEUE_DEVICE_LOW, REG_QUEUE_DEVICE_HIGH, used);
        w32(base, REG_QUEUE_READY, 1);

        // Устройство готово к работе.
        status |= STATUS_DRIVER_OK;
        w32(base, REG_STATUS, status);

        // Ёмкость диска (в секторах) из конфиг-пространства.
        let capacity = r32(base, REG_CONFIG) as u64 | (r32(base, REG_CONFIG + 4) as u64) << 32;

        publish(Transport::Mmio { base }, desc, avail, used, capacity);
        IRQ_ACK_MMIO.store(base, Ordering::Relaxed);
    }
    true
}

/// Инициализация по virtio-pci modern (QEMU q35): та же машина состояний, но поля —
/// в структуре common_cfg (MMIO из BAR, отображён архом), notify — отдельное окно,
/// прерывание — MSI-X-вектор `irq` (запись 0 таблицы запрограммировал арх).
fn init_pci(common: usize, notify_base: usize, notify_mult: u32, isr: usize, device: usize) -> bool {
    let r8p = |off: usize| unsafe { read_volatile((common + off) as *const u8) };
    let w8p = |off: usize, v: u8| unsafe { write_volatile((common + off) as *mut u8, v) };
    let r16p = |off: usize| unsafe { read_volatile((common + off) as *const u16) };
    let w16p = |off: usize, v: u16| unsafe { write_volatile((common + off) as *mut u16, v) };
    let w32p = |off: usize, v: u32| unsafe { write_volatile((common + off) as *mut u32, v) };
    let w64p = |off: usize, v: u64| unsafe { write_volatile((common + off) as *mut u64, v) };

    // Сброс и рукопожатие статуса (те же биты, что в mmio).
    w8p(PCI_DEVICE_STATUS, 0);
    let mut status = STATUS_ACKNOWLEDGE as u8;
    w8p(PCI_DEVICE_STATUS, status);
    status |= STATUS_DRIVER as u8;
    w8p(PCI_DEVICE_STATUS, status);

    // Фичи: только VIRTIO_F_VERSION_1.
    w32p(PCI_DRIVER_FEATURE_SEL, 1);
    w32p(PCI_DRIVER_FEATURE, DRIVER_FEATURE_HI_VERSION_1);
    w32p(PCI_DRIVER_FEATURE_SEL, 0);
    w32p(PCI_DRIVER_FEATURE, 0);
    status |= STATUS_FEATURES_OK as u8;
    w8p(PCI_DEVICE_STATUS, status);
    if r8p(PCI_DEVICE_STATUS) & STATUS_FEATURES_OK as u8 == 0 {
        return false;
    }

    // Конфиг-события не нужны; прерывание завершений очереди 0 — MSI-X-запись 0.
    w16p(PCI_MSIX_CONFIG, PCI_NO_VECTOR);
    w16p(PCI_QUEUE_SEL, 0);
    if r16p(PCI_QUEUE_SIZE) == 0 {
        return false;
    }
    w16p(PCI_QUEUE_SIZE, QSIZE as u16);
    w16p(PCI_QUEUE_MSIX_VECTOR, 0);
    if r16p(PCI_QUEUE_MSIX_VECTOR) != 0 {
        return false; // устройство не приняло вектор (NO_VECTOR) — без MSI-X не работаем
    }

    let (desc, avail, used) = alloc_rings();
    w64p(PCI_QUEUE_DESC, desc as u64);
    w64p(PCI_QUEUE_DRIVER, avail as u64);
    w64p(PCI_QUEUE_DEVICE, used as u64);
    let notify = notify_base + r16p(PCI_QUEUE_NOTIFY_OFF) as usize * notify_mult as usize;
    w16p(PCI_QUEUE_ENABLE, 1);

    status |= STATUS_DRIVER_OK as u8;
    w8p(PCI_DEVICE_STATUS, status);

    // Ёмкость (в секторах) — первые 8 байт конфиг-области устройства.
    let capacity = unsafe {
        read_volatile(device as *const u32) as u64
            | (read_volatile((device + 4) as *const u32) as u64) << 32
    };

    publish(Transport::Pci { notify, isr }, desc, avail, used, capacity);
    IRQ_ACK_ISR.store(isr, Ordering::Relaxed);
    true
}

/// Три кольца очереди — в обнулённых фреймах (RAM идентична: адрес = физический).
fn alloc_rings() -> (usize, usize, usize) {
    (
        frame::alloc().expect("virtio desc frame"),
        frame::alloc().expect("virtio avail frame"),
        frame::alloc().expect("virtio used frame"),
    )
}

/// Опубликовать готовое устройство под замком.
fn publish(t: Transport, desc: usize, avail: usize, used: usize, capacity: u64) {
    *BLK.lock() = Some(VirtioBlk { t, desc, avail, used, used_idx: 0, capacity_sectors: capacity });
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

// ─── низкоуровневые помощники ────────────────────────────────────────────────

#[inline]
unsafe fn r32(base: usize, off: usize) -> u32 {
    read_volatile((base + off) as *const u32)
}

#[inline]
unsafe fn w32(base: usize, off: usize, val: u32) {
    write_volatile((base + off) as *mut u32, val);
}

/// Записать 64-битный адрес в пару регистров low/high.
#[inline]
unsafe fn write_addr(base: usize, lo: usize, hi: usize, addr: usize) {
    w32(base, lo, addr as u32);
    w32(base, hi, (addr as u64 >> 32) as u32);
}

/// Заполнить дескриптор с индексом `i`.
#[inline]
unsafe fn set_desc(desc: *mut Desc, i: usize, addr: u64, len: u32, flags: u16, next: u16) {
    let d = desc.add(i);
    write_volatile(&mut (*d).addr, addr);
    write_volatile(&mut (*d).len, len);
    write_volatile(&mut (*d).flags, flags);
    write_volatile(&mut (*d).next, next);
}
