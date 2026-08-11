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

use core::ptr::read_volatile;

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

/// virtio-rng: источник энтропии. Modern id = 0x1040 + 4, legacy — 0x1005.
const DEV_RNG_MODERN: u16 = 0x1044;
const DEV_RNG_TRANSITIONAL: u16 = 0x1005;

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

// ─── опись шины (Веха 130) ───────────────────────────────────────────────────
//
// Зачем отдельно от probe-функций. Все они ищут ЗНАКОМОЕ устройство и молчат, если не нашли, —
// на QEMU этого хватало, потому что состав машины известен заранее. На настоящем ноутбуке
// вопрос обратный: что там вообще стоит? Без ответа драйвер писать не по чему, а угадывать по
// модели ноутбука нельзя — одна и та же модель приезжает с разными картами.
//
// Опись идёт в ЖУРНАЛ ЯДРА, а не в serial: у ноутбука COM-порта нет, и `bin/klog` — единственный
// способ прочитать сказанное ядром (ровно для этого журнал и заводился, Веха 116).

/// Адрес конфиг-регистра с УЧЁТОМ ШИНЫ. Обе прежние версии (`cfg_addr`, `cfg_addr_f`) знают
/// только шину 0 — на q35 этого достаточно, а на живой машине почти всё интересное сидит за
/// мостами PCIe, каждый со своей шиной.
fn cfg_addr_bdf(bus: u32, slot: u32, off: u32) -> u32 {
    0x8000_0000 | bus << 16 | slot << 8 | (off & 0xfc)
}

fn cfg_r32b(bus: u32, slot: u32, off: u32) -> u32 {
    outl(CFG_ADDR, cfg_addr_bdf(bus, slot, off));
    inl(CFG_DATA)
}

fn cfg_r8b(bus: u32, slot: u32, off: u32) -> u8 {
    (cfg_r32b(bus, slot, off & !3) >> ((off & 3) * 8)) as u8
}

/// Имя класса устройства по (class, subclass) — только то, что различаешь глазами при
/// bring-up'е. Незнакомое печатается кодом: врать именем хуже, чем сказать «не знаю».
fn class_name(class: u8, sub: u8) -> &'static str {
    match (class, sub) {
        (0x01, 0x01) => "диск IDE",
        (0x01, 0x06) => "диск SATA/AHCI",
        (0x01, 0x08) => "диск NVMe",
        (0x01, _) => "накопитель",
        (0x02, 0x00) => "СЕТЬ Ethernet",
        (0x02, 0x80) => "СЕТЬ прочая",
        (0x02, _) => "СЕТЬ",
        (0x03, _) => "видео",
        (0x04, 0x01) => "звук",
        (0x04, 0x03) => "звук HD Audio",
        (0x04, _) => "мультимедиа",
        (0x06, 0x00) => "мост: хост",
        (0x06, 0x01) => "мост: ISA",
        (0x06, 0x04) => "мост: PCI-PCI",
        (0x06, _) => "мост",
        (0x0c, 0x03) => "USB",
        (0x0c, 0x05) => "SMBus",
        (0x0c, _) => "последовательная шина",
        (0x0d, 0x80) => "РАДИО (Wi-Fi?)",
        (0x0d, _) => "радио",
        (0x08, _) => "системное",
        (0x11, _) => "измерения",
        _ => "?",
    }
}

/// Имя производителя по vendor id. Список короткий и намеренно такой: он нужен, чтобы в описи
/// сразу было видно «а вот и сетевая карта чья», а не чтобы заменять базу pci.ids.
fn vendor_name(v: u16) -> &'static str {
    match v {
        0x8086 => "Intel",
        0x10ec => "Realtek",
        0x168c => "Atheros",
        0x14e4 => "Broadcom",
        0x1969 => "Attansic/Atheros",
        0x11ab => "Marvell",
        0x1022 | 0x1002 => "AMD/ATI",
        0x10de => "NVIDIA",
        0x17cb => "Qualcomm",
        0x1af4 | 0x1b36 => "Red Hat/virtio",
        0x1234 => "QEMU",
        0x15b7 => "WD/SanDisk",
        0x144d => "Samsung",
        _ => "",
    }
}

/// Напечатать ВСЁ, что отвечает на шине PCI: адрес, идентификаторы, класс, подсистему и первый
/// назначенный BAR. Это первое, что смотришь на незнакомой машине.
///
/// Обход перебором шин 0..=255 вместо спуска по мостам. Спуск точнее, но требует разбора
/// secondary/subordinate у каждого моста, а ошибка в нём выглядит как «устройства нет» — то есть
/// как раз то, что мы ищем. Перебор же не может ничего пропустить: несуществующая шина отвечает
/// одними единицами. Цена — восемь тысяч чтений портов на загрузку, единицы миллисекунд.
pub fn dump() {
    let mut count = 0usize;
    // Сетевые карты собираем отдельной строкой: на ноутбуке опись — это два десятка строк, а
    // вопрос сейчас ровно один. Пусть ответ будет виден сразу, а не выискивался глазами.
    let mut nets = [(0u16, 0u16, 0u8); 8];
    let mut nnet = 0usize;
    for bus in 0..=255u32 {
        for dev in 0..32u32 {
            // Функции 1..7 существуют только у многофункциональных устройств (бит 7 header type).
            let multi = cfg_r8b(bus, dev << 3, 0x0e) & 0x80 != 0;
            let funcs = if multi { 8 } else { 1 };
            for func in 0..funcs {
                let slot = dev << 3 | func;
                let id = cfg_r32b(bus, slot, 0);
                let (vendor, device) = (id as u16, (id >> 16) as u16);
                if vendor == 0xffff || vendor == 0 {
                    continue;
                }
                let cls = cfg_r32b(bus, slot, 0x08);
                let (class, sub, progif) = ((cls >> 24) as u8, (cls >> 16) as u8, (cls >> 8) as u8);
                let sub_id = cfg_r32b(bus, slot, 0x2c);
                let irq = cfg_r8b(bus, slot, 0x3c);
                // Первый ненулевой BAR: по нему видно, отдала ли прошивка устройству окно
                // памяти (без окна драйвер писать не по чему).
                let mut bar = 0u32;
                for i in 0..6u32 {
                    let b = cfg_r32b(bus, slot, 0x10 + 4 * i);
                    if b != 0 && b & 1 == 0 {
                        bar = b & !0xf;
                        break;
                    }
                }
                crate::println!(
                    "  [pci] {:02x}:{:02x}.{} {:04x}:{:04x} {:5} {:16} класс {:02x}{:02x}{:02x} подсист {:04x}:{:04x} bar {:#010x} irq {}",
                    bus, dev, func, vendor, device, vendor_name(vendor),
                    class_name(class, sub), class, sub, progif,
                    sub_id as u16, (sub_id >> 16) as u16, bar, irq,
                );
                count += 1;
                // Класс 02 — Ethernet и родня, 0d/80 — беспроводное.
                if (class == 0x02 || (class == 0x0d && sub == 0x80)) && nnet < nets.len() {
                    nets[nnet] = (vendor, device, sub);
                    nnet += 1;
                }
            }
        }
    }
    crate::println!("  [pci] устройств на шине: {}", count);
    if nnet == 0 {
        crate::println!("  [pci] СЕТЕВЫХ КАРТ НЕ НАЙДЕНО");
    } else {
        crate::print!("  [pci] СЕТЕВЫЕ КАРТЫ:");
        for &(v, d, s) in &nets[..nnet] {
            crate::print!(" {:04x}:{:04x} ({} подкласс {:02x})", v, d, vendor_name(v), s);
        }
        crate::println!();
    }
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
            // Веха 91: сети тоже нужен MSI-X. Не вышло взвести - карта остаётся на опросе.
            let transport = setup_transport(dev)?;
            let irq = setup_msix(dev, trap::VEC_NET).then_some(trap::VEC_NET as u32).unwrap_or(0);
            return Some(NetDevice { transport, irq });
        }
    }
    None
}

/// Найти virtio-rng на PCI. Прерывания ему не заводим: запросы энтропии редкие и синхронные,
/// драйвер ждёт завершения в used-кольце.
pub fn probe_virtio_rng() -> Option<BlkTransport> {
    for dev in 0..32u32 {
        let id = cfg_r32(dev, 0);
        let (vendor, device) = (id as u16, (id >> 16) as u16);
        if vendor == VENDOR_VIRTIO && (device == DEV_RNG_MODERN || device == DEV_RNG_TRANSITIONAL) {
            return setup_transport(dev);
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

// ─── AHCI (Веха 47) ──────────────────────────────────────────────────────────
// Конфиг-доступ С УЧЁТОМ ФУНКЦИИ: на Intel-PCH SATA-контроллер сидит на 00:1f.2 —
// функция 2, которую обычный virtio-скан (только функция 0) не видит. Индекс `slot`
// = dev<<3|func; адрес = enable | slot<<8 | off (равно dev<<11|func<<8 у железа PCI).
fn cfg_addr_f(slot: u32, off: u32) -> u32 {
    0x8000_0000 | slot << 8 | (off & 0xfc)
}
fn cfg_r32f(slot: u32, off: u32) -> u32 {
    outl(CFG_ADDR, cfg_addr_f(slot, off));
    inl(CFG_DATA)
}
fn cfg_r16f(slot: u32, off: u32) -> u16 {
    (cfg_r32f(slot, off & !3) >> ((off & 3) * 8)) as u16
}
fn cfg_r8f(slot: u32, off: u32) -> u8 {
    (cfg_r32f(slot, off & !3) >> ((off & 3) * 8)) as u8
}
fn cfg_w16f(slot: u32, off: u32, v: u16) {
    let (a, sh) = (off & !3, (off & 3) * 8);
    let old = cfg_r32f(slot, a);
    outl(CFG_ADDR, cfg_addr_f(slot, a));
    outl(CFG_DATA, (old & !(0xffff << sh)) | (v as u32) << sh);
}

/// Веха 47 — найти AHCI-контроллер (SATA) с ПОДКЛЮЧЁННЫМ диском. Скан шины 0, ВСЕ функции
/// (класс 01/06/01 = Mass Storage / SATA / AHCI). У кандидата включаем память+bus-master,
/// берём ABAR (BAR5), отображаем, включаем AHCI (GHC.AE) и ищем порт с устройством
/// (PxSSTS.DET==3). Возвращает `(ABAR, номер порта)`; None — AHCI с диском не нашли (тогда
/// драйвер откатится на virtio-blk). Много-контроллерный случай QEMU (встроенный ich9 без
/// диска на 1f.2 + добавленный с диском) разрулён проверкой наличия диска в самом порту.
pub fn probe_ahci() -> Option<(usize, u32)> {
    for dev in 0..32u32 {
        for func in 0..8u32 {
            let slot = dev << 3 | func;
            let id = cfg_r32f(slot, 0);
            if id == 0xffff_ffff {
                if func == 0 {
                    break; // нет функции 0 — устройства в слоте нет вовсе
                }
                continue;
            }
            let cc = cfg_r32f(slot, 0x08); // [31:24] class, [23:16] subclass, [15:8] prog-if
            if (cc >> 24) as u8 == 0x01 && (cc >> 16) as u8 == 0x06 && (cc >> 8) as u8 == 0x01 {
                if let Some(res) = setup_ahci(slot) {
                    return Some(res);
                }
            }
            // Одно-функциональное устройство (бит 7 header-type = 0) — функции 1..7 не сканируем.
            if func == 0 && cfg_r8f(slot, 0x0e) & 0x80 == 0 {
                break;
            }
        }
    }
    None
}

/// Включить контроллер AHCI на `slot`, отобразить ABAR, найти порт с диском.
fn setup_ahci(slot: u32) -> Option<(usize, u32)> {
    cfg_w16f(slot, 0x04, cfg_r16f(slot, 0x04) | 0x6); // память + bus master (DMA)
    let lo = cfg_r32f(slot, 0x24); // BAR5 = 0x10 + 4*5
    if lo & 1 != 0 {
        return None; // ABAR обязан быть memory-BAR
    }
    let mut abar = (lo & !0xf) as u64;
    if lo & 0x4 != 0 {
        abar |= (cfg_r32f(slot, 0x28) as u64) << 32; // 64-битный BAR — верхняя половина
    }
    let abar = abar as usize;
    if abar == 0 {
        return None;
    }
    unsafe {
        paging::map_mmio(abar, 0x2000); // generic-регистры + до 32 портов (0x100 + 32*0x80)
        let ghc = (abar + 0x04) as *mut u32;
        ghc.write_volatile(ghc.read_volatile() | 1 << 31); // GHC.AE — включить AHCI
    }
    let pi = unsafe { read_volatile((abar + 0x0c) as *const u32) }; // Ports Implemented
    for port in 0..32u32 {
        if pi & (1 << port) == 0 {
            continue;
        }
        let pbase = abar + 0x100 + port as usize * 0x80;
        let ssts = unsafe { read_volatile((pbase + 0x28) as *const u32) }; // PxSSTS
        if ssts & 0xf == 3 {
            return Some((abar, port)); // DET==3: устройство есть и связь установлена
        }
    }
    None
}

/// Веха 52 — настроить прерывание e1000 для userspace-драйвера: включить INTx, замаршрутизировать
/// его IRQ через IOAPIC на вектор [`trap::VEC_USERDRV`]. QEMU-шная e1000 — legacy INTx (без MSI):
/// маршрутизируем и её строку прерывания (PCI 0x3C), и PCI-диапазон GSI 16..24 (с запасом — на q35
/// INTx может уйти туда; лишние маршруты безвредны, драйвер всё равно сверяется с ICR). Возвращает
/// вектор. `None` — e1000 нет.
pub fn e1000_irq_setup() -> Option<u8> {
    for dev in 0..32u32 {
        let id = cfg_r32(dev, 0);
        let (vendor, device) = (id as u16, (id >> 16) as u16);
        if vendor == 0x8086 && (device == 0x100e || device == 0x10d3 || device == 0x100f) {
            cfg_w16(dev, 0x04, cfg_r16(dev, 0x04) & !(1 << 10)); // снять Interrupt Disable — вкл INTx
            // PCI INTx под IOAPIC приходит на GSI 16..23 (PIRQA..H), не на ISA-номер из Interrupt
            // Line. Точное соответствие слот→PIRQ дал бы ACPI _PRT (не парсим) — маршрутизируем все
            // четыре PCI-линии на наш вектор level/active-low: какую бы карта ни дёрнула, поймаем.
            for gsi in 16..24 {
                super::ioapic::route_level_low(gsi, trap::VEC_USERDRV);
            }
            return Some(trap::VEC_USERDRV);
        }
    }
    None
}

/// Веха 50 — найти контроллер USB **xHCI** (класс 0x0c/0x03/0x30 — Serial Bus / USB / xHCI),
/// как probe_ahci — по ВСЕМ функциям (на Intel-PCH xHCI на 00:14.0, но бывает и функция != 0).
/// Включаем память+bus-master, отображаем BAR0 (регистры, 64 КиБ), отдаём базу. `None` — нет.
pub fn probe_xhci() -> Option<usize> {
    for dev in 0..32u32 {
        for func in 0..8u32 {
            let slot = dev << 3 | func;
            let id = cfg_r32f(slot, 0);
            if id == 0xffff_ffff {
                if func == 0 {
                    break;
                }
                continue;
            }
            let cc = cfg_r32f(slot, 0x08);
            if (cc >> 24) as u8 == 0x0c && (cc >> 16) as u8 == 0x03 && (cc >> 8) as u8 == 0x30 {
                cfg_w16f(slot, 0x04, cfg_r16f(slot, 0x04) | 0x6); // память + bus master
                let lo = cfg_r32f(slot, 0x10); // BAR0
                if lo & 1 != 0 {
                    return None;
                }
                let mut base = (lo & !0xf) as u64;
                if lo & 0x4 != 0 {
                    base |= (cfg_r32f(slot, 0x14) as u64) << 32;
                }
                let base = base as usize;
                if base == 0 {
                    return None;
                }
                unsafe { paging::map_mmio(base, 0x10000) };
                return Some(base);
            }
            if func == 0 && cfg_r8f(slot, 0x0e) & 0x80 == 0 {
                break;
            }
        }
    }
    None
}

/// Веха 49 — найти сетевую карту Intel e1000 (PRO/1000) на шине 0. Vendor 0x8086, device
/// 0x100e (82540EM — то, что даёт QEMU `-device e1000`) либо 0x10d3 (82574L, «e1000e»).
/// Включаем память+bus-master, отдаём базу BAR0 (MMIO с регистрами). `None` — карты нет
/// (тогда драйвер откатится на virtio-net). Опрос, без прерываний — как virtio-net.
pub fn probe_e1000() -> Option<usize> {
    const VENDOR_INTEL: u16 = 0x8086;
    for dev in 0..32u32 {
        let id = cfg_r32(dev, 0);
        let (vendor, device) = (id as u16, (id >> 16) as u16);
        if vendor == VENDOR_INTEL && (device == 0x100e || device == 0x10d3 || device == 0x100f) {
            cfg_w16(dev, 0x04, cfg_r16(dev, 0x04) | 0x6); // память + bus master (DMA колец)
            let base = bar_addr(dev, 0); // BAR0 — регистры карты (MMIO)
            if base == 0 {
                return None;
            }
            unsafe { paging::map_mmio(base, 0x20000) }; // 128 КиБ регистрового окна
            return Some(base);
        }
    }
    None
}

/// Включить virtio-blk и взвести его MSI-X (диску прерывание нужно — async I/O).
fn setup(dev: u32) -> Option<BlkDevice> {
    let transport = setup_transport(dev)?;

    if !setup_msix(dev, trap::VEC_BLK) {
        return None; // без MSI-X диск не поддерживаем: async I/O без прерывания не собрать
    }
    Some(BlkDevice { transport, irq: trap::VEC_BLK as u32 })
}

/// Веха 91 - взвести MSI-X функции `dev` на вектор `vec` (запись 0 таблицы -> LAPIC, dest id 0,
/// fixed/edge, без маски) и включить MSI-X. `false` - у устройства нет такой capability.
/// Общее для диска и сети: механика одна, отличается только вектор.
fn setup_msix(dev: u32, vec: u8) -> bool {
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
        return false;
    }
    unsafe {
        paging::map_mmio(msix_table, 16);
        let e = msix_table as *mut u32;
        e.add(0).write_volatile(0xfee0_0000); // message address (LAPIC, dest id 0)
        e.add(1).write_volatile(0);
        e.add(2).write_volatile(vec as u32); // data: fixed, edge, вектор
        e.add(3).write_volatile(0); // vector control: размаскирован
    }
    let ctrl = cfg_r16(dev, msix_ptr + 2);
    cfg_w16(dev, msix_ptr + 2, (ctrl | 0x8000) & !0x4000);
    true
}
