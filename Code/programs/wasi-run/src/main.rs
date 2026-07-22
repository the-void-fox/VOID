//! wasi-run — WASI-раннер VOID (Веха 39, бэкенд C [[void-pkg]]): интерпретатор wasmi поверх
//! порта std запускает неизменённые **wasm32-wasi**-модули. Универсальный медленный fallback
//! пакетной дорожки ([[0004-void-pkg]]): один и тот же .wasm работает на ОБЕИХ архитектурах без
//! пересборки — интерпретатор арх-нейтрален, а cap-модель WASI (preopen-дескрипторы) идейно наша.
//!
//! Микроядерно чисто: это ОБЫЧНАЯ userspace-программа (тулчейн `void`, как std-hello), а не код
//! в ядре. .wasm читается как обычный файл (`std::fs` → IPC к posixfs), stdout WASI (`fd_write`)
//! уходит в `println!` → SYS_WRITE, `proc_exit` — в `std::process::exit` → SYS_EXIT.
//!
//! Запуск из vsh: `run bin/wasirun hello.wasm [аргументы…]` (файл .wasm доставлен мостом в
//! персоналию). Реализованы импорты `wasi_snapshot_preview1`, которых требует Rust/C hello:
//! args_{sizes_get,get}, environ_{sizes_get,get}, fd_write, proc_exit.

use std::io::Write;

use wasmi::{Caller, Engine, Extern, Linker, Memory, Module, Store};

/// Состояние хоста, видное импортам: argv гостя (каждый — с завершающим NUL, как ждёт WASI).
struct Wasi {
    args: Vec<Vec<u8>>,
}

/// Достать линейную память модуля (её экспорт называется `memory`). Без неё импортам некуда
/// писать — это негодный wasi-модуль.
fn memory(caller: &mut Caller<Wasi>) -> Memory {
    match caller.get_export("memory") {
        Some(Extern::Memory(m)) => m,
        _ => {
            eprintln!("wasi-run: у модуля нет экспорта memory");
            std::process::exit(71);
        }
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() < 2 {
        eprintln!("usage: wasirun <файл.wasm> [аргументы…]");
        std::process::exit(2);
    }
    let path = argv[1].clone();
    let wasm = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("wasi-run: не прочитать {path}: {e}");
            std::process::exit(66);
        }
    };
    // argv гостя = имя модуля + доп. аргументы вызывающего, каждый NUL-терминирован.
    let guest_args: Vec<Vec<u8>> = argv[1..]
        .iter()
        .map(|s| {
            let mut v = s.clone().into_bytes();
            v.push(0);
            v
        })
        .collect();

    let engine = Engine::default();
    let module = match Module::new(&engine, &wasm[..]) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("wasi-run: негодный wasm: {e}");
            std::process::exit(65);
        }
    };
    let mut store = Store::new(&engine, Wasi { args: guest_args });
    let mut linker = <Linker<Wasi>>::new(&engine);
    let w = "wasi_snapshot_preview1";

    // args_sizes_get(argc_out, argv_buf_size_out) — сколько аргументов и байт под них.
    linker
        .func_wrap(w, "args_sizes_get", |mut caller: Caller<Wasi>, argc_ptr: i32, buf_ptr: i32| -> i32 {
            let args = caller.data().args.clone();
            let argc = args.len() as u32;
            let buf: u32 = args.iter().map(|a| a.len() as u32).sum();
            let m = memory(&mut caller);
            m.write(&mut caller, argc_ptr as usize, &argc.to_le_bytes()).ok();
            m.write(&mut caller, buf_ptr as usize, &buf.to_le_bytes()).ok();
            0
        })
        .unwrap();

    // args_get(argv_out, argv_buf_out) — заполнить массив указателей и строковый буфер.
    linker
        .func_wrap(w, "args_get", |mut caller: Caller<Wasi>, argv_ptr: i32, buf_ptr: i32| -> i32 {
            let args = caller.data().args.clone();
            let m = memory(&mut caller);
            let mut p = argv_ptr as usize;
            let mut b = buf_ptr as u32;
            for a in &args {
                m.write(&mut caller, p, &b.to_le_bytes()).ok();
                p += 4;
                m.write(&mut caller, b as usize, a).ok();
                b += a.len() as u32;
            }
            0
        })
        .unwrap();

    // Окружение гостю не отдаём (пусто) — но интерфейс обязателен, иначе линковка модуля падает.
    linker
        .func_wrap(w, "environ_sizes_get", |mut caller: Caller<Wasi>, c_ptr: i32, s_ptr: i32| -> i32 {
            let m = memory(&mut caller);
            m.write(&mut caller, c_ptr as usize, &0u32.to_le_bytes()).ok();
            m.write(&mut caller, s_ptr as usize, &0u32.to_le_bytes()).ok();
            0
        })
        .unwrap();
    linker
        .func_wrap(w, "environ_get", |_caller: Caller<Wasi>, _a: i32, _b: i32| -> i32 { 0 })
        .unwrap();

    // fd_write(fd, iovs, iovs_len, nwritten_out) — собрать iovec'ы из памяти гостя в stdout/stderr.
    linker
        .func_wrap(
            w,
            "fd_write",
            |mut caller: Caller<Wasi>, fd: i32, iovs: i32, iovs_len: i32, nwritten: i32| -> i32 {
                let m = memory(&mut caller);
                let mut out: Vec<u8> = Vec::new();
                for i in 0..iovs_len as usize {
                    let mut ent = [0u8; 8]; // ciovec { buf: u32, buf_len: u32 }
                    if m.read(&caller, iovs as usize + i * 8, &mut ent).is_err() {
                        return 21; // WASI EFAULT
                    }
                    let buf = u32::from_le_bytes(ent[0..4].try_into().unwrap()) as usize;
                    let len = u32::from_le_bytes(ent[4..8].try_into().unwrap()) as usize;
                    let mut data = vec![0u8; len];
                    if m.read(&caller, buf, &mut data).is_err() {
                        return 21;
                    }
                    out.extend_from_slice(&data);
                }
                let n = out.len() as u32;
                if fd == 2 {
                    std::io::stderr().write_all(&out).ok();
                } else {
                    std::io::stdout().write_all(&out).ok();
                }
                m.write(&mut caller, nwritten as usize, &n.to_le_bytes()).ok();
                0
            },
        )
        .unwrap();

    // proc_exit(code) — код возврата гостя становится кодом самого wasi-run (→ SYS_EXIT).
    linker
        .func_wrap(w, "proc_exit", |_caller: Caller<Wasi>, code: i32| -> () {
            std::io::stdout().flush().ok();
            std::process::exit(code);
        })
        .unwrap();

    let instance = match linker.instantiate(&mut store, &module).and_then(|pre| pre.start(&mut store)) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("wasi-run: инстанциация не удалась: {e}");
            std::process::exit(70);
        }
    };
    let start = match instance.get_typed_func::<(), ()>(&store, "_start") {
        Ok(f) => f,
        Err(_) => {
            eprintln!("wasi-run: у модуля нет экспорта _start");
            std::process::exit(70);
        }
    };
    // Гость обычно уходит через proc_exit (→ process::exit). Сюда возвращаемся, только если он
    // отработал _start до конца сам — тогда выходим с нулём; настоящий трап печатаем.
    match start.call(&mut store, ()) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("wasi-run: гость завершился ошибкой: {e}");
            std::process::exit(70);
        }
    }
}
