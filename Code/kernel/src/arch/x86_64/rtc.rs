//! Веха 86 — часы реального времени x86: CMOS RTC (MC146818 и его наследники).
//!
//! До этой вехи у VOID было только МОНОТОННОЕ время (rdtsc/rdtime с момента загрузки), а
//! «настенные часы» подделывались фиктивной базой в порте std. RTC — единственный источник,
//! который помнит дату между выключениями, и без него бессмысленна проверка сертификатов
//! (Фаза 7) и времена файлов.
//!
//! Доступ — через пару портов: в `0x70` пишем номер регистра, из `0x71` читаем значение.
//! Тонкость железа: раз в секунду чип ОБНОВЛЯЕТ регистры, и чтение в этот момент даёт кашу.
//! Защита стандартная: ждём сброса бита UIP (update in progress) в регистре A, читаем всё,
//! читаем ещё раз и сравниваем — совпало, значит обновление нас не задело.
//!
//! Формат регистров описан в регистре B: значения могут быть двоично-десятичными (BCD) или
//! обычными, часы — 24- или 12-часовыми (с битом PM в старшем разряде). Век живёт в отдельном
//! регистре (`0x32` — так его кладут QEMU и большинство прошивок; если там мусор, считаем 20xx).

use super::{inb, outb};

const CMOS_ADDR: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;

const REG_SECONDS: u8 = 0x00;
const REG_MINUTES: u8 = 0x02;
const REG_HOURS: u8 = 0x04;
const REG_DAY: u8 = 0x07;
const REG_MONTH: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_CENTURY: u8 = 0x32;
const REG_STATUS_A: u8 = 0x0a;
const REG_STATUS_B: u8 = 0x0b;

/// Бит «идёт обновление» в регистре A.
const STATUS_A_UIP: u8 = 0x80;
/// Регистр B: значения двоичные (иначе BCD).
const STATUS_B_BINARY: u8 = 0x04;
/// Регистр B: 24-часовой формат (иначе 12-часовой с битом PM).
const STATUS_B_24H: u8 = 0x02;

/// Снимок регистров времени (сырой, до преобразования формата).
#[derive(PartialEq, Eq, Clone, Copy)]
struct Raw {
    sec: u8,
    min: u8,
    hour: u8,
    day: u8,
    month: u8,
    year: u8,
    century: u8,
}

/// Прочитать регистр CMOS. Старший бит адреса — маска NMI; не трогаем его (пишем номер как есть),
/// чтобы не оставить NMI запрещённым после чтения.
fn read_reg(reg: u8) -> u8 {
    outb(CMOS_ADDR, reg);
    inb(CMOS_DATA)
}

/// Идёт ли прямо сейчас обновление регистров чипом.
fn update_in_progress() -> bool {
    read_reg(REG_STATUS_A) & STATUS_A_UIP != 0
}

/// Снять регистры одним заходом (без ожидания — вызывающий следит за UIP).
fn read_raw() -> Raw {
    Raw {
        sec: read_reg(REG_SECONDS),
        min: read_reg(REG_MINUTES),
        hour: read_reg(REG_HOURS),
        day: read_reg(REG_DAY),
        month: read_reg(REG_MONTH),
        year: read_reg(REG_YEAR),
        century: read_reg(REG_CENTURY),
    }
}

/// BCD → обычное число (0x59 → 59).
fn bcd(v: u8) -> u8 {
    (v & 0x0f) + (v >> 4) * 10
}

/// Дней от 1970-01-01 до `y-m-d` (алгоритм Хиннанта, работает для любого григорианского года).
/// Месяцы 1..12, дни 1..31.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // год внутри эры, [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 }; // март = 0 (сдвиг, чтобы 29 февраля было в конце)
    let doy = (153 * mp + 2) / 5 + d - 1; // день года от 1 марта, [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // день эры, [0, 146096]
    era * 146097 + doe - 719468 // 719468 — дней от 0000-03-01 до 1970-01-01
}

/// Прочитать настенное время из CMOS RTC → секунды Unix (UTC). `None` — часов нет
/// (регистры не отвечают) или значения бессмысленны.
///
/// Оговорка: RTC хранит ЛОКАЛЬНОЕ время, если так настроила прошивка. Часового пояса у нас нет
/// (и понятия «пользователь» тоже — [[void-no-users-root]]), поэтому трактуем как UTC: у QEMU и
/// у машин, где прошивка держит RTC в UTC, это верно; на «локальных» часах дата будет смещена на
/// пояс. Осознанный компромисс до появления настроек времени в конфиге.
pub fn unix_seconds() -> Option<u64> {
    // Дождаться окна без обновления: пропускаем начатое обновление и ждём его конца.
    for _ in 0..1_000_000 {
        if !update_in_progress() {
            break;
        }
    }
    if update_in_progress() {
        return None; // чип не отвечает — часов нет
    }

    // Два согласованных чтения подряд: если между ними тикнула секунда, читаем ещё раз.
    let mut prev = read_raw();
    let mut raw = prev;
    for _ in 0..10 {
        if update_in_progress() {
            continue;
        }
        raw = read_raw();
        if raw == prev {
            break;
        }
        prev = raw;
    }

    let b = read_reg(REG_STATUS_B);
    let binary = b & STATUS_B_BINARY != 0;
    let h24 = b & STATUS_B_24H != 0;

    let conv = |v: u8| if binary { v } else { bcd(v) };
    let sec = conv(raw.sec) as i64;
    let min = conv(raw.min) as i64;
    // В 12-часовом формате старший бит часов — признак PM; снять его ДО преобразования BCD.
    let pm = !h24 && raw.hour & 0x80 != 0;
    let mut hour = conv(raw.hour & 0x7f) as i64;
    if !h24 {
        hour = match (hour, pm) {
            (12, false) => 0,  // 12 AM = полночь
            (h, false) => h,
            (12, true) => 12,  // 12 PM = полдень
            (h, true) => h + 12,
        };
    }
    let day = conv(raw.day) as i64;
    let month = conv(raw.month) as i64;
    let year_in_century = conv(raw.year) as i64;
    // Век: обычный регистр 0x32 (BCD «20»). Мусор/ноль → считаем 21-й век.
    let century = match conv(raw.century) as i64 {
        c @ 19..=99 => c,
        _ => 20,
    };
    let year = century * 100 + year_in_century;

    // Санитарная проверка: явная чушь (чип отсутствует и читается как 0xff) — лучше признать,
    // что часов нет, чем поставить систему в 2255 год.
    if !(1970..=2200).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || min > 59
        || sec > 60
    {
        return None;
    }

    let days = days_from_civil(year, month, day);
    Some((days * 86400 + hour * 3600 + min * 60 + sec) as u64)
}
