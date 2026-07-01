//! Драйвер блочного устройства virtio-blk поверх virtio-mmio (Веха 7.1).
//!
//! Это «руки» для Вехи 7.2: умеет читать и писать 512-байтные секторы виртуального диска.
//! QEMU `virt` выставляет virtio-устройства как MMIO-регистры; мы работаем с диском через
//! **split virtqueue** — три общих с устройством кольца в RAM:
//!   - **desc** (таблица дескрипторов): куски буферов [адрес, длина, флаги, next];
//!   - **avail** (кольцо доступных): мы кладём сюда индексы готовых запросов;
//!   - **used** (кольцо использованных): устройство кладёт сюда завершённые.
//!
//! Один запрос к диску — цепочка из 3 дескрипторов: заголовок (тип+сектор), буфер данных
//! (512 Б), байт статуса. Мы публикуем цепочку в avail, «дёргаем» устройство записью в
//! QueueNotify и **опрашиваем** used до завершения (без прерываний — так проще для старта).
//!
//! Целимся в virtio **версии 2** (modern, VIRTIO 1.0). Запускать QEMU с
//! `-global virtio-mmio.force-legacy=false` (см. .cargo/config.toml).
//!
//! Два пути завершения запроса:
//! - **Синхронный** ([`read`]/[`write`]) — опрос кольца used. Используется на ранней загрузке
//!   (прерывания ещё выключены) — там всё равно делать нечего, кроме ожидания диска.
//! - **Асинхронный** ([`read_async`]) — прерывание+пробуждение: запрос публикуется, future
//!   паркуется; по завершении устройство шлёт IRQ через PLIC → [`on_irq`] будит future.
//!   Это «диск без опроса», настоящий async I/O над [[async-executor]].

use core::future::Future;
use core::pin::Pin;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, AtomicU32, AtomicUsize, Ordering};
use core::task::{Context, Poll, Waker};

use alloc::boxed::Box;

use crate::sync::SpinLock;
use crate::{csr, frame};

/// Размер сектора virtio-blk.
pub const SECTOR_SIZE: usize = 512;

/// Размер очереди (число дескрипторов). Нам хватает 8 (запрос = 3 дескриптора).
const QSIZE: usize = 8;

// virt-машина QEMU: 8 слотов virtio-mmio по 0x1000, начиная с 0x1000_1000.
const MMIO_BASE: usize = 0x1000_1000;
const MMIO_STRIDE: usize = 0x1000;
const MMIO_SLOTS: usize = 8;

// Регистры virtio-mmio (смещения от базы слота).
const REG_MAGIC: usize = 0x000; // "virt" = 0x74726976
const REG_VERSION: usize = 0x004; // 2 = modern
const REG_DEVICE_ID: usize = 0x008; // 2 = block
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

/// Состояние инициализированного устройства. Кольца/база хранятся как адреса (usize),
/// а не как сырые указатели, — чтобы структура была `Send` и жила под `SpinLock`.
struct VirtioBlk {
    base: usize,
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

        let desc = self.desc as *mut Desc;
        let avail = self.avail as *mut Avail;
        let used = self.used as *const Used;

        unsafe {
            // Дескриптор 0 — заголовок (устройство читает).
            set_desc(desc, 0, &hdr as *const _ as u64, 16, DESC_F_NEXT, 1);
            // Дескриптор 1 — данные. Для чтения устройство ПИШЕТ в буфер (DESC_F_WRITE).
            let data_flags = DESC_F_NEXT | if write { 0 } else { DESC_F_WRITE };
            set_desc(desc, 1, buf as u64, SECTOR_SIZE as u32, data_flags, 2);
            // Дескриптор 2 — байт статуса (устройство пишет).
            set_desc(desc, 2, &mut status as *mut _ as u64, 1, DESC_F_WRITE, 0);

            // Опубликовать голову цепочки (дескриптор 0) в avail.
            fence(Ordering::SeqCst);
            let ai = read_volatile(&(*avail).idx);
            write_volatile(&mut (*avail).ring[(ai as usize) % QSIZE], 0);
            fence(Ordering::SeqCst);
            write_volatile(&mut (*avail).idx, ai.wrapping_add(1));
            fence(Ordering::SeqCst);

            // Разбудить устройство.
            w32(self.base, REG_QUEUE_NOTIFY, 0);

            // Опрашиваем used, пока устройство не завершит наш запрос.
            while read_volatile(&(*used).idx) == self.used_idx {
                core::hint::spin_loop();
            }
            fence(Ordering::SeqCst);
            self.used_idx = self.used_idx.wrapping_add(1);

            // Мы опрашиваем, но подтвердим прерывание устройства, чтобы оно не залипло.
            let is = r32(self.base, REG_INTERRUPT_STATUS);
            if is != 0 {
                w32(self.base, REG_INTERRUPT_ACK, is);
            }

            read_volatile(&status) == 0
        }
    }
}

impl VirtioBlk {
    /// Опубликовать цепочку чтения (3 дескриптора) и дёрнуть устройство. НЕ ждёт завершения —
    /// его сообщит прерывание. Адреса hdr/buf/status должны жить до завершения (у future — в Box).
    fn submit_read(&mut self, hdr: u64, buf: u64, status: u64) {
        let desc = self.desc as *mut Desc;
        let avail = self.avail as *mut Avail;
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
            w32(self.base, REG_QUEUE_NOTIFY, 0);
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

/// База устройства и номер IRQ — читаются из обработчика прерывания (без замка BLK).
static IRQ_BASE: AtomicUsize = AtomicUsize::new(0);
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

/// Номер IRQ нашего устройства на PLIC (0 — не инициализировано).
pub fn irq() -> u32 {
    IRQ_NUM.load(Ordering::Relaxed)
}

/// Сколько прерываний устройства обработано.
pub fn irq_count() -> u32 {
    IRQ_COUNT.load(Ordering::Relaxed)
}

/// Обработчик прерывания устройства (из [`crate::plic::handle_external`]). Выполняется в
/// trap-контексте (SIE=0), поэтому замки берёт без доп. отключения прерываний. Замок BLK НЕ
/// трогает (его может держать синхронный путь) — только подтверждает прерывание и будит future.
pub fn on_irq() {
    let base = IRQ_BASE.load(Ordering::Relaxed);
    if base != 0 {
        unsafe {
            let is = r32(base, REG_INTERRUPT_STATUS);
            if is != 0 {
                w32(base, REG_INTERRUPT_ACK, is);
            }
        }
    }
    IRQ_COUNT.fetch_add(1, Ordering::Relaxed);

    let waker = {
        let mut a = ASYNC.lock();
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

/// Найти и инициализировать первое virtio-blk устройство. Возвращает true при успехе.
pub fn init() -> bool {
    let Some(base) = probe() else {
        return false;
    };

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
        let desc = frame::alloc().expect("virtio desc frame");
        let avail = frame::alloc().expect("virtio avail frame");
        let used = frame::alloc().expect("virtio used frame");
        write_addr(base, REG_QUEUE_DESC_LOW, REG_QUEUE_DESC_HIGH, desc);
        write_addr(base, REG_QUEUE_DRIVER_LOW, REG_QUEUE_DRIVER_HIGH, avail);
        write_addr(base, REG_QUEUE_DEVICE_LOW, REG_QUEUE_DEVICE_HIGH, used);
        w32(base, REG_QUEUE_READY, 1);

        // Устройство готово к работе.
        status |= STATUS_DRIVER_OK;
        w32(base, REG_STATUS, status);

        // Ёмкость диска (в секторах) из конфиг-пространства.
        let cap_lo = r32(base, REG_CONFIG) as u64;
        let cap_hi = r32(base, REG_CONFIG + 4) as u64;
        let capacity = cap_lo | (cap_hi << 32);

        *BLK.lock() = Some(VirtioBlk {
            base,
            desc,
            avail,
            used,
            used_idx: 0,
            capacity_sectors: capacity,
        });

        // Запомнить для обработчика прерываний: база + номер IRQ (slot+1 на QEMU virt).
        IRQ_BASE.store(base, Ordering::Relaxed);
        let slot = (base - MMIO_BASE) / MMIO_STRIDE;
        IRQ_NUM.store(slot as u32 + 1, Ordering::Relaxed);
    }
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
            let hp = &*this.hdr as *const ReqHeader as u64;
            let bp = this.buf.as_ptr() as u64;
            let sp = &*this.status as *const u8 as u64;
            let mut g = BLK.lock();
            let Some(blk) = g.as_mut() else {
                return Poll::Ready(None); // диск не инициализирован
            };
            // Зарегистрировать ожидание ДО notify, чтобы не разминуться с прерыванием.
            // Замок ASYNC берём с выключенными прерываниями: иначе IRQ посреди удержания
            // замка → on_irq на том же замке → взаимоблокировка.
            let sie = csr::irq_save_disable();
            {
                let mut a = ASYNC.lock();
                a.active = true;
                a.done = false;
                a.waker = Some(cx.waker().clone());
            }
            csr::irq_restore(sie);
            blk.submit_read(hp, bp, sp);
            this.submitted = true;
            return Poll::Pending;
        }

        // Уже отправлено — проверить завершение (флаг выставляет on_irq).
        let sie = csr::irq_save_disable();
        let done = {
            let mut a = ASYNC.lock();
            if a.done {
                a.active = false;
                true
            } else {
                a.waker = Some(cx.waker().clone()); // обновить waker
                false
            }
        };
        csr::irq_restore(sie);

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

/// Просканировать слоты virtio-mmio, вернуть базу первого блочного устройства (version 2).
fn probe() -> Option<usize> {
    for slot in 0..MMIO_SLOTS {
        let base = MMIO_BASE + slot * MMIO_STRIDE;
        unsafe {
            if r32(base, REG_MAGIC) != 0x7472_6976 {
                continue; // "virt" не найден — слот пуст
            }
            if r32(base, REG_VERSION) == 2 && r32(base, REG_DEVICE_ID) == 2 {
                return Some(base); // modern virtio-blk
            }
        }
    }
    None
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
