//! Драйвер NVMe — блочное устройство современных машин (Веха 194).
//!
//! На ноутбуке новее примерно 2016 года SATA часто просто НЕТ: диск сидит на PCIe и говорит по
//! NVMe. Без этого драйвера VOID на такой машине не загрузится вовсе — не потому, что чего-то не
//! хватает, а потому, что диска для него не существует.
//!
//! Интерфейс тот же, что у [`crate::ahci`] и [`crate::virtio_blk`]: чтение и запись 512-байтных
//! секторов store'а плюс ёмкость. Выше этого слоя разницы между носителями нет ([`crate::object`]).
//!
//! ## Почему СВОЙ драйвер, а не хостинг чужого
//!
//! Конвейер хостинга у нас есть и доказан (Вехи 192–193), но здесь он невыгоден. NVMe — открытая
//! спецификация, и вся её суть в одной фразе: **команды кладутся в кольцо в оперативной памяти, а
//! контроллеру звонят в дверной звонок**. Ни шины устройств, ни поддерживающей подсистемы; то,
//! что в Linux зовётся `nvme-core`, нужно для многоочередности, пространств имён, SMART и горячей
//! замены — ничего из этого store не спрашивает. Взять чужое значило бы притащить подсистему ради
//! двух колец.
//!
//! ## Как устроено здесь
//!
//! Одна команда за раз под общим замком, **опросом** очереди завершений — как у AHCI и по той же
//! причине: store читает и пишет синхронно, а прерывание тут экономит не время ожидания диска, а
//! такты процессора, которых у нас в избытке.
//!
//! Пять фреймов, все обнулённые, RAM отображена идентично (физический адрес = виртуальный):
//! очередь команд администратора, её очередь завершений, то же для ввода-вывода и буфер данных.
//!
//! ## Чего здесь нет
//!
//! Нескольких пространств имён (берём первое), очередей на каждое ядро, прерываний, SMART,
//! энергосбережения, размера блока кроме 512 (при другом честно отказываемся — см. [`init`]).

use core::sync::atomic::{compiler_fence, Ordering};

use crate::frame;
use crate::sync::SpinLock;

const SECTOR: usize = 512;
/// Тот же тип MBR-раздела, что ищет AHCI: след явного согласия человека отдать диск под VOID.
const VOID_STORE_TYPE: u8 = crate::ahci::VOID_STORE_TYPE;

/// Регистры контроллера (смещения от BAR0).
const CAP: usize = 0x00;
const CC: usize = 0x14;
const CSTS: usize = 0x1c;
const AQA: usize = 0x24;
const ASQ: usize = 0x28;
const ACQ: usize = 0x30;
/// Дверные звонки начинаются сразу за страницей регистров.
const DOORBELL: usize = 0x1000;

/// Глубина колец. Двух хватило бы (спецификация требует минимум два элемента), но восемь так же
/// бесплатны — всё равно кольцо занимает страницу целиком.
const QD: u16 = 8;

/// Коды команд, которые мы шлём. Больше нам не нужно ни одной.
const ADMIN_CREATE_SQ: u8 = 0x01;
const ADMIN_CREATE_CQ: u8 = 0x05;
const ADMIN_IDENTIFY: u8 = 0x06;
const IO_WRITE: u8 = 0x01;
const IO_READ: u8 = 0x02;

struct Nvme {
    regs: usize,    // BAR0, отображён идентично
    stride: usize,  // шаг между дверными звонками (CAP.DSTRD)
    asq: usize,     // очередь команд администратора
    acq: usize,     // её очередь завершений
    iosq: usize,    // очередь команд ввода-вывода
    iocq: usize,    // её очередь завершений
    buf: usize,     // буфер данных (один сектор)
    admin_tail: u16,
    admin_head: u16,
    admin_phase: u8,
    io_tail: u16,
    io_head: u16,
    io_phase: u8,
    base: u64,     // LBA начала store: 0 (весь диск) или начало раздела VOID
    capacity: u64, // ёмкость store в 512-байтных секторах
    nsze: u64,     // ёмкость ВСЕГО пространства имён — ею мерит диск установщик (Веха 194.1)
}

static NVME: SpinLock<Option<Nvme>> = SpinLock::new(None);

unsafe fn r32(base: usize, off: usize) -> u32 {
    core::ptr::read_volatile((base + off) as *const u32)
}
unsafe fn w32(base: usize, off: usize, v: u32) {
    core::ptr::write_volatile((base + off) as *mut u32, v);
}
unsafe fn r64(base: usize, off: usize) -> u64 {
    core::ptr::read_volatile((base + off) as *const u64)
}
unsafe fn w64(base: usize, off: usize, v: u64) {
    core::ptr::write_volatile((base + off) as *mut u64, v);
}

impl Nvme {
    /// Положить команду в кольцо и дождаться её завершения ОПРОСОМ.
    ///
    /// `admin` различает два кольца; всё остальное у них одинаково, поэтому и код один. Возврат —
    /// поле состояния из элемента завершения: ноль значит «сделано», прочее — код ошибки, и его
    /// лучше показать, чем молча считать сектор нечитаемым.
    fn submit(&mut self, admin: bool, cmd: &[u32; 16]) -> u16 {
        let (sq, cq, qid) = if admin {
            (self.asq, self.acq, 0u16)
        } else {
            (self.iosq, self.iocq, 1u16)
        };
        let (tail, head, phase) = if admin {
            (&mut self.admin_tail, &mut self.admin_head, &mut self.admin_phase)
        } else {
            (&mut self.io_tail, &mut self.io_head, &mut self.io_phase)
        };

        // Запись команды в хвост кольца.
        unsafe {
            let slot = frame::ptr(sq).add(*tail as usize * 64) as *mut u32;
            for (i, w) in cmd.iter().enumerate() {
                core::ptr::write_volatile(slot.add(i), *w);
            }
        }
        *tail = (*tail + 1) % QD;
        compiler_fence(Ordering::SeqCst);
        // Звонок: «хвост теперь здесь».
        unsafe { w32(self.regs, DOORBELL + (2 * qid as usize) * self.stride, *tail as u32) };

        // Ожидание опросом. Признак готовности — БИТ ФАЗЫ: он меняется на каждом круге кольца,
        // поэтому отличить свежий элемент от прошлогоднего можно без всякого обнуления.
        let status;
        loop {
            let dw3 = unsafe {
                core::ptr::read_volatile(
                    (frame::ptr(cq).add(*head as usize * 16) as *const u32).add(3),
                )
            };
            if ((dw3 >> 16) & 1) as u8 == *phase {
                status = ((dw3 >> 17) & 0x7fff) as u16;
                break;
            }
            core::hint::spin_loop();
        }
        *head = (*head + 1) % QD;
        if *head == 0 {
            *phase ^= 1; // круг замкнулся — ждём противоположную фазу
        }
        compiler_fence(Ordering::SeqCst);
        unsafe { w32(self.regs, DOORBELL + (2 * qid as usize + 1) * self.stride, *head as u32) };
        status
    }
}

/// Собрать команду: код, пространство имён, буфер и четыре слова, зависящих от команды.
fn cmd(op: u8, nsid: u32, prp1: u64, cdw10: u32, cdw11: u32, cdw12: u32) -> [u32; 16] {
    let mut c = [0u32; 16];
    c[0] = op as u32; // идентификатор команды оставляем нулевым: она одна за раз
    c[1] = nsid;
    c[6] = prp1 as u32;
    c[7] = (prp1 >> 32) as u32;
    c[10] = cdw10;
    c[11] = cdw11;
    c[12] = cdw12;
    c
}

/// Номер, под которым NVMe-диски называются установщику (Веха 194.1).
///
/// Слоты 0..[`crate::arch::MAX_DISKS`] заняты портами AHCI, и смешивать их нельзя: человек
/// выбирает диск номером, а номер обязан значить одно и то же до и после перезагрузки. Отсюда
/// отдельная сотня — она же сразу видна в журнале, если что-то пойдёт не так.
pub const SLOT_BASE: usize = 100;

/// Цель установки — второй контроллер (или тот же, но пока свободный). Держится отдельно от
/// носителя store по той же причине, что у AHCI: ставить на диск, с которого работаешь, — не
/// установка, а потеря.
static TARGET: SpinLock<Option<Nvme>> = SpinLock::new(None);

/// Поднять контроллер: кольца администратора, кольца ввода-вывода, размер блока. Общая половина
/// носителя store и цели установки — две копии этого кода разошлись бы на первой же правке.
///
/// `Ok(устройство)` — контроллер отвечает, блок 512 Б, очереди созданы, `nsze` заполнено.
/// Смещение раздела вызывающий выясняет сам: носителю нужен раздел VOID, установщику — весь диск.
fn bringup() -> Result<Nvme, &'static str> {
    let Some(regs) = crate::arch::probe_nvme() else {
        return Err("контроллера NVMe на этой машине нет");
    };
    // Пять фреймов: четыре кольца и буфер. Освобождать их некому и не надо — драйвер живёт,
    // пока живёт машина.
    let (Some(asq), Some(acq), Some(iosq), Some(iocq), Some(buf)) = (
        frame::alloc(),
        frame::alloc(),
        frame::alloc(),
        frame::alloc(),
        frame::alloc(),
    ) else {
        return Err("нет памяти под кольца");
    };

    let cap = unsafe { r64(regs, CAP) };
    let stride = 4usize << ((cap >> 32) & 0xf);
    // Минимальный размер страницы контроллера. Мы работаем страницами 4 КиБ, и если контроллер
    // такого не умеет — отказываемся вслух, а не пишем мимо.
    if (cap >> 48) & 0xf != 0 {
        return Err("контроллеру мало 4 КиБ на страницу");
    }

    let mut d = Nvme {
        regs,
        stride,
        asq,
        acq,
        iosq,
        iocq,
        buf,
        admin_tail: 0,
        admin_head: 0,
        admin_phase: 1,
        io_tail: 0,
        io_head: 0,
        io_phase: 1,
        base: 0,
        capacity: 0,
        nsze: 0,
    };

    unsafe {
        // Выключить, дождаться готовности к настройке.
        w32(regs, CC, 0);
        while r32(regs, CSTS) & 1 != 0 {
            core::hint::spin_loop();
        }
        // Кольца администратора: размеры и адреса.
        w32(regs, AQA, ((QD as u32 - 1) << 16) | (QD as u32 - 1));
        w64(regs, ASQ, asq as u64);
        w64(regs, ACQ, acq as u64);
        // Включить: элемент команды 64 байта (2^6), элемент завершения 16 (2^4), страница 4 КиБ.
        w32(regs, CC, (6 << 16) | (4 << 20) | 1);
        while r32(regs, CSTS) & 1 == 0 {
            if r32(regs, CSTS) & 2 != 0 {
                return Err("контроллер сообщил о неисправности при включении");
            }
            core::hint::spin_loop();
        }
    }

    // Опознать пространство имён 1: размер в блоках и размер блока.
    if d.submit(true, &cmd(ADMIN_IDENTIFY, 1, buf as u64, 0, 0, 0)) != 0 {
        return Err("identify пространства имён отказал");
    }
    let (nsze, lba_bytes) = unsafe {
        let p = frame::ptr(buf);
        let nsze = core::ptr::read_unaligned(p as *const u64);
        // FLBAS (байт 26) выбирает формат из таблицы с байта 128; в каждом формате LBADS —
        // байт 2, и это СТЕПЕНЬ ДВОЙКИ, а не размер.
        let flbas = (core::ptr::read_volatile(p.add(26)) & 0xf) as usize;
        let lbads = core::ptr::read_volatile(p.add(128 + flbas * 4 + 2));
        (nsze, 1usize << lbads)
    };
    if lba_bytes != SECTOR {
        // Трансляция 512 ↔ 4096 — это чтение-правка-запись и отдельный разговор про атомарность.
        // Пока такого диска под рукой нет, честнее отказаться, чем сделать вид.
        return Err("блок не 512 Б — такой диск пока не наш");
    }

    // Кольца ввода-вывода. Порядок обязателен: очередь завершений создаётся ПЕРВОЙ, иначе
    // контроллеру некуда сложить ответ о создании очереди команд.
    let qsz = (QD as u32 - 1) << 16;
    if d.submit(true, &cmd(ADMIN_CREATE_CQ, 0, iocq as u64, qsz | 1, 1, 0)) != 0 {
        return Err("не создалась очередь завершений");
    }
    if d.submit(true, &cmd(ADMIN_CREATE_SQ, 0, iosq as u64, qsz | 1, (1 << 16) | 1, 0)) != 0 {
        return Err("не создалась очередь команд");
    }
    d.nsze = nsze;
    Ok(d)
}

/// Найти контроллер, поднять его и выбрать раздел store. `false` — NVMe на этой машине нет либо
/// диск не отдан VOID.
pub fn init() -> bool {
    let mut d = match bringup() {
        Ok(v) => v,
        Err(e) => {
            // «Контроллера нет» — не событие: на большинстве машин его и не должно быть.
            if e != "контроллера NVMe на этой машине нет" {
                crate::println!("  [nvme] {}", e);
            }
            return false;
        }
    };
    let buf = d.buf;

    // Где на этом диске store. Правило то же, что у AHCI (Веха 174): носителем становится ТОЛЬКО
    // диск с разделом VOID — он и есть след явного согласия человека, а «нет таблицы разделов,
    // значит весь диск наш» на чужой машине означало бы затереть чужие данные.
    if d.submit(false, &cmd(IO_READ, 1, buf as u64, 0, 0, 0)) != 0 {
        crate::println!("  [nvme] первый сектор не читается");
        return false;
    }
    let mut found = false;
    unsafe {
        let p = frame::ptr(buf);
        let sig = core::ptr::read_unaligned(p.add(510) as *const u16);
        if sig == 0xaa55 {
            for i in 0..4 {
                let e = p.add(446 + i * 16);
                if core::ptr::read_volatile(e.add(4)) == VOID_STORE_TYPE {
                    d.base = core::ptr::read_unaligned(e.add(8) as *const u32) as u64;
                    d.capacity = core::ptr::read_unaligned(e.add(12) as *const u32) as u64;
                    found = true;
                    break;
                }
            }
        }
    }
    if !found {
        crate::println!(
            "  [nvme] диск на {} секторов есть, раздела VOID на нём нет — не трогаем (поставить систему: `install`)",
            d.nsze,
        );
        // Веха 194.1: поднятый контроллер не выбрасываем, а отдаём установщику. Второй `bringup`
        // сбросил бы его заново и занял ещё пять фреймов — а это ровно тот случай, ради которого
        // установщик и нужен: живой ISO плюс пустой NVMe.
        *TARGET.lock() = Some(d);
        return false;
    }

    *NVME.lock() = Some(d);
    true
}

/// Ёмкость store в 512-байтных секторах.
pub fn capacity_sectors() -> u64 {
    NVME.lock().as_ref().map_or(0, |d| d.capacity)
}

/// Прочитать сектор store'а (со смещением раздела).
pub fn read(sector: u64, buf: &mut [u8; SECTOR]) -> bool {
    let mut g = NVME.lock();
    let Some(d) = g.as_mut() else { return false };
    let lba = d.base + sector;
    let dbuf = d.buf;
    let st = d.submit(false, &cmd(IO_READ, 1, dbuf as u64, lba as u32, (lba >> 32) as u32, 0));
    if st != 0 {
        return false;
    }
    unsafe { core::ptr::copy_nonoverlapping(frame::ptr(dbuf) as *const u8, buf.as_mut_ptr(), SECTOR) };
    true
}

/// Записать сектор store'а (со смещением раздела).
pub fn write(sector: u64, buf: &[u8; SECTOR]) -> bool {
    let mut g = NVME.lock();
    let Some(d) = g.as_mut() else { return false };
    let lba = d.base + sector;
    let dbuf = d.buf;
    unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), frame::ptr(dbuf), SECTOR) };
    d.submit(false, &cmd(IO_WRITE, 1, dbuf as u64, lba as u32, (lba >> 32) as u32, 0)) == 0
}

// ─── установщик (Веха 194.1) ─────────────────────────────────────────────────────

/// Перечислить NVMe-диски для установщика. Возвращает, сколько записано в `out`.
///
/// Контроллер сейчас берётся один — тот, что нашла проба. Машины с двумя NVMe бывают, но список
/// дисков и так собирается по обеим шинам, и заводить перечисление контроллеров ради второго
/// диска, которого мы пока не видели, значило бы писать код под догадку.
pub fn disks(out: &mut [crate::ahci::Disk]) -> usize {
    if out.is_empty() {
        return 0;
    }
    // Диск, на котором РАБОТАЕТ store, поднимать второй раз нельзя: `bringup` сбрасывает
    // контроллер, а это выбило бы у store носитель из-под рук. Всё нужное про него мы знаем.
    if let Some(d) = NVME.lock().as_ref() {
        out[0] = crate::ahci::Disk {
            slot: SLOT_BASE,
            sectors: d.nsze,
            model: model_name(),
            void: true,
            live: true,
        };
        return 1;
    }
    if TARGET.lock().is_none() {
        // Контроллер ещё не поднимали (например, `init` не нашёл NVMe вовсе — тогда и здесь
        // не найдёт, и список просто останется без этой строки).
        match bringup() {
            Ok(d) => *TARGET.lock() = Some(d),
            Err(_) => return 0,
        }
    }
    let g = TARGET.lock();
    let d = g.as_ref().expect("только что положили");
    out[0] = crate::ahci::Disk {
        slot: SLOT_BASE,
        sectors: d.nsze,
        model: model_name(),
        // Раздел VOID на этом диске искал `init`; нашёл бы — диск стал бы носителем store и
        // сюда мы бы не дошли. Значит пометки «здесь уже есть VOID» здесь быть не может.
        void: false,
        live: false,
    };
    1
}

/// Как диск называется в списке. Настоящую модель отдаёт `Identify Controller`, и однажды её
/// стоит прочитать; пока честнее показать род устройства, чем пустую строку.
fn model_name() -> [u8; crate::ahci::MODEL_LEN] {
    let mut m = [b' '; crate::ahci::MODEL_LEN];
    let name = b"NVMe";
    m[..name.len()].copy_from_slice(name);
    m
}

/// Открыть NVMe как ЦЕЛЬ установки. `false` — контроллера нет либо с него работает система.
pub fn target_open(slot: usize) -> bool {
    if slot != SLOT_BASE {
        return false;
    }
    if NVME.lock().is_some() {
        return false; // ставить на диск, с которого работаем, нельзя
    }
    if TARGET.lock().is_some() {
        return true; // уже открыт перечислением
    }
    match bringup() {
        Ok(d) => {
            *TARGET.lock() = Some(d);
            true
        }
        Err(e) => {
            crate::println!("  [nvme] цель установки не открылась: {}", e);
            false
        }
    }
}

/// Полная ёмкость цели в секторах — ёмкость ПРОСТРАНСТВА ИМЁН, а не раздела: установщик
/// размечает диск целиком.
pub fn target_sectors() -> u64 {
    TARGET.lock().as_ref().map_or(0, |d| d.nsze)
}

/// Абсолютная запись сектора на цель установки (без смещения раздела).
pub fn target_write(sector: u64, buf: &[u8; SECTOR]) -> bool {
    let mut g = TARGET.lock();
    let Some(d) = g.as_mut() else { return false };
    let dbuf = d.buf;
    unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), frame::ptr(dbuf), SECTOR) };
    d.submit(false, &cmd(IO_WRITE, 1, dbuf as u64, sector as u32, (sector >> 32) as u32, 0)) == 0
}

/// Абсолютное чтение сектора с цели установки.
pub fn target_read(sector: u64, buf: &mut [u8; SECTOR]) -> bool {
    let mut g = TARGET.lock();
    let Some(d) = g.as_mut() else { return false };
    let dbuf = d.buf;
    if d.submit(false, &cmd(IO_READ, 1, dbuf as u64, sector as u32, (sector >> 32) as u32, 0)) != 0 {
        return false;
    }
    unsafe { core::ptr::copy_nonoverlapping(frame::ptr(dbuf) as *const u8, buf.as_mut_ptr(), SECTOR) };
    true
}
