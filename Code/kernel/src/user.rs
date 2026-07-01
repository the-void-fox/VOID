//! Пользовательские программы (исполняются в U-mode). Живут в секции `.user` (страницы
//! `U|R|X`, см. [[user-mode]]) и общаются с ядром/друг с другом только через `ecall`.
//! Планирование, адресные пространства и IPC — в [`crate::proc`].
//!
//! Соглашение syscall'ов (ABI): номер в `a7`, аргументы в `a0..`, результат в `a0`.
//!   1 = WRITE(ptr,len), 2 = EXIT(code), 3 = YIELD,
//!   4 = RECV -> (a0=msg, a1=from), 5 = CALL(dest,msg) -> a0=reply, 6 = REPLY(dest,val).

use core::arch::asm;

static DONE_MSG: [u8; b"[client] all replies received, exiting\n".len()] =
    *b"[client] all replies received, exiting\n";

/// Процесс-**сервер**: бесконечно принимает запрос, вычисляет ответ (удвоение — сервис
/// целиком в userspace) и отвечает клиенту. Только `ecall`.
#[link_section = ".user"]
extern "C" fn server_proc(_arg: usize) -> ! {
    loop {
        let msg: usize;
        let from: usize;
        unsafe {
            // SYS_RECV -> a0=msg, a1=from
            asm!("ecall", in("a7") 4usize, lateout("a0") msg, lateout("a1") from, options(nostack));
        }
        let reply = msg.wrapping_mul(2); // «сервис»: удвоить число
        unsafe {
            // SYS_REPLY(from, reply)
            asm!("ecall", in("a7") 6usize, inout("a0") from => _, in("a1") reply, options(nostack));
        }
    }
}

/// Процесс-**клиент**: трижды вызывает сервер (`arg` = его id), затем печатает сообщение и
/// завершается. Ответы сервера логирует ядро (форматировать число в U-mode пока нечем).
#[link_section = ".user"]
extern "C" fn client_proc(server: usize) -> ! {
    let mut n = 21usize;
    let mut i = 0;
    while i < 3 {
        unsafe {
            // SYS_CALL(server, n) -> ответ в a0 (игнорируем — его печатает ядро)
            asm!("ecall", in("a7") 5usize, inout("a0") server => _, in("a1") n, options(nostack));
        }
        n = n.wrapping_add(21);
        i += 1;
    }
    unsafe {
        // SYS_WRITE(msg) + SYS_EXIT(0)
        asm!(
            "ecall",
            in("a7") 1usize,
            inout("a0") DONE_MSG.as_ptr() as usize => _,
            in("a1") DONE_MSG.len(),
            options(nostack),
        );
        asm!("ecall", in("a7") 2usize, in("a0") 0usize, options(nostack, noreturn));
    }
}

/// Точки входа (identity VA секции `.user`).
pub fn server_entry() -> usize {
    server_proc as *const () as usize
}
pub fn client_entry() -> usize {
    client_proc as *const () as usize
}
