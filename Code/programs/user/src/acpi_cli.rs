//! Веха 240 — протокол сервера ACPI и клиентская обёртка: **состояние питания**.
//!
//! Одно место, где записан формат сообщений к `acpi-srv`; зеркало на стороне сервера —
//! `Code/programs/acpi-srv/power.h`. Два файла, потому что стороны на разных языках; один
//! формат, потому что иначе они разъедутся молча — ровно так уже расходились номера операций
//! у звука (см. [`crate::snd_cli`]).
//!
//! Право спросить о питании — это **cap на эндпоинт `acpi-srv`** (`endpoint:acpi-srv` в
//! конфиге системы). Своего отдельного права у «батареи» нет, и канал НЕ помечен «не
//! наследуется»: заряд — то же, что часы, его видно на панели и знать его может всякий, кто
//! смотрит на экран. Прятать за правом число, которое и так нарисовано в углу, значит не
//! защитить, а усложнить (тот же довод, что у звука).
//!
//! Спрашивать можно часто: сервер держит СНИМОК и обновляет его не чаще раза в секунду, а
//! также сразу, когда прошивка сама сказала, что состояние изменилось.

use crate as sys;

/// «Состояние питания»: `[]` → 16 байт (см. [`Power`]).
pub const OP_POWER: usize = 1;

/// Длина ответа. Фиксированная — поля простые, а разбор переменной длины на двух языках это
/// два разных разбора.
pub const MSG_LEN: usize = 16;

pub const HAS_BATTERY: u16 = 1 << 0;
pub const HAS_PERCENT: u16 = 1 << 1;
pub const CHARGING: u16 = 1 << 2;
pub const DISCHARGING: u16 = 1 << 3;
pub const CRITICAL: u16 = 1 << 4;
pub const HAS_AC: u16 = 1 << 5;
pub const ON_AC: u16 = 1 << 6;
pub const HAS_LID: u16 = 1 << 7;
pub const LID_OPEN: u16 = 1 << 8;

/// Снимок состояния питания машины.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Power {
    pub flags: u16,
    /// Проценты заряда; значимы только при [`Power::has_percent`].
    pub percent: u8,
    /// Остаток, мВт·ч.
    pub remaining: u32,
    /// Полная ёмкость, мВт·ч (не проектная: батарея стареет).
    pub full: u32,
    /// Напряжение, мВ.
    pub voltage: u32,
}

impl Power {
    pub fn has_battery(&self) -> bool {
        self.flags & HAS_BATTERY != 0
    }
    /// Известен ли процент. Отдельно от наличия батареи: ACPI законно отвечает «неизвестно»
    /// (`0xFFFFFFFF`), и показать в этом случае ноль значило бы соврать правдоподобно.
    pub fn has_percent(&self) -> bool {
        self.flags & HAS_PERCENT != 0
    }
    pub fn charging(&self) -> bool {
        self.flags & CHARGING != 0
    }
    pub fn critical(&self) -> bool {
        self.flags & CRITICAL != 0
    }
    /// Подключён ли адаптер. `None` — адаптера машина не объявляет (настольная).
    pub fn on_ac(&self) -> Option<bool> {
        (self.flags & HAS_AC != 0).then_some(self.flags & ON_AC != 0)
    }
    /// Открыта ли крышка. `None` — крышки машина не объявляет.
    pub fn lid_open(&self) -> Option<bool> {
        (self.flags & HAS_LID != 0).then_some(self.flags & LID_OPEN != 0)
    }
    /// Есть ли что показывать человеку вообще.
    pub fn any(&self) -> bool {
        self.flags & (HAS_BATTERY | HAS_AC | HAS_LID) != 0
    }
}

/// Разобрать ответ сервера. `None` — ответ не той длины: лучше не показать ничего, чем
/// показать разобранный наугад мусор.
pub fn parse(buf: &[u8]) -> Option<Power> {
    if buf.len() < MSG_LEN {
        return None;
    }
    Some(Power {
        flags: u16::from_le_bytes([buf[0], buf[1]]),
        percent: buf[2],
        remaining: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
        full: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
        voltage: u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
    })
}

/// Спросить состояние питания. `None` — сервера нет, он не ответил или ответил не тем.
pub fn power(ep: usize) -> Option<Power> {
    if ep == sys::NO_CAP {
        return None;
    }
    let mut rsp = [0u8; MSG_LEN];
    let n = sys::call(ep, OP_POWER, &[], &mut rsp);
    (n != usize::MAX && n >= MSG_LEN).then(|| parse(&rsp)).flatten()
}

/// Канал к серверу ACPI, если он выдан (`endpoint:acpi-srv` в конфиге; дефис в имени права
/// становится подчёркиванием — см. `cap_name` в ядре).
pub fn find_cap() -> Option<usize> {
    sys::cap_named("ACPI_SRV")
}
