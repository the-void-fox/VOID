//! virtio-rng — аппаратный источник случайности от гипервизора (долг Вехи 86, закрыт перед 95).
//!
//! **Зачем именно сейчас.** До этого драйвера `arch::hw_random_u64()` на riscv возвращал
//! буквально `None`: у QEMU virt нет ни `Zkr`, ни другого источника, поэтому вся случайность
//! шла из пула джиттера прерываний ([`crate::random`]). На свежей загрузке, до накопления
//! событий, такая энтропия предсказуема настолько, насколько предсказуемо время загрузки — а
//! для TLS (Веха 95) это означает предсказуемые ключи, то есть TLS понарошку. На x86 роль
//! источника играл `RDRAND`, и там всё было честно; virtio-rng выравнивает арх.
//!
//! **Устройство простейшее из всех virtio:** одна очередь, никакого протокола. Драйвер кладёт в
//! неё ПИШУЩИЙ буфер, дёргает notify — устройство заполняет его энтропией и отдаёт в used с
//! фактической длиной (может быть меньше запрошенного, это нормально). Прерывания не нужны:
//! запросы редкие и синхронные, ждём в used-кольце.
//!
//! Транспорт — тот же split-virtqueue, что у блока и сети (см. [[virtio-blk]]); отличий два:
//! очередь одна и дескриптор всегда с `DESC_F_WRITE`.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

use alloc::boxed::Box;

use crate::arch;
use crate::frame;
use crate::sync::SpinLock;

/// Длина кольца. Нам хватает одного дескриптора за раз, но кольцо меньше 2 некрасиво и
/// упирается в требования выравнивания — берём 4.
const QSIZE: usize = 4;
/// Сколько байт просим у устройства за один поход. 64 байта = восемь выдач `u64` из одного
/// обращения: устройство трогаем редко, а свежесть сохраняем.
const BUF: usize = 64;

// Регистры virtio-mmio (те же, что у blk/net — раскладка общая для любого virtio).
const REG_DRIVER_FEATURES: usize = 0x020;
const REG_DRIVER_FEATURES_SEL: usize = 0x024;
const REG_QUEUE_SEL: usize = 0x030;
const REG_QUEUE_NUM_MAX: usize = 0x034;
const REG_QUEUE_NUM: usize = 0x038;
const REG_QUEUE_READY: usize = 0x044;
const REG_QUEUE_NOTIFY: usize = 0x050;
const REG_STATUS: usize = 0x070;
const REG_QUEUE_DESC_LOW: usize = 0x080;
const REG_QUEUE_DESC_HIGH: usize = 0x084;
const REG_QUEUE_DRIVER_LOW: usize = 0x090;
const REG_QUEUE_DRIVER_HIGH: usize = 0x094;
const REG_QUEUE_DEVICE_LOW: usize = 0x0a0;
const REG_QUEUE_DEVICE_HIGH: usize = 0x0a4;

// Регистры common-структуры virtio-pci modern.
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
const PCI_NO_VECTOR: u16 = 0xffff;

const STATUS_ACKNOWLEDGE: u32 = 1;
const STATUS_DRIVER: u32 = 2;
const STATUS_DRIVER_OK: u32 = 4;
const STATUS_FEATURES_OK: u32 = 8;

const FEATURE_HI_VERSION_1: u32 = 1;
const DESC_F_WRITE: u16 = 2;

#[allow(dead_code)]
#[repr(C)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[allow(dead_code)]
#[repr(C)]
struct Avail {
    flags: u16,
    idx: u16,
    ring: [u16; QSIZE],
    used_event: u16,
}

#[allow(dead_code)]
#[repr(C)]
struct UsedElem {
    id: u32,
    len: u32,
}

#[allow(dead_code)]
#[repr(C)]
struct Used {
    flags: u16,
    idx: u16,
    ring: [UsedElem; QSIZE],
    avail_event: u16,
}

/// Как дёрнуть очередь: на mmio — общий регистр с номером, на PCI — свой адрес.
enum Notify {
    Mmio { base: usize },
    Pci { addr: usize },
}

struct VirtioRng {
    notify: Notify,
    desc: usize,
    avail: usize,
    used: usize,
    avail_idx: u16,
    used_idx: u16,
    buf: Box<[u8; BUF]>,
    /// Кэш выданного устройством: `[pos, len)` ещё не отдано наружу.
    cache: [u8; BUF],
    pos: usize,
    len: usize,
}

impl VirtioRng {
    /// Сходить к устройству за свежей порцией. `false` — устройство не отдало ничего.
    fn refill(&mut self) -> bool {
        unsafe {
            let desc = frame::ptr(self.desc) as *mut Desc;
            // Веха 87: буфер в куче ядра — устройству отдаём ФИЗИЧЕСКИЙ адрес.
            let pa = arch::virt_to_phys(self.buf.as_ptr() as usize) as u64;
            let slot = (self.avail_idx as usize) % QSIZE;
            let d = desc.add(slot);
            write_volatile(&mut (*d).addr, pa);
            write_volatile(&mut (*d).len, BUF as u32);
            write_volatile(&mut (*d).flags, DESC_F_WRITE);
            write_volatile(&mut (*d).next, 0);
            fence(Ordering::SeqCst);

            let avail = frame::ptr(self.avail) as *mut Avail;
            write_volatile(&mut (*avail).ring[slot], slot as u16);
            fence(Ordering::SeqCst);
            self.avail_idx = self.avail_idx.wrapping_add(1);
            write_volatile(&mut (*avail).idx, self.avail_idx);
            fence(Ordering::SeqCst);

            match &self.notify {
                Notify::Mmio { base } => w32(*base, REG_QUEUE_NOTIFY, 0),
                Notify::Pci { addr } => write_volatile(*addr as *mut u16, 0u16),
            }

            // Ждём завершения. Потолок оборотов — чтобы молчащее устройство не подвесило ядро:
            // энтропия важна, но не ценой зависшей загрузки (пул джиттера останется запасным).
            let used = frame::ptr(self.used) as *const Used;
            let mut spins = 0u32;
            while read_volatile(&(*used).idx) == self.used_idx {
                spins += 1;
                if spins > 10_000_000 {
                    return false;
                }
                core::hint::spin_loop();
            }
            let e = &(*used).ring[(self.used_idx as usize) % QSIZE];
            let n = (read_volatile(&e.len) as usize).min(BUF);
            fence(Ordering::SeqCst);
            self.used_idx = self.used_idx.wrapping_add(1);
            if n == 0 {
                return false;
            }
            self.cache[..n].copy_from_slice(&self.buf[..n]);
            self.pos = 0;
            self.len = n;
            true
        }
    }

    /// Выдать 8 байт энтропии. `None` — устройство молчит.
    fn next_u64(&mut self) -> Option<u64> {
        if self.len - self.pos < 8 && !self.refill() {
            return None;
        }
        if self.len - self.pos < 8 {
            return None;
        }
        let mut w = [0u8; 8];
        w.copy_from_slice(&self.cache[self.pos..self.pos + 8]);
        self.pos += 8;
        Some(u64::from_le_bytes(w))
    }
}

static RNG: SpinLock<Option<VirtioRng>> = SpinLock::new(None);

/// Инициализировать первое найденное virtio-rng. `false` — устройства нет.
pub fn init() -> bool {
    let Some(transport) = arch::probe_virtio_rng() else {
        return false;
    };
    let dev = match transport {
        arch::BlkTransport::Mmio { base } => init_mmio(base),
        arch::BlkTransport::Pci { common, notify_base, notify_mult, .. } => {
            init_pci(common, notify_base, notify_mult)
        }
    };
    let Some(mut dev) = dev else { return false };
    // Первая порция прямо здесь: заодно проверяем, что устройство отвечает, а не только
    // отрапортовало о готовности.
    if !dev.refill() {
        return false;
    }
    *RNG.lock() = Some(dev);
    true
}

/// Есть ли рабочий источник.
pub fn present() -> bool {
    RNG.lock().is_some()
}

/// Слово энтропии от устройства. `None` — устройства нет или оно молчит.
///
/// Замок здесь `lock_irq`: [`crate::random::fill`] зовётся из syscall-контекста, но пул
/// подмешивается из обработчиков прерываний — брать обычный замок значило бы позволить
/// прерыванию встрять посреди работы с кольцом.
pub fn next_u64() -> Option<u64> {
    RNG.lock_irq().as_mut()?.next_u64()
}

fn alloc_rings() -> (usize, usize, usize) {
    (
        frame::alloc().expect("virtio-rng desc"),
        frame::alloc().expect("virtio-rng avail"),
        frame::alloc().expect("virtio-rng used"),
    )
}

fn init_mmio(base: usize) -> Option<VirtioRng> {
    unsafe {
        w32(base, REG_STATUS, 0);
        let mut status = STATUS_ACKNOWLEDGE;
        w32(base, REG_STATUS, status);
        status |= STATUS_DRIVER;
        w32(base, REG_STATUS, status);

        // Своих feature-битов у virtio-rng нет — берём только VIRTIO_F_VERSION_1.
        w32(base, REG_DRIVER_FEATURES_SEL, 0);
        w32(base, REG_DRIVER_FEATURES, 0);
        w32(base, REG_DRIVER_FEATURES_SEL, 1);
        w32(base, REG_DRIVER_FEATURES, FEATURE_HI_VERSION_1);
        status |= STATUS_FEATURES_OK;
        w32(base, REG_STATUS, status);
        if r32(base, REG_STATUS) & STATUS_FEATURES_OK == 0 {
            return None;
        }

        w32(base, REG_QUEUE_SEL, 0);
        if r32(base, REG_QUEUE_NUM_MAX) == 0 {
            return None;
        }
        w32(base, REG_QUEUE_NUM, QSIZE as u32);
        let (desc, avail, used) = alloc_rings();
        write_addr(base, REG_QUEUE_DESC_LOW, REG_QUEUE_DESC_HIGH, desc);
        write_addr(base, REG_QUEUE_DRIVER_LOW, REG_QUEUE_DRIVER_HIGH, avail);
        write_addr(base, REG_QUEUE_DEVICE_LOW, REG_QUEUE_DEVICE_HIGH, used);
        w32(base, REG_QUEUE_READY, 1);

        status |= STATUS_DRIVER_OK;
        w32(base, REG_STATUS, status);

        Some(new_dev(Notify::Mmio { base }, desc, avail, used))
    }
}

fn init_pci(common: usize, notify_base: usize, notify_mult: u32) -> Option<VirtioRng> {
    let r8p = |off: usize| unsafe { read_volatile((common + off) as *const u8) };
    let w8p = |off: usize, v: u8| unsafe { write_volatile((common + off) as *mut u8, v) };
    let r16p = |off: usize| unsafe { read_volatile((common + off) as *const u16) };
    let w16p = |off: usize, v: u16| unsafe { write_volatile((common + off) as *mut u16, v) };
    let w32p = |off: usize, v: u32| unsafe { write_volatile((common + off) as *mut u32, v) };
    let w64p = |off: usize, v: u64| unsafe { write_volatile((common + off) as *mut u64, v) };

    w8p(PCI_DEVICE_STATUS, 0);
    let mut status = STATUS_ACKNOWLEDGE as u8;
    w8p(PCI_DEVICE_STATUS, status);
    status |= STATUS_DRIVER as u8;
    w8p(PCI_DEVICE_STATUS, status);

    w32p(PCI_DRIVER_FEATURE_SEL, 0);
    w32p(PCI_DRIVER_FEATURE, 0);
    w32p(PCI_DRIVER_FEATURE_SEL, 1);
    w32p(PCI_DRIVER_FEATURE, FEATURE_HI_VERSION_1);
    status |= STATUS_FEATURES_OK as u8;
    w8p(PCI_DEVICE_STATUS, status);
    if r8p(PCI_DEVICE_STATUS) & STATUS_FEATURES_OK as u8 == 0 {
        return None;
    }
    w16p(PCI_MSIX_CONFIG, PCI_NO_VECTOR);

    w16p(PCI_QUEUE_SEL, 0);
    if r16p(PCI_QUEUE_SIZE) == 0 {
        return None;
    }
    w16p(PCI_QUEUE_SIZE, QSIZE as u16);
    w16p(PCI_QUEUE_MSIX_VECTOR, PCI_NO_VECTOR); // прерывания не нужны — ждём в кольце
    let (desc, avail, used) = alloc_rings();
    w64p(PCI_QUEUE_DESC, desc as u64);
    w64p(PCI_QUEUE_DRIVER, avail as u64);
    w64p(PCI_QUEUE_DEVICE, used as u64);
    let notify = notify_base + r16p(PCI_QUEUE_NOTIFY_OFF) as usize * notify_mult as usize;
    w16p(PCI_QUEUE_ENABLE, 1);

    status |= STATUS_DRIVER_OK as u8;
    w8p(PCI_DEVICE_STATUS, status);

    Some(new_dev(Notify::Pci { addr: notify }, desc, avail, used))
}

fn new_dev(notify: Notify, desc: usize, avail: usize, used: usize) -> VirtioRng {
    VirtioRng {
        notify,
        desc,
        avail,
        used,
        avail_idx: 0,
        used_idx: 0,
        buf: Box::new([0u8; BUF]),
        cache: [0u8; BUF],
        pos: 0,
        len: 0,
    }
}

#[inline]
unsafe fn r32(base: usize, off: usize) -> u32 {
    read_volatile((base + off) as *const u32)
}

#[inline]
unsafe fn w32(base: usize, off: usize, val: u32) {
    write_volatile((base + off) as *mut u32, val);
}

#[inline]
unsafe fn write_addr(base: usize, lo: usize, hi: usize, addr: usize) {
    w32(base, lo, addr as u32);
    w32(base, hi, (addr as u64 >> 32) as u32);
}
