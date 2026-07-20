//! Демо потоков VOID (Веха 35) на НАСТОЯЩЕЙ std: `std::thread::spawn` + `Arc<Mutex>` +
//! `thread_local!`. Ни одного syscall-шима в исходнике — всё через порт std (vendor/rust):
//! нити → SYS_THREAD_SPAWN/JOIN, Mutex/Condvar → SYS_FUTEX, thread_local → нативный TLS
//! (tp на riscv / база %fs на x86), выставленный рантаймом pal.
//!
//! Две проверки:
//!   1. **TLS-изоляция** — каждая нить видит СВОЁ значение `thread_local!` даже под
//!      вытеснением; главная нить — своё (ключевой тест раскладки TLS Variant I/II).
//!   2. **Mutex+Arc** — N нитей инкрементируют общий счётчик под мьютексом, итог ровно N·M.

use std::cell::Cell;
use std::sync::{Arc, Mutex};
use std::thread;

const NTHREADS: u64 = 4;
const ITERS: u64 = 20_000;

thread_local! {
    /// Своё у каждой нити: если TLS-раскладка верна, значение не «протекает» между нитями.
    static MY_ID: Cell<u64> = Cell::new(0);
}

fn main() {
    println!("[threads-std] std::thread на VOID: {NTHREADS} нити × {ITERS} инкрементов");

    // Главная нить помечает свой TLS — после join'ов он обязан уцелеть.
    MY_ID.set(9999);

    let counter = Arc::new(Mutex::new(0u64));
    let mut handles = Vec::new();
    for id in 0..NTHREADS {
        let counter = Arc::clone(&counter);
        // Скромный стек — арена pal 16 МиБ (по умолчанию было бы 2 МиБ на нить).
        let h = thread::Builder::new()
            .name(format!("worker-{id}"))
            .stack_size(256 * 1024)
            .spawn(move || {
                MY_ID.set(id); // своё значение TLS этой нити
                for i in 0..ITERS {
                    *counter.lock().unwrap() += 1; // общий счётчик под Mutex (futex)
                    if id == 0 && i % 2000 == 0 {
                        thread::yield_now(); // изредка подтолкнуть перемешивание нитей
                    }
                }
                // TLS не протёк под вытеснением? Вернём флаг наверх.
                MY_ID.get() == id
            })
            .expect("spawn нити");
        handles.push((id, h));
    }

    // Присоединить всех, собрать вердикты TLS-изоляции.
    let mut tls_ok = true;
    for (id, h) in handles {
        match h.join() {
            Ok(true) => {}
            Ok(false) => {
                tls_ok = false;
                println!("[threads-std] нить {id}: TLS ПРОТЁК!");
            }
            Err(_) => {
                tls_ok = false;
                println!("[threads-std] нить {id}: паника");
            }
        }
    }

    let total = *counter.lock().unwrap();
    let expected = NTHREADS * ITERS;
    let main_tls_ok = MY_ID.get() == 9999;

    println!("[threads-std] счётчик = {total} (ожидалось {expected})");
    println!("[threads-std] TLS: нити {} · главная {}",
        if tls_ok { "ок" } else { "СБОЙ" },
        if main_tls_ok { "ок" } else { "СБОЙ" });

    if total == expected && tls_ok && main_tls_ok {
        println!("[threads-std] ✓ потоки, Mutex и thread_local работают");
        std::process::exit(0);
    } else {
        println!("[threads-std] ✗ провал");
        std::process::exit(1);
    }
}
