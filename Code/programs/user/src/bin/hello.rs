//! Первая userspace-программа VOID (Веха 19.1) — исторически первый отдельный ELF, грузившийся
//! ядром по content-id из объектного store, когда все остальные ещё жили в секции `.user`.
//! С Вехи 23 так устроены ВСЕ программы; hello остаётся наглядным демо exec-по-хэшу
//! (`exec_demo` в kmain и команда `run bin/hello` в vsh).
//!
//! Веха 30 — витрина контракта запуска: если вызывающий передал аргументы
//! (`run bin/hello мир`), hello печатает свой argv, унаследованное окружение и число
//! стартовых capability (SYS_ARGS / SYS_STARTCAP). Без аргументов — молчит о них
//! (exec_demo и бенчи видят прежний однострочный вывод).
#![no_std]
#![no_main]

use void_user as sys;

/// Точка входа. Адрес берётся из `e_entry` ELF-заголовка загрузчиком ядра (kernel/src/elf.rs).
/// Стек (`sp`) и аргументы (`a0`/`a1`) уже выставлены ядром в стартовом trap-кадре процесса.
#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    sys::write(b"[hello] I'm an ELF loaded by content-id from the object store, not baked into the kernel image!\n");

    let mut buf = [0u8; 256];
    let n = sys::args(&mut buf).min(buf.len());
    let mut argv = buf[..n].split(|&b| b == 0).filter(|s| !s.is_empty());
    let _name = argv.next(); // argv[0] — имя программы, его не показываем
    let mut got_args = false;
    for a in argv {
        if !got_args {
            sys::write(b"[hello] argv:");
            got_args = true;
        }
        sys::write(b" ");
        sys::write(a);
    }
    if got_args {
        sys::write(b"\n[hello] env: ");
        let n = sys::env(&mut buf).min(buf.len());
        let mut first = true;
        for e in buf[..n].split(|&b| b == 0).filter(|s| !s.is_empty()) {
            if !first {
                sys::write(b" ");
            }
            sys::write(e);
            first = false;
        }
        // Стартовые capability (унаследованы от vsh): перечислить таблицу до конца.
        let mut caps = 0usize;
        while sys::start_cap(caps) != sys::NO_CAP && caps < 9 {
            caps += 1;
        }
        sys::write(b"\n[hello] start-caps: ");
        sys::write(&[b'0' + caps as u8]);
        sys::write(b"\n");
    }
    sys::exit(0);
}
