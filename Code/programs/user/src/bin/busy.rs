//! CPU-bound процесс для демо вытеснения (Веха 16): крутит длинный busy-loop БЕЗ единого
//! `yield`/IPC и лишь печатает свою метку. Без вытеснения первый процесс отработал бы все свои
//! печати до второго; с вытеснением по таймеру их вывод перемежается.
//!
//! `a0` выбирает метку (0 = " A ", иначе " B "). `black_box` не даёт компилятору свернуть
//! цикл в константу (сумма 0..N вычислима на этапе компиляции — а нам нужно ЖЕЧЬ процессор).
#![no_std]
#![no_main]

use core::hint::black_box;

#[no_mangle]
pub extern "C" fn _start(which: usize, _a1: usize) -> ! {
    let label: &[u8] = if which == 0 { b" A " } else { b" B " };
    for _ in 0..5 {
        let mut acc = 0u64;
        let mut i = 0u64;
        while i < 20_000_000 {
            acc = acc.wrapping_add(black_box(i));
            i += 1;
        }
        black_box(acc);
        void_user::write(label);
    }
    void_user::exit(0);
}
