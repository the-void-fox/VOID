//! Первая std-программа VOID (Веха 31) — обычный Rust: без `#![no_std]`,
//! без `#![no_main]`, без единого syscall-шима в исходнике. Всё, что здесь
//! видно, идёт через порт std (vendor/rust): println → SYS_WRITE, куча →
//! SYS_MAP, args/env → SYS_ARGS, Instant → rdtime/rdtsc, exit → SYS_EXIT.

use std::collections::HashMap;
use std::time::Instant;

fn main() {
    println!("[hello-std] Привет от НАСТОЯЩЕЙ std на VOID!");

    let args: Vec<String> = std::env::args().collect();
    println!("[hello-std] argv: {args:?}");
    println!(
        "[hello-std] env ARCH={} SYSTEM={}",
        std::env::var("ARCH").unwrap_or_else(|_| "?".into()),
        std::env::var("SYSTEM").unwrap_or_else(|_| "?".into()),
    );

    // Куча, итераторы, сортировка — рабочая нагрузка на аллокатор и стек.
    let t = Instant::now();
    let mut v: Vec<u64> = (1..=100_000).rev().collect();
    v.sort_unstable();
    let sum: u64 = v.iter().sum();
    println!("[hello-std] sort 100k + сумма = {sum} за {:?}", t.elapsed());

    // HashMap упражняет ГСЧ сидов (sys/random) и хэширование.
    let mut m = HashMap::new();
    m.insert("ядро", "VOID");
    m.insert("std", "родная");
    println!("[hello-std] HashMap работает: {} записи", m.len());

    println!("[hello-std] выходим с кодом 7 — проверка exit-кода");
    std::process::exit(7);
}
