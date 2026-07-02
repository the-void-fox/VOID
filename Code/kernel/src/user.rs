//! Пользовательские программы (исполняются в U-mode). Живут в секции `.user` (страницы
//! `U|R|X`, см. [[user-mode]]) и общаются с ядром/друг с другом только через `ecall`.
//! Планирование, адресные пространства и IPC — в [`crate::proc`].
//!
//! Соглашение syscall'ов (ABI): номер в `a7`, аргументы в `a0..`, результат в `a0`.
//!   1 = WRITE(ptr,len), 2 = EXIT(code), 4 = RECV -> (a0=msg, a1=from),
//!   5 = CALL(ep_cap,msg,recv_buf,recv_cap) -> a0=байт, 6 = REPLY(dest,src_buf,len),
//!   7 = BLK_READ(dev_cap, sector, buf).
//!
//! Веха 12: **capability-защищённые IPC-эндпоинты**. Драйвер-сервер получает при старте (`a0`)
//! cap на **устройство**, клиент — cap на **эндпоинт** сервера. `CALL` и `BLK_READ` берут не
//! сырой pid/номер, а этот дескриптор; без нужного права ядро отказывает ([[ipc-endpoints]]).

use core::arch::asm;
use core::mem::MaybeUninit;

static PREFIX: [u8; b"[blk-cli] sector 0 via driver-server: ".len()] =
    *b"[blk-cli] sector 0 via driver-server: ";
static NL: [u8; 1] = *b"\n";
// ASCII-строка (в byte-строках нельзя не-ASCII): демонстрация отказа при прямом доступе к диску.
static DENIED: [u8; b"[blk-cli] direct disk read DENIED by kernel (no device capability)\n".len()] =
    *b"[blk-cli] direct disk read DENIED by kernel (no device capability)\n";

/// Процесс-**драйвер блоков**: принимает номер сектора, читает его через шлюз `BLK_READ`
/// (предъявляя `dev_cap` — свой cap на устройство) и возвращает 512 байт клиенту через IPC.
/// Буфер — на стеке (без обнуления: `MaybeUninit`, иначе компилятор мог бы вызвать `memset`
/// вне секции `.user`).
#[link_section = ".user"]
extern "C" fn blk_server(dev_cap: usize) -> ! {
    let mut buf = MaybeUninit::<[u8; 512]>::uninit();
    let bptr = buf.as_mut_ptr() as usize;
    loop {
        let sector: usize;
        let from: usize;
        unsafe {
            // SYS_RECV -> a0=sector, a1=from
            asm!("ecall", in("a7") 4usize, lateout("a0") sector, lateout("a1") from, options(nostack));
            // SYS_BLK_READ(dev_cap, sector, buf) — прочитать сектор в свой буфер по cap на устройство
            asm!("ecall", in("a7") 7usize, inout("a0") dev_cap => _, in("a1") sector, in("a2") bptr, options(nostack));
            // SYS_REPLY(from, buf, 512) — отдать данные клиенту
            asm!("ecall", in("a7") 6usize, inout("a0") from => _, in("a1") bptr, in("a2") 512usize, options(nostack));
        }
    }
}

/// Процесс-**клиент**: `arg` = cap на эндпоинт сервера. Просит сектор 0 через `CALL`, печатает
/// его магию, затем пытается прочитать диск НАПРЯМУЮ (предъявляя эндпоинт-cap, не device) —
/// и получает отказ ядра: наглядно, что доступ к железу — только по правильному capability.
#[link_section = ".user"]
extern "C" fn blk_client(ep_cap: usize) -> ! {
    let mut buf = MaybeUninit::<[u8; 512]>::uninit();
    let bptr = buf.as_mut_ptr() as usize;
    unsafe {
        // SYS_CALL(ep_cap, sector=0, recv_buf=bptr, recv_cap=512) -> a0 = принято байт
        asm!(
            "ecall",
            in("a7") 5usize,
            inout("a0") ep_cap => _,
            in("a1") 0usize,
            in("a2") bptr,
            in("a3") 512usize,
            options(nostack),
        );
        // Напечатать: префикс + первые 6 байт сектора (магия "VOIDFS") + перевод строки.
        asm!("ecall", in("a7") 1usize, inout("a0") PREFIX.as_ptr() as usize => _, in("a1") PREFIX.len(), options(nostack));
        asm!("ecall", in("a7") 1usize, inout("a0") bptr => _, in("a1") 6usize, options(nostack));
        asm!("ecall", in("a7") 1usize, inout("a0") NL.as_ptr() as usize => _, in("a1") NL.len(), options(nostack));
        // Попытка прямого доступа: BLK_READ с эндпоинт-cap (у клиента НЕТ cap на устройство).
        let denied: usize;
        asm!("ecall", in("a7") 7usize, inout("a0") ep_cap => denied, in("a1") 0usize, in("a2") bptr, options(nostack));
        if denied != 0 {
            asm!("ecall", in("a7") 1usize, inout("a0") DENIED.as_ptr() as usize => _, in("a1") DENIED.len(), options(nostack));
        }
        // SYS_EXIT(0)
        asm!("ecall", in("a7") 2usize, in("a0") 0usize, options(nostack, noreturn));
    }
}

/// Точки входа (identity VA секции `.user`).
pub fn blk_server_entry() -> usize {
    blk_server as *const () as usize
}
pub fn blk_client_entry() -> usize {
    blk_client as *const () as usize
}
