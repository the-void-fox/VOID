//! `ps` — обзор процессов (Веха 153, CLI-харнесс диспетчера задач [[task-manager]]).
//!
//! `ps`        — список ЖИВЫХ процессов: content-id ОБРАЗА (что именно исполняется — подделать
//!               нельзя, другой код → другой хэш), происхождение (СИСТЕМНЫЙ — поднят init'ом из
//!               конфига поколения — или ПОЛЬЗОВАТЕЛЬСКИЙ), родитель, состояние. Имя рядом — для
//!               человека, удостоверением оно у нас не является.
//! `ps <pid>`  — c-space процесса ГРАФОМ: не «список прав» (успокаивающая ложь), а СВЯЗИ. Право
//!               `endpoint→P<n>` значит «может позвать процесс n» — так виден «подставной
//!               посредник» (confused deputy): нет своего права на сеть, но есть эндпоинт к тому,
//!               у кого оно есть.
//!
//! Сам обзор — под правом (`sysview`): без него `ps` молчит, ambient-доступа к «что запущено» в
//! VOID нет (в отличие от `/proc`). Право ищется среди стартовых по ВИДУ (13), не по позиции.
#![no_std]
#![no_main]

use void_user as sys;

const REC: usize = sys::PROC_REC; // 64 байта на запись процесса
const CAP_REC: usize = sys::PROC_CAP_REC; // 12 байт на запись права

/// Напечатать строку (UTF-8): `b"…"` не держит кириллицу, поэтому через `as_bytes`.
fn w(s: &str) {
    sys::write(s.as_bytes());
}

/// Напечатать десятичное число (no_std, без форматтера).
fn put_num(mut v: usize) {
    if v == 0 {
        sys::write(b"0");
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    sys::write(&buf[i..]);
}

/// Напечатать байты как hex (короткий префикс content-id).
fn put_hex(bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for &b in bytes {
        sys::write(&[HEX[(b >> 4) as usize], HEX[(b & 0xf) as usize]]);
    }
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
        13 => "sysview",
        _ => "?",
    }
}

/// Права буквами (READ 1 · WRITE 2 · GRANT 4 · SEND 8 · EXEC 16).
fn put_rights(r: u32) {
    if r == 0 {
        w("-");
        return;
    }
    let mut any = false;
    for (bit, ch) in [(1u32, "r"), (2, "w"), (16, "x"), (8, "s"), (4, "g")] {
        if r & bit != 0 {
            w(ch);
            any = true;
        }
    }
    if !any {
        w("?");
    }
}

/// Найти СВОЙ cap обзора среди стартовых прав по виду (Sysview = 13). Порядок задаёт конфиг,
/// поэтому перебираем, а не берём start_cap(0) (та же ловушка, что закрыл probe).
fn find_sysview() -> usize {
    let mut i = 0;
    while i < 16 {
        let c = sys::start_cap(i);
        if c == sys::NO_CAP {
            break;
        }
        if let Some((13, _)) = sys::cap_info(c) {
            return c;
        }
        i += 1;
    }
    sys::NO_CAP
}

/// Список процессов (без аргументов).
fn print_list(sysview: usize) {
    let mut buf = [0u8; REC * 48];
    let total = match sys::proc_list(sysview, &mut buf) {
        Some(t) => t,
        None => {
            w("ps: отказ обзора (право без READ?)\n");
            sys::exit(1);
        }
    };
    let shown = if total < buf.len() / REC { total } else { buf.len() / REC };
    let states: [&str; 10] =
        ["раб", "прм", "отв", "ввд", "exe", "join", "ftx", "irq", "сон", "мёр"];

    w(" PID  PPID  ИСТОК  СОСТ  ОБРАЗ(хэш)          ИМЯ\n");
    for k in 0..shown {
        let r = &buf[k * REC..k * REC + REC];
        let pid = u16::from_le_bytes([r[0], r[1]]);
        let ppid = u16::from_le_bytes([r[2], r[3]]);
        let flags = u16::from_le_bytes([r[4], r[5]]);
        let state = r[6] as usize;
        let nlen = (r[7] as usize).min(24);

        w(" ");
        put_num(pid as usize);
        w("   ");
        if ppid == 0xFFFF {
            w("  -");
        } else {
            put_num(ppid as usize);
        }
        w("   ");
        w(if flags & 0x01 != 0 { "сист " } else { "польз" });
        w("  ");
        w(states.get(state).copied().unwrap_or("?"));
        w("   ");
        if flags & 0x04 != 0 {
            put_hex(&r[8..16]);
            w("…  ");
        } else {
            w("(linux-пакет)     ");
        }
        sys::write(&r[40..40 + nlen]);
        w("\n");
    }
    if total > shown {
        w("ps: показаны не все процессы (буфер мал)\n");
    }
}

/// Граф прав одного процесса (`ps <pid>`).
fn print_caps(sysview: usize, pid: usize) {
    let mut buf = [0u8; CAP_REC * 96];
    let total = match sys::proc_caps(sysview, pid, &mut buf) {
        Some(t) => t,
        None => {
            w("ps: нет такого процесса или отказ обзора\n");
            sys::exit(1);
        }
    };
    let shown = if total < buf.len() / CAP_REC { total } else { buf.len() / CAP_REC };
    w("права процесса P");
    put_num(pid);
    w(" (");
    put_num(total);
    w("):\n");
    for k in 0..shown {
        let r = &buf[k * CAP_REC..k * CAP_REC + CAP_REC];
        let slot = u16::from_le_bytes([r[0], r[1]]);
        let kind = r[2];
        let rights = u32::from_le_bytes([r[4], r[5], r[6], r[7]]);
        let aux = u16::from_le_bytes([r[8], r[9]]);
        w("  слот ");
        put_num(slot as usize);
        w(": ");
        w(kind_name(kind));
        w(" [");
        put_rights(rights);
        w("]");
        if aux != 0xFFFF {
            // Ребро графа: endpoint/reply на процесс aux.
            w(" →P");
            put_num(aux as usize);
        }
        w("\n");
    }
    if total > shown {
        w("ps: показаны не все права (буфер мал)\n");
    }
    // «Что делает сейчас» — учёт IPC (Веха 153.3). Счётчики накопительные.
    if let Some(st) = sys::proc_stat(sysview, pid) {
        w("  IPC: сделал ");
        put_num(st.calls_made as usize);
        w(" вызовов (");
        put_num(st.bytes_sent as usize);
        w(" Б) · принял ");
        put_num(st.calls_recv as usize);
        w(" (");
        put_num(st.bytes_recv as usize);
        w(" Б)");
        if st.holds_screen {
            w(" · держит ЭКРАН");
        }
        w("\n");
    }
}

/// Разобрать первый аргумент как десятичный pid. `None` — аргумента нет/не число.
fn arg_pid() -> Option<usize> {
    let argv = sys::argv::Argv::take();
    let a = argv.rest().next()?;
    let mut v: usize = 0;
    let mut got = false;
    for &b in a {
        if !b.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (b - b'0') as usize;
        got = true;
    }
    got.then_some(v)
}

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let sysview = find_sysview();
    if sysview == sys::NO_CAP {
        w("ps: нет права обзора (sysview) — список процессов недоступен\n");
        sys::exit(1);
    }
    match arg_pid() {
        Some(pid) => print_caps(sysview, pid),
        None => print_list(sysview),
    }
    sys::exit(0);
}
