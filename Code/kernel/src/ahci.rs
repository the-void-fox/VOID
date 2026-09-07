//! Драйвер AHCI (SATA) — блочное устройство на РЕАЛЬНОМ x86-железе (Веха 47).
//!
//! На настоящей машине (ASUS X54C и вообще любой ПК) диск висит не на virtio, а на
//! SATA-контроллере в режиме AHCI. Этот драйвер даёт store'у тот же интерфейс, что и
//! [`crate::virtio_blk`] (чтение/запись 512-байтных секторов), но поверх AHCI-порта.
//! Устройство ищет арх ([`crate::arch::probe_ahci`] — PCI-скан, ABAR, порт с диском),
//! разговор ведём здесь. На riscv/QEMU-virt AHCI нет — `probe_ahci` вернёт `None`, и
//! [`init`] тихо откажется (диск придёт через virtio-blk).
//!
//! Ввод-вывод **опросом** (без прерываний): store читает/пишет синхронно, а для первой
//! вехи железного диска этого достаточно (async-путь остаётся у virtio). Одна команда за
//! раз под общим замком — слот 0, одна PRDT-запись на 512 байт.
//!
//! Раскладка в RAM (два обнулённых фрейма [`crate::frame`], RAM отображена идентично —
//! физический адрес = виртуальный, HBA читает их DMA):
//! - фрейм A: список команд (0x000, 1 КиБ) + принятый FIS (0x400, 256 Б) + таблица
//!   команд (0x500: CFIS 64 + ACMD 16 + резерв + PRDT);
//! - фрейм B: DMA-буфер данных (используем первые 512 Б).

use core::sync::atomic::{compiler_fence, Ordering};

use crate::frame;
use crate::sync::SpinLock;

const SECTOR: usize = 512;

// ─── смещения регистров порта (от базы порта = ABAR + 0x100 + порт*0x80) ──────
const PX_CLB: usize = 0x00; // command list base (низ)
const PX_CLBU: usize = 0x04;
const PX_FB: usize = 0x08; // FIS base (низ)
const PX_FBU: usize = 0x0c;
const PX_IS: usize = 0x10; // interrupt status
const PX_CMD: usize = 0x18; // command and status
const PX_TFD: usize = 0x20; // task file data (BSY/DRQ/ERR)
const PX_SERR: usize = 0x30; // SATA error
const PX_CI: usize = 0x38; // command issue (бит слота)

const CMD_ST: u32 = 1 << 0; // Start
const CMD_FRE: u32 = 1 << 4; // FIS Receive Enable
const CMD_FR: u32 = 1 << 14; // FIS Receive Running
const CMD_CR: u32 = 1 << 15; // Command list Running

const TFD_BSY: u32 = 1 << 7;
const TFD_DRQ: u32 = 1 << 3;
const TFD_ERR: u32 = 1 << 0;

const IS_TFES: u32 = 1 << 30; // Task File Error Status

// ATA-команды.
const ATA_READ_DMA_EXT: u8 = 0x25;
const ATA_WRITE_DMA_EXT: u8 = 0x35;
const ATA_IDENTIFY: u8 = 0xec;

/// Тип MBR-раздела под store VOID (Веха 48). Произвольный незанятый байт — по нему AHCI
/// узнаёт «свой» раздел на разбитом диске (загрузчик+ядро в p1, store в p2). Если MBR/раздела
/// нет — store лежит с сектора 0 на весь диск (как `void-disk.img` в QEMU, обратная совместимость).
pub const VOID_STORE_TYPE: u8 = 0x9f;

/// Смещения структур внутри фрейма A.
const OFF_FIS: usize = 0x400; // принятый FIS
const OFF_CT: usize = 0x500; // таблица команд (CFIS+ACMD+резерв+PRDT)
const OFF_PRDT: usize = OFF_CT + 0x80; // PRDT — после 128-байтной шапки таблицы команд

struct Ahci {
    port: usize, // база порта: ABAR + 0x100 + порт*0x80
    frame_a: usize, // список команд + FIS + таблица команд
    buf: usize, // DMA-буфер данных (фрейм B)
    capacity: u64, // ёмкость store в секторах (раздел p2 или весь диск)
    base: u64, // Веха 48 — LBA начала store: 0 (весь диск) или начало раздела VOID
    total: u64, // Веха 48 — полная ёмкость диска в секторах (для установщика)
    /// Веха 174 — номер порта AHCI. Он же ИМЯ диска для установщика: список дисков нумеруется
    /// по порядку портов, и по этому номеру человек выбирает, куда ставить.
    slot: usize,
    /// Веха 174 — модель диска из IDENTIFY (слова 27..46, по два знака на слово, старший первым).
    /// Показывается человеку: «поставить на диск 0» — не выбор, а угадывание.
    model: [u8; MODEL_LEN],
    /// Веха 174 — есть ли на диске раздел VOID. Установщику это единственный способ сказать
    /// «здесь уже стоит система», не читая её содержимого.
    void: bool,
}

/// Длина строки модели в IDENTIFY: слова 27..46 — это 40 знаков.
pub const MODEL_LEN: usize = 40;

/// Веха 174 — что установщик показывает человеку про один диск.
#[derive(Clone, Copy)]
pub struct Disk {
    /// Номер порта — им же диск и выбирают.
    pub slot: usize,
    /// Полная ёмкость в секторах по 512 Б.
    pub sectors: u64,
    pub model: [u8; MODEL_LEN],
    /// На диске уже есть раздел VOID.
    pub void: bool,
    /// С этого диска работает СЕЙЧАШНИЙ store — ставить на него нельзя.
    pub live: bool,
}

unsafe impl Send for Ahci {}

static AHCI: SpinLock<Option<Ahci>> = SpinLock::new(None);

#[inline]
unsafe fn rd(addr: usize) -> u32 {
    core::ptr::read_volatile(addr as *const u32)
}
#[inline]
unsafe fn wr(addr: usize, v: u32) {
    core::ptr::write_volatile(addr as *mut u32, v);
}

/// Подключить диск на порту `port_num` и опросить его. `None` — порт не отвечает.
///
/// Веха 174 — отдельно от [`init`], потому что портов бывает несколько: store живёт на одном, а
/// установщик спрашивает про все и пишет на выбранный. Раньше эта работа была телом `init`, и
/// «диск» в драйвере существовал ровно один — тот, что нашёлся первым.
fn open(abar: usize, port_num: u32) -> Option<Ahci> {
    let port = abar + 0x100 + port_num as usize * 0x80;
    let frame_a = frame::alloc()?;
    let buf = frame::alloc()?;

    unsafe {
        // 1) Остановить порт: снять ST, дождаться CR=0; снять FRE, дождаться FR=0.
        let cmd = rd(port + PX_CMD);
        wr(port + PX_CMD, cmd & !(CMD_ST | CMD_FRE));
        for _ in 0..1_000_000 {
            if rd(port + PX_CMD) & (CMD_CR | CMD_FR) == 0 {
                break;
            }
        }

        // 2) Прописать базы списка команд и приёмного FIS (в пределах фрейма A).
        wr(port + PX_CLB, frame_a as u32);
        wr(port + PX_CLBU, (frame_a as u64 >> 32) as u32);
        wr(port + PX_FB, (frame_a + OFF_FIS) as u32);
        wr(port + PX_FBU, ((frame_a + OFF_FIS) as u64 >> 32) as u32);

        // 3) Сбросить ошибки/статус и запустить порт (сперва FRE, потом ST).
        wr(port + PX_SERR, 0xffff_ffff);
        wr(port + PX_IS, 0xffff_ffff);
        wr(port + PX_CMD, rd(port + PX_CMD) | CMD_FRE);
        wr(port + PX_CMD, rd(port + PX_CMD) | CMD_ST);
    }

    let mut dev = Ahci {
        port,
        frame_a,
        buf,
        capacity: 0,
        base: 0,
        total: 0,
        slot: port_num as usize,
        model: [0; MODEL_LEN],
        void: false,
    };

    // 4) IDENTIFY DEVICE → полная ёмкость диска (LBA48 в словах 100..103, иначе LBA28 в 60..61)
    //    и МОДЕЛЬ (слова 27..46: по два знака на слово, старший байт первым).
    if !dev.command(ATA_IDENTIFY, 0, false) {
        frame::free(frame_a);
        frame::free(buf);
        return None;
    }
    unsafe {
        let id = crate::frame::ptr(buf) as *const u16;
        let lba48 = id.add(100).read_volatile() as u64
            | (id.add(101).read_volatile() as u64) << 16
            | (id.add(102).read_volatile() as u64) << 32
            | (id.add(103).read_volatile() as u64) << 48;
        let lba28 =
            id.add(60).read_volatile() as u64 | (id.add(61).read_volatile() as u64) << 16;
        dev.total = if lba48 != 0 { lba48 } else { lba28 };
        for i in 0..MODEL_LEN / 2 {
            let w = id.add(27 + i).read_volatile();
            dev.model[i * 2] = (w >> 8) as u8;
            dev.model[i * 2 + 1] = w as u8;
        }
    }
    dev.capacity = dev.total; // по умолчанию весь диск

    // 5) Веха 48 — прочитать MBR (СЫРОЙ сектор 0, base ещё 0) и найти раздел store VOID.
    //
    // Веха 171 — **чужой диск НЕ БЕРЁМ, и чистый тоже.** Правило было «нет нашего раздела —
    // значит весь диск наш»: пока VOID запускал только владелец на своей машине, оно было
    // безобидным. С живого ISO система загружается у постороннего человека, и первый же коммит
    // store лёг бы поверх чужой таблицы разделов — или поверх диска, который никто не отдавал.
    //
    // Поэтому носителем становится ТОЛЬКО диск с разделом [`VOID_STORE_TYPE`]: такой раздел
    // делает `install`, то есть он и есть след явного согласия человека. Всё остальное система
    // не трогает и уезжает на образ в памяти ([`crate::ramdisk`]). `install` при этом доступен:
    // устройство зарегистрировано, полная ёмкость известна, и разметить диск по просьбе он
    // может. Обратная совместимость сырого образа «store с сектора 0» осталась там, где ей и
    // место, — у virtio-blk (образ riscv `void-disk.img`); у AHCI её никогда не требовалось.
    let mut ours = false;
    if dev.command(ATA_READ_DMA_EXT, 0, false) {
        unsafe {
            let m = crate::frame::ptr(buf) as *const u8;
            if m.add(510).read_volatile() == 0x55 && m.add(511).read_volatile() == 0xaa {
                for i in 0..4 {
                    let e = m.add(446 + i * 16);
                    if e.add(4).read_volatile() == VOID_STORE_TYPE {
                        let rd_le = |o: usize| {
                            e.add(o).read_volatile() as u64
                                | (e.add(o + 1).read_volatile() as u64) << 8
                                | (e.add(o + 2).read_volatile() as u64) << 16
                                | (e.add(o + 3).read_volatile() as u64) << 24
                        };
                        dev.base = rd_le(8); // LBA начала раздела
                        dev.capacity = rd_le(12); // число секторов раздела
                        ours = true;
                        break;
                    }
                }
            }
        }
    }

    dev.void = ours;
    if !ours {
        dev.capacity = 0; // не носитель: ни одного сектора store на этом диске нет
    }
    Some(dev)
}

/// Отпустить фреймы диска, которым больше не пользуемся (перечисление, смена цели установки).
fn close(dev: Ahci) {
    unsafe {
        // Порт остановить: DMA по нашим фреймам после их освобождения — порча памяти, которую
        // не свяжет с причиной никто.
        wr(dev.port + PX_CMD, rd(dev.port + PX_CMD) & !(CMD_ST | CMD_FRE));
        for _ in 0..1_000_000 {
            if rd(dev.port + PX_CMD) & (CMD_CR | CMD_FR) == 0 {
                break;
            }
        }
    }
    frame::free(dev.frame_a);
    frame::free(dev.buf);
}

/// Инициализировать диск ПОД STORE, если он есть. `true` — готов к чтению/записи.
///
/// Веха 174 — перебираем все порты и берём тот, на котором лежит раздел VOID. Раньше брался
/// первый попавшийся, и на машине с двумя дисками система молча зависела от порядка портов.
pub fn init() -> bool {
    let Some((abar, ports, n)) = crate::arch::probe_ahci_ports() else {
        return false;
    };
    let mut seen = 0usize;
    for &p in ports.iter().take(n) {
        let Some(dev) = open(abar, p) else { continue };
        seen += 1;
        if dev.void {
            *AHCI.lock() = Some(dev);
            return true;
        }
        close(dev);
    }
    if seen > 0 {
        crate::println!(
            "  [blk]  SATA-дисков {} — ни на одном нет раздела VOID, не трогаем их (поставить систему: `install`)",
            seen,
        );
    }
    false
}

/// Веха 174 — ПЕРЕЧИСЛИТЬ диски для установщика. Возвращает, сколько записано в `out`.
///
/// Каждый порт открывается и тут же закрывается: держать восемь устройств живыми ради списка,
/// который смотрят раз в жизни, значило бы занять по два фрейма на каждое навсегда.
pub fn disks(out: &mut [Disk]) -> usize {
    // ABAR спрашиваем У ПРОБЫ, а не помним с загрузки: на живом носителе `init` не звали вовсе
    // (store в памяти — Веха 174.1), и запомненный адрес остался бы нулём. Проба идемпотентна.
    let Some((abar, ports, n)) = crate::arch::probe_ahci_ports() else {
        return 0;
    };
    let live = AHCI.lock().as_ref().map(|d| d.slot);
    let mut k = 0usize;
    for &p in ports.iter().take(n) {
        if k == out.len() {
            break;
        }
        // Порт, на котором работает store, НЕ ТРОГАЕМ: он уже открыт, и второй `open` сбросил бы
        // ему базы команд посреди чужой работы. Всё нужное про него мы и так знаем.
        if live == Some(p as usize) {
            if let Some(d) = AHCI.lock().as_ref() {
                out[k] = Disk { slot: d.slot, sectors: d.total, model: d.model, void: d.void, live: true };
                k += 1;
            }
            continue;
        }
        let Some(dev) = open(abar, p) else { continue };
        out[k] = Disk { slot: dev.slot, sectors: dev.total, model: dev.model, void: dev.void, live: false };
        k += 1;
        close(dev);
    }
    k
}

/// Веха 174 — ЦЕЛЬ УСТАНОВКИ: диск, выбранный человеком. Отдельно от [`AHCI`], потому что это
/// разные вещи: с одного система работает, на другой её ставят, и путать их нельзя ни на шаг.
static TARGET: SpinLock<Option<Ahci>> = SpinLock::new(None);

/// Открыть диск на порту `slot` как цель установки. `false` — порта нет либо это наш store.
pub fn target_open(slot: usize) -> bool {
    if AHCI.lock().as_ref().map(|d| d.slot) == Some(slot) {
        return false; // ставить на диск, с которого работаем, нельзя — см. [`TARGET`]
    }
    let Some((abar, ports, n)) = crate::arch::probe_ahci_ports() else {
        return false;
    };
    if !ports.iter().take(n).any(|&p| p as usize == slot) {
        return false;
    }
    let Some(dev) = open(abar, slot as u32) else { return false };
    if let Some(old) = TARGET.lock().take() {
        close(old);
    }
    *TARGET.lock() = Some(dev);
    true
}

/// Полная ёмкость ЦЕЛИ установки в секторах (0 — цель не выбрана).
pub fn target_sectors() -> u64 {
    TARGET.lock().as_ref().map_or(0, |d| d.total)
}

/// Абсолютная запись сектора на ЦЕЛЬ установки (без смещения раздела).
pub fn target_write(sector: u64, buf: &[u8; SECTOR]) -> bool {
    let g = TARGET.lock();
    let Some(d) = g.as_ref() else { return false };
    unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), crate::frame::ptr(d.buf), SECTOR) };
    d.command(ATA_WRITE_DMA_EXT, sector, true)
}

/// Абсолютное чтение сектора с ЦЕЛИ установки.
pub fn target_read(sector: u64, buf: &mut [u8; SECTOR]) -> bool {
    let g = TARGET.lock();
    let Some(d) = g.as_ref() else { return false };
    if !d.command(ATA_READ_DMA_EXT, sector, false) {
        return false;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(crate::frame::ptr(d.buf) as *const u8, buf.as_mut_ptr(), SECTOR)
    };
    true
}

impl Ahci {
    /// Выполнить одну ATA-команду `cmd` над LBA `lba` (0 для IDENTIFY), передав 512 байт
    /// через PRDT в/из [`Self::buf`]. `write` — направление (H2D-бит W). Опрос до
    /// завершения (`PxCI` бит 0 очищен) или ошибки. `true` — успех.
    fn command(&self, cmd: u8, lba: u64, write: bool) -> bool {
        unsafe {
            // Дождаться, пока порт свободен (не BSY/DRQ) и слот 0 не занят.
            for _ in 0..1_000_000 {
                if rd(self.port + PX_TFD) & (TFD_BSY | TFD_DRQ) == 0
                    && rd(self.port + PX_CI) & 1 == 0
                {
                    break;
                }
            }

            // Шапка команды, слот 0: CFL=5 dword (Register H2D FIS = 20 байт), W-бит, PRDTL=1.
            let ch = crate::frame::ptr(self.frame_a) as *mut u32;
            let flags = 5u32 | if write { 1 << 6 } else { 0 } | (1u32 << 16); // PRDTL=1 в [31:16]
            ch.add(0).write_volatile(flags);
            ch.add(1).write_volatile(0); // PRDBC
            ch.add(2).write_volatile((self.frame_a + OFF_CT) as u32); // CTBA
            ch.add(3).write_volatile(((self.frame_a + OFF_CT) as u64 >> 32) as u32);

            // Таблица команд: CFIS (Register H2D FIS).
            let cfis = crate::frame::ptr(self.frame_a + OFF_CT);
            core::ptr::write_bytes(cfis, 0, 0x40);
            cfis.add(0).write_volatile(0x27); // FIS type: Register H2D
            cfis.add(1).write_volatile(0x80); // C=1 (команда)
            cfis.add(2).write_volatile(cmd);
            cfis.add(4).write_volatile(lba as u8);
            cfis.add(5).write_volatile((lba >> 8) as u8);
            cfis.add(6).write_volatile((lba >> 16) as u8);
            cfis.add(7).write_volatile(0x40); // device: LBA-режим
            cfis.add(8).write_volatile((lba >> 24) as u8);
            cfis.add(9).write_volatile((lba >> 32) as u8);
            cfis.add(10).write_volatile((lba >> 40) as u8);
            // count: 1 сектор (для IDENTIFY поле игнорируется).
            cfis.add(12).write_volatile(1);
            cfis.add(13).write_volatile(0);

            // PRDT[0]: адрес буфера + число байт-1, бит I не ставим (опрашиваем).
            let prdt = crate::frame::ptr(self.frame_a + OFF_PRDT) as *mut u32;
            prdt.add(0).write_volatile(self.buf as u32);
            prdt.add(1).write_volatile((self.buf as u64 >> 32) as u32);
            prdt.add(2).write_volatile(0);
            prdt.add(3).write_volatile((SECTOR - 1) as u32); // DBC = 512-1

            // Гарантировать, что структуры записаны ДО выдачи команды (DMA на x86 когерентен).
            compiler_fence(Ordering::SeqCst);

            // Выдать команду (слот 0) и опрашивать до завершения либо ошибки.
            wr(self.port + PX_IS, 0xffff_ffff);
            wr(self.port + PX_CI, 1);
            for _ in 0..10_000_000 {
                let ci = rd(self.port + PX_CI);
                let is = rd(self.port + PX_IS);
                if is & IS_TFES != 0 {
                    return false; // ошибка задачи (task file error)
                }
                if ci & 1 == 0 {
                    // Команда снята — успех, если в статусе нет ERR.
                    return rd(self.port + PX_TFD) & TFD_ERR == 0;
                }
            }
            false // таймаут
        }
    }
}

/// Ёмкость store в 512-байтных секторах (раздел p2, если диск разбит, иначе весь диск).
pub fn capacity_sectors() -> u64 {
    AHCI.lock().as_ref().map_or(0, |d| d.capacity)
}

/// Веха 48 — полная ёмкость физического диска в секторах (нужна установщику для разметки).
pub fn total_sectors() -> u64 {
    AHCI.lock().as_ref().map_or(0, |d| d.total)
}

/// Прочитать сектор `sector` store'а в `buf` (со смещением раздела `base`).
pub fn read(sector: u64, buf: &mut [u8; SECTOR]) -> bool {
    let g = AHCI.lock();
    let Some(d) = g.as_ref() else { return false };
    if !d.command(ATA_READ_DMA_EXT, d.base + sector, false) {
        return false;
    }
    unsafe { core::ptr::copy_nonoverlapping(crate::frame::ptr(d.buf) as *const u8, buf.as_mut_ptr(), SECTOR) };
    true
}

/// Записать `buf` в сектор `sector` store'а (со смещением раздела `base`).
pub fn write(sector: u64, buf: &[u8; SECTOR]) -> bool {
    let g = AHCI.lock();
    let Some(d) = g.as_ref() else { return false };
    unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), crate::frame::ptr(d.buf), SECTOR) };
    d.command(ATA_WRITE_DMA_EXT, d.base + sector, true)
}

