//! `probe` — **зонд конфайнмента** (Веха 152): проверяет, держит ли capability-модель.
//!
//! Не «а вдруг сломается», как [`hostile`](../hostile.rs) на границе syscall'а, а вопрос уровнем
//! выше: **может ли процесс дотянуться до права, которого ему не выдавали?** Это и есть тезис
//! VOID ([[0002-persistent-content-addressed-capability-core]]) — обладание capability И ЕСТЬ
//! право, подделать дескриптор нельзя. Зонд ставит это под сомнение перебором.
//!
//! ## Оракул — «достижимая ВЛАСТЬ против выданной» (Веха 152.2)
//!
//! Дескриптор `Cap` это `(слот, поколение)`, и ядро проверяет его по таблице СВОЕГО домена.
//! Зонд перебирает слоты и поколения и у каждого ЖИВОГО спрашивает `SYS_CAP_INFO` — вид цели и
//! права (read-only, без побочного эффекта, без порчи c-space). Потом сравнивает НЕ числа слотов,
//! а ВЛАСТЬ: набор `(вид, права)` достижимого против выданного (`SYS_STARTCAP`).
//!
//! Это и отвечает на «дыра или особенность»:
//! - лишние слоты ТОЙ ЖЕ власти (второй `store:r`, который и так есть) — **DUP**, накопление, не
//!   эскалация;
//! - власть СВЕРХ выданной (POWER, запись, сеть, которых не давали) — **ESCALATION**, находка.
//!
//! ## Что печатает
//!
//! Машиночитаемые строки для харнесса ([[redteam]]):
//! ```text
//! PROBE granted=<G> reachable=<R>
//! PROBE VERDICT CONFINED                — достижимая власть = выданной
//! PROBE VERDICT DUP +<N> (<виды>)       — лишние слоты, но той же власти (накопление)
//! PROBE VERDICT ESCALATION: <вид:права> — власть сверх выданной
//! ```
//! Признак пробоя для харнесса — `ESCALATION`; `DUP` тревогой не считается.

#![no_std]
#![no_main]

use void_user as sys;

/// Докуда перебирать слоты и поколения. `SYS_CAP_INFO` c-space не трогает (в отличие от копии
/// через derive), поэтому перебор безопасен и не гоняется за собственным хвостом — границу искать
/// не нужно. Слотов у честного процесса единицы; восемь поколений ловят отзыв-и-переиздание.
const MAX_SLOT: u32 = 64;
const MAX_GEN: u32 = 8;

/// Собрать сырые биты дескриптора из слота и поколения (`слот << 32 | поколение`).
fn cap_bits(slot: u32, generation: u32) -> usize {
    (((slot as u64) << 32) | generation as u64) as usize
}

/// Человеку — в stdio (шелл или окно).
fn say(s: &str) {
    sys::write(s.as_bytes());
}

/// Харнессу — СТРОГО в консоль ядра (serial): вердикт обязан лечь в serial-лог, чем бы ни был
/// stdout (в окне композитора serial'а нет вовсе).
fn mark(s: &str) {
    sys::write_console(s.as_bytes());
}

fn dec(mut v: usize) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    sys::write_console(&buf[i..]);
}

/// Имя вида цели — общий словарь с ядром (`cap::info_kind`).
fn kind_name(k: u8) -> &'static str {
    match k {
        1 => "store",
        2 => "root",
        3 => "value",
        4 => "endpoint",
        5 => "reply",
        6 => "blk",
        7 => "net",
        8 => "mmio",
        9 => "dma",
        10 => "power",
        11 => "shm",
        12 => "irq",
        _ => "?",
    }
}

/// Права буквами (`Rights`: READ 1 · WRITE 2 · EXEC 4 · SEND 8 · GRANT 16). В консоль ядра.
fn mark_rights(r: u32) {
    if r == 0 {
        mark("-");
        return;
    }
    for (bit, ch) in [(1u32, "r"), (2, "w"), (4, "x"), (8, "s"), (16, "g")] {
        if r & bit != 0 {
            mark(ch);
        }
    }
}

/// Одна власть: вид + права. Мелко — набор власти держим фиксированным массивом (кучи у зонда нет).
#[derive(Clone, Copy, PartialEq, Eq)]
struct Auth {
    kind: u8,
    rights: u32,
}

/// Набор власти без дублей (по паре вид+права). Ёмкости 32 хватает: видов дюжина.
struct AuthSet {
    items: [Auth; 32],
    len: usize,
}

impl AuthSet {
    fn new() -> AuthSet {
        AuthSet { items: [Auth { kind: 0, rights: 0 }; 32], len: 0 }
    }
    fn add(&mut self, a: Auth) {
        for i in 0..self.len {
            if self.items[i] == a {
                return;
            }
        }
        if self.len < self.items.len() {
            self.items[self.len] = a;
            self.len += 1;
        }
    }
    /// Покрыта ли власть `a` этим набором: есть вид `a.kind` с правами-НАДмножеством.
    fn covers(&self, a: Auth) -> bool {
        for i in 0..self.len {
            let g = self.items[i];
            if g.kind == a.kind && a.rights & !g.rights == 0 {
                return true;
            }
        }
        false
    }
}

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    // Веха 152.2 — режим `seed`: зафиксировать домен в `.cspace` (одна копия-аттенуация через
    // `SYS_CAP_DERIVE` — она персистит c-space). Нужен КОНТРОЛЬ оракула: обычный spawn c-space не
    // персистит (`endow` без persist), поэтому cross-reboot утечка сама не возникает. `seed`
    // ставит её НАРОЧНО — привилегированный тёзка оставляет своё право в домене, — чтобы
    // проверить, что зонд эскалацию ВИДИТ, а не молчит всегда.
    if sys::argv::Argv::take().str(0) == Some("seed") {
        let c = sys::start_cap(0);
        if c != sys::NO_CAP {
            sys::cap_derive(c, 0xffff); // копия с теми же правами → persist домена
        }
        say("[probe] seed: домен зафиксирован в .cspace\n");
        sys::exit(0);
    }

    say("[probe] зонд конфайнмента (Веха 152)\n");

    // 1. ВЫДАННАЯ власть. Стартовые права — то, что нам дали честно; каждый описываем `cap_info`.
    let mut granted = AuthSet::new();
    let mut gi = 0;
    let mut granted_slots = 0usize;
    loop {
        let c = sys::start_cap(gi);
        if c == sys::NO_CAP {
            break;
        }
        if let Some((kind, rights)) = sys::cap_info(c) {
            granted.add(Auth { kind, rights });
            granted_slots += 1;
        }
        gi += 1;
    }

    // 2. ДОСТИЖИМАЯ власть. Перебор слот×поколение; у каждого живого — `cap_info`. Побочного
    //    эффекта нет, c-space не растёт, границу искать не нужно. Слот жив ровно в одном поколении
    //    — нашли, записали, дальше по нему смысла нет.
    let mut reachable = AuthSet::new();
    let mut reachable_slots = 0usize;
    // Достижимая власть СВЕРХ выданной — она и есть эскалация. Считаем сразу.
    let mut escalation = AuthSet::new();
    for slot in 0..MAX_SLOT {
        for generation in 1..MAX_GEN {
            if let Some((kind, rights)) = sys::cap_info(cap_bits(slot, generation)) {
                let a = Auth { kind, rights };
                reachable.add(a);
                reachable_slots += 1;
                if !granted.covers(a) {
                    escalation.add(a);
                }
                break;
            }
        }
    }

    // 3. ВЕРДИКТ.
    mark("PROBE granted=");
    dec(granted_slots);
    mark(" reachable=");
    dec(reachable_slots);
    mark("\n");

    if escalation.len > 0 {
        // Власть, которой не давали. Это находка.
        mark("PROBE VERDICT ESCALATION:");
        for i in 0..escalation.len {
            let a = escalation.items[i];
            mark(" ");
            mark(kind_name(a.kind));
            mark(":");
            mark_rights(a.rights);
        }
        mark("\n");
        say("[probe] готово: НАЙДЕНА эскалация\n");
        sys::exit(1);
    } else if reachable_slots > granted_slots {
        // Лишние слоты, но власть та же — накопление, не эскалация.
        mark("PROBE VERDICT DUP +");
        dec(reachable_slots - granted_slots);
        mark(" (");
        for i in 0..reachable.len {
            if i > 0 {
                mark(",");
            }
            mark(kind_name(reachable.items[i].kind));
        }
        mark(")\n");
        say("[probe] готово: накопление той же власти (не эскалация)\n");
        sys::exit(0);
    } else {
        mark("PROBE VERDICT CONFINED\n");
        say("[probe] готово: конфайнмент держит\n");
        sys::exit(0);
    }
}
