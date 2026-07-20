//! PCI для QEMU q35 (Веха 27): поиск virtio-blk и его MSI-X — ровно столько PCI,
//! сколько нужно диску.
//!
//! Конфиг-пространство — через классические порты 0xCF8/0xCFC (шина 0, функция 0:
//! `-device virtio-blk-pci` на q35 садится именно туда). BAR'ы уже назначены SeaBIOS —
//! мы их только читаем и отображаем ([`paging::map_mmio`]). Адреса структур virtio
//! (common/notify/isr/device) приходят из **vendor-capabilities** (id 0x09) — это
//! спецификация virtio-pci modern.
//!
//! Прерывание — **MSI-X** (cap 0x11): запись 0 таблицы указывает прямо в LAPIC
//! (адрес 0xFEE0_0000, данные = вектор [`trap::VEC_BLK`]). Ни IOAPIC-маршрутизации,
//! ни PIRQ-свопов INTx — сообщение приходит вектором, как и положено на PCIe.

use crate::arch::{BlkDevice, BlkTransport, NetDevice};

use super::{paging, trap};

const CFG_ADDR: u16 = 0xcf8;
const CFG_DATA: u16 = 0xcfc;

const VENDOR_VIRTIO: u16 = 0x1af4;
/// Device id: 0x1042 — modern-only virtio-blk (`disable-legacy=on` в раннере),
/// 0x1041+1 transitional (0x1001) тоже несёт modern-capabilities — принимаем оба.
const DEV_BLK_MODERN: u16 = 0x1042;
const DEV_BLK_TRANSITIONAL: u16 = 0x1001;
/// virtio-net: 0x1041 modern, 0x1000 transitional (Веха 34).
const DEV_NET_MODERN: u16 = 0x1041;
const DEV_NET_TRANSITIONAL: u16 = 0x1000;

#[inline]
fn outl(port: u16, v: u32) {
    unsafe { core::arch::asm!("out dx, eax", in("dx") port, in("eax") v, options(nomem, nostack)) }
}

#[inline]
fn inl(port: u16) -> u32 {
    let v: u32;
    unsafe { core::arch::asm!("in eax, dx", in("dx") port, out("eax") v, options(nomem, nostack)) }
    v
}

/// Адрес конфиг-регистра `off` устройства `dev` (шина 0, функция 0) для порта 0xCF8.
fn cfg_addr(dev: u32, off: u32) -> u32 {
    0x8000_0000 | dev << 11 | (off & 0xfc)
}

fn cfg_r32(dev: u32, off: u32) -> u32 {
    outl(CFG_ADDR, cfg_addr(dev, off));
    inl(CFG_DATA)
}

fn cfg_w32(dev: u32, off: u32, v: u32) {
    outl(CFG_ADDR, cfg_addr(dev, off));
    outl(CFG_DATA, v);
}

fn cfg_r16(dev: u32, off: u32) -> u16 {
    (cfg_r32(dev, off & !3) >> ((off & 3) * 8)) as u16
}

fn cfg_r8(dev: u32, off: u32) -> u8 {
    (cfg_r32(dev, off & !3) >> ((off & 3) * 8)) as u8
}

/// Записать 16 бит через чтение-модификацию dword'а (порт 0xCFC — 32-битный).
fn cfg_w16(dev: u32, off: u32, v: u16) {
    let (a, sh) = (off & !3, (off & 3) * 8);
    let old = cfg_r32(dev, a);
    cfg_w32(dev, a, (old & !(0xffff << sh)) | (v as u32) << sh);
}

/// Прочитать адрес memory-BAR `idx` (64-битные — из пары регистров). 0 — не назначен/IO.
fn bar_addr(dev: u32, idx: u8) -> usize {
    let lo = cfg_r32(dev, 0x10 + 4 * idx as u32);
    if lo & 1 != 0 {
        return 0; // I/O BAR — не используем
    }
    let mut addr = (lo & !0xf) as u64;
    if lo & 0x4 != 0 {
        addr |= (cfg_r32(dev, 0x14 + 4 * idx as u32) as u64) << 32;
    }
    addr as usize
}

/// Найти virtio-blk на шине 0 и подготовить его: BAR-окна отображены, MSI-X взведён.
pub fn probe_virtio_blk() -> Option<BlkDevice> {
    for dev in 0..32u32 {
        let id = cfg_r32(dev, 0);
        let (vendor, device) = (id as u16, (id >> 16) as u16);
        if vendor == VENDOR_VIRTIO && (device == DEV_BLK_MODERN || device == DEV_BLK_TRANSITIONAL) {
            return setup(dev);
        }
    }
    None
}

/// Найти virtio-net на шине 0 (Веха 34) и подготовить транспорт БЕЗ MSI-X: сеть
/// работает опросом колец, прерывание не программируется (отложено до потребности).
pub fn probe_virtio_net() -> Option<NetDevice> {
    for dev in 0..32u32 {
        let id = cfg_r32(dev, 0);
        let (vendor, device) = (id as u16, (id >> 16) as u16);
        if vendor == VENDOR_VIRTIO && (device == DEV_NET_MODERN || device == DEV_NET_TRANSITIONAL) {
            return setup_transport(dev).map(|t| NetDevice { transport: t });
        }
    }
    None
}

/// Пройти vendor-capabilities virtio, отобразить BAR-окна структур, вернуть транспорт.
/// Общее для blk и net: разговор по virtqueue одинаков, отличается лишь MSI-X (у сети нет).
fn setup_transport(dev: u32) -> Option<BlkTransport> {
    // Command: memory space + bus master (DMA колец). Верхняя половина dword'а —
    // status (биты RW1C); запись прочитанного их сбрасывает — безвредно.
    cfg_w16(dev, 0x04, cfg_r16(dev, 0x04) | 0x6);

    let (mut common, mut notify_base, mut notify_mult) = (0usize, 0usize, 0u32);
    let (mut isr, mut device_cfg) = (0usize, 0usize);

    // Пройти список capabilities (status.bit4 у virtio-устройств QEMU всегда есть).
    let mut ptr = cfg_r8(dev, 0x34) as u32 & !3;
    while ptr != 0 {
        // Vendor-capability virtio: тип структуры + [BAR, смещение, длина].
        if cfg_r8(dev, ptr) == 0x09 {
            let cfg_type = cfg_r8(dev, ptr + 3);
            let bar = cfg_r8(dev, ptr + 4);
            let off = cfg_r32(dev, ptr + 8) as usize;
            let len = cfg_r32(dev, ptr + 12) as usize;
            let base = bar_addr(dev, bar);
            if base != 0 && len != 0 {
                let addr = base + off;
                unsafe { paging::map_mmio(addr, len) };
                match cfg_type {
                    1 => common = addr,
                    2 => {
                        notify_base = addr;
                        notify_mult = cfg_r32(dev, ptr + 16);
                    }
                    3 => isr = addr,
                    4 => device_cfg = addr,
                    _ => {} // 5 = pci-cfg-доступ, не нужен: BAR'ы отображаемы
                }
            }
        }
        ptr = cfg_r8(dev, ptr + 1) as u32 & !3;
    }

    if common == 0 || notify_base == 0 || isr == 0 || device_cfg == 0 {
        return None; // не modern virtio
    }
    Some(BlkTransport::Pci { common, notify_base, notify_mult, isr, device: device_cfg })
}

/// Включить virtio-blk и взвести его MSI-X (диску прерывание нужно — async I/O).
fn setup(dev: u32) -> Option<BlkDevice> {
    let transport = setup_transport(dev)?;

    // Найти MSI-X capability и адрес его таблицы.
    let (mut msix_ptr, mut msix_table) = (0u32, 0usize);
    let mut ptr = cfg_r8(dev, 0x34) as u32 & !3;
    while ptr != 0 {
        if cfg_r8(dev, ptr) == 0x11 {
            msix_ptr = ptr;
            let t = cfg_r32(dev, ptr + 4);
            msix_table = bar_addr(dev, (t & 7) as u8) + (t & !7) as usize;
        }
        ptr = cfg_r8(dev, ptr + 1) as u32 & !3;
    }
    if msix_ptr == 0 {
        return None; // без MSI-X диск не поддерживаем
    }

    // MSI-X: запись 0 таблицы → LAPIC (физический dest, APIC ID 0), вектор диска,
    // без маски; затем включить MSI-X на функции (enable=1, function mask=0).
    unsafe {
        paging::map_mmio(msix_table, 16);
        let e = msix_table as *mut u32;
        e.add(0).write_volatile(0xfee0_0000); // message address (LAPIC, dest id 0)
        e.add(1).write_volatile(0);
        e.add(2).write_volatile(trap::VEC_BLK as u32); // data: fixed, edge, вектор
        e.add(3).write_volatile(0); // vector control: размаскирован
    }
    let ctrl = cfg_r16(dev, msix_ptr + 2);
    cfg_w16(dev, msix_ptr + 2, (ctrl | 0x8000) & !0x4000);

    Some(BlkDevice { transport, irq: trap::VEC_BLK as u32 })
}
