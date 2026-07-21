//! Демо чекпойнта процессов (Веха 37): вычисление, переживающее перезагрузку.
//!
//! Программа заполняет кучу псевдослучайным паттерном, считает «работу» по шагам и на
//! середине морозит СЕБЯ (`SYS_CHECKPOINT` → образ в store под `proc/<arch>/пример`).
//! Возврат 0 — жизнь продолжается (образ снят фоном); возврат 1 — этот процесс
//! РАЗМОРОЖЕН (`thaw пример` в vsh — хоть после перезагрузки QEMU): он продолжает с
//! того же шага, с тем же стеком и кучей — и доказывает это, доверив прошлое
//! состояние только контрольной сумме кучи и локалам на стеке.
#![no_std]
#![no_main]

use void_user as sys;

const HEAP_LEN: usize = 64 * 1024;
const STEPS: usize = 10;
const FREEZE_AT: usize = 5;

/// xorshift64 — воспроизводимый паттерн кучи без каких-либо таблиц.
fn xs(mut s: u64) -> u64 {
    s ^= s << 13;
    s ^= s >> 7;
    s ^= s << 17;
    s
}

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    // Старт-права детей vsh: слот 1 — store-cap (Веха 37: EXEC|WRITE — им и морозимся).
    let scap = sys::start_cap(1);

    let base = sys::heap_map(HEAP_LEN);
    if scap == sys::NO_CAP || base == usize::MAX {
        sys::write("[freeze] нет прав или кучи — отказ\n".as_bytes());
        sys::exit(1);
    }
    let heap = unsafe { core::slice::from_raw_parts_mut(base as *mut u8, HEAP_LEN) };

    // Куча: паттерн, который обязан пережить заморозку байт-в-байт.
    let mut seed = 0xC0FF_EE11_D00D_2026u64;
    for chunk in heap.chunks_mut(8) {
        seed = xs(seed);
        for (i, b) in chunk.iter_mut().enumerate() {
            *b = (seed >> (8 * i)) as u8;
        }
    }
    let sum_before = checksum(heap);

    let mut work = 0usize; // «результат вычисления» — локал на стеке
    for step in 1..=STEPS {
        work += step * step;
        sys::write("[freeze] шаг ".as_bytes());
        put_dec(step);
        if step == FREEZE_AT {
            match sys::checkpoint(scap, "пример".as_bytes()) {
                0 => sys::write(" — образ снят, живу дальше\n".as_bytes()),
                1 => {
                    // Мы — разморозка (возможно, из другой сессии/после перезагрузки).
                    sys::write(" — Я РАЗМОРОЖЕН: продолжаю с шага ".as_bytes());
                    put_dec(step);
                    sys::write(", куча ".as_bytes());
                    sys::write(if checksum(heap) == sum_before {
                        "цела ✓\n".as_bytes()
                    } else {
                        "ИСПОРЧЕНА ✗\n".as_bytes()
                    });
                }
                _ => {
                    sys::write(" — SYS_CHECKPOINT отказал ✗\n".as_bytes());
                    sys::exit(2);
                }
            }
        } else {
            sys::write("\n".as_bytes());
        }
    }

    // 1²+…+10² = 385 — сумма верна только если ни один шаг не потерян и не повторён.
    let ok = work == (1..=STEPS).map(|s| s * s).sum::<usize>() && checksum(heap) == sum_before;
    sys::write("[freeze] итог работы = ".as_bytes());
    put_dec(work);
    sys::write(if ok { " ✓ вычисление и куча целы\n" } else { " ✗ состояние побито\n" }.as_bytes());
    sys::exit(if ok { 0 } else { 3 });
}

fn checksum(bytes: &[u8]) -> u64 {
    // FNV-1a: хватает, чтобы отличить «та же куча» от «любая другая».
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        h = (h ^ b as u64).wrapping_mul(0x1000_0000_01b3);
    }
    h
}

/// Напечатать usize десятично (форматтера в no_std-бинаре нет).
fn put_dec(mut v: usize) {
    if v == 0 {
        sys::write(b"0");
        return;
    }
    let mut nb = [0u8; 20];
    let mut n = 0;
    while v > 0 {
        nb[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    let mut out = [0u8; 20];
    for i in 0..n {
        out[i] = nb[n - 1 - i];
    }
    sys::write(&out[..n]);
}
