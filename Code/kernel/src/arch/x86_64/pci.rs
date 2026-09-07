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

use super::{lapic, paging, trap};

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

const VENDOR_INTEL: u16 = 0x8086;
/// Веха 49 — Intel PRO/1000: 0x100e (82540EM, то что даёт QEMU `-device e1000`), 0x10d3
/// (82574L, «e1000e»), 0x100f (82545EM). Список один на поиск карты и на настройку её INTx:
/// разойдясь, они дали бы «карта есть, а прерывания у неё нет».
const E1000_IDS: [u16; 3] = [0x100e, 0x10d3, 0x100f];

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

// ─── адрес функции на шине и доступ к её конфигу ─────────────────────────────
//
// Веха 148.9 — ОДИН тип вместо трёх семейств аксессоров. Их было три, и различались они ровно
// сборкой адреса: `cfg_r32` знала шину 0 и функцию 0 (`dev << 11`), `…f` умела функцию
// (`slot << 8`), `…b` — ещё и шину (`bus << 16 | slot << 8`). Это одно и то же число, записанное
// трижды: `dev << 11` есть `(dev << 3) << 8`, а функция и шина просто добавляют свои поля.
// Цена копий была не в байтах, а в развилке на каждом новом месте: «а этому устройству каким
// семейством ходить?» — и ответ «тем, где шину видно» приходил уже после того, как карту за
// мостом PCIe не нашли.

/// Функция на шине PCI: `bus:dev.func`, упакованные как у самого железа.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Bdf(u16);

impl Bdf {
    fn new(bus: u32, dev: u32, func: u32) -> Bdf {
        Bdf((bus << 8 | dev << 3 | func) as u16)
    }

    fn bus(self) -> u32 {
        self.0 as u32 >> 8
    }

    fn dev(self) -> u32 {
        self.0 as u32 >> 3 & 0x1f
    }

    fn func(self) -> u32 {
        self.0 as u32 & 7
    }

    /// Адрес регистра `off` для порта 0xCF8. Смещение выравнивается по dword'у — порт данных
    /// 0xCFC 32-битный, и других обращений у механизма конфигурации нет.
    fn addr(self, off: u32) -> u32 {
        0x8000_0000 | (self.0 as u32) << 8 | (off & 0xfc)
    }

    fn r32(self, off: u32) -> u32 {
        outl(CFG_ADDR, self.addr(off));
        inl(CFG_DATA)
    }

    fn w32(self, off: u32, v: u32) {
        outl(CFG_ADDR, self.addr(off));
        outl(CFG_DATA, v);
    }

    fn r16(self, off: u32) -> u16 {
        (self.r32(off) >> ((off & 3) * 8)) as u16
    }

    fn r8(self, off: u32) -> u8 {
        (self.r32(off) >> ((off & 3) * 8)) as u8
    }

    /// 16 бит пишутся чтением-модификацией dword'а — по той же причине, по какой адрес
    /// выравнивается.
    fn w16(self, off: u32, v: u16) {
        let sh = (off & 3) * 8;
        let old = self.r32(off);
        self.w32(off, (old & !(0xffff << sh)) | (v as u32) << sh);
    }

    /// Идентификаторы `(vendor, device)`. `None` — на этот адрес никто не отвечает.
    fn id(self) -> Option<(u16, u16)> {
        let id = self.r32(0);
        let vendor = id as u16;
        (vendor != 0xffff && vendor != 0).then(|| (vendor, (id >> 16) as u16))
    }

    /// `(class, subclass, prog-if)` — чем устройство себя объявляет.
    fn class(self) -> (u8, u8, u8) {
        let cc = self.r32(0x08);
        ((cc >> 24) as u8, (cc >> 16) as u8, (cc >> 8) as u8)
    }

    /// Физическая база memory-BAR `idx` (64-битные — из пары регистров). `0` — окно не назначено
    /// прошивкой либо BAR в пространстве ввода-вывода: регистров там нет, отображать нечего.
    fn bar(self, idx: u8) -> usize {
        let lo = self.r32(0x10 + 4 * idx as u32);
        if lo & 1 != 0 {
            return 0;
        }
        let mut addr = (lo & !0xf) as u64;
        if lo & 0x4 != 0 {
            addr |= (self.r32(0x14 + 4 * idx as u32) as u64) << 32;
        }
        addr as usize
    }

    /// Память + bus-master (DMA). Верхняя половина dword'а команды — status (биты RW1C);
    /// запись прочитанного их сбрасывает, и это безвредно.
    fn enable(self) {
        self.w16(0x04, self.r16(0x04) | 0x6);
    }
}

// ─── обход шины ──────────────────────────────────────────────────────────────

/// Только шина 0 — q35 и всё, что на ней стоит.
const BUS0: u32 = 0;
/// Все шины: на живой машине почти всё интересное сидит за мостами PCIe, каждый со своей.
const ALL_BUSES: u32 = 255;

/// Обойти шины `0..=last_bus` и вернуть первое, на чём `f` дала ответ.
///
/// Одно место, где записано правило «функции 1..7 существуют только у многофункционального
/// устройства» (бит 7 header type). Прежде оно было переписано в четырёх обходах, и все четыре
/// расходились в мелочах: `dump` читал header type РАНЬШЕ, чем проверял, отвечает ли функция 0,
/// а поиск virtio смотрел вообще только функцию 0 — устройство на функции 1 для него не
/// существовало.
///
/// Перебор, а не спуск по мостам: спуск точнее, но требует разбора secondary/subordinate у
/// каждого моста, а ошибка в нём выглядит как «устройства нет» — то есть как раз то, что ищем.
/// Несуществующая шина отвечает одними единицами, так что перебор пропустить ничего не может.
fn find<T>(last_bus: u32, mut f: impl FnMut(Bdf) -> Option<T>) -> Option<T> {
    for bus in 0..=last_bus {
        for dev in 0..32u32 {
            let first = Bdf::new(bus, dev, 0);
            if first.id().is_none() {
                continue; // нет функции 0 — устройства в слоте нет вовсе
            }
            let funcs = if first.r8(0x0e) & 0x80 != 0 { 8 } else { 1 };
            for func in 0..funcs {
                let d = Bdf::new(bus, dev, func);
                if d.id().is_none() {
                    continue;
                }
                if let Some(v) = f(d) {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// Обойти всё, ни на чём не останавливаясь.
fn for_each(last_bus: u32, mut f: impl FnMut(Bdf)) {
    find(last_bus, |d| {
        f(d);
        None::<()>
    });
}

/// Первая функция с такими идентификаторами. Список device id, а не один: одна и та же карта
/// приезжает под несколькими (`e1000` — три, virtio — modern и transitional).
fn find_id(last_bus: u32, vendor: u16, devices: &[u16]) -> Option<Bdf> {
    find(last_bus, |d| {
        let (v, dev) = d.id()?;
        (v == vendor && devices.contains(&dev)).then_some(d)
    })
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
        // 07/80 на Intel-PCH — это MEI (Management Engine). Первый прогон на X54C напечатал его
        // как «?», и это правильно: врать именем хуже, чем сказать «не знаю».
        (0x07, 0x80) => "связь: MEI",
        (0x07, _) => "связь",
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
        // Всё, что ниже, опознано на X54C: RT5390 (Wi-Fi), ASM1042 (USB3), сам ноутбук и его ODM.
        0x1814 => "Ralink/MTK",
        0x1b21 => "ASMedia",
        0x1043 => "ASUS",
        0x105b => "Foxconn",
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
/// Цена обхода — восемь тысяч чтений портов на загрузку, единицы миллисекунд.
pub fn dump() {
    let mut count = 0usize;
    // Сетевые карты собираем отдельной строкой: на ноутбуке опись — это два десятка строк, а
    // вопрос сейчас ровно один. Пусть ответ будет виден сразу, а не выискивался глазами.
    let mut nets = [(0u16, 0u16, 0u8); 8];
    let mut nnet = 0usize;
    for_each(ALL_BUSES, |d| {
        let Some((vendor, device)) = d.id() else { return };
        let (class, sub, progif) = d.class();
        let sub_id = d.r32(0x2c);
        let irq = d.r8(0x3c);
        // Первый ненулевой BAR: по нему видно, отдала ли прошивка устройству окно памяти
        // (без окна драйвер писать не по чему).
        let bar = (0..6u8).map(|i| d.bar(i)).find(|&b| b != 0).unwrap_or(0);
        crate::println!(
            "  [pci] {:02x}:{:02x}.{} {:04x}:{:04x} {:5} {:16} класс {:02x}{:02x}{:02x} подсист {:04x}:{:04x} bar {:#010x} irq {}",
            d.bus(), d.dev(), d.func(), vendor, device, vendor_name(vendor),
            class_name(class, sub), class, sub, progif,
            sub_id as u16, (sub_id >> 16) as u16, bar, irq,
        );
        count += 1;
        // Класс 02 — Ethernet и родня, 0d/80 — беспроводное.
        if (class == 0x02 || (class == 0x0d && sub == 0x80)) && nnet < nets.len() {
            nets[nnet] = (vendor, device, sub);
            nnet += 1;
        }
    });
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

/// Веха 132 — найти устройство `vendor:device` НА ЛЮБОЙ ШИНЕ и отдать его BAR0 (физ. база).
/// Включает память и bus-master, отображает окно регистров.
///
/// Отдельно от `probe_e1000` и родни не из любви к обобщению: те смотрят только шину 0, потому
/// что на q35 всё там и стоит. На живом ноутбуке карта сидит за мостом PCIe (X54C: AR8151 на
/// шине 04) — обход по одной шине не нашёл бы её никогда, и выглядело бы это как «карты нет».
///
/// Длина окна берётся параметром: у каждой карты она своя, а читать её размером BAR'а (запись
/// единиц и обратное чтение) значит на мгновение снять устройство с его адреса — на живой
/// машине с работающей прошивкой это лишний риск ради числа, которое мы и так знаем.
pub fn probe_bar0(vendor_want: u16, device_want: u16, len: usize) -> Option<usize> {
    let d = find_id(ALL_BUSES, vendor_want, &[device_want])?;
    // Память + bus-master: без первого не отвечают регистры, без второго карта не сможет ходить
    // в память сама (кольца дескрипторов — следующая веха).
    d.enable();
    // 0 — либо BAR0 в пространстве ввода-вывода (регистров там нет), либо прошивка окна не
    // назначила: отображать нечего.
    let base = d.bar(0);
    (base != 0).then(|| {
        unsafe { paging::map_mmio(base, len) };
        base
    })
}

/// Веха 133.2 — включить INTx устройства `vendor:device` (на любой шине) и замаршрутизировать
/// линии PCI на вектор userspace-драйверов. Возвращает вектор.
///
/// Тот же приём, что у e1000 (`e1000_irq_setup`), но с обходом по всем шинам. Маршрутизируем
/// ВСЕ четыре линии PCI (GSI 16..23) на один вектор: точное соответствие «слот → PIRQ» знает
/// только таблица ACPI `_PRT`, которую мы не разбираем. Приём грубый и осознанно такой —
/// userspace-драйвер в системе один, и лишнее пробуждение стоит ему одного холостого чтения
/// регистра причины. Появится второй драйвер — придётся разбирать `_PRT` по-настоящему.
pub fn intx_irq_setup(vendor_want: u16, device_want: u16) -> Option<u8> {
    let d = find_id(ALL_BUSES, vendor_want, &[device_want])?;
    // Снять Interrupt Disable (бит 10 команды) — иначе карта дёргать линию не станет.
    d.w16(0x04, d.r16(0x04) & !(1 << 10));
    for gsi in 16..24 {
        super::ioapic::route_level_low(gsi, trap::VEC_USERDRV);
    }
    Some(trap::VEC_USERDRV)
}

/// Найти virtio-blk на шине 0 и подготовить его: BAR-окна отображены, MSI-X взведён.
pub fn probe_virtio_blk() -> Option<BlkDevice> {
    let d = find_id(BUS0, VENDOR_VIRTIO, &[DEV_BLK_MODERN, DEV_BLK_TRANSITIONAL])?;
    let transport = setup_transport(d)?;
    // Без MSI-X диск не поддерживаем: async I/O без прерывания не собрать.
    setup_msix(d, trap::VEC_BLK).then_some(BlkDevice { transport, irq: trap::VEC_BLK as u32 })
}

/// Найти virtio-net на шине 0 (Веха 34) и подготовить транспорт БЕЗ MSI-X: сеть
/// работает опросом колец, прерывание не программируется (отложено до потребности).
pub fn probe_virtio_net() -> Option<NetDevice> {
    let d = find_id(BUS0, VENDOR_VIRTIO, &[DEV_NET_MODERN, DEV_NET_TRANSITIONAL])?;
    // Веха 91: сети тоже нужен MSI-X. Не вышло взвести — карта остаётся на опросе.
    let transport = setup_transport(d)?;
    let irq = setup_msix(d, trap::VEC_NET).then_some(trap::VEC_NET as u32).unwrap_or(0);
    Some(NetDevice { transport, irq })
}

/// Найти virtio-rng на PCI. Прерывания ему не заводим: запросы энтропии редкие и синхронные,
/// драйвер ждёт завершения в used-кольце.
pub fn probe_virtio_rng() -> Option<BlkTransport> {
    setup_transport(find_id(BUS0, VENDOR_VIRTIO, &[DEV_RNG_MODERN, DEV_RNG_TRANSITIONAL])?)
}

/// Пройти vendor-capabilities virtio, отобразить BAR-окна структур, вернуть транспорт.
/// Общее для blk и net: разговор по virtqueue одинаков, отличается лишь MSI-X (у сети нет).
fn setup_transport(d: Bdf) -> Option<BlkTransport> {
    d.enable(); // память + bus master (DMA колец)

    let (mut common, mut notify_base, mut notify_mult) = (0usize, 0usize, 0u32);
    let (mut isr, mut device_cfg) = (0usize, 0usize);

    // Пройти список capabilities (status.bit4 у virtio-устройств QEMU всегда есть).
    let mut ptr = d.r8(0x34) as u32 & !3;
    while ptr != 0 {
        // Vendor-capability virtio: тип структуры + [BAR, смещение, длина].
        if d.r8(ptr) == 0x09 {
            let cfg_type = d.r8(ptr + 3);
            let bar = d.r8(ptr + 4);
            let off = d.r32(ptr + 8) as usize;
            let len = d.r32(ptr + 12) as usize;
            let base = d.bar(bar);
            if base != 0 && len != 0 {
                let addr = base + off;
                unsafe { paging::map_mmio(addr, len) };
                match cfg_type {
                    1 => common = addr,
                    2 => {
                        notify_base = addr;
                        notify_mult = d.r32(ptr + 16);
                    }
                    3 => isr = addr,
                    4 => device_cfg = addr,
                    _ => {} // 5 = pci-cfg-доступ, не нужен: BAR'ы отображаемы
                }
            }
        }
        ptr = d.r8(ptr + 1) as u32 & !3;
    }

    if common == 0 || notify_base == 0 || isr == 0 || device_cfg == 0 {
        return None; // не modern virtio
    }
    Some(BlkTransport::Pci { common, notify_base, notify_mult, isr, device: device_cfg })
}

// ─── AHCI (Веха 47) ──────────────────────────────────────────────────────────
//
// Ради этого случая и заводилось второе семейство аксессоров: на Intel-PCH SATA-контроллер сидит
// на 00:1f.2 — ФУНКЦИЯ 2, которую тогдашний virtio-скан (только функция 0) не видел. Теперь
// функцию видит общий обход, и отдельного семейства не нужно.

/// Веха 47 — найти AHCI-контроллер (SATA) с ПОДКЛЮЧЁННЫМ диском. Скан шины 0, ВСЕ функции
/// (класс 01/06/01 = Mass Storage / SATA / AHCI). У кандидата включаем память+bus-master,
/// берём ABAR (BAR5), отображаем, включаем AHCI (GHC.AE) и ищем порт с устройством
/// (PxSSTS.DET==3). Возвращает `(ABAR, номер порта)`; None — AHCI с диском не нашли (тогда
/// драйвер откатится на virtio-blk). Много-контроллерный случай QEMU (встроенный ich9 без
/// диска на 1f.2 + добавленный с диском) разрулён проверкой наличия диска в самом порту:

/// Веха 174 — ВСЕ порты контроллера, на которых есть диск: `(ABAR, порты, сколько)`.
///
/// Установщику мало «первого попавшегося»: он спрашивает человека, КУДА ставить, и список из
/// одного диска на машине с двумя — это не выбор, а лотерея. Контроллер берём один (первый
/// найденный): на ноутбуке он и есть один, а «второй AHCI» — задача того дня, когда он появится.
pub fn probe_ahci_ports() -> Option<(usize, [u32; MAX_DISKS], usize)> {
    find(BUS0, |d| (d.class() == (0x01, 0x06, 0x01)).then(|| setup_ahci_all(d))?)
}

/// Сколько дисков перечисляем. Портов у AHCI до 32, но столько их не бывает ни на одной машине,
/// куда ставят VOID, а список — это ещё и экран, на котором его читают.
pub const MAX_DISKS: usize = 8;

/// Включить контроллер AHCI, отобразить ABAR, собрать ВСЕ порты с дисками.
fn setup_ahci_all(d: Bdf) -> Option<(usize, [u32; MAX_DISKS], usize)> {
    d.enable(); // память + bus master (DMA)
    let abar = d.bar(5); // ABAR обязан быть memory-BAR; 0 — не он либо окна нет
    if abar == 0 {
        return None;
    }
    unsafe {
        paging::map_mmio(abar, 0x2000); // generic-регистры + до 32 портов (0x100 + 32*0x80)
        let ghc = (abar + 0x04) as *mut u32;
        ghc.write_volatile(ghc.read_volatile() | 1 << 31); // GHC.AE — включить AHCI
    }
    let pi = unsafe { read_volatile((abar + 0x0c) as *const u32) }; // Ports Implemented
    let mut ports = [0u32; MAX_DISKS];
    let mut n = 0usize;
    for port in 0..32u32 {
        if pi & (1 << port) == 0 || n == MAX_DISKS {
            continue;
        }
        let pbase = abar + 0x100 + port as usize * 0x80;
        let ssts = unsafe { read_volatile((pbase + 0x28) as *const u32) }; // PxSSTS
        if ssts & 0xf == 3 {
            ports[n] = port; // DET==3: устройство есть и связь установлена
            n += 1;
        }
    }
    (n > 0).then_some((abar, ports, n))
}

/// Веха 52 — настроить прерывание e1000 для userspace-драйвера: включить INTx, замаршрутизировать
/// его IRQ через IOAPIC на вектор [`trap::VEC_USERDRV`]. QEMU-шная e1000 — legacy INTx (без MSI):
/// маршрутизируем и её строку прерывания (PCI 0x3C), и PCI-диапазон GSI 16..24 (с запасом — на q35
/// INTx может уйти туда; лишние маршруты безвредны, драйвер всё равно сверяется с ICR). Возвращает
/// вектор. `None` — e1000 нет.
pub fn e1000_irq_setup() -> Option<u8> {
    let d = find_id(BUS0, VENDOR_INTEL, &E1000_IDS)?;
    d.w16(0x04, d.r16(0x04) & !(1 << 10)); // снять Interrupt Disable — включить INTx
    // PCI INTx под IOAPIC приходит на GSI 16..23 (PIRQA..H), не на ISA-номер из Interrupt Line.
    // Точное соответствие слот→PIRQ дал бы ACPI _PRT (не парсим) — маршрутизируем все четыре
    // PCI-линии на наш вектор level/active-low: какую бы карта ни дёрнула, поймаем.
    for gsi in 16..24 {
        super::ioapic::route_level_low(gsi, trap::VEC_USERDRV);
    }
    Some(trap::VEC_USERDRV)
}

/// Веха 50 — найти контроллер USB **xHCI** (класс 0x0c/0x03/0x30 — Serial Bus / USB / xHCI),
/// как probe_ahci — по ВСЕМ функциям (на Intel-PCH xHCI на 00:14.0, но бывает и функция != 0).
/// Включаем память+bus-master, отображаем BAR0 (регистры, 64 КиБ), отдаём базу. `None` — нет.
pub fn probe_xhci() -> Option<usize> {
    find(BUS0, |d| {
        if d.class() != (0x0c, 0x03, 0x30) {
            return None;
        }
        d.enable(); // память + bus master
        let base = d.bar(0); // регистры контроллера
        (base != 0).then(|| {
            unsafe { paging::map_mmio(base, 0x10000) };
            base
        })
    })
}

/// Веха 49 — найти сетевую карту Intel e1000 (PRO/1000) на шине 0. Vendor 0x8086, device
/// 0x100e (82540EM — то, что даёт QEMU `-device e1000`) либо 0x10d3 (82574L, «e1000e»).
/// Включаем память+bus-master, отдаём базу BAR0 (MMIO с регистрами). `None` — карты нет
/// (тогда драйвер откатится на virtio-net). Опрос, без прерываний — как virtio-net.
pub fn probe_e1000() -> Option<usize> {
    let d = find_id(BUS0, VENDOR_INTEL, &E1000_IDS)?;
    d.enable(); // память + bus master (DMA колец)
    let base = d.bar(0); // BAR0 — регистры карты (MMIO)
    (base != 0).then(|| {
        unsafe { paging::map_mmio(base, 0x20000) }; // 128 КиБ регистрового окна
        base
    })
}

/// Веха 91 - взвести MSI-X функции `d` на вектор `vec` (запись 0 таблицы -> LAPIC, dest id 0,
/// fixed/edge, без маски) и включить MSI-X. `false` - у устройства нет такой capability.
/// Общее для диска и сети: механика одна, отличается только вектор.
fn setup_msix(d: Bdf, vec: u8) -> bool {
    let (mut msix_ptr, mut msix_table) = (0u32, 0usize);
    let mut ptr = d.r8(0x34) as u32 & !3;
    while ptr != 0 {
        if d.r8(ptr) == 0x11 {
            msix_ptr = ptr;
            let t = d.r32(ptr + 4);
            msix_table = d.bar((t & 7) as u8) + (t & !7) as usize;
        }
        ptr = d.r8(ptr + 1) as u32 & !3;
    }
    if msix_ptr == 0 {
        return false;
    }
    unsafe {
        paging::map_mmio(msix_table, 16);
        let e = msix_table as *mut u32;
        // Веха 170 — адресат в битах 19..12 адреса сообщения: ЗАГРУЗОЧНОЕ ядро (см. `ioapic`).
        // Прежний ноль был верен лишь потому, что у него такой номер в QEMU.
        e.add(0).write_volatile(0xfee0_0000 | ((lapic::id() as u32) << 12));
        e.add(1).write_volatile(0);
        e.add(2).write_volatile(vec as u32); // data: fixed, edge, вектор
        e.add(3).write_volatile(0); // vector control: размаскирован
    }
    let ctrl = d.r16(msix_ptr + 2);
    d.w16(msix_ptr + 2, (ctrl | 0x8000) & !0x4000);
    true
}
