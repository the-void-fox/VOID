//! Пользовательские программы (исполняются в U-mode). Живут в секции `.user` (страницы
//! `U|R|X`, см. [[user-mode]]) и общаются с ядром/друг с другом только через `ecall`.
//! Планирование, адресные пространства и IPC — в [`crate::proc`].
//!
//! Соглашение syscall'ов (ABI): номер в `a7`, аргументы в `a0..`, результат в `a0`.
//!   1 = WRITE(ptr,len), 2 = EXIT(code),
//!   4 = RECV(recv_buf,recv_cap) -> (a0=op, a1=from, a2=len),
//!   5 = CALL(ep_cap,op,send_buf,send_len,recv_buf,recv_cap) -> a0=байт ответа,
//!   6 = REPLY(reply_cap,src_buf,len), 7 = BLK_READ(dev_cap,sector,buf),
//!   8 = OBJ_PUT(store_cap,buf,len,id_out) -> 0/MAX, 9 = OBJ_GET(store_cap,id_ptr,out,cap) -> len,
//!   10 = OBJ_SET_ROOT(store_cap,name,name_len,id_ptr) -> 0/MAX,
//!   11 = OBJ_GET_ROOT(store_cap,name,name_len,id_out) -> 32/0/MAX,
//!   12 = BLK_WRITE(dev_cap,sector,buf,len) -> 0/MAX,
//!   13 = OBJ_DEL_ROOT(store_cap,name,name_len) -> 0/1/MAX  (Веха 18.3: unlink).
//!
//! Веха 12: **capability-защищённые эндпоинты** (драйвер-сервер + клиент через IPC).
//! Веха 13: **сервер объектного store** — процесс с cap на store отдаёт put/get объектов по IPC
//! клиенту, у которого лишь cap на эндпоинт; передача буфера запроса клиент→сервер ([[object-store-server]]).
//! Веха 14: **именованный корень как capability** — set_root/get_root по IPC; объект,
//! привязанный к корню, переживает перезагрузку ([[persistent-root-ipc]]).

use core::arch::asm;
use core::mem::MaybeUninit;

static PREFIX: [u8; b"[blk-cli] sector 0 via driver-server: ".len()] =
    *b"[blk-cli] sector 0 via driver-server: ";
static NL: [u8; 1] = *b"\n";
// Веха 17: BLK_WRITE. В `op` блок-запроса младшие биты = сектор, бит 40 = флаг записи.
const BLK_WRITE_FLAG: usize = 1 << 40;
const BLK_TEST_SECTOR: usize = 20000; // заведомо свободный сектор (store использует низкие)
static PATTERN: [u8; b"VOID block-write via server works".len()] =
    *b"VOID block-write via server works";
static WPREFIX: [u8; b"[blk-cli] sector 20000 read back after write: ".len()] =
    *b"[blk-cli] sector 20000 read back after write: ";
// ASCII-строка (в byte-строках нельзя не-ASCII): демонстрация отказа при прямом доступе к диску.
static DENIED: [u8; b"[blk-cli] direct disk read DENIED by kernel (no device capability)\n".len()] =
    *b"[blk-cli] direct disk read DENIED by kernel (no device capability)\n";
// Веха 15: попытка подделать REPLY (предъявив НЕ reply-cap) — ядро отвергает.
static RFORGE: [u8; b"[blk-cli] forged REPLY DENIED by kernel (not a reply-capability)\n".len()] =
    *b"[blk-cli] forged REPLY DENIED by kernel (not a reply-capability)\n";
// Веха 16: метки CPU-bound процессов для демо вытеснения (в .rodata — их читает ЯДРО в WRITE).
static LBL_A: [u8; 3] = *b" A ";
static LBL_B: [u8; 3] = *b" B ";

// ── Веха 18.1/18.3: POSIX-персоналия (файлы поверх IPC) ──
// op персоналии кодирует операцию (младший байт), дескриптор fd (байт 8..16) и режим open
// (байт 16..24): op | (fd<<8) | (mode<<16).
const PX_OPEN: usize = 0;
const PX_READ: usize = 1;
const PX_WRITE: usize = 2;
const PX_CLOSE: usize = 3;
const PX_STAT: usize = 4; // Веха 18.3: stat(name) -> [exists:1|size:4]
const PX_UNLINK: usize = 5; // Веха 18.3: unlink(name) — снять корень + убрать из каталога
const PX_READDIR: usize = 6; // Веха 18.3: readdir() -> имена через '\n'
const O_APPEND: usize = 1 << 0; // Веха 18.3: открыть с курсором в конце
const O_TRUNC: usize = 1 << 1; // Веха 18.3: открыть, обнулив содержимое
const PX_NFILES: usize = 4; // namespace фикс. размера (у процессов нет кучи — всё на стеке)
const PX_NAME_MAX: usize = 16;
const PX_DATA_MAX: usize = 256;
// Индекс каталога (для readdir/unlink): персистится под спец-корнем ".dir" — формат
// count(1) | [nlen(1) | name]* . Роты-файлы ядро перечислять не даёт, поэтому список имён ведём сами.
static DIRROOT: [u8; b".dir".len()] = *b".dir";
// ── Веха 18.4: программа поверх POSIX-shim (mini-shell) ──
// Байтовые строки — только ASCII (в них нельзя не-ASCII). Читает их ЯДРО (SYS_WRITE/CALL).
static MOTD: [u8; b"motd.txt".len()] = *b"motd.txt";
static MOTDMSG: [u8; b"VOID says hi, written by mini-echo, kept by the personality\n".len()] =
    *b"VOID says hi, written by mini-echo, kept by the personality\n";
static CATLBL: [u8; b"[mini-sh] $ cat motd.txt\n".len()] = *b"[mini-sh] $ cat motd.txt\n";
static ECHOLBL: [u8; b"[mini-sh] $ echo \"...\" > motd.txt\n".len()] =
    *b"[mini-sh] $ echo \"...\" > motd.txt\n";
static LSLBL: [u8; b"[mini-sh] $ ls\n".len()] = *b"[mini-sh] $ ls\n";

// ── Веха 13/14: сервер объектного store ──
const OP_PUT: usize = 0;
const OP_GET: usize = 1;
const OP_SET_ROOT: usize = 2;
const OP_GET_ROOT: usize = 3;
static MSG: [u8; b"hello from VOID object-store client".len()] =
    *b"hello from VOID object-store client";
// Имя корня, к которому клиент привязывает значение (переживает перезагрузку). В секции `.user`
// (U|R|X): клиент читает его САМ в U-mode (собирает буфер запроса), а не только передаёт указатель
// ядру — значит, данные должны быть в U-доступной странице, а не в `.rodata` (там нет флага U).
#[link_section = ".user"]
static GREETING: [u8; b"greeting".len()] = *b"greeting";
static SP_PREV: [u8; b"[store-cli] root 'greeting' from previous boot: ".len()] =
    *b"[store-cli] root 'greeting' from previous boot: ";
static SP_FIRST: [u8; b"[store-cli] root 'greeting' not set yet (first boot)\n".len()] =
    *b"[store-cli] root 'greeting' not set yet (first boot)\n";
static SP_STORED: [u8; b"[store-cli] value stored and bound to root 'greeting' (survives reboot)\n".len()] =
    *b"[store-cli] value stored and bound to root 'greeting' (survives reboot)\n";
static SDENIED: [u8; b"[store-cli] direct OBJ_PUT DENIED by kernel (no store capability)\n".len()] =
    *b"[store-cli] direct OBJ_PUT DENIED by kernel (no store capability)\n";

/// Процесс-**драйвер блоков**: принимает номер сектора, читает его через шлюз `BLK_READ`
/// (предъявляя `dev_cap` — свой cap на устройство) и возвращает 512 байт клиенту через IPC.
/// Буфер — на стеке (без обнуления: `MaybeUninit`, иначе компилятор мог бы вызвать `memset`
/// вне секции `.user`).
#[link_section = ".user"]
extern "C" fn blk_server(dev_cap: usize) -> ! {
    let mut buf = MaybeUninit::<[u8; 512]>::uninit(); // буфер чтения/ответа
    let mut req = MaybeUninit::<[u8; 512]>::uninit(); // данные запроса (для записи)
    let bptr = buf.as_mut_ptr() as usize;
    let rptr = req.as_mut_ptr() as usize;
    loop {
        let op: usize;
        let from: usize;
        let len: usize;
        unsafe {
            // SYS_RECV(req, 512) -> a0=op (сектор | флаг записи), a1=from, a2=len (данные в req)
            asm!("ecall", in("a7") 4usize, inout("a0") rptr => op, inout("a1") 512usize => from, out("a2") len, options(nostack));
        }
        let sector = op & 0xffff_ffff; // младшие биты op = номер сектора
        if op & BLK_WRITE_FLAG == 0 {
            unsafe {
                // READ: BLK_READ(dev_cap, sector, buf); REPLY(from, buf, 512)
                asm!("ecall", in("a7") 7usize, inout("a0") dev_cap => _, in("a1") sector, in("a2") bptr, options(nostack));
                asm!("ecall", in("a7") 6usize, inout("a0") from => _, in("a1") bptr, in("a2") 512usize, options(nostack));
            }
        } else {
            unsafe {
                // WRITE: BLK_WRITE(dev_cap, sector, req, len); REPLY(from, ack=0)
                asm!("ecall", in("a7") 12usize, inout("a0") dev_cap => _, in("a1") sector, in("a2") rptr, in("a3") len, options(nostack));
                asm!("ecall", in("a7") 6usize, inout("a0") from => _, in("a1") bptr, in("a2") 0usize, options(nostack));
            }
        }
    }
}

/// Процесс-**клиент**: `arg` = cap на эндпоинт сервера. Читает сектор 0 (магия VOIDFS), затем
/// ЗАПИСЫВАЕТ паттерн в свободный сектор через сервер и читает его обратно (доказывая запись).
/// В конце пытается обратиться к диску НАПРЯМУЮ и подделать REPLY — оба раза получает отказ ядра.
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

        // Веха 17: ЗАПИСЬ сектора TEST через сервер (op = сектор | флаг записи). Данные = PATTERN;
        // ядро читает её из .rodata при копировании запроса, сам процесс её не трогает.
        asm!("ecall", in("a7") 5usize, inout("a0") ep_cap => _, in("a1") (BLK_TEST_SECTOR | BLK_WRITE_FLAG), in("a2") PATTERN.as_ptr() as usize, in("a3") PATTERN.len(), in("a4") 0usize, in("a5") 0usize, options(nostack));
        // Прочитать тот же сектор ОБРАТНО и напечатать — доказательство записи.
        asm!("ecall", in("a7") 5usize, inout("a0") ep_cap => _, in("a1") BLK_TEST_SECTOR, in("a2") 0usize, in("a3") 0usize, in("a4") bptr, in("a5") 512usize, options(nostack));
        asm!("ecall", in("a7") 1usize, inout("a0") WPREFIX.as_ptr() as usize => _, in("a1") WPREFIX.len(), options(nostack));
        asm!("ecall", in("a7") 1usize, inout("a0") bptr => _, in("a1") PATTERN.len(), options(nostack));
        asm!("ecall", in("a7") 1usize, inout("a0") NL.as_ptr() as usize => _, in("a1") NL.len(), options(nostack));

        // Попытка прямого доступа: BLK_READ с эндпоинт-cap (у клиента НЕТ cap на устройство).
        let denied: usize;
        asm!("ecall", in("a7") 7usize, inout("a0") ep_cap => denied, in("a1") 0usize, in("a2") bptr, options(nostack));
        if denied != 0 {
            asm!("ecall", in("a7") 1usize, inout("a0") DENIED.as_ptr() as usize => _, in("a1") DENIED.len(), options(nostack));
        }
        // Веха 15: попытка подделать REPLY эндпоинт-cap'ом (не reply-cap) → отказ ядра.
        let rforge: usize;
        asm!("ecall", in("a7") 6usize, inout("a0") ep_cap => rforge, in("a1") bptr, in("a2") 8usize, options(nostack));
        if rforge != 0 {
            asm!("ecall", in("a7") 1usize, inout("a0") RFORGE.as_ptr() as usize => _, in("a1") RFORGE.len(), options(nostack));
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
        } else if op == OP_GET {
            let vlen: usize;
            unsafe {
                // OBJ_GET(store_cap, id=req, val, 512) → длина; затем REPLY(from, val, длина)
                asm!("ecall", in("a7") 9usize, inout("a0") store_cap => vlen, in("a1") rptr, in("a2") vptr, in("a3") 512usize, options(nostack));
                asm!("ecall", in("a7") 6usize, inout("a0") from => _, in("a1") vptr, in("a2") vlen, options(nostack));
            }
        } else if op == OP_SET_ROOT {
            // req = [id(32) | name(len-32)]. OBJ_SET_ROOT(store_cap, name=req+32, len-32, id=req).
            unsafe {
                asm!("ecall", in("a7") 10usize, inout("a0") store_cap => _, in("a1") rptr + 32, in("a2") len - 32, in("a3") rptr, options(nostack));
                asm!("ecall", in("a7") 6usize, inout("a0") from => _, in("a1") rptr, in("a2") 0usize, options(nostack)); // ack
            }
        } else {
            // OP_GET_ROOT: req = name(len). OBJ_GET_ROOT(store_cap, req, len, id) → n (32/0).
            let n: usize;
            unsafe {
                asm!("ecall", in("a7") 11usize, inout("a0") store_cap => n, in("a1") rptr, in("a2") len, in("a3") iptr, options(nostack));
                asm!("ecall", in("a7") 6usize, inout("a0") from => _, in("a1") iptr, in("a2") n, options(nostack));
            }
        }
    }
}

/// Процесс-**клиент store**: `arg` = cap на эндпоинт сервера. Кладёт сообщение (`put`), получает
/// его content-id, привязывает к именованному корню `greeting` (переживёт перезагрузку) и на
/// СЛЕДУЮЩЕМ запуске читает прежнее значение обратно по этому корню. Затем пытается `OBJ_PUT`
/// НАПРЯМУЮ (эндпоинт-cap, не store) → отказ. Всё — только через сервер по IPC.
#[link_section = ".user"]
extern "C" fn store_client(ep_cap: usize) -> ! {
    let mut id = MaybeUninit::<[u8; 32]>::uninit();
    let mut val = MaybeUninit::<[u8; 512]>::uninit();
    let mut rq = MaybeUninit::<[u8; 64]>::uninit(); // [id(32) | name] для SET_ROOT
    let iptr = id.as_mut_ptr() as usize;
    let vptr = val.as_mut_ptr() as usize;
    let rqptr = rq.as_mut_ptr() as usize;
    let msg = MSG.as_ptr() as usize;
    let name = GREETING.as_ptr() as usize;
    unsafe {
        // 1. GET_ROOT("greeting") → в id лёг content-id (n=32), либо n=0 (корня ещё нет).
        let n: usize;
        asm!("ecall", in("a7") 5usize, inout("a0") ep_cap => n, in("a1") OP_GET_ROOT, in("a2") name, in("a3") GREETING.len(), in("a4") iptr, in("a5") 32usize, options(nostack));
        if n == 32 {
            // Корень есть с прошлого запуска: прочитать по нему значение и напечатать.
            let vlen: usize;
            asm!("ecall", in("a7") 5usize, inout("a0") ep_cap => vlen, in("a1") OP_GET, in("a2") iptr, in("a3") 32usize, in("a4") vptr, in("a5") 512usize, options(nostack));
            asm!("ecall", in("a7") 1usize, inout("a0") SP_PREV.as_ptr() as usize => _, in("a1") SP_PREV.len(), options(nostack));
            asm!("ecall", in("a7") 1usize, inout("a0") vptr => _, in("a1") vlen, options(nostack));
            asm!("ecall", in("a7") 1usize, inout("a0") NL.as_ptr() as usize => _, in("a1") NL.len(), options(nostack));
        } else {
            asm!("ecall", in("a7") 1usize, inout("a0") SP_FIRST.as_ptr() as usize => _, in("a1") SP_FIRST.len(), options(nostack));
        }
        // 2. PUT(MSG) → в id лёг content-id значения.
        asm!("ecall", in("a7") 5usize, inout("a0") ep_cap => _, in("a1") OP_PUT, in("a2") msg, in("a3") MSG.len(), in("a4") iptr, in("a5") 32usize, options(nostack));
        // 3. Собрать запрос SET_ROOT = [id(32) | "greeting"] сырыми записями (без memcpy в .user).
        let rb = rqptr as *mut u8;
        let mut i = 0usize;
        while i < 32 {
            *rb.add(i) = *(iptr as *const u8).add(i);
            i += 1;
        }
        let mut j = 0usize;
        while j < GREETING.len() {
            *rb.add(32 + j) = *(name as *const u8).add(j);
            j += 1;
        }
        // 4. SET_ROOT("greeting" → id): привязать корень (переживёт перезагрузку).
        asm!("ecall", in("a7") 5usize, inout("a0") ep_cap => _, in("a1") OP_SET_ROOT, in("a2") rqptr, in("a3") 32 + GREETING.len(), in("a4") 0usize, in("a5") 0usize, options(nostack));
        asm!("ecall", in("a7") 1usize, inout("a0") SP_STORED.as_ptr() as usize => _, in("a1") SP_STORED.len(), options(nostack));
        // 5. Попытка прямого доступа: OBJ_PUT с эндпоинт-cap (у клиента НЕТ cap на store).
        let denied: usize;
        asm!("ecall", in("a7") 8usize, inout("a0") ep_cap => denied, in("a1") msg, in("a2") MSG.len(), in("a3") iptr, options(nostack));
        if denied != 0 {
            asm!("ecall", in("a7") 1usize, inout("a0") SDENIED.as_ptr() as usize => _, in("a1") SDENIED.len(), options(nostack));
        }
        // SYS_EXIT(0)
        asm!("ecall", in("a7") 2usize, in("a0") 0usize, options(nostack, noreturn));
    }
}

/// Процесс-**CPU-bound** для демо вытеснения (Веха 16): крутит длинный busy-loop БЕЗ единого
/// `yield`/IPC и лишь печатает свою метку (`arg` = указатель на неё). Без вытеснения первый
/// процесс отработал бы все свои печати до второго; с вытеснением по таймеру их вывод
/// перемежается — видно, что ядро переключает процессы принудительно.
#[link_section = ".user"]
extern "C" fn busy(label: usize) -> ! {
    let mut round = 0usize;
    while round < 5 {
        // Чистый счёт в U-mode (никаких syscall) — во время него таймер и вытесняет.
        let mut acc = 0u64;
        let mut i = 0u64;
        while i < 12_000_000 {
            acc = acc.wrapping_add(i);
            i += 1;
        }
        unsafe {
            core::arch::asm!("/* {0} */", in(reg) acc, options(nostack)); // не дать выкинуть цикл
            // SYS_WRITE(label, 3) — ядро читает метку (в .rodata) и печатает.
            asm!("ecall", in("a7") 1usize, inout("a0") label => _, in("a1") 3usize, options(nostack));
        }
        round += 1;
    }
    unsafe { asm!("ecall", in("a7") 2usize, in("a0") 0usize, options(nostack, noreturn)) }
}

// ── Веха 18.3: индекс каталога (для readdir/unlink) ──
// Формат в RAM = в персистентном виде: count(1) | [nlen(1) | name]* . Всё сырыми указателями,
// чтобы не тянуть memcpy/паники в `.user`.

/// Есть ли имя в индексе каталога.
#[link_section = ".user"]
unsafe fn dir_contains(dir: *const u8, name: *const u8, nlen: usize) -> bool {
    let cnt = *dir as usize;
    let mut off = 1usize;
    let mut e = 0;
    while e < cnt {
        let l = *dir.add(off) as usize;
        off += 1;
        if l == nlen {
            let mut k = 0;
            let mut eq = true;
            while k < l {
                if *dir.add(off + k) != *name.add(k) {
                    eq = false;
                    break;
                }
                k += 1;
            }
            if eq {
                return true;
            }
        }
        off += l;
        e += 1;
    }
    false
}

/// Добавить имя в индекс, если его ещё нет. Возвращает `true`, если индекс изменился.
#[link_section = ".user"]
unsafe fn dir_add(dir: *mut u8, dir_len: &mut usize, name: *const u8, nlen: usize) -> bool {
    if dir_contains(dir, name, nlen) {
        return false;
    }
    if *dir_len + 1 + nlen > PX_DATA_MAX {
        return false; // нет места — упрощение (без ENOSPC)
    }
    let at = *dir_len;
    *dir.add(at) = nlen as u8;
    let mut k = 0;
    while k < nlen {
        *dir.add(at + 1 + k) = *name.add(k);
        k += 1;
    }
    *dir_len = at + 1 + nlen;
    *dir = *dir + 1; // count++
    true
}

/// Убрать имя из индекса (сдвиг хвоста). Возвращает `true`, если что-то удалили.
#[link_section = ".user"]
unsafe fn dir_remove(dir: *mut u8, dir_len: &mut usize, name: *const u8, nlen: usize) -> bool {
    let cnt = *dir as usize;
    let mut off = 1usize;
    let mut e = 0;
    while e < cnt {
        let l = *dir.add(off) as usize;
        let entry = 1 + l;
        if l == nlen {
            let mut k = 0;
            let mut eq = true;
            while k < l {
                if *dir.add(off + 1 + k) != *name.add(k) {
                    eq = false;
                    break;
                }
                k += 1;
            }
            if eq {
                let mut s = off + entry;
                let mut d = off;
                while s < *dir_len {
                    *dir.add(d) = *dir.add(s);
                    s += 1;
                    d += 1;
                }
                *dir_len -= entry;
                *dir = (cnt - 1) as u8; // count--
                return true;
            }
        }
        off += entry;
        e += 1;
    }
    false
}

/// Записать индекс каталога в store и привязать к спец-корню ".dir" (переживёт перезагрузку).
#[link_section = ".user"]
unsafe fn dir_persist(store_cap: usize, dir: *const u8, dir_len: usize, idb: usize) {
    asm!("ecall", in("a7") 8usize, inout("a0") store_cap => _, in("a1") dir as usize, in("a2") dir_len, in("a3") idb, options(nostack));
    asm!("ecall", in("a7") 10usize, inout("a0") store_cap => _, in("a1") DIRROOT.as_ptr() as usize, in("a2") DIRROOT.len(), in("a3") idb, options(nostack));
}

/// Процесс-**персоналия POSIX** (Вехи 18.1–18.3): даёт клиентам файловый API
/// `open/read/write/close/stat/unlink/readdir` по IPC. Namespace (имена → данные) и таблица
/// дескрипторов живут на СТЕКЕ сервера — у процессов нет кучи, поэтому фикс. размер. Данные — в
/// `MaybeUninit` (без обнуления), валидность отслеживаем сами (`fused`/`size`). Всё через сырые
/// указатели: ни `memcpy`, ни паник-путей в `.user`.
///
/// Веха 18.2 — **персистентность через store** (`arg` = cap на store): содержимое файла = значение
/// в объектном пространстве, привязанное к **корню-имени** (`OBJ_SET_ROOT`). Роты хранилища и есть
/// директория: `open(name)` = `get_root(name)` → есть? загрузить (`OBJ_GET`) : создать. Привязка к
/// корню и переживает GC ядра (достижимо от корня), и перезагрузку.
#[link_section = ".user"]
extern "C" fn posix_server(store_cap: usize) -> ! {
    // Namespace на стеке: имена, данные, метаданные.
    let mut names = MaybeUninit::<[[u8; PX_NAME_MAX]; PX_NFILES]>::uninit();
    let mut datas = MaybeUninit::<[[u8; PX_DATA_MAX]; PX_NFILES]>::uninit();
    let names_ptr = names.as_mut_ptr() as *mut u8;
    let datas_ptr = datas.as_mut_ptr() as *mut u8;
    let mut name_len = [0usize; PX_NFILES];
    let mut size = [0usize; PX_NFILES];
    let mut fused = [false; PX_NFILES];
    let mut dirty = [false; PX_NFILES]; // изменён с последней записи в store
    // Дескрипторы: fd → (файл, смещение). Смещение — СВОЁ на каждый дескриптор: два open одного
    // файла дают два независимых курсора (Веха 18.3).
    let mut fd_file = [0usize; PX_NFILES];
    let mut fd_off = [0usize; PX_NFILES];
    let mut fd_used = [false; PX_NFILES];

    // Веха 18.3: индекс каталога (readdir) — в RAM в персистентном виде, спец-корень ".dir".
    let mut dir = MaybeUninit::<[u8; PX_DATA_MAX]>::uninit();
    let dir_ptr = dir.as_mut_ptr() as *mut u8;
    let mut dir_len;

    let mut req = MaybeUninit::<[u8; 512]>::uninit();
    let mut rep = MaybeUninit::<[u8; 512]>::uninit();
    let mut idb = MaybeUninit::<[u8; 32]>::uninit(); // content-id для OBJ_PUT/GET
    let rptr = req.as_mut_ptr() as usize;
    let pptr = rep.as_mut_ptr() as usize;
    let idptr = idb.as_mut_ptr() as usize;

    // Поднять индекс каталога с прошлого запуска (get_root(".dir") → get), иначе — пустой.
    unsafe {
        let gn: usize;
        asm!("ecall", in("a7") 11usize, inout("a0") store_cap => gn, in("a1") DIRROOT.as_ptr() as usize, in("a2") DIRROOT.len(), in("a3") idptr, options(nostack));
        if gn == 32 {
            let n: usize;
            asm!("ecall", in("a7") 9usize, inout("a0") store_cap => n, in("a1") idptr, in("a2") dir_ptr as usize, in("a3") PX_DATA_MAX, options(nostack));
            if n == 0 {
                *dir_ptr = 0;
                dir_len = 1;
            } else {
                dir_len = n;
            }
        } else {
            *dir_ptr = 0; // count = 0
            dir_len = 1;
        }
    }

    loop {
        let op: usize;
        let from: usize;
        let len: usize;
        unsafe {
            // RECV(req, 512) -> op (opcode | fd<<8), from (reply-cap), len (нагрузка в req)
            asm!("ecall", in("a7") 4usize, inout("a0") rptr => op, inout("a1") 512usize => from, out("a2") len, options(nostack));
        }
        let opcode = op & 0xff;
        let fd = (op >> 8) & 0xff;
        let mode = (op >> 16) & 0xff; // Веха 18.3: режимы open (O_APPEND/O_TRUNC)
        let mut reply_len = 0usize; // сколько байт вернём в pptr

        if opcode == PX_OPEN {
            // req[..len] — имя. Найти файл или создать; выделить fd; вернуть [fd] (0xff — ошибка).
            let mut fidx = usize::MAX;
            let mut i = 0;
            while i < PX_NFILES {
                if fused[i] && name_len[i] == len {
                    let mut eq = true;
                    let mut k = 0;
                    while k < len {
                        let a = unsafe { *names_ptr.add(i * PX_NAME_MAX + k) };
                        let b = unsafe { *(rptr as *const u8).add(k) };
                        if a != b {
                            eq = false;
                            break;
                        }
                        k += 1;
                    }
                    if eq {
                        fidx = i;
                        break;
                    }
                }
                i += 1;
            }
            if fidx == usize::MAX {
                // Не в RAM — занять свободный слот и попробовать поднять из store, иначе создать.
                let mut j = 0;
                while j < PX_NFILES {
                    if !fused[j] {
                        fidx = j;
                        break;
                    }
                    j += 1;
                }
                if fidx != usize::MAX {
                    fused[fidx] = true;
                    dirty[fidx] = false;
                    let nl = if len > PX_NAME_MAX { PX_NAME_MAX } else { len };
                    name_len[fidx] = nl;
                    let mut k = 0;
                    while k < nl {
                        unsafe { *names_ptr.add(fidx * PX_NAME_MAX + k) = *(rptr as *const u8).add(k) };
                        k += 1;
                    }
                    // Веха 18.2: есть ли корень с этим именем? (get_root)
                    let gn: usize;
                    unsafe {
                        asm!("ecall", in("a7") 11usize, inout("a0") store_cap => gn, in("a1") rptr, in("a2") nl, in("a3") idptr, options(nostack));
                    }
                    if gn == 32 {
                        // Загрузить содержимое по content-id в буфер данных файла (OBJ_GET).
                        let dbuf = datas_ptr as usize + fidx * PX_DATA_MAX;
                        let sz: usize;
                        unsafe {
                            asm!("ecall", in("a7") 9usize, inout("a0") store_cap => sz, in("a1") idptr, in("a2") dbuf, in("a3") PX_DATA_MAX, options(nostack));
                        }
                        size[fidx] = sz;
                        // Веха 18.3: персистентный файл (в т.ч. привязанный к корню ДО появления
                        // индекса каталога, как hello.txt из 18.2) — занести в индекс, чтобы его
                        // видел readdir.
                        unsafe {
                            let np = names_ptr as usize + fidx * PX_NAME_MAX;
                            if dir_add(dir_ptr, &mut dir_len, np as *const u8, nl) {
                                dir_persist(store_cap, dir_ptr, dir_len, idptr);
                            }
                        }
                    } else {
                        size[fidx] = 0; // новый файл
                    }
                }
            }
            let mut nfd = usize::MAX;
            if fidx != usize::MAX {
                // Веха 18.3: O_TRUNC обнуляет содержимое (пометив изменённым — перезапишется на close).
                if mode & O_TRUNC != 0 {
                    size[fidx] = 0;
                    dirty[fidx] = true;
                }
                let mut d = 0;
                while d < PX_NFILES {
                    if !fd_used[d] {
                        nfd = d;
                        break;
                    }
                    d += 1;
                }
                if nfd != usize::MAX {
                    fd_used[nfd] = true;
                    fd_file[nfd] = fidx;
                    // O_APPEND ставит курсор в конец, иначе — в начало.
                    fd_off[nfd] = if mode & O_APPEND != 0 { size[fidx] } else { 0 };
                }
            }
            unsafe { *(pptr as *mut u8) = if nfd == usize::MAX { 0xff } else { nfd as u8 } };
            reply_len = 1;
        } else if opcode == PX_WRITE {
            // req[..len] — данные; дописать в файл дескриптора со смещения fd_off.
            if fd < PX_NFILES && fd_used[fd] {
                let fi = fd_file[fd];
                let mut w = fd_off[fd];
                let mut k = 0;
                while k < len && w < PX_DATA_MAX {
                    unsafe { *datas_ptr.add(fi * PX_DATA_MAX + w) = *(rptr as *const u8).add(k) };
                    w += 1;
                    k += 1;
                }
                fd_off[fd] = w;
                if w > size[fi] {
                    size[fi] = w;
                }
                dirty[fi] = true; // Веха 18.2: пометить для записи в store при close
            }
        } else if opcode == PX_READ {
            // Прочитать из файла со смещения до конца (клиент ограничит своим recv_cap).
            if fd < PX_NFILES && fd_used[fd] {
                let fi = fd_file[fd];
                let mut r = fd_off[fd];
                while r < size[fi] && reply_len < 512 {
                    unsafe { *(pptr as *mut u8).add(reply_len) = *datas_ptr.add(fi * PX_DATA_MAX + r) };
                    r += 1;
                    reply_len += 1;
                }
                fd_off[fd] = r;
            }
        } else if opcode == PX_STAT {
            // req[..len] — имя. Вернуть [exists:1 | size:4 LE]. Ищем в RAM, затем среди корней.
            let mut sz = usize::MAX;
            let mut i = 0;
            while i < PX_NFILES {
                if fused[i] && name_len[i] == len {
                    let mut eq = true;
                    let mut k = 0;
                    while k < len {
                        let a = unsafe { *names_ptr.add(i * PX_NAME_MAX + k) };
                        let b = unsafe { *(rptr as *const u8).add(k) };
                        if a != b {
                            eq = false;
                            break;
                        }
                        k += 1;
                    }
                    if eq {
                        sz = size[i];
                        break;
                    }
                }
                i += 1;
            }
            if sz == usize::MAX {
                // Не в RAM — есть ли персистентный корень? Если да, подгрузить ради длины.
                let gn: usize;
                unsafe {
                    asm!("ecall", in("a7") 11usize, inout("a0") store_cap => gn, in("a1") rptr, in("a2") len, in("a3") idptr, options(nostack));
                }
                if gn == 32 {
                    let n: usize;
                    unsafe {
                        asm!("ecall", in("a7") 9usize, inout("a0") store_cap => n, in("a1") idptr, in("a2") pptr + 8, in("a3") PX_DATA_MAX, options(nostack));
                    }
                    sz = n;
                }
            }
            let exists: u8 = if sz == usize::MAX { 0 } else { 1 };
            let szv = (if sz == usize::MAX { 0 } else { sz }) as u32;
            unsafe {
                *(pptr as *mut u8) = exists;
                *(pptr as *mut u8).add(1) = (szv & 0xff) as u8;
                *(pptr as *mut u8).add(2) = ((szv >> 8) & 0xff) as u8;
                *(pptr as *mut u8).add(3) = ((szv >> 16) & 0xff) as u8;
                *(pptr as *mut u8).add(4) = ((szv >> 24) & 0xff) as u8;
            }
            reply_len = 5;
        } else if opcode == PX_UNLINK {
            // req[..len] — имя. Убрать из RAM namespace, снять корень (OBJ_DEL_ROOT) и из каталога.
            let mut i = 0;
            while i < PX_NFILES {
                if fused[i] && name_len[i] == len {
                    let mut eq = true;
                    let mut k = 0;
                    while k < len {
                        let a = unsafe { *names_ptr.add(i * PX_NAME_MAX + k) };
                        let b = unsafe { *(rptr as *const u8).add(k) };
                        if a != b {
                            eq = false;
                            break;
                        }
                        k += 1;
                    }
                    if eq {
                        fused[i] = false;
                        // закрыть висящие дескрипторы на удаляемый файл
                        let mut d = 0;
                        while d < PX_NFILES {
                            if fd_used[d] && fd_file[d] == i {
                                fd_used[d] = false;
                            }
                            d += 1;
                        }
                        break;
                    }
                }
                i += 1;
            }
            unsafe {
                // снять персистентный корень — объект уйдёт в GC (честный unlink, Веха 18.3)
                asm!("ecall", in("a7") 13usize, inout("a0") store_cap => _, in("a1") rptr, in("a2") len, options(nostack));
                // убрать имя из индекса каталога и переписать ".dir"
                if dir_remove(dir_ptr, &mut dir_len, rptr as *const u8, len) {
                    dir_persist(store_cap, dir_ptr, dir_len, idptr);
                }
                *(pptr as *mut u8) = 0;
            }
            reply_len = 1;
        } else if opcode == PX_READDIR {
            // Вернуть имена файлов из индекса каталога, разделённые '\n'.
            let cnt = unsafe { *dir_ptr } as usize;
            let mut off = 1usize;
            let mut e = 0;
            while e < cnt {
                let nl = unsafe { *dir_ptr.add(off) } as usize;
                off += 1;
                let mut k = 0;
                while k < nl && reply_len < 511 {
                    unsafe { *(pptr as *mut u8).add(reply_len) = *dir_ptr.add(off + k) };
                    reply_len += 1;
                    k += 1;
                }
                off += nl;
                if reply_len < 511 {
                    unsafe { *(pptr as *mut u8).add(reply_len) = b'\n' };
                    reply_len += 1;
                }
                e += 1;
            }
        } else {
            // PX_CLOSE — если файл менялся, записать содержимое в store, привязать к корню-имени и
            // занести имя в индекс каталога (Веха 18.2/18.3: переживёт GC/перезагрузку, виден в
            // readdir). Затем освободить дескриптор.
            if fd < PX_NFILES && fd_used[fd] {
                let fi = fd_file[fd];
                if dirty[fi] {
                    let dbuf = datas_ptr as usize + fi * PX_DATA_MAX;
                    let nptr = names_ptr as usize + fi * PX_NAME_MAX;
                    unsafe {
                        // OBJ_PUT(content) -> id; OBJ_SET_ROOT(name, id)
                        asm!("ecall", in("a7") 8usize, inout("a0") store_cap => _, in("a1") dbuf, in("a2") size[fi], in("a3") idptr, options(nostack));
                        asm!("ecall", in("a7") 10usize, inout("a0") store_cap => _, in("a1") nptr, in("a2") name_len[fi], in("a3") idptr, options(nostack));
                        // Веха 18.3: имя файла — в индекс каталога (idempotent) и переписать ".dir".
                        if dir_add(dir_ptr, &mut dir_len, nptr as *const u8, name_len[fi]) {
                            dir_persist(store_cap, dir_ptr, dir_len, idptr);
                        }
                    }
                    dirty[fi] = false;
                }
                fd_used[fd] = false;
            }
        }

        unsafe {
            // REPLY(reply-cap, pptr, reply_len)
            asm!("ecall", in("a7") 6usize, inout("a0") from => _, in("a1") pptr, in("a2") reply_len, options(nostack));
        }
    }
}

// ── Веха 18.4: POSIX-shim (libc-заглушка) ──
// Тонкий слой, ПРЯЧУЩИЙ ecall/op-коды/capability/IPC. Программа зовёт только эти функции и
// «не знает», что под ней VOID: `ep` — непрозрачный дескриптор «связи с ОС», выданный при запуске
// (как контекст libc). Дескрипторы наружу смещены на +3: 0/1/2 зарезервированы под stdin/stdout/
// stderr (POSIX). `write` на 1/2 идёт в консоль ядра, на ≥3 — в персоналию.
const STDOUT: usize = 1;
const FD_BASE: usize = 3;

/// `open(name, mode) -> fd` (или `usize::MAX`). Программе не видны ни op-код, ни reply-cap.
#[link_section = ".user"]
unsafe fn sh_open(ep: usize, name: *const u8, name_len: usize, mode: usize) -> usize {
    let mut r = MaybeUninit::<[u8; 4]>::uninit();
    let rp = r.as_mut_ptr() as usize;
    asm!("ecall", in("a7") 5usize, inout("a0") ep => _, in("a1") PX_OPEN | (mode << 16), in("a2") name as usize, in("a3") name_len, in("a4") rp, in("a5") 4usize, options(nostack));
    let pfd = *(rp as *const u8) as usize;
    if pfd == 0xff { usize::MAX } else { pfd + FD_BASE }
}

/// `read(fd, buf, cap) -> n`.
#[link_section = ".user"]
unsafe fn sh_read(ep: usize, fd: usize, buf: *mut u8, cap: usize) -> usize {
    if fd < FD_BASE {
        return 0;
    }
    let pfd = fd - FD_BASE;
    let n: usize;
    asm!("ecall", in("a7") 5usize, inout("a0") ep => n, in("a1") PX_READ | (pfd << 8), in("a2") 0usize, in("a3") 0usize, in("a4") buf as usize, in("a5") cap, options(nostack));
    n
}

/// `write(fd, buf, len) -> len`. `fd`=1/2 → консоль (программа не знает, что это UART ядра).
#[link_section = ".user"]
unsafe fn sh_write(ep: usize, fd: usize, buf: *const u8, len: usize) -> usize {
    if fd < FD_BASE {
        // stdout/stderr → SYS_WRITE ядра.
        asm!("ecall", in("a7") 1usize, inout("a0") buf as usize => _, in("a1") len, options(nostack));
        return len;
    }
    let pfd = fd - FD_BASE;
    asm!("ecall", in("a7") 5usize, inout("a0") ep => _, in("a1") PX_WRITE | (pfd << 8), in("a2") buf as usize, in("a3") len, in("a4") 0usize, in("a5") 0usize, options(nostack));
    len
}

/// `close(fd)`.
#[link_section = ".user"]
unsafe fn sh_close(ep: usize, fd: usize) {
    if fd < FD_BASE {
        return;
    }
    let pfd = fd - FD_BASE;
    asm!("ecall", in("a7") 5usize, inout("a0") ep => _, in("a1") PX_CLOSE | (pfd << 8), in("a2") 0usize, in("a3") 0usize, in("a4") 0usize, in("a5") 0usize, options(nostack));
}

/// `readdir(buf, cap) -> n`: имена файлов через '\n' (для `ls`).
#[link_section = ".user"]
unsafe fn sh_readdir(ep: usize, buf: *mut u8, cap: usize) -> usize {
    let n: usize;
    asm!("ecall", in("a7") 5usize, inout("a0") ep => n, in("a1") PX_READDIR, in("a2") 0usize, in("a3") 0usize, in("a4") buf as usize, in("a5") cap, options(nostack));
    n
}

/// `exit(code)`: завершить процесс (роль C-runtime, не файловый I/O).
#[link_section = ".user"]
unsafe fn sh_exit(code: usize) -> ! {
    asm!("ecall", in("a7") 2usize, in("a0") code, options(nostack, noreturn));
}

/// `mini-echo`: записать строку в файл (как `echo msg > path`). Только через shim.
#[link_section = ".user"]
unsafe fn mini_echo(ep: usize, path: *const u8, path_len: usize, msg: *const u8, msg_len: usize) {
    let fd = sh_open(ep, path, path_len, O_TRUNC);
    if fd != usize::MAX {
        sh_write(ep, fd, msg, msg_len);
        sh_close(ep, fd);
    }
}

/// `mini-cat`: прочитать файл и вывести в stdout (как `cat path`). Только через shim.
#[link_section = ".user"]
unsafe fn mini_cat(ep: usize, path: *const u8, path_len: usize) {
    let mut buf = MaybeUninit::<[u8; 512]>::uninit();
    let bp = buf.as_mut_ptr() as *mut u8;
    let fd = sh_open(ep, path, path_len, 0);
    if fd == usize::MAX {
        return;
    }
    loop {
        let n = sh_read(ep, fd, bp, 512);
        if n == 0 {
            break;
        }
        sh_write(ep, STDOUT, bp, n);
    }
    sh_close(ep, fd);
}

/// Процесс-**mini-shell** (Веха 18.4): `arg` = дескриптор персоналии. Написан ЦЕЛИКОМ на POSIX-shim
/// — ни одного `ecall`, op-кода или capability в теле; «не знает» ни про IPC, ни про VOID. Играет
/// маленькую сессию `cat; echo > ; cat; ls` над `motd.txt` (файл переживает перезагрузку).
#[link_section = ".user"]
extern "C" fn mini_sh(ep: usize) -> ! {
    let mut buf = MaybeUninit::<[u8; 512]>::uninit();
    let bp = buf.as_mut_ptr() as *mut u8;
    unsafe {
        // $ cat motd.txt   (покажет содержимое с прошлой загрузки — на первой пусто)
        sh_write(ep, STDOUT, CATLBL.as_ptr(), CATLBL.len());
        mini_cat(ep, MOTD.as_ptr(), MOTD.len());
        // $ echo "..." > motd.txt
        sh_write(ep, STDOUT, ECHOLBL.as_ptr(), ECHOLBL.len());
        mini_echo(ep, MOTD.as_ptr(), MOTD.len(), MOTDMSG.as_ptr(), MOTDMSG.len());
        // $ cat motd.txt   (только что записанное)
        sh_write(ep, STDOUT, CATLBL.as_ptr(), CATLBL.len());
        mini_cat(ep, MOTD.as_ptr(), MOTD.len());
        // $ ls
        sh_write(ep, STDOUT, LSLBL.as_ptr(), LSLBL.len());
        let n = sh_readdir(ep, bp, 512);
        sh_write(ep, STDOUT, bp, n);
        sh_exit(0);
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
pub fn busy_entry() -> usize {
    busy as *const () as usize
}
pub fn posix_server_entry() -> usize {
    posix_server as *const () as usize
}
pub fn mini_sh_entry() -> usize {
    mini_sh as *const () as usize
}
pub fn label_a() -> usize {
    LBL_A.as_ptr() as usize
}
pub fn label_b() -> usize {
    LBL_B.as_ptr() as usize
}
