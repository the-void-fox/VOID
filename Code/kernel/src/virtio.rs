//! virtio: транспорт, ОБЩИЙ трём драйверам (Веха 148.7).
//!
//! ## Зачем
//!
//! `virtio_blk`, `virtio_net` и `virtio_rng` держали по своей копии одного и того же: карты
//! регистров mmio и pci, биты статуса, структуры колец, `alloc_rings`, `r32`/`w32`/`write_addr`
//! и обе машины рукопожатия — `init_mmio` и `init_pci`. Около сорока процентов объёма каждого
//! файла. Устройства разные, транспорт один: это и записано в шапках всех трёх драйверов, но
//! написан он был трижды.
//!
//! **Копия уже стоила бага, и он записан в самом коде.** У `virtio_net` в `init_pci` стоит
//! комментарий: «привязать очередь приёма к записи 0 таблицы MSI-X; без этого устройство
//! остаётся с NO_VECTOR и прерываний не шлёт вовсе — ровно этот шаг делает virtio-blk, и ровно
//! его тут не хватало, сеть поэтому и жила опросом». Один шаг рукопожатия, пропущенный в одной
//! из копий, — и целый драйвер тихо работал не так, как задумано, целую веху.
//!
//! Здесь рукопожатие ОДНО. Различия устройств остались параметрами: свои feature-биты, сколько
//! очередей, нужен ли им MSI-X-вектор, что лежит в конфиг-области.
//!
//! ## Чего здесь нет
//!
//! Работы с запросом: у блока это цепочка из трёх дескрипторов и байт статуса, у сети — заголовок
//! `virtio_net_hdr` и два кольца, у энтропии — один пишущий дескриптор. Это УСТРОЙСТВО, а не
//! транспорт, и общего у них ровно столько, сколько даёт [`Ring`].

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

use crate::arch::BlkTransport;
use crate::frame;

// ── карта регистров virtio-mmio (version 2 = modern) ─────────────────────────
//
// Смещения от базы слота. magic/version/device-id проверяет арх-поиск.

pub const REG_DRIVER_FEATURES: usize = 0x020;
pub const REG_DRIVER_FEATURES_SEL: usize = 0x024;
pub const REG_QUEUE_SEL: usize = 0x030;
pub const REG_QUEUE_NUM_MAX: usize = 0x034;
pub const REG_QUEUE_NUM: usize = 0x038;
pub const REG_QUEUE_READY: usize = 0x044;
pub const REG_QUEUE_NOTIFY: usize = 0x050;
pub const REG_INTERRUPT_STATUS: usize = 0x060;
pub const REG_INTERRUPT_ACK: usize = 0x064;
pub const REG_STATUS: usize = 0x070;
pub const REG_QUEUE_DESC_LOW: usize = 0x080;
pub const REG_QUEUE_DESC_HIGH: usize = 0x084;
pub const REG_QUEUE_DRIVER_LOW: usize = 0x090;
pub const REG_QUEUE_DRIVER_HIGH: usize = 0x094;
pub const REG_QUEUE_DEVICE_LOW: usize = 0x0a0;
pub const REG_QUEUE_DEVICE_HIGH: usize = 0x0a4;
/// Конфиг-область устройства: ёмкость у блока, MAC у сети.
pub const REG_CONFIG: usize = 0x100;

// ── поля common_cfg virtio-pci modern (все LE) ───────────────────────────────

const PCI_DEVICE_FEATURE_SEL: usize = 0x00;
const PCI_DEVICE_FEATURE: usize = 0x04;
const PCI_DRIVER_FEATURE_SEL: usize = 0x08;
const PCI_DRIVER_FEATURE: usize = 0x0c;
const PCI_MSIX_CONFIG: usize = 0x10;
const PCI_DEVICE_STATUS: usize = 0x14;
const PCI_QUEUE_SEL: usize = 0x16;
const PCI_QUEUE_SIZE: usize = 0x18;
const PCI_QUEUE_MSIX_VECTOR: usize = 0x1a;
const PCI_QUEUE_ENABLE: usize = 0x1c;
const PCI_QUEUE_NOTIFY_OFF: usize = 0x1e;
const PCI_QUEUE_DESC: usize = 0x20;
const PCI_QUEUE_DRIVER: usize = 0x28;
const PCI_QUEUE_DEVICE: usize = 0x30;
/// «Вектора нет» в полях msix_vector.
const PCI_NO_VECTOR: u16 = 0xffff;

// ── биты статуса и дескриптора ───────────────────────────────────────────────

const STATUS_ACKNOWLEDGE: u32 = 1;
const STATUS_DRIVER: u32 = 2;
const STATUS_DRIVER_OK: u32 = 4;
const STATUS_FEATURES_OK: u32 = 8;

/// Есть следующий дескриптор в цепочке.
pub const DESC_F_NEXT: u16 = 1;
/// Устройство ПИШЕТ в этот буфер (для нас — чтение).
pub const DESC_F_WRITE: u16 = 2;

/// Бит 32 (старший dword, бит 0): `VIRTIO_F_VERSION_1` — обязателен для modern.
const FEATURE_HI_VERSION_1: u32 = 1;

// ── кольца ───────────────────────────────────────────────────────────────────

/// Дескриптор split virtqueue (16 байт).
#[repr(C)]
pub struct Desc {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

/// Запись кольца использованных: голова завершённой цепочки и сколько байт записано.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct UsedElem {
    pub id: u32,
    pub len: u32,
}

/// Заголовок кольца avail/used: `flags` и `idx`, дальше идёт САМО кольцо.
///
/// Заголовком, а не структурой с массивом `[u16; QSIZE]`, — и это не мелочь стиля. Массив
/// фиксированной длины в типе означает, что размер очереди известен на этапе сборки, а он
/// СОГЛАСОВЫВАЕТСЯ с устройством: `virtio_net` уже просит `QSIZE.min(dev_max)` и всё равно
/// индексирует структуру, объявленную на 64. Работает это по совпадению — кольцо начинается с
/// фиксированного смещения, — но тип при этом врёт, а `used_event`/`avail_event` в нём лежат не
/// там, где на самом деле.
#[repr(C)]
struct RingHdr {
    flags: u16,
    idx: u16,
}

/// Три кольца одной очереди — каждое в своём обнулённом фрейме.
///
/// Адреса, а не указатели: структура обязана быть `Send` и жить под `SpinLock`. RAM отображена
/// идентично, поэтому адрес фрейма он же и физический — то, что уезжает устройству.
pub struct Ring {
    desc: usize,
    avail: usize,
    used: usize,
    /// Согласованный с устройством размер очереди (число слотов).
    pub size: usize,
}

impl Ring {
    /// Занять три фрейма под кольца. `who` — только для сообщения о нехватке памяти.
    fn alloc(who: &str, size: usize) -> Ring {
        let f = |what: &str| match frame::alloc() {
            Some(p) => p,
            None => panic!("{who}: нет фрейма под {what} virtqueue"),
        };
        Ring { desc: f("desc"), avail: f("avail"), used: f("used"), size }
    }

    /// Записать дескриптор в слот.
    ///
    /// # Safety
    /// Слот не должен быть занят незавершённым запросом: устройство читает его асинхронно.
    pub unsafe fn set_desc(&self, slot: usize, addr: u64, len: u32, flags: u16, next: u16) {
        let d = (frame::ptr(self.desc) as *mut Desc).add(slot % self.size);
        write_volatile(&mut (*d).addr, addr);
        write_volatile(&mut (*d).len, len);
        write_volatile(&mut (*d).flags, flags);
        write_volatile(&mut (*d).next, next);
    }

    /// Положить голову цепочки в кольцо доступных (ещё не объявляя её устройству).
    ///
    /// # Safety
    /// Кольцо общее с устройством; порядок записей держится барьерами вызывающего.
    unsafe fn write_avail(&self, slot: usize, head: u16) {
        let ring = (frame::ptr(self.avail) as *mut u8).add(4) as *mut u16;
        write_volatile(ring.add(slot % self.size), head);
    }

    /// Объявить устройству новое значение `avail.idx` — с барьером до и после.
    ///
    /// # Safety
    /// Всё, на что этот индекс указывает, должно быть уже записано.
    unsafe fn publish(&self, idx: u16) {
        fence(Ordering::SeqCst);
        let hdr = frame::ptr(self.avail) as *mut RingHdr;
        write_volatile(&mut (*hdr).idx, idx);
        fence(Ordering::SeqCst);
    }

    /// Текущий `used.idx` устройства.
    ///
    /// # Safety
    /// Чтение общей с устройством памяти.
    unsafe fn used_idx(&self) -> u16 {
        read_volatile(&(*(frame::ptr(self.used) as *const RingHdr)).idx)
    }

    /// Запись кольца использованных.
    ///
    /// # Safety
    /// Осмысленна только для слотов, о которых устройство уже отчиталось.
    unsafe fn used(&self, slot: usize) -> UsedElem {
        let ring = (frame::ptr(self.used) as *const u8).add(4) as *const UsedElem;
        read_volatile(ring.add(slot % self.size))
    }
}

// ── очередь: кольца плюс способ дёрнуть устройство ───────────────────────────

/// Как разбудить очередь: у mmio общий регистр с номером очереди, у pci — свой адрес.
enum Kick {
    Mmio { base: usize, q: u16 },
    Pci { addr: usize },
}

/// Настроенная очередь: кольца, способ разбудить устройство и ТЕНЕВЫЕ ИНДЕКСЫ — сколько мы
/// опубликовали и сколько завершений разобрали.
///
/// Индексы здесь, а не у драйверов, ровно потому, что все трое вели их одинаково и по-разному
/// ошибались в мелочах: один читал `avail.idx` из общей памяти вместо своего счётчика, другой
/// заворачивал слот по вместимости кольца вместо СОГЛАСОВАННОГО размера.
pub struct Queue {
    ring: Ring,
    kick: Kick,
    avail_idx: u16,
    used_idx: u16,
}

impl Queue {
    /// «Очередь готова, забирай».
    pub fn kick(&self) {
        unsafe {
            match self.kick {
                Kick::Mmio { base, q } => w32(base, REG_QUEUE_NOTIFY, q as u32),
                Kick::Pci { addr } => write_volatile(addr as *mut u16, 0u16),
            }
        }
    }

    /// Размер очереди, согласованный с устройством.
    pub fn size(&self) -> usize {
        self.ring.size
    }

    /// Слот, в который ляжет следующая публикация. Нужен тем, кто крутит дескрипторы по кругу.
    pub fn slot(&self) -> usize {
        self.avail_idx as usize % self.ring.size
    }

    /// Записать дескриптор.
    ///
    /// # Safety
    /// Слот не должен быть занят незавершённым запросом.
    pub unsafe fn set_desc(&self, slot: usize, addr: u64, len: u32, flags: u16, next: u16) {
        self.ring.set_desc(slot, addr, len, flags, next);
    }

    /// Опубликовать голову цепочки: положить её в кольцо доступных и объявить устройству.
    /// Разбудить его — отдельно ([`Queue::kick`]): цепочек можно опубликовать несколько, а
    /// дёрнуть один раз.
    ///
    /// # Safety
    /// Дескрипторы цепочки должны быть уже записаны.
    pub unsafe fn offer(&mut self, head: u16) {
        self.ring.write_avail(self.avail_idx as usize, head);
        self.avail_idx = self.avail_idx.wrapping_add(1);
        self.ring.publish(self.avail_idx);
    }

    /// Есть ли необработанные завершения.
    ///
    /// # Safety
    /// Чтение общей с устройством памяти.
    pub unsafe fn has_used(&self) -> bool {
        self.ring.used_idx() != self.used_idx
    }

    /// Снять следующее завершение: голова цепочки и сколько байт записало устройство.
    ///
    /// # Safety
    /// Звать только когда [`Queue::has_used`] истинно.
    pub unsafe fn take_used(&mut self) -> UsedElem {
        let e = self.ring.used(self.used_idx as usize);
        fence(Ordering::SeqCst);
        self.used_idx = self.used_idx.wrapping_add(1);
        e
    }
}

// ── рукопожатие ──────────────────────────────────────────────────────────────

/// Конфигурационная сторона устройства: то, чем с ним разговаривают ДО того, как заработала
/// очередь. Одна машина состояний на оба транспорта — различаются только ширина регистров и
/// адреса.
#[derive(Clone, Copy)]
pub struct Cfg {
    t: BlkTransport,
}

impl Cfg {
    pub fn new(t: BlkTransport) -> Cfg {
        Cfg { t }
    }

    /// Сброс, `ACKNOWLEDGE`, `DRIVER` — начало разговора.
    pub fn begin(&self) {
        self.set_status(0);
        self.set_status(STATUS_ACKNOWLEDGE);
        self.set_status(STATUS_ACKNOWLEDGE | STATUS_DRIVER);
    }

    /// Какие фичи ПРЕДЛАГАЕТ устройство (младший dword). Нужно сети: MAC берут только тогда,
    /// когда его дают, — иначе конфиг-область не прочтётся.
    pub fn device_features_lo(&self) -> u32 {
        match self.t {
            BlkTransport::Mmio { .. } => u32::MAX, // у mmio читать нечем — берём как есть
            BlkTransport::Pci { common, .. } => unsafe {
                write_volatile((common + PCI_DEVICE_FEATURE_SEL) as *mut u32, 0);
                read_volatile((common + PCI_DEVICE_FEATURE) as *const u32)
            },
        }
    }

    /// Согласовать фичи: `lo` — свои (младший dword), старший всегда `VIRTIO_F_VERSION_1`.
    /// `false` — устройство наш набор не приняло, дальше идти нельзя.
    pub fn accept_features(&self, lo: u32) -> bool {
        unsafe {
            match self.t {
                BlkTransport::Mmio { base } => {
                    w32(base, REG_DRIVER_FEATURES_SEL, 0);
                    w32(base, REG_DRIVER_FEATURES, lo);
                    w32(base, REG_DRIVER_FEATURES_SEL, 1);
                    w32(base, REG_DRIVER_FEATURES, FEATURE_HI_VERSION_1);
                }
                BlkTransport::Pci { common, .. } => {
                    let w32p = |off: usize, v: u32| write_volatile((common + off) as *mut u32, v);
                    w32p(PCI_DRIVER_FEATURE_SEL, 0);
                    w32p(PCI_DRIVER_FEATURE, lo);
                    w32p(PCI_DRIVER_FEATURE_SEL, 1);
                    w32p(PCI_DRIVER_FEATURE, FEATURE_HI_VERSION_1);
                }
            }
        }
        self.set_status(STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK);
        self.status() & STATUS_FEATURES_OK != 0
    }

    /// Настроить очередь `q`: кольца, размер (не больше `want` и не больше разрешённого
    /// устройством), MSI-X-вектор. `None` — очереди нет либо вектор не принят.
    ///
    /// `msix`: `None` — прерывания этой очереди не нужны; `Some(v)` — привязать вектор `v` и
    /// **проверить, что устройство его принял**. Проверка обязательна, и это тот самый шаг,
    /// которого не хватало сети целую веху: не принятый вектор означает, что прерываний не будет
    /// вовсе, а узнать об этом иначе неоткуда — устройство просто молчит.
    ///
    /// У mmio MSI-X нет вовсе: там прерывание одно на устройство, и назначает его арх.
    pub fn queue(&self, who: &str, q: u16, want: usize, msix: Option<u16>) -> Option<Queue> {
        unsafe {
            match self.t {
                BlkTransport::Mmio { base } => {
                    w32(base, REG_QUEUE_SEL, q as u32);
                    let dev_max = r32(base, REG_QUEUE_NUM_MAX) as usize;
                    if dev_max == 0 {
                        return None;
                    }
                    let size = want.min(dev_max);
                    w32(base, REG_QUEUE_NUM, size as u32);
                    let ring = Ring::alloc(who, size);
                    write_addr(base, REG_QUEUE_DESC_LOW, REG_QUEUE_DESC_HIGH, ring.desc);
                    write_addr(base, REG_QUEUE_DRIVER_LOW, REG_QUEUE_DRIVER_HIGH, ring.avail);
                    write_addr(base, REG_QUEUE_DEVICE_LOW, REG_QUEUE_DEVICE_HIGH, ring.used);
                    w32(base, REG_QUEUE_READY, 1);
                    Some(Queue { ring, kick: Kick::Mmio { base, q }, avail_idx: 0, used_idx: 0 })
                }
                BlkTransport::Pci { common, notify_base, notify_mult, .. } => {
                    let r16p = |off: usize| read_volatile((common + off) as *const u16);
                    let w16p = |off: usize, v: u16| write_volatile((common + off) as *mut u16, v);
                    let w64p = |off: usize, v: u64| write_volatile((common + off) as *mut u64, v);

                    w16p(PCI_QUEUE_SEL, q);
                    // Прочитанное здесь — МАКСИМУМ устройства (0 значит «очереди нет»).
                    let dev_max = r16p(PCI_QUEUE_SIZE) as usize;
                    if dev_max == 0 {
                        return None;
                    }
                    let size = want.min(dev_max);
                    w16p(PCI_QUEUE_SIZE, size as u16);
                    w16p(PCI_QUEUE_MSIX_VECTOR, msix.unwrap_or(PCI_NO_VECTOR));
                    if let Some(v) = msix {
                        if r16p(PCI_QUEUE_MSIX_VECTOR) != v {
                            return None;
                        }
                    }
                    let ring = Ring::alloc(who, size);
                    w64p(PCI_QUEUE_DESC, ring.desc as u64);
                    w64p(PCI_QUEUE_DRIVER, ring.avail as u64);
                    w64p(PCI_QUEUE_DEVICE, ring.used as u64);
                    let addr =
                        notify_base + r16p(PCI_QUEUE_NOTIFY_OFF) as usize * notify_mult as usize;
                    w16p(PCI_QUEUE_ENABLE, 1);
                    Some(Queue { ring, kick: Kick::Pci { addr }, avail_idx: 0, used_idx: 0 })
                }
            }
        }
    }

    /// Конфиг-события устройства нам не нужны ни в одном драйвере (только pci).
    pub fn no_config_msix(&self) {
        if let BlkTransport::Pci { common, .. } = self.t {
            unsafe { write_volatile((common + PCI_MSIX_CONFIG) as *mut u16, PCI_NO_VECTOR) };
        }
    }

    /// `DRIVER_OK` — устройство в работе.
    pub fn ready(&self) {
        self.set_status(
            STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK,
        );
    }

    /// Адрес конфиг-области устройства: ёмкость у блока, MAC у сети.
    pub fn config(&self) -> usize {
        match self.t {
            BlkTransport::Mmio { base } => base + REG_CONFIG,
            BlkTransport::Pci { device, .. } => device,
        }
    }

    fn status(&self) -> u32 {
        unsafe {
            match self.t {
                BlkTransport::Mmio { base } => r32(base, REG_STATUS),
                BlkTransport::Pci { common, .. } => {
                    read_volatile((common + PCI_DEVICE_STATUS) as *const u8) as u32
                }
            }
        }
    }

    fn set_status(&self, v: u32) {
        unsafe {
            match self.t {
                BlkTransport::Mmio { base } => w32(base, REG_STATUS, v),
                BlkTransport::Pci { common, .. } => {
                    write_volatile((common + PCI_DEVICE_STATUS) as *mut u8, v as u8)
                }
            }
        }
    }
}

// ── доступ к регистрам mmio ──────────────────────────────────────────────────

/// # Safety
/// `base + off` должен быть отображённым регистром устройства.
#[inline]
pub unsafe fn r32(base: usize, off: usize) -> u32 {
    read_volatile((base + off) as *const u32)
}

/// # Safety
/// `base + off` должен быть отображённым регистром устройства.
#[inline]
pub unsafe fn w32(base: usize, off: usize, val: u32) {
    write_volatile((base + off) as *mut u32, val);
}

/// Записать 64-битный адрес в пару регистров low/high.
///
/// # Safety
/// Оба смещения должны быть отображёнными регистрами устройства.
#[inline]
pub unsafe fn write_addr(base: usize, lo: usize, hi: usize, addr: usize) {
    w32(base, lo, addr as u32);
    w32(base, hi, (addr as u64 >> 32) as u32);
}
