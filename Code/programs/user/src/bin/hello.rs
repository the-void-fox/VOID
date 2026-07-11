//! Первая userspace-программа VOID (Веха 19.1) — исторически первый отдельный ELF, грузившийся
//! ядром по content-id из объектного store, когда все остальные ещё жили в секции `.user`.
//! С Вехи 23 так устроены ВСЕ программы; hello остаётся наглядным демо exec-по-хэшу
//! (`exec_demo` в kmain и команда `run bin/hello` в vsh).
#![no_std]
#![no_main]

/// Точка входа. Адрес берётся из `e_entry` ELF-заголовка загрузчиком ядра (kernel/src/elf.rs).
/// Стек (`sp`) и аргументы (`a0`/`a1`) уже выставлены ядром в стартовом trap-кадре процесса.
#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    void_user::write(b"[hello] I'm an ELF loaded by content-id from the object store, not baked into the kernel image!\n");
    void_user::exit(0);
}
