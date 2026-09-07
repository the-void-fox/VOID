//! Носитель store В ПАМЯТИ — образ диска, привезённый загрузчиком (Веха 171).
//!
//! ## Зачем
//!
//! VOID грузится с ISO, у которого нет ни одного записываемого носителя: компакт-диск только
//! читается, а жёсткого диска у гостя может не быть вовсе. До этого модуля такая загрузка
//! доходила до рабочего стола и там ломалась — коммит store не мог состояться, и консоль
//! заливало отказами записи. Это не мелочь оформления: **store и есть система**; поколения,
//! конфиг, установленные программы — всё живёт в нём, и машина, которая не может его записать,
//! не умеет ни `rebuild`, ни `switch`, ни поставить пакет.
//!
//! Модуль отдаёт store обычный носитель — тот же самый образ диска, который GRUB уже привозит
//! в оперативной памяти командой `module2` (Веха 48: из него `install` разворачивает VOID на
//! SATA). Раскладка у него настоящая: MBR, p1 с загрузчиком и ядром, p2 под store. Поэтому
//! **живой носитель и источник установки — один и тот же файл**, и расходиться им негде.
//!
//! ## Чем он честно ХУЖЕ диска
//!
//! Записанное живёт до перезагрузки. Это единственное отличие, и врать про него нельзя: человек,
//! настроивший систему на живом носителе, обязан узнать, что настройка исчезнет, — от системы, а
//! не опытным путём. Поэтому загрузка говорит это вслух ([`crate::main`]), а `install` остаётся
//! ровно тем, чем был: способом превратить сеанс в постоянный.
//!
//! ## Почему не «просто кэш в RAM без носителя»
//!
//! Потому что тогда пришлось бы завести второй код пути записи — «как будто записали». Он был бы
//! непроверяемым (никто не сравнит его с настоящим) и разошёлся бы с диском на первой же правке
//! формата. Здесь же путь ОДИН: те же сектора, тот же суперблок, та же проверка целостности.
//! Разница ровно в том, куда ложится сектор, — и это восемь строк ниже.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use void_store::SECTOR;

/// Тип MBR-раздела под store VOID — тот же байт, по которому свой раздел ищет AHCI.
use crate::ahci::VOID_STORE_TYPE;

/// Физический адрес первого сектора store внутри образа (0 — носителя нет).
static BASE: AtomicUsize = AtomicUsize::new(0);
/// Сколько секторов у store на этом носителе.
static SECTORS: AtomicU64 = AtomicU64::new(0);

/// Подключить образ, привезённый загрузчиком, как носитель store. `false` — модуля нет или он
/// не похож на диск VOID.
///
/// Ищем раздел так же, как [`crate::ahci`]: подпись `0x55AA`, затем запись таблицы с типом
/// [`VOID_STORE_TYPE`]. Без MBR носитель не принимаем вовсе — на живом носителе «весь образ с
/// нулевого сектора» означало бы, что store пишет поверх собственного загрузчика и ядра.
pub fn init() -> bool {
    let Some((mbase, mlen)) = crate::arch::boot_module() else {
        return false;
    };
    if mlen < 2 * SECTOR {
        return false;
    }
    // Веха 87: загрузчик сообщает ФИЗИЧЕСКИЙ адрес — читаем через прямую карту.
    let img = unsafe { core::slice::from_raw_parts(crate::frame::ptr(mbase), mlen) };
    if img[510] != 0x55 || img[511] != 0xaa {
        return false;
    }
    let le32 = |o: usize| {
        img[o] as u64
            | (img[o + 1] as u64) << 8
            | (img[o + 2] as u64) << 16
            | (img[o + 3] as u64) << 24
    };
    for i in 0..4 {
        let e = 446 + i * 16;
        if img[e + 4] != VOID_STORE_TYPE {
            continue;
        }
        let start = le32(e + 8) as usize;
        let count = le32(e + 12);
        // Раздел обязан целиком лежать внутри привезённого образа: `module2` мог привезти
        // усечённый файл, и обнаружить это на первой же записи в хвост — значит обнаружить
        // порчей чужой памяти.
        let end = start.saturating_add(count as usize).saturating_mul(SECTOR);
        if start == 0 || count == 0 || end > mlen {
            return false;
        }
        BASE.store(mbase + start * SECTOR, Ordering::Relaxed);
        SECTORS.store(count, Ordering::Relaxed);
        return true;
    }
    false
}

/// Ёмкость носителя в секторах (0 — носителя нет).
pub fn capacity_sectors() -> u64 {
    SECTORS.load(Ordering::Relaxed)
}

/// Адрес сектора внутри образа, если он существует.
fn at(sector: u64) -> Option<*mut u8> {
    let base = BASE.load(Ordering::Relaxed);
    if base == 0 || sector >= SECTORS.load(Ordering::Relaxed) {
        return None;
    }
    Some(unsafe { crate::frame::ptr(base).add(sector as usize * SECTOR) })
}

/// Прочитать сектор store'а.
pub fn read(sector: u64, buf: &mut [u8; SECTOR]) -> bool {
    match at(sector) {
        Some(p) => {
            unsafe { core::ptr::copy_nonoverlapping(p, buf.as_mut_ptr(), SECTOR) };
            true
        }
        None => false,
    }
}

/// Записать сектор store'а.
pub fn write(sector: u64, buf: &[u8; SECTOR]) -> bool {
    match at(sector) {
        Some(p) => {
            unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), p, SECTOR) };
            true
        }
        None => false,
    }
}
