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

/// Инициализировать AHCI-диск, если он есть. `true` — готов к чтению/записи.
pub fn init() -> bool {
    let Some((abar, port_num)) = crate::arch::probe_ahci() else {
        return false;
    };
    let port = abar + 0x100 + port_num as usize * 0x80;
    let Some(frame_a) = frame::alloc() else { return false };
    let Some(buf) = frame::alloc() else { return false };

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

    let mut dev = Ahci { port, frame_a, buf, capacity: 0, base: 0, total: 0 };

    // 4) IDENTIFY DEVICE → полная ёмкость диска (LBA48 в словах 100..103, иначе LBA28 в 60..61).
    if !dev.command(ATA_IDENTIFY, 0, false) {
        return false;
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

    if !ours {
        dev.capacity = 0; // не носитель: ни одного сектора store на этом диске нет
    }
    *AHCI.lock() = Some(dev);
    if !ours {
        crate::println!(
            "  [blk]  SATA-диск ({} секторов) — БЕЗ раздела VOID, не трогаем его; поставить систему: `install`",
            dev_total(),
        );
    }
    ours
}

/// Полная ёмкость подключённого диска — для сообщения выше (устройство уже под замком).
fn dev_total() -> u64 {
    AHCI.lock().as_ref().map_or(0, |d| d.total)
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

/// Веха 48 — АБСОЛЮТНАЯ запись сектора диска (БЕЗ смещения раздела) — для установщика:
/// он кладёт загрузочный образ (MBR, ядро, GRUB) в начало ДИСКА, а не в раздел store.
pub fn write_abs(sector: u64, buf: &[u8; SECTOR]) -> bool {
    let g = AHCI.lock();
    let Some(d) = g.as_ref() else { return false };
    unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), crate::frame::ptr(d.buf), SECTOR) };
    d.command(ATA_WRITE_DMA_EXT, sector, true)
}

/// Веха 48 — АБСОЛЮТНОЕ чтение сектора диска (для установщика: правка таблицы разделов в MBR).
pub fn read_abs(sector: u64, buf: &mut [u8; SECTOR]) -> bool {
    let g = AHCI.lock();
    let Some(d) = g.as_ref() else { return false };
    if !d.command(ATA_READ_DMA_EXT, sector, false) {
        return false;
    }
    unsafe { core::ptr::copy_nonoverlapping(crate::frame::ptr(d.buf) as *const u8, buf.as_mut_ptr(), SECTOR) };
    true
}
