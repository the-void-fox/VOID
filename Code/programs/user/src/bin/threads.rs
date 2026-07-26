//! Демо нитей (Веха 35) на чистом void_user (no_std) — проверяет ядро БЕЗ участия std.
//!
//! Заводит `NTHREADS` нитей в одном процессе (общее адресное пространство и домен): каждая
//! `ITERS` раз инкрементирует ОДИН неатомарный счётчик под futex-мьютексом. Без мьютекса
//! итог был бы меньше ожидаемого (потерянные обновления при вытеснении); с ним — ровно
//! `NTHREADS * ITERS`. Главная нить join'ит всех и печатает результат. Стек каждой нити —
//! статический массив (.bss, замаплен ELF-загрузчиком как страницы процесса); TLS не нужен
//! (thread_local тут не используется — это забота std, Веха 35 форк).

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, Ordering};
use void_user as sys;

const NTHREADS: usize = 4;
const ITERS: usize = 50_000;
const STACK: usize = 16 * 1024;

/// Стеки нитей — в .bss процесса. Вершина = адрес + STACK, выровненная вниз на 16.
static mut STACKS: [[u8; STACK]; NTHREADS] = [[0; STACK]; NTHREADS];

/// Разделяемый НЕатомарный счётчик — сердце демо: корректен только под мьютексом.
static mut COUNTER: u64 = 0;

/// Слово futex-мьютекса (модель Drepper): 0 — свободен, 1 — занят, 2 — занят и есть ждущие.
static LOCK: AtomicU32 = AtomicU32::new(0);

fn lock() {
    // Быстрый путь: свободен → занят.
    if LOCK.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_ok() {
        return;
    }
    // Спорный путь: пометить «есть ждущие» (2) и спать, пока владелец не отпустит.
    loop {
        if LOCK.swap(2, Ordering::Acquire) == 0 {
            return; // перехватили свободный
        }
        sys::futex_wait(LOCK.as_ptr(), 2, 0); // спим, пока *LOCK == 2
    }
}

fn unlock() {
    // Если были ждущие (значение 2) — разбудить одного.
    if LOCK.swap(0, Ordering::Release) == 2 {
        sys::futex_wake(LOCK.as_ptr(), 1);
    }
}

/// Тело нити: `arg` — её номер. Инкрементирует общий счётчик под мьютексом и завершается,
/// возвращая свой номер как retval (его заберёт join). НЕ возвращается — стек свежий, ra=0.
extern "C" fn worker(arg: usize) -> ! {
    for _ in 0..ITERS {
        lock();
        // SAFETY: доступ к общему счётчику сериализован мьютексом — гонки нет.
        unsafe {
            let p = core::ptr::addr_of_mut!(COUNTER);
            p.write(p.read() + 1);
        }
        unlock();
    }
    sys::thread_exit(arg)
}

/// Вершина стека нити `i`, выровненная вниз на 16 (как у главной нити процесса).
fn stack_top(i: usize) -> usize {
    // SAFETY: берём лишь АДРЕС элемента статического массива стеков (не разыменовываем).
    let base = unsafe { core::ptr::addr_of!(STACKS[i]) as usize };
    (base + STACK) & !0xf
}

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    sys::write("[threads] запускаю ".as_bytes());
    put_dec(NTHREADS);
    sys::write(" нитей по ".as_bytes());
    put_dec(ITERS);
    sys::write(" инкрементов под futex-мьютексом\n".as_bytes());

    let mut tids = [0usize; NTHREADS];
    for i in 0..NTHREADS {
        tids[i] = sys::thread_spawn(worker as *const () as usize, i, stack_top(i));
    }
    // Присоединить всех: каждая нить вернёт свой номер (проверка канала retval).
    let mut sum_ids = 0usize;
    for i in 0..NTHREADS {
        sum_ids += sys::thread_join(tids[i]);
    }

    let total = unsafe { core::ptr::addr_of!(COUNTER).read() };
    let expected = (NTHREADS * ITERS) as u64;
    sys::write("[threads] счётчик = ".as_bytes());
    put_dec(total as usize);
    sys::write(" (ожидалось ".as_bytes());
    put_dec(expected as usize);
    sys::write(")".as_bytes());
    if total == expected && sum_ids == (0..NTHREADS).sum() {
        sys::write(" ✓ мьютекс держит, join'ы отдали retval\n".as_bytes());
        sys::exit(0)
    } else {
        sys::write(" ✗ гонка!\n".as_bytes());
        sys::exit(1)
    }
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
