//! CPU-bound процесс для демо вытеснения (Веха 16): крутит длинный busy-loop БЕЗ единого
//! `yield`/IPC и лишь печатает свою метку. Без вытеснения первый процесс отработал бы все свои
//! печати до второго; с вытеснением по таймеру их вывод перемежается.
//!
//! `a0` выбирает метку (0 = " A ", иначе " B "). `black_box` не даёт компилятору свернуть
//! цикл в константу (сумма 0..N вычислима на этапе компиляции — а нам нужно ЖЕЧЬ процессор).
#![no_std]
#![no_main]

use core::hint::black_box;

/// Веха 163 — `busy [секунд]`: сколько ЖЕЧЬ. Без аргумента — пять кругов, как было (демо
/// вытеснения). С аргументом крутится по ЧАСАМ, а не по числу сложений: под KVM пять кругов
/// отрабатывают за ~70 мс, и увидеть в диспетчере долю процессора на них нечем — окно замера
/// полсекунды. Число секунд — то, что человек и хочет сказать: «займи процессор на три секунды».
#[no_mangle]
pub extern "C" fn _start(which: usize, _a1: usize) -> ! {
    let label: &[u8] = if which == 0 { b" A " } else { b" B " };
    // Argv держит буфер на стеке — её надо связать, а не читать из временной.
    let av = void_user::argv::Argv::take();
    let secs = av.str(0).and_then(|s| s.parse::<u64>().ok()).filter(|&n| n > 0);
    let deadline = secs.map(|n| void_user::monotonic_ns() + n * 1_000_000_000);
    let mut round = 0;
    loop {
        let mut acc = 0u64;
        let mut i = 0u64;
        while i < 20_000_000 {
            acc = acc.wrapping_add(black_box(i));
            i += 1;
        }
        black_box(acc);
        void_user::write(label);
        round += 1;
        match deadline {
            Some(t) if void_user::monotonic_ns() >= t => break,
            None if round >= 5 => break,
            _ => {}
        }
    }
    void_user::exit(0);
}
