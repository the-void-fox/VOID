//! Пользовательские программы (исполняются в U-mode). Живут в секции `.user` (страницы
//! `U|R|X`, см. [[user-mode]]) и общаются с ядром/друг с другом только через `ecall`.
//! Планирование, адресные пространства и IPC — в [`crate::proc`].
//!
//! Соглашение syscall'ов (ABI): номер в `a7`, аргументы в `a0..`, результат в `a0`.
//!   1 = WRITE(ptr,len), 2 = EXIT(code),
//!   4 = RECV(recv_buf,recv_cap) -> (a0=op, a1=from, a2=len),
//!   5 = CALL(ep_cap,op,send_buf,send_len,recv_buf,recv_cap) -> a0=байт ответа,
//!   6 = REPLY(dest,src_buf,len), 7 = BLK_READ(dev_cap,sector,buf),
//!   8 = OBJ_PUT(store_cap,buf,len,id_out) -> 0/MAX, 9 = OBJ_GET(store_cap,id_ptr,out,cap) -> len.
//!
//! Веха 12: **capability-защищённые эндпоинты** (драйвер-сервер + клиент через IPC).
//! Веха 13: **сервер объектного store** — процесс с cap на store отдаёт put/get объектов по IPC
//! клиенту, у которого лишь cap на эндпоинт; передача буфера запроса клиент→сервер ([[object-store-server]]).

use core::arch::asm;
use core::mem::MaybeUninit;

static PREFIX: [u8; b"[blk-cli] sector 0 via driver-server: ".len()] =
    *b"[blk-cli] sector 0 via driver-server: ";
static NL: [u8; 1] = *b"\n";
// ASCII-строка (в byte-строках нельзя не-ASCII): демонстрация отказа при прямом доступе к диску.
static DENIED: [u8; b"[blk-cli] direct disk read DENIED by kernel (no device capability)\n".len()] =
    *b"[blk-cli] direct disk read DENIED by kernel (no device capability)\n";

// ── Веха 13: сервер объектного store ──
const OP_PUT: usize = 0;
const OP_GET: usize = 1;
static MSG: [u8; b"hello from VOID object-store client".len()] =
    *b"hello from VOID object-store client";
static SPREFIX: [u8; b"[store-cli] get by content-id -> ".len()] =
    *b"[store-cli] get by content-id -> ";
static SDENIED: [u8; b"[store-cli] direct OBJ_PUT DENIED by kernel (no store capability)\n".len()] =
    *b"[store-cli] direct OBJ_PUT DENIED by kernel (no store capability)\n";

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
            // SYS_RECV(recv_buf=0, recv_cap=0) -> a0=op(=sector), a1=from, a2=len (нагрузки нет)
            asm!("ecall", in("a7") 4usize, inout("a0") 0usize => sector, inout("a1") 0usize => from, out("a2") _, options(nostack));
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
        // SYS_CALL(ep_cap, op=0(сектор), send_buf=0, send_len=0, recv_buf=bptr, recv_cap=512)
        asm!(
            "ecall",
            in("a7") 5usize,
            inout("a0") ep_cap => _,
            in("a1") 0usize,   // op = номер сектора 0
            in("a2") 0usize,   // send_buf: нагрузки нет
            in("a3") 0usize,   // send_len = 0
            in("a4") bptr,     // recv_buf для ответа
            in("a5") 512usize, // recv_cap
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

/// Процесс-**сервер объектного store**: держит cap на store (`store_cap` в `a0`) и обслуживает
/// put/get по IPC. `RECV` кладёт нагрузку запроса в `req`; по `op` — либо `OBJ_PUT` (сохранить
/// `req[..len]`, вернуть 32-байтный content-id), либо `OBJ_GET` (по `req[..32]`=id прочитать
/// значение в `val`). Клиент store-syscall'ов не имеет — только этот сервер их делает.
#[link_section = ".user"]
extern "C" fn store_server(store_cap: usize) -> ! {
    let mut req = MaybeUninit::<[u8; 512]>::uninit();
    let mut val = MaybeUninit::<[u8; 512]>::uninit();
    let mut id = MaybeUninit::<[u8; 32]>::uninit();
    let rptr = req.as_mut_ptr() as usize;
    let vptr = val.as_mut_ptr() as usize;
    let iptr = id.as_mut_ptr() as usize;
    loop {
        let op: usize;
        let from: usize;
        let len: usize;
        unsafe {
            // SYS_RECV(req, 512) -> a0=op, a1=from, a2=len (нагрузка уже в req)
            asm!("ecall", in("a7") 4usize, inout("a0") rptr => op, inout("a1") 512usize => from, out("a2") len, options(nostack));
        }
        if op == OP_PUT {
            unsafe {
                // OBJ_PUT(store_cap, req, len, id) → content-id в id; затем REPLY(from, id, 32)
                asm!("ecall", in("a7") 8usize, inout("a0") store_cap => _, in("a1") rptr, in("a2") len, in("a3") iptr, options(nostack));
                asm!("ecall", in("a7") 6usize, inout("a0") from => _, in("a1") iptr, in("a2") 32usize, options(nostack));
            }
        } else {
            let vlen: usize;
            unsafe {
                // OBJ_GET(store_cap, id=req, val, 512) → длина; затем REPLY(from, val, длина)
                asm!("ecall", in("a7") 9usize, inout("a0") store_cap => vlen, in("a1") rptr, in("a2") vptr, in("a3") 512usize, options(nostack));
                asm!("ecall", in("a7") 6usize, inout("a0") from => _, in("a1") vptr, in("a2") vlen, options(nostack));
            }
        }
    }
}

/// Процесс-**клиент store**: `arg` = cap на эндпоинт сервера. Кладёт сообщение (`put`), получает
/// его content-id, читает по нему обратно (`get`) и печатает значение — доказывая круговой путь
/// через userspace-сервер. Затем пытается `OBJ_PUT` НАПРЯМУЮ (эндпоинт-cap, не store) → отказ.
#[link_section = ".user"]
extern "C" fn store_client(ep_cap: usize) -> ! {
    let mut id = MaybeUninit::<[u8; 32]>::uninit();
    let mut val = MaybeUninit::<[u8; 512]>::uninit();
    let iptr = id.as_mut_ptr() as usize;
    let vptr = val.as_mut_ptr() as usize;
    let msg = MSG.as_ptr() as usize;
    unsafe {
        // PUT: CALL(ep, op=PUT, send=MSG, send_len, recv=id, recv_cap=32) → в id лёг content-id
        asm!("ecall", in("a7") 5usize, inout("a0") ep_cap => _, in("a1") OP_PUT, in("a2") msg, in("a3") MSG.len(), in("a4") iptr, in("a5") 32usize, options(nostack));
        // GET: CALL(ep, op=GET, send=id, send_len=32, recv=val, recv_cap=512) → a0 = длина
        let vlen: usize;
        asm!("ecall", in("a7") 5usize, inout("a0") ep_cap => vlen, in("a1") OP_GET, in("a2") iptr, in("a3") 32usize, in("a4") vptr, in("a5") 512usize, options(nostack));
        // Напечатать: префикс + полученное значение + перевод строки.
        asm!("ecall", in("a7") 1usize, inout("a0") SPREFIX.as_ptr() as usize => _, in("a1") SPREFIX.len(), options(nostack));
        asm!("ecall", in("a7") 1usize, inout("a0") vptr => _, in("a1") vlen, options(nostack));
        asm!("ecall", in("a7") 1usize, inout("a0") NL.as_ptr() as usize => _, in("a1") NL.len(), options(nostack));
        // Попытка прямого доступа: OBJ_PUT с эндпоинт-cap (у клиента НЕТ cap на store).
        let denied: usize;
        asm!("ecall", in("a7") 8usize, inout("a0") ep_cap => denied, in("a1") msg, in("a2") MSG.len(), in("a3") iptr, options(nostack));
        if denied != 0 {
            asm!("ecall", in("a7") 1usize, inout("a0") SDENIED.as_ptr() as usize => _, in("a1") SDENIED.len(), options(nostack));
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
pub fn store_server_entry() -> usize {
    store_server as *const () as usize
}
pub fn store_client_entry() -> usize {
    store_client as *const () as usize
}
