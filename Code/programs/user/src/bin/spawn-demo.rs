//! `spawn-demo` — проверка неблокирующего запуска (Веха 98, шаг 4 фазы терминала).
//!
//! До сих пор `SYS_EXEC` укладывал родителя спать до конца ребёнка, и это делало мультиплексор
//! невозможным: хост обязан работать, ПОКА работают дети. Здесь показано обратное — родитель
//! запускает двоих, продолжает считать и забирает коды выхода, когда те закончатся.
//!
//! Что именно доказывается:
//! 1. `SYS_SPAWN` возвращает управление сразу (родитель печатает между запусками);
//! 2. `SYS_WAIT(nonblock)` отвечает «ещё работает», не усыпляя родителя;
//! 3. коды выхода доходят и **не теряются**, если ребёнок закончился РАНЬШЕ, чем его спросили
//!    (ради этого ядро придерживает слот зомби — см. `proc.rs`).

#![no_std]
#![no_main]

use void_user as sys;
use void_user::Wait;

/// Кого запускаем: `hello` печатает строку и выходит с кодом 0 — короткая и предсказуемая жертва.
const CHILD: &[u8] = b"bin/hello";

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    // Право на запуск — стартовое (у шелла это `store:rwx`, слот 1). Берём из таблицы, а не из
    // регистра: `run` наделяет нас теми же правами, что у шелла, и их ПОРЯДОК — его дело.
    let exec_cap = sys::start_cap(1);
    sys::write("[spawn-demo] запускаю двоих БЕЗ ожидания\n".as_bytes());

    let a = sys::spawn(exec_cap, CHILD, b"");
    sys::write("[spawn-demo] первый запущен — и я всё ещё жив\n".as_bytes());
    let b = sys::spawn(exec_cap, CHILD, b"");
    sys::write("[spawn-demo] второй запущен\n".as_bytes());

    let (a, b) = match (a, b) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            sys::write("[spawn-demo] ОШИБКА: запустить не удалось\n".as_bytes());
            sys::exit(1);
        }
    };

    // Опрос без сна: пока дети работают, родитель волен заниматься своими делами — ровно то,
    // что нужно реактору мультиплексора.
    let mut polls = 0usize;
    let mut left = 2;
    let mut codes = [usize::MAX; 2];
    while left > 0 && polls < 100_000 {
        polls += 1;
        for (i, pid) in [a, b].into_iter().enumerate() {
            if codes[i] == usize::MAX {
                match sys::wait(pid, true) {
                    Wait::Exited(c) => {
                        codes[i] = c;
                        left -= 1;
                    }
                    Wait::Running => {}
                    Wait::NoChild => {
                        sys::write("[spawn-demo] ОШИБКА: ядро не признало ребёнка\n".as_bytes());
                        sys::exit(1);
                    }
                }
            }
        }
        sys::yield_now();
    }

    if left > 0 {
        sys::write("[spawn-demo] ОШИБКА: дети не завершились\n".as_bytes());
        sys::exit(1);
    }

    let mut out = [0u8; 96];
    let n = fmt(&mut out, codes[0], codes[1], polls);
    sys::write(&out[..n]);
    sys::write("[spawn-demo] неблокирующий запуск РАБОТАЕТ\n".as_bytes());
    sys::exit(0);
}

/// Собрать отчёт без alloc: у этой программы нет кучи и она ей не нужна.
fn fmt(out: &mut [u8], c0: usize, c1: usize, polls: usize) -> usize {
    let mut n = 0;
    let mut put = |s: &[u8], out: &mut [u8], n: &mut usize| {
        for &b in s {
            if *n < out.len() {
                out[*n] = b;
                *n += 1;
            }
        }
    };
    let mut num = |v: usize, out: &mut [u8], n: &mut usize| {
        let mut buf = [0u8; 20];
        let mut i = buf.len();
        let mut v = v;
        loop {
            i -= 1;
            buf[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        put(&buf[i..], out, n);
    };
    put("[spawn-demo] коды выхода: ".as_bytes(), out, &mut n);
    num(c0, out, &mut n);
    put(" и ".as_bytes(), out, &mut n);
    num(c1, out, &mut n);
    put(", опросов без сна: ".as_bytes(), out, &mut n);
    num(polls, out, &mut n);
    put(b"\n", out, &mut n);
    n
}
