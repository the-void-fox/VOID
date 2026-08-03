//! `stdio-demo` — проверка чужого stdio (Веха 98, вторая половина шага 4).
//!
//! Доказывает то, без чего мультиплексор невозможен: **вывод ребёнка приходит РОДИТЕЛЮ**, а не
//! в общую консоль ядра. Ребёнок при этом обычный, ничего не знающий о хосте: он зовёт
//! `sys::write`, а маршрут выбирает библиотека по соглашению ([[process-stdio]]).
//!
//! Здесь видно, что перехват настоящий: каждая пойманная строка печатается с пометкой
//! `[поймано]`. Если бы соглашение не сработало, строки ребёнка появились бы БЕЗ пометки —
//! прямо из консоли ядра.

#![no_std]
#![no_main]

use void_user as sys;
use void_user::{stdio, Wait};

/// Ребёнок — обычная программа, ничего не знающая про хост.
const CHILD: &[u8] = b"bin/hello";

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let exec_cap = sys::start_cap(1);

    // Право на СЕБЯ — то, что мы отдадим ребёнку как адрес его stdio.
    let me = sys::self_endpoint();
    if me == sys::NO_CAP {
        sys::write_console("[stdio-demo] ОШИБКА: ядро не выдало право на себя\n".as_bytes());
        sys::exit(1);
    }

    sys::write_console("[stdio-demo] запускаю ребёнка со СВОИМ stdio\n".as_bytes());
    let Some(child) = sys::spawn_with_stdio(exec_cap, CHILD, b"", me) else {
        sys::write_console("[stdio-demo] ОШИБКА: запустить не удалось\n".as_bytes());
        sys::exit(1);
    };

    // Реактор: принимаем вывод ребёнка, пока он не закончится. Печатаем СВОИМ путём
    // (`write_console`) — иначе ушли бы в собственный stdio, которого у нас нет, и запутали бы
    // проверку.
    let mut buf = [0u8; stdio::CHUNK];
    let mut caught = 0usize;
    let mut idle = 0usize;
    loop {
        if let Some(m) = sys::try_recv(&mut buf) {
            if m.op == stdio::OP_STDOUT {
                caught += 1;
                sys::write_console("[поймано] ".as_bytes());
                sys::write_console(&buf[..m.len.min(buf.len())]);
                sys::reply(m.reply_cap, &[]);
            } else {
                // Чужой запрос — вежливо отказать, а не молчать: молчание повесило бы вызвавшего.
                sys::reply(m.reply_cap, &[]);
            }
            idle = 0;
            continue;
        }
        // Ребёнок мог закончиться — проверяем НЕ блокируясь, иначе пропустили бы его последние
        // строки, уснув в ожидании кода выхода.
        if let Wait::Exited(code) = sys::wait(child, true) {
            let mut out = [0u8; 96];
            let n = report(&mut out, caught, code);
            sys::write_console(&out[..n]);
            sys::write_console(if caught > 0 {
                "[stdio-demo] чужой stdio РАБОТАЕТ\n".as_bytes()
            } else {
                "[stdio-demo] ОШИБКА: вывод ребёнка прошёл мимо\n".as_bytes()
            });
            sys::exit(if caught > 0 { 0 } else { 1 });
        }
        idle += 1;
        if idle > 1_000_000 {
            sys::write_console("[stdio-demo] ОШИБКА: ребёнок не отвечает\n".as_bytes());
            sys::exit(1);
        }
        sys::yield_now();
    }
}

/// Отчёт без alloc — кучи у этой программы нет.
fn report(out: &mut [u8], caught: usize, code: usize) -> usize {
    let mut n = 0;
    let mut put = |s: &[u8], out: &mut [u8], n: &mut usize| {
        for &b in s {
            if *n < out.len() {
                out[*n] = b;
                *n += 1;
            }
        }
    };
    put("[stdio-demo] перехвачено посылок: ".as_bytes(), out, &mut n);
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    let mut v = caught;
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    put(&buf[i..], out, &mut n);
    put(", код выхода ребёнка: ".as_bytes(), out, &mut n);
    put(if code == 0 { b"0" } else { b"!=0" }, out, &mut n);
    put(b"\n", out, &mut n);
    n
}
