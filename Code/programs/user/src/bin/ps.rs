//! `ps` — перечислить ЖИВЫЕ процессы (Веха 153, CLI-харнесс диспетчера задач [[task-manager]]).
//!
//! Не «кто это?», а «что оно такое и откуда»: ядро знает точно content-id ОБРАЗА (что именно
//! исполняется — подделать нельзя, другой код → другой хэш), происхождение (СИСТЕМНЫЙ — поднят
//! init'ом из конфига поколения — или ПОЛЬЗОВАТЕЛЬСКИЙ), родителя и состояние. Имя показываем
//! рядом, но удостоверением оно у нас не является.
//!
//! Сам список процессов — под правом (`sysview`): без него `ps` молчит, ambient-доступа к «что
//! запущено» в VOID нет (в отличие от `/proc`, который читает кто угодно). Право ищется среди
//! стартовых прав по ВИДУ (13 = Sysview), а не по позиции.
#![no_std]
#![no_main]

use void_user as sys;

const REC: usize = sys::PROC_REC; // 64 байта на запись

/// Напечатать строку (UTF-8): `b"…"` не держит кириллицу, поэтому через `as_bytes`.
fn w(s: &str) {
    sys::write(s.as_bytes());
}

/// Напечатать десятичное число (без выделений — no_std, без форматтера).
fn put_u16(mut v: u16) {
    if v == 0 {
        sys::write(b"0");
        return;
    }
    let mut buf = [0u8; 5];
    let mut i = buf.len();
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    sys::write(&buf[i..]);
}

/// Напечатать байты как hex (для короткого префикса content-id).
fn put_hex(bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for &b in bytes {
        sys::write(&[HEX[(b >> 4) as usize], HEX[(b & 0xf) as usize]]);
    }
}

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    // Найти СВОЙ cap обзора среди стартовых прав по виду (Sysview = 13). Порядок прав задаёт
    // конфиг, поэтому перебираем, а не берём start_cap(0) (та же ловушка, что закрыл probe).
    let mut sysview = sys::NO_CAP;
    let mut i = 0;
    while i < 16 {
        let c = sys::start_cap(i);
        if c == sys::NO_CAP {
            break;
        }
        if let Some((kind, _rights)) = sys::cap_info(c) {
            if kind == 13 {
                sysview = c;
                break;
            }
        }
        i += 1;
    }
    if sysview == sys::NO_CAP {
        w("ps: нет права обзора (sysview) — список процессов недоступен\n");
        sys::exit(1);
    }

    // Процессов у нас единицы; 48 записей с запасом.
    let mut buf = [0u8; REC * 48];
    let total = match sys::proc_list(sysview, &mut buf) {
        Some(t) => t,
        None => {
            w("ps: отказ обзора (право без READ?)\n");
            sys::exit(1);
        }
    };
    let shown = if total < buf.len() / REC { total } else { buf.len() / REC };

    // Состояния — те же коды, что в ядре (SYS_PROC_LIST).
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
        put_u16(pid);
        w("   ");
        if ppid == 0xFFFF {
            w("  -");
        } else {
            put_u16(ppid);
        }
        w("   ");
        w(if flags & 0x01 != 0 { "сист " } else { "польз" });
        w("  ");
        w(states.get(state).copied().unwrap_or("?"));
        w("   ");
        if flags & 0x04 != 0 {
            put_hex(&r[8..16]); // первые 8 байт content-id
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
    sys::exit(0);
}
