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
//!   12 = BLK_WRITE(dev_cap,sector,buf,len) -> 0/MAX.
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

// ── Веха 18.1: POSIX-персоналия (файлы поверх IPC) ──
// op персоналии кодирует операцию (младший байт) и дескриптор fd (следующий байт): op | (fd<<8).
const PX_OPEN: usize = 0;
const PX_READ: usize = 1;
const PX_WRITE: usize = 2;
const PX_CLOSE: usize = 3;
const PX_NFILES: usize = 4; // namespace фикс. размера (у процессов нет кучи — всё на стеке)
const PX_NAME_MAX: usize = 16;
const PX_DATA_MAX: usize = 256;
static FNAME: [u8; b"hello.txt".len()] = *b"hello.txt";
static FCONTENT: [u8; b"Hello, POSIX personality on VOID!".len()] =
    *b"Hello, POSIX personality on VOID!";
static PXPREV: [u8; b"[posix-app] hello.txt from previous boot: ".len()] =
    *b"[posix-app] hello.txt from previous boot: ";
static PXFIRST: [u8; b"[posix-app] hello.txt not found, writing it (first boot)\n".len()] =
    *b"[posix-app] hello.txt not found, writing it (first boot)\n";

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

/// Процесс-**персоналия POSIX** (Вехи 18.1/18.2): даёт клиентам файловый API `open/read/write/close`
/// по IPC. Namespace (имена → данные) и таблица дескрипторов живут на СТЕКЕ сервера — у процессов
/// нет кучи, поэтому фикс. размер. Данные — в `MaybeUninit` (без обнуления), валидность отслеживаем
/// сами (`fused`/`size`). Всё через сырые указатели: ни `memcpy`, ни паник-путей в `.user`.
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
    // Дескрипторы: fd → (файл, смещение).
    let mut fd_file = [0usize; PX_NFILES];
    let mut fd_off = [0usize; PX_NFILES];
    let mut fd_used = [false; PX_NFILES];

    let mut req = MaybeUninit::<[u8; 512]>::uninit();
    let mut rep = MaybeUninit::<[u8; 512]>::uninit();
    let mut idb = MaybeUninit::<[u8; 32]>::uninit(); // content-id для OBJ_PUT/GET
    let rptr = req.as_mut_ptr() as usize;
    let pptr = rep.as_mut_ptr() as usize;
    let idptr = idb.as_mut_ptr() as usize;

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
                    } else {
                        size[fidx] = 0; // новый файл
                    }
                }
            }
            let mut nfd = usize::MAX;
            if fidx != usize::MAX {
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
                    fd_off[nfd] = 0;
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
        } else {
            // PX_CLOSE — если файл менялся, записать содержимое в store и привязать к корню-имени
            // (Веха 18.2: переживёт GC и перезагрузку). Затем освободить дескриптор.
            if fd < PX_NFILES && fd_used[fd] {
                let fi = fd_file[fd];
                if dirty[fi] {
                    let dbuf = datas_ptr as usize + fi * PX_DATA_MAX;
                    let nptr = names_ptr as usize + fi * PX_NAME_MAX;
                    unsafe {
                        // OBJ_PUT(content) -> id; OBJ_SET_ROOT(name, id)
                        asm!("ecall", in("a7") 8usize, inout("a0") store_cap => _, in("a1") dbuf, in("a2") size[fi], in("a3") idptr, options(nostack));
                        asm!("ecall", in("a7") 10usize, inout("a0") store_cap => _, in("a1") nptr, in("a2") name_len[fi], in("a3") idptr, options(nostack));
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

/// Процесс-**POSIX-программа** (Вехи 18.1/18.2): `arg` = cap на эндпоинт персоналии. Пользуется
/// только POSIX-подобными вызовами через IPC-shim — и «не знает», что под ним VOID. Открывает
/// `hello.txt` и читает: если файл есть с прошлого запуска — печатает содержимое (persistence);
/// если пусто (первый запуск) — записывает строку. На `close` персоналия сохраняет файл в store.
#[link_section = ".user"]
extern "C" fn posix_client(ep: usize) -> ! {
    let mut buf = MaybeUninit::<[u8; 512]>::uninit();
    let bptr = buf.as_mut_ptr() as usize;
    let name = FNAME.as_ptr() as usize;
    let content = FCONTENT.as_ptr() as usize;
    unsafe {
        // fd = open("hello.txt")
        asm!("ecall", in("a7") 5usize, inout("a0") ep => _, in("a1") PX_OPEN, in("a2") name, in("a3") FNAME.len(), in("a4") bptr, in("a5") 4usize, options(nostack));
        let fd = *(bptr as *const u8) as usize;
        // n = read(fd) — содержимое файла (пусто на первом запуске).
        let n: usize;
        asm!("ecall", in("a7") 5usize, inout("a0") ep => n, in("a1") PX_READ | (fd << 8), in("a2") 0usize, in("a3") 0usize, in("a4") bptr, in("a5") 512usize, options(nostack));
        if n > 0 {
            // Файл есть с прошлого запуска — напечатать его содержимое.
            asm!("ecall", in("a7") 1usize, inout("a0") PXPREV.as_ptr() as usize => _, in("a1") PXPREV.len(), options(nostack));
            asm!("ecall", in("a7") 1usize, inout("a0") bptr => _, in("a1") n, options(nostack));
            asm!("ecall", in("a7") 1usize, inout("a0") NL.as_ptr() as usize => _, in("a1") NL.len(), options(nostack));
        } else {
            // Первый запуск — записать содержимое (сохранится на close).
            asm!("ecall", in("a7") 1usize, inout("a0") PXFIRST.as_ptr() as usize => _, in("a1") PXFIRST.len(), options(nostack));
            asm!("ecall", in("a7") 5usize, inout("a0") ep => _, in("a1") PX_WRITE | (fd << 8), in("a2") content, in("a3") FCONTENT.len(), in("a4") 0usize, in("a5") 0usize, options(nostack));
        }
        // close(fd) — персоналия сохранит изменённый файл в store.
        asm!("ecall", in("a7") 5usize, inout("a0") ep => _, in("a1") PX_CLOSE | (fd << 8), in("a2") 0usize, in("a3") 0usize, in("a4") 0usize, in("a5") 0usize, options(nostack));
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
pub fn busy_entry() -> usize {
    busy as *const () as usize
}
pub fn posix_server_entry() -> usize {
    posix_server as *const () as usize
}
pub fn posix_client_entry() -> usize {
    posix_client as *const () as usize
}
pub fn label_a() -> usize {
    LBL_A.as_ptr() as usize
}
pub fn label_b() -> usize {
    LBL_B.as_ptr() as usize
}
