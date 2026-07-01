//! Пользовательские программы (исполняются в U-mode). Живут в секции `.user` (страницы
//! `U|R|X`, см. [[user-mode]]) и общаются с ядром только через `ecall` (системные вызовы).
//! Планирование и адресные пространства — в [`crate::proc`].
//!
//! Соглашение syscall'ов (ABI): номер в `a7`, аргументы в `a0..`, результат в `a0`.
//!   1 = WRITE(ptr, len), 2 = EXIT(code), 3 = YIELD.

use core::arch::asm;

// Сообщения процессов. Размер выводится из литерала (`.len()` — const), считать вручную не нужно.
static MSG_A: [u8; b"[proc A] hello from U-mode\n".len()] = *b"[proc A] hello from U-mode\n";
static MSG_B: [u8; b"[proc B] hello from U-mode\n".len()] = *b"[proc B] hello from U-mode\n";

/// Единая программа обоих процессов: по `arg` выбирает своё сообщение, дважды печатает его,
/// уступая процессор между итерациями, затем завершается. Только `ecall` — никаких обращений к
/// ядру напрямую (иначе исполнение ушло бы в не-`U` страницу).
#[link_section = ".user"]
extern "C" fn user_proc(arg: usize) -> ! {
    let (ptr, len): (*const u8, usize) = if arg == 0 {
        (MSG_A.as_ptr(), MSG_A.len())
    } else {
        (MSG_B.as_ptr(), MSG_B.len())
    };

    let mut i = 0;
    while i < 2 {
        unsafe {
            // SYS_WRITE(ptr, len)
            asm!(
                "ecall",
                in("a7") 1usize,
                inout("a0") ptr as usize => _,
                in("a1") len,
                options(nostack),
            );
            // SYS_YIELD — дать поработать другому процессу
            asm!("ecall", in("a7") 3usize, lateout("a0") _, options(nostack));
        }
        i += 1;
    }

    unsafe {
        // SYS_EXIT(0)
        asm!("ecall", in("a7") 2usize, in("a0") 0usize, options(nostack, noreturn));
    }
}

/// Адрес точки входа пользовательской программы (identity VA секции `.user`).
pub fn proc_entry() -> usize {
    user_proc as *const () as usize
}
