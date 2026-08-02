//! Библиотека userspace-программ VOID (Веха 23): syscall-шимы + POSIX-shim.
//!
//! До Вехи 23 код процессов жил в секции `.user` образа ядра и не смел касаться ничего за
//! пределами своих страниц: ни memset на zero-init, ни jump-таблиц в .rodata, ни вызовов
//! core-функций — всё писалось сырыми указателями с `MaybeUninit`. Теперь каждая программа —
//! собственный статический ELF (см. `src/bin/*`), где работает обычный Rust: массивы, слайсы,
//! `copy_from_slice` — всё линкуется в сам бинарь и исполняется со страниц процесса.
//!
//! Здесь — единственное место, где программы видят инструкцию syscall'а (модуль [`abi`],
//! Веха 26: `ecall` на riscv64, `int 0x80` на x86_64 — один исходник, обе архитектуры).
//! Соглашение (ABI v1) одинаково ПО СМЫСЛУ: номер + до 7 аргументов + до 4 регистров
//! результата (подробности — в шапке `kernel/src/proc.rs`; раскладка по регистрам —
//! в `kernel/src/arch/*/trap.rs`). `usize::MAX` (== [`NO_CAP`]) в позиции capability
//! значит «права нет».
#![no_std]

/// lx_emul — минимальный Linux-API шим поверх фундамента userspace-драйверов (Веха 53):
/// `ioremap`/`kmalloc`/`dma_alloc_coherent`/`request_irq`/`readl`-`writel`/driver-model
/// поверх [`mmio_map`]/[`dma_alloc`]/[`irq_wait`]/[`thread_spawn`]. Первый шаг к хостингу
/// Linux-драйверов (Genode `dde_linux`-стиль).
/// Веха 90 — мост smoltcp↔VOID (устройство поверх сырых кадров ядра).
pub mod net_phy;

/// Веха 93 — протокол `net-srv` (номера операций, коды статуса) и клиентская обёртка:
/// `tcp_connect`/`tcp_send`/`tcp_recv`/`tcp_close`. Общий для сервера и его клиентов.
pub mod net_cli;

pub mod lx_emul;

// ─── номера syscall'ов (ABI v1, см. libs/void-abi и kernel/src/proc.rs) ───────
const SYS_WRITE: usize = 1;
const SYS_EXIT: usize = 2;
const SYS_YIELD: usize = 3;
const SYS_RECV: usize = 4;
const SYS_CALL: usize = 5;
const SYS_REPLY: usize = 6;
const SYS_BLK_READ: usize = 7;
const SYS_OBJ_PUT: usize = 8;
const SYS_OBJ_GET: usize = 9;
const SYS_OBJ_SET_ROOT: usize = 10;
const SYS_OBJ_GET_ROOT: usize = 11;
const SYS_BLK_WRITE: usize = 12;
const SYS_OBJ_DEL_ROOT: usize = 13;
const SYS_READ: usize = 14;
const SYS_EXEC: usize = 15;
const SYS_CAP_DERIVE: usize = 16;
const SYS_MAP: usize = 17;
const SYS_ARGS: usize = 18;
const SYS_STARTCAP: usize = 19;
const SYS_NET_SEND: usize = 20;
const SYS_NET_RECV: usize = 21;
const SYS_NET_MAC: usize = 22;
const SYS_THREAD_SPAWN: usize = 23;
const SYS_THREAD_EXIT: usize = 24;
const SYS_THREAD_JOIN: usize = 25;
const SYS_FUTEX: usize = 26;
const SYS_SET_TLS: usize = 27;
const SYS_CHECKPOINT: usize = 28;
const SYS_RESTORE: usize = 29;
const SYS_INSTALL: usize = 30;
const SYS_MMIO_MAP: usize = 31;
const SYS_DMA_ALLOC: usize = 32;
const SYS_IRQ_WAIT: usize = 33;
const SYS_OBJ_LIST_ROOTS: usize = 34;
const SYS_LOG: usize = 35;
const SYS_TIME: usize = 36;
const SYS_RANDOM: usize = 37;

/// «Capability отсутствует» — в аргументах и результатах IPC.
pub const NO_CAP: usize = usize::MAX;

/// Паника программы — завершиться ненулевым кодом, не трогая ядро: раскрутки стека нет
/// (panic="abort"), а печатать backtrace — не забота userspace-программы.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(101)
}

// ─── арх-слой: инструкция syscall'а (Веха 26) ─────────────────────────────────

/// Единственное арх-специфичное место userspace: как передать ядру номер, до 7
/// аргументов и забрать до 4 регистров результата. Раскладка зеркалит методы
/// `TrapFrame` контракта ядра (`arg(i)`, `set_ret_at(i)`).
mod abi {
    /// riscv64: номер в a7, аргументы a0..a6, результаты a0..a3 (`ecall`).
    #[cfg(target_arch = "riscv64")]
    #[inline(always)]
    pub fn syscall(
        num: usize, mut a0: usize, mut a1: usize, mut a2: usize, mut a3: usize,
        a4: usize, a5: usize, a6: usize,
    ) -> (usize, usize, usize, usize) {
        unsafe {
            core::arch::asm!("ecall", in("a7") num,
                inout("a0") a0, inout("a1") a1, inout("a2") a2, inout("a3") a3,
                in("a4") a4, in("a5") a5, in("a6") a6, options(nostack));
        }
        (a0, a1, a2, a3)
    }

    /// riscv64: невозвращающийся syscall (SYS_EXIT).
    #[cfg(target_arch = "riscv64")]
    pub fn syscall_noreturn(num: usize, a0: usize) -> ! {
        unsafe {
            core::arch::asm!("ecall", in("a7") num, in("a0") a0, options(nostack, noreturn))
        }
    }

    /// x86_64: номер в rax, аргументы rdi,rsi,rdx,r10,r8,r9,rbx, результаты
    /// rax,rdi,rsi,rdx (`int 0x80`). rbx нельзя назвать операндом asm! (резерв LLVM) —
    /// 7-й аргумент заезжает в него через `xchg` и выезжает обратно.
    #[cfg(target_arch = "x86_64")]
    #[inline(always)]
    pub fn syscall(
        num: usize, mut a0: usize, mut a1: usize, mut a2: usize, a3: usize,
        a4: usize, a5: usize, a6: usize,
    ) -> (usize, usize, usize, usize) {
        let r0;
        unsafe {
            core::arch::asm!(
                "xchg rbx, {a6}",
                "int 0x80",
                "xchg rbx, {a6}",
                a6 = inout(reg) a6 => _,
                inout("rax") num => r0,
                inout("rdi") a0, inout("rsi") a1, inout("rdx") a2,
                in("r10") a3, in("r8") a4, in("r9") a5,
                options(nostack),
            );
        }
        (r0, a0, a1, a2)
    }

    /// x86_64: невозвращающийся syscall (SYS_EXIT).
    #[cfg(target_arch = "x86_64")]
    pub fn syscall_noreturn(num: usize, a0: usize) -> ! {
        unsafe {
            core::arch::asm!("int 0x80", in("rax") num, in("rdi") a0,
                options(nostack, noreturn))
        }
    }
}

// ─── базовые syscall'ы ────────────────────────────────────────────────────────

/// `SYS_WRITE`: напечатать байты в консоль (ядро читает буфер процесса напрямую).
pub fn write(buf: &[u8]) {
    abi::syscall(SYS_WRITE, buf.as_ptr() as usize, buf.len(), 0, 0, 0, 0, 0);
}

/// `SYS_EXIT`: завершить процесс с кодом (родителю в `SYS_EXEC` вернётся именно он).
pub fn exit(code: usize) -> ! {
    abi::syscall_noreturn(SYS_EXIT, code)
}

/// `SYS_YIELD`: уступить процессор следующему готовому процессу.
pub fn yield_now() {
    abi::syscall(SYS_YIELD, 0, 0, 0, 0, 0, 0, 0);
}

// ─── нити (Веха 35) ───────────────────────────────────────────────────────────

/// `SYS_THREAD_SPAWN`: завести нить, исполняющую `entry(arg)` на стеке с вершиной
/// `stack_top` (в том же адресном пространстве и домене). Возвращает id нити (для
/// [`thread_join`]) или [`usize::MAX`]. Стек — забота вызывающего (обычно область,
/// выделенная из кучи процесса).
pub fn thread_spawn(entry: usize, arg: usize, stack_top: usize) -> usize {
    abi::syscall(SYS_THREAD_SPAWN, entry, arg, stack_top, 0, 0, 0, 0).0
}

/// `SYS_THREAD_EXIT`: завершить ТЕКУЩУЮ нить, отдав `retval` присоединяющемуся
/// ([`thread_join`]). Процесс продолжают жить прочие нити (в отличие от [`exit`]).
pub fn thread_exit(retval: usize) -> ! {
    abi::syscall_noreturn(SYS_THREAD_EXIT, retval)
}

/// `SYS_THREAD_JOIN`: дождаться нити `tid` своей группы и забрать её `retval`
/// ([`usize::MAX`] — нет такой нити / чужая группа).
pub fn thread_join(tid: usize) -> usize {
    abi::syscall(SYS_THREAD_JOIN, tid, 0, 0, 0, 0, 0, 0).0
}

/// `SYS_FUTEX` WAIT: уснуть на слове `*uaddr`, пока оно равно `expected`. `timeout_ticks`
/// = 0 — бессрочно (иначе бюджет в тиках [`now`]). Возврат: 0 — разбужены, 1 — таймаут.
/// Примитив для Mutex/Condvar/Parker в std.
pub fn futex_wait(uaddr: *const u32, expected: u32, timeout_ticks: usize) -> usize {
    abi::syscall(SYS_FUTEX, 0, uaddr as usize, expected as usize, timeout_ticks, 0, 0, 0).0
}

/// `SYS_FUTEX` WAKE: разбудить до `count` нитей, спящих на слове `*uaddr`. Возврат —
/// число разбуженных.
pub fn futex_wake(uaddr: *const u32, count: usize) -> usize {
    abi::syscall(SYS_FUTEX, 1, uaddr as usize, count, 0, 0, 0, 0).0
}

/// `SYS_SET_TLS`: задать TLS-указатель текущей нити (`tp` на riscv / база `%fs` на x86).
/// Зовётся один раз при старте нити, после построения её TLS-блока.
pub fn set_tls(tp: usize) {
    abi::syscall(SYS_SET_TLS, tp, 0, 0, 0, 0, 0, 0);
}

/// Принятый запрос IPC: op отправителя, одноразовый reply-cap, длина нагрузки в буфере
/// и capability, переданная в сообщении ([`NO_CAP`] — не было).
pub struct Message {
    pub op: usize,
    pub reply_cap: usize,
    pub len: usize,
    pub cap: usize,
}

/// `SYS_RECV`: ждать запрос; нагрузка ложится в `buf` (усечённая по его размеру).
pub fn recv(buf: &mut [u8]) -> Message {
    let (op, reply_cap, len, cap) =
        abi::syscall(SYS_RECV, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0, 0, 0);
    Message { op, reply_cap, len, cap }
}

/// Веха 90 — `SYS_RECV` БЕЗ блокировки: `None`, если запросов нет прямо сейчас.
/// Нужен серверам, которым между запросами есть чем заняться, — прежде всего сетевому:
/// стек обязан тикать (входящие, ретрансмиссии), даже когда клиенты молчат.
pub fn try_recv(buf: &mut [u8]) -> Option<Message> {
    let (op, reply_cap, len, cap) =
        abi::syscall(SYS_RECV, buf.as_mut_ptr() as usize, buf.len(), 1, 0, 0, 0, 0);
    (op != usize::MAX).then_some(Message { op, reply_cap, len, cap })
}

/// Веха 91 — `SYS_RECV` со СНОМ до дедлайна: `None`, если за `timeout_ticks` запроса не было.
/// Это «сон вместо опроса» для серверов-реакторов: пока никто не зовёт и делать нечего, процесс
/// не занимает процессор вовсе, но просыпается к моменту, который назвал сам (у сетевого стека
/// это `poll_at` — ближайший таймер ретрансмиссии).
pub fn recv_timeout(buf: &mut [u8], timeout_ticks: usize) -> Option<Message> {
    let (op, reply_cap, len, cap) = abi::syscall(
        SYS_RECV, buf.as_mut_ptr() as usize, buf.len(), 2, timeout_ticks, 0, 0, 0,
    );
    (op != usize::MAX).then_some(Message { op, reply_cap, len, cap })
}

/// Веха 91 - `SYS_RECV` со сном до дедлайна ИЛИ до прихода СЕТЕВОГО КАДРА. То, ради чего веха:
/// сетевой сервер спит, ничего не занимая, и просыпается ровно тогда, когда карта что-то
/// приняла, - а не на ближайшем тике таймера. `None` - проснулись не из-за запроса.
pub fn recv_net(buf: &mut [u8], timeout_ticks: usize) -> Option<Message> {
    let (op, reply_cap, len, cap) = abi::syscall(
        SYS_RECV, buf.as_mut_ptr() as usize, buf.len(), 3, timeout_ticks, 0, 0, 0,
    );
    (op != usize::MAX).then_some(Message { op, reply_cap, len, cap })
}

/// `SYS_CALL` с передачей capability: послать `send` эндпоинту `ep`, ждать ответа в `recv`.
/// Возвращает (байт ответа | MAX, право из ответа | [`NO_CAP`]). На передаваемое право
/// (`cap` != NO_CAP) нужен `GRANT` — иначе ядро отклонит весь вызов.
pub fn call_full(ep: usize, op: usize, send: &[u8], recv: &mut [u8], cap: usize) -> (usize, usize) {
    let (n, got, _, _) = abi::syscall(
        SYS_CALL, ep, op,
        send.as_ptr() as usize, send.len(),
        recv.as_mut_ptr() as usize, recv.len(),
        cap,
    );
    (n, got)
}

/// `SYS_CALL` без передачи права — обычный вызов сервера. Возвращает байты ответа (или MAX).
pub fn call(ep: usize, op: usize, send: &[u8], recv: &mut [u8]) -> usize {
    call_full(ep, op, send, recv, NO_CAP).0
}

/// `SYS_REPLY` с передачей capability: ответить клиенту по одноразовому reply-cap.
pub fn reply_full(reply_cap: usize, buf: &[u8], cap: usize) -> usize {
    abi::syscall(SYS_REPLY, reply_cap, buf.as_ptr() as usize, buf.len(), cap, 0, 0, 0).0
}

/// `SYS_REPLY` без права — обычный ответ сервера.
pub fn reply(reply_cap: usize, buf: &[u8]) -> usize {
    reply_full(reply_cap, buf, NO_CAP)
}

/// `SYS_BLK_READ`: прочитать сектор диска (нужен cap на устройство с `READ`).
pub fn blk_read(dev_cap: usize, sector: usize, buf: &mut [u8; 512]) -> usize {
    abi::syscall(SYS_BLK_READ, dev_cap, sector, buf.as_mut_ptr() as usize, 0, 0, 0, 0).0
}

/// `SYS_BLK_WRITE`: записать сектор диска (нужен cap на устройство с `WRITE`).
pub fn blk_write(dev_cap: usize, sector: usize, data: &[u8]) -> usize {
    abi::syscall(SYS_BLK_WRITE, dev_cap, sector, data.as_ptr() as usize, data.len(), 0, 0, 0).0
}

/// `SYS_OBJ_PUT`: сохранить значение в store, content-id — в `id_out` (нужен `WRITE`).
pub fn obj_put(store_cap: usize, data: &[u8], id_out: &mut [u8; 32]) -> usize {
    abi::syscall(
        SYS_OBJ_PUT, store_cap,
        data.as_ptr() as usize, data.len(),
        id_out.as_mut_ptr() as usize, 0, 0, 0,
    ).0
}

/// `SYS_OBJ_GET`: прочитать значение по content-id (нужен `READ`). Возвращает длину (0 — нет).
pub fn obj_get(store_cap: usize, id: &[u8; 32], out: &mut [u8]) -> usize {
    abi::syscall(
        SYS_OBJ_GET, store_cap,
        id.as_ptr() as usize,
        out.as_mut_ptr() as usize, out.len(), 0, 0, 0,
    ).0
}

/// `SYS_OBJ_SET_ROOT`: привязать именованный корень к значению — атомарный чекпойнт store
/// (нужен `WRITE`). Привязанное переживает и GC, и перезагрузку.
pub fn obj_set_root(store_cap: usize, name: &[u8], id: &[u8; 32]) -> usize {
    abi::syscall(
        SYS_OBJ_SET_ROOT, store_cap,
        name.as_ptr() as usize, name.len(),
        id.as_ptr() as usize, 0, 0, 0,
    ).0
}

/// `SYS_OBJ_GET_ROOT`: content-id именованного корня → `id_out`. 32 — есть, 0 — нет, MAX — отказ.
pub fn obj_get_root(store_cap: usize, name: &[u8], id_out: &mut [u8; 32]) -> usize {
    abi::syscall(
        SYS_OBJ_GET_ROOT, store_cap,
        name.as_ptr() as usize, name.len(),
        id_out.as_mut_ptr() as usize, 0, 0, 0,
    ).0
}

/// `SYS_OBJ_DEL_ROOT`: отвязать корень (объект уйдёт в GC, если недостижим). 0/1/MAX.
pub fn obj_del_root(store_cap: usize, name: &[u8]) -> usize {
    abi::syscall(SYS_OBJ_DEL_ROOT, store_cap, name.as_ptr() as usize, name.len(), 0, 0, 0, 0).0
}

/// `SYS_OBJ_LIST_ROOTS`: заполнить `buf` текстом «короткий id  имя\n» по каждому СЫРОМУ корню
/// store (vsh `roots`). Возвращает число записанных байт (0 при отказе/пустом). Нужен store-cap
/// с READ или WRITE.
pub fn obj_list_roots(store_cap: usize, buf: &mut [u8]) -> usize {
    let r = abi::syscall(SYS_OBJ_LIST_ROOTS, store_cap, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0, 0).0;
    if r == usize::MAX { 0 } else { r }
}

/// `SYS_LOG`: вкл/выкл подробный трейс ядра ([ipc]/[obj]/[mm]/…). По умолчанию интерактивная
/// сессия ТИХАЯ (трейс сбивал вывод команд); включить на лету — `log(true)`.
pub fn log(on: bool) {
    abi::syscall(SYS_LOG, on as usize, 0, 0, 0, 0, 0, 0);
}

/// `SYS_TIME(0)` — настенное время, наносекунды Unix (UTC). Веха 86: часы читаются у прошивки
/// (CMOS RTC на x86, goldfish-rtc на riscv) один раз на загрузке, дальше идут от монотонного
/// счётчика. Часов нет — время идёт от эпохи Unix, то есть равно uptime (видно по «1970» в дате).
pub fn time_ns() -> u64 {
    abi::syscall(SYS_TIME, 0, 0, 0, 0, 0, 0, 0).0 as u64
}

/// `SYS_TIME(1)` — монотонное время с загрузки, наносекунды. В отличие от [`now`] (сырые тики,
/// читаются прямо из U-mode) это уже наносекунды, посчитанные ядром по своей таймбазе.
pub fn monotonic_ns() -> u64 {
    abi::syscall(SYS_TIME, 1, 0, 0, 0, 0, 0, 0).0 as u64
}

/// `SYS_RANDOM`: заполнить буфер случайными байтами (аппаратный ГСЧ ядра + пул событий).
/// Возвращает число заполненных байт (`usize::MAX` — буфер недоступен).
pub fn random(buf: &mut [u8]) -> usize {
    abi::syscall(SYS_RANDOM, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0, 0, 0).0
}

/// Разложить Unix-секунды в (год, месяц, день, час, минута, секунда) UTC — зеркало
/// `clock::civil_from_unix` в ядре (алгоритм Хиннанта); нужно программам, печатающим дату.
pub fn civil_from_unix(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, (rem / 3600) as u32, (rem % 3600 / 60) as u32, (rem % 60) as u32)
}

/// `SYS_READ`: прочитать доступный ввод консоли (хотя бы один байт; блокируется до ввода).
pub fn read_stdin(buf: &mut [u8]) -> usize {
    abi::syscall(SYS_READ, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0, 0, 0).0
}

/// `SYS_EXEC`: запустить программу из store по имени корня и дождаться завершения
/// (нужно право `EXEC` на store). Возвращает код выхода ребёнка или MAX.
pub fn exec(exec_cap: usize, name: &[u8]) -> usize {
    exec_args(exec_cap, name, &[])
}

/// `SYS_EXEC` с аргументами (Веха 30): `args` — argv БЕЗ имени программы, записи разделены
/// NUL (`b"-l\0/doc"`); имя ядро поставит в argv[0] само. Ребёнок прочитает их `SYS_ARGS(0)`,
/// env и стартовые capability унаследует от вызывающего.
pub fn exec_args(exec_cap: usize, name: &[u8], args: &[u8]) -> usize {
    abi::syscall(
        SYS_EXEC, exec_cap,
        name.as_ptr() as usize, name.len(),
        args.as_ptr() as usize, args.len(), 0, 0,
    ).0
}

/// `SYS_ARGS(0)`: argv процесса → `buf` (NUL-разделённые записи, [0] — имя программы).
/// Возвращает ПОЛНУЮ длину блоба (даже если `buf` меньше — обрежется).
pub fn args(buf: &mut [u8]) -> usize {
    abi::syscall(SYS_ARGS, 0, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0, 0).0
}

/// `SYS_ARGS(1)`: окружение процесса (`KEY=VAL\0…`), унаследованное от родителя.
pub fn env(buf: &mut [u8]) -> usize {
    abi::syscall(SYS_ARGS, 1, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0, 0).0
}

/// `SYS_STARTCAP(i)`: i-й стартовый capability процесса (преоткрытые права — как preopen'ы
/// WASI: выданы ядром при spawn'е или унаследованы при exec). [`NO_CAP`] — конец таблицы.
pub fn start_cap(i: usize) -> usize {
    abi::syscall(SYS_STARTCAP, i, 0, 0, 0, 0, 0, 0).0
}

/// `SYS_INSTALL(store_cap)` (Веха 48): установить VOID на AHCI-диск из загрузочного модуля
/// (образ с USB). Нужен store-cap с правом WRITE. **ДИСК СТИРАЕТСЯ.** `Some(p2_start)` — успех
/// (store заморожен, нужен ребут без USB); `None` — отказ (нет диска/образа/прав), система цела.
pub fn install(store_cap: usize) -> Option<u64> {
    let r = abi::syscall(SYS_INSTALL, store_cap, 0, 0, 0, 0, 0, 0).0;
    (r != NO_CAP).then_some(r as u64)
}

/// `SYS_MMIO_MAP(mmio_cap, va)` (Веха 51): замапить окно MMIO устройства (регистры железа) в свой
/// адресный простор по `va`. `true` — успех (дальше читать/писать регистры по `va` volatile'ом).
pub fn mmio_map(mmio_cap: usize, va: usize) -> bool {
    abi::syscall(SYS_MMIO_MAP, mmio_cap, va, 0, 0, 0, 0, 0).0 == 0
}

/// `SYS_DMA_ALLOC(dma_cap, va)` (Веха 51): выделить DMA-страницу, замапить по `va`, вернуть её
/// ФИЗИЧЕСКИЙ адрес — им драйвер программирует DMA устройства. `None` — отказ.
pub fn dma_alloc(dma_cap: usize, va: usize) -> Option<usize> {
    let r = abi::syscall(SYS_DMA_ALLOC, dma_cap, va, 0, 0, 0, 0, 0).0;
    (r != NO_CAP).then_some(r)
}

/// `SYS_IRQ_WAIT(irq_cap)` (Веха 52): усыпить драйвер до прерывания его устройства (вместо опроса).
/// `true` — проснулись по IRQ; `false` — нет права. Драйвер сам сверяется с ICR устройства.
pub fn irq_wait(irq_cap: usize) -> bool {
    abi::syscall(SYS_IRQ_WAIT, irq_cap, 0, 0, 0, 0, 0, 0).0 == 0
}

/// `SYS_CAP_DERIVE`: урезанная копия СВОЕГО права (права ∩ mask) — аттенуация у себя,
/// `GRANT` не нужен. Возвращает новый дескриптор или MAX.
pub fn cap_derive(cap: usize, mask: usize) -> usize {
    abi::syscall(SYS_CAP_DERIVE, cap, mask, 0, 0, 0, 0, 0).0
}

/// `SYS_MAP`: лениво зарезервировать `len` байт кучи (роль mmap/sbrk). Физические страницы
/// придут по page fault при первом обращении — обнулёнными. Возвращает VA начала или MAX.
pub fn heap_map(len: usize) -> usize {
    abi::syscall(SYS_MAP, len, 0, 0, 0, 0, 0, 0).0
}

// ─── checkpoint процессов (Веха 37) ───────────────────────────────────────────

/// `SYS_CHECKPOINT`: заморозить СЕБЯ в store под корнем `proc/<arch>/<name>` (нужно
/// право `WRITE` на store — образ пишется объектами). Семантика setjmp: **0** — образ
/// снят, «живой» продолжает; **1** — этот возврат случился в РАЗМОРОЖЕННОМ процессе
/// («прошлая жизнь» вернулась из этого же вызова); MAX — отказ.
pub fn checkpoint(store_cap: usize, name: &[u8]) -> usize {
    abi::syscall(SYS_CHECKPOINT, store_cap, name.as_ptr() as usize, name.len(), 0, 0, 0, 0).0
}

/// `SYS_RESTORE`: разморозить процесс из образа `proc/<arch>/<name>` (нужно право
/// `EXEC` — это запуск). Как [`exec`]: вызывающий ждёт завершения, возврат — код
/// выхода размороженного (или MAX). args/env тот берёт из образа, стартовые
/// capability — свежее наследство вызывающего.
pub fn restore(exec_cap: usize, name: &[u8]) -> usize {
    abi::syscall(SYS_RESTORE, exec_cap, name.as_ptr() as usize, name.len(), 0, 0, 0, 0).0
}

// ─── сеть (Веха 34) ────────────────────────────────────────────────────────────

/// `SYS_NET_SEND`: отправить сырой Ethernet-кадр (нужен cap на сетевое устройство,
/// право `WRITE`). Возвращает 0 или [`NO_CAP`]-подобный MAX при отказе.
pub fn net_send(dev_cap: usize, frame: &[u8]) -> usize {
    abi::syscall(SYS_NET_SEND, dev_cap, frame.as_ptr() as usize, frame.len(), 0, 0, 0, 0).0
}

/// `SYS_NET_RECV`: принять один кадр в `buf` (неблокирующе, опрос). Возвращает число байт
/// (0 — приёмник пуст; MAX — отказ). Нужен cap на устройство, право `READ`.
pub fn net_recv(dev_cap: usize, buf: &mut [u8]) -> usize {
    abi::syscall(SYS_NET_RECV, dev_cap, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0, 0).0
}

/// `SYS_NET_MAC`: записать MAC карты (6 байт) в `out`. Возвращает 0/MAX.
pub fn net_mac(dev_cap: usize, out: &mut [u8; 6]) -> usize {
    abi::syscall(SYS_NET_MAC, dev_cap, out.as_mut_ptr() as usize, 0, 0, 0, 0, 0).0
}

// ─── время (Веха 28 — микробенчи) ─────────────────────────────────────────────

/// Наносекунд в одном тике [`now`]: riscv — таймбаза QEMU virt 10 МГц (100 нс/тик);
/// x86 — TSC, который в QEMU TCG ходит на ~1 ГГц (1 нс/тик; на железе пересчитать).
#[cfg(target_arch = "riscv64")]
pub const TICK_NS: usize = 100;
#[cfg(target_arch = "x86_64")]
pub const TICK_NS: usize = 1;

/// Монотонный счётчик времени, читаемый ПРЯМО из U-mode (не syscall): `rdtime`
/// (ядро открывает его через scounteren) / `rdtsc` (CR4.TSD=0). Для замеров.
pub fn now() -> usize {
    #[cfg(target_arch = "riscv64")]
    {
        let t: usize;
        unsafe { core::arch::asm!("rdtime {0}", out(reg) t, options(nomem, nostack)) };
        t
    }
    #[cfg(target_arch = "x86_64")]
    {
        let (lo, hi): (u32, u32);
        unsafe {
            core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack))
        };
        (hi as usize) << 32 | lo as usize
    }
}

// ─── POSIX-shim (Веха 18.4) ───────────────────────────────────────────────────

/// Тонкий слой, ПРЯЧУЩИЙ ecall/op-коды/capability/IPC: программа зовёт `open/read/write/...`
/// и «не знает», что под ней VOID. `ep` — непрозрачный дескриптор «связи с ОС» (эндпоинт
/// персоналии), выданный при запуске, как контекст libc. Дескрипторы наружу смещены на
/// [`FD_BASE`]: 0/1/2 зарезервированы под stdin/stdout/stderr (POSIX); `write` на 1/2 идёт
/// в консоль ядра, `read` с 0 — в `SYS_READ` (блокируется до ввода).
pub mod posix {
    /// op-коды персоналии: операция (младший байт) | fd (байт 8..16) | режим open (байт 16..24).
    pub const OP_OPEN: usize = 0;
    pub const OP_READ: usize = 1;
    pub const OP_WRITE: usize = 2;
    pub const OP_CLOSE: usize = 3;
    /// stat(name) -> [exists:1 | size:4 LE]
    pub const OP_STAT: usize = 4;
    /// unlink(name): снять корень + убрать из каталога
    pub const OP_UNLINK: usize = 5;
    /// readdir(path) -> имена через '\n' (Веха 44: каталог в запросе; у подкаталогов хвост '/')
    pub const OP_READDIR: usize = 6;
    /// seek(fd): смещение курсора (Веха 30); whence едет в байте режима op
    pub const OP_SEEK: usize = 7;
    /// rename(old, new): перевесить корень + запись каталога (Веха 30)
    pub const OP_RENAME: usize = 8;
    /// mkdir(path): создать каталог (Веха 44)
    pub const OP_MKDIR: usize = 9;

    pub const SEEK_SET: usize = 0;
    pub const SEEK_CUR: usize = 1;
    pub const SEEK_END: usize = 2;

    /// открыть с курсором в конце
    pub const O_APPEND: usize = 1 << 0;
    /// открыть, обнулив содержимое
    pub const O_TRUNC: usize = 1 << 1;

    pub const STDIN: usize = 0;
    pub const STDOUT: usize = 1;
    /// Первый настоящий файловый дескриптор (0/1/2 — потоки консоли).
    pub const FD_BASE: usize = 3;

    /// `open(name, mode) -> fd` (или `usize::MAX`).
    pub fn open(ep: usize, name: &[u8], mode: usize) -> usize {
        let mut r = [0u8; 4];
        crate::call(ep, OP_OPEN | (mode << 16), name, &mut r);
        if r[0] == 0xff {
            usize::MAX
        } else {
            r[0] as usize + FD_BASE
        }
    }

    /// `read(fd, buf) -> n`. `fd`=0 — stdin консоли (блокируется до ввода).
    pub fn read(ep: usize, fd: usize, buf: &mut [u8]) -> usize {
        if fd == STDIN {
            return crate::read_stdin(buf);
        }
        if fd < FD_BASE {
            return 0;
        }
        crate::call(ep, OP_READ | ((fd - FD_BASE) << 8), &[], buf)
    }

    /// `write(fd, buf) -> len`. `fd`=1/2 → консоль ядра.
    pub fn write(ep: usize, fd: usize, buf: &[u8]) -> usize {
        if fd < FD_BASE {
            crate::write(buf);
            return buf.len();
        }
        crate::call(ep, OP_WRITE | ((fd - FD_BASE) << 8), buf, &mut []);
        buf.len()
    }

    /// `close(fd)` — персоналия при закрытии пишет изменённый файл в store (персистентность).
    pub fn close(ep: usize, fd: usize) {
        if fd < FD_BASE {
            return;
        }
        crate::call(ep, OP_CLOSE | ((fd - FD_BASE) << 8), &[], &mut []);
    }

    /// `readdir(path, buf) -> n` (Веха 44): имена в каталоге `path` через '\n'; у подкаталогов —
    /// хвостовой '/'. Пустой путь / `.` / `/` — корень.
    pub fn readdir(ep: usize, path: &[u8], buf: &mut [u8]) -> usize {
        crate::call(ep, OP_READDIR, path, buf)
    }

    /// `mkdir(path) -> 0 | MAX` (Веха 44): создать каталог (родитель должен существовать).
    pub fn mkdir(ep: usize, path: &[u8]) -> usize {
        let mut r = [0u8; 1];
        crate::call(ep, OP_MKDIR, path, &mut r);
        if r[0] == 0 { 0 } else { usize::MAX }
    }

    /// `unlink(path) -> 0 | MAX` (Веха 44): удалить файл (или ПУСТОЙ каталог).
    pub fn unlink(ep: usize, path: &[u8]) -> usize {
        let mut r = [0u8; 1];
        crate::call(ep, OP_UNLINK, path, &mut r);
        if r[0] == 0 { 0 } else { usize::MAX }
    }

    /// `stat(path) -> Some((каталог?, размер)) | None` (Веха 44): для `cd`/проверок существования.
    pub fn stat(ep: usize, path: &[u8]) -> Option<(bool, usize)> {
        let mut r = [0u8; 6];
        let n = crate::call(ep, OP_STAT, path, &mut r);
        if n < 6 || r[0] == 0 {
            return None;
        }
        let size = u32::from_le_bytes([r[1], r[2], r[3], r[4]]) as usize;
        Some((r[5] != 0, size))
    }

    /// `spawn(name) -> код выхода` (аналог `posix_spawn`+`wait`): запустить программу из store
    /// по имени корня. `exec_cap` — непрозрачный дескриптор «права запускать». MAX — не запустилось.
    pub fn spawn(exec_cap: usize, name: &[u8]) -> usize {
        crate::exec(exec_cap, name)
    }

    /// `spawn` с аргументами (Веха 30): `args` — argv без имени, записи разделены NUL.
    pub fn spawn_args(exec_cap: usize, name: &[u8], args: &[u8]) -> usize {
        crate::exec_args(exec_cap, name, args)
    }

    /// `lseek(fd, off, whence) -> новая позиция | MAX`. Смещение знаковое (END обычно с
    /// минусом); курсор зажимается в [0, размер файла] — дыр в файлах у персоналии нет.
    pub fn seek(ep: usize, fd: usize, off: isize, whence: usize) -> usize {
        if fd < FD_BASE {
            return usize::MAX;
        }
        let mut r = [0u8; 8];
        let n = crate::call(
            ep,
            OP_SEEK | ((fd - FD_BASE) << 8) | (whence << 16),
            &(off as i64).to_le_bytes(),
            &mut r,
        );
        let pos = u64::from_le_bytes(r);
        if n < 8 || pos == u64::MAX {
            usize::MAX
        } else {
            pos as usize
        }
    }

    /// `rename(old, new) -> 0 | MAX`: перевесить файл на новое имя — атомарно для store
    /// (смена корня) и честно для каталога. Открытые дескрипторы продолжают работать.
    pub fn rename(ep: usize, old: &[u8], new: &[u8]) -> usize {
        if old.is_empty() || new.is_empty() || 1 + old.len() + new.len() > 256 {
            return usize::MAX;
        }
        let mut req = [0u8; 256];
        req[0] = old.len() as u8;
        req[1..1 + old.len()].copy_from_slice(old);
        req[1 + old.len()..1 + old.len() + new.len()].copy_from_slice(new);
        let mut r = [0u8; 1];
        crate::call(ep, OP_RENAME, &req[..1 + old.len() + new.len()], &mut r);
        if r[0] == 0 { 0 } else { usize::MAX }
    }

    /// `cat path` — прочитать файл и вывести в stdout. Только через shim.
    pub fn cat(ep: usize, path: &[u8]) {
        let mut buf = [0u8; 512];
        let fd = open(ep, path, 0);
        if fd == usize::MAX {
            return;
        }
        loop {
            let n = read(ep, fd, &mut buf);
            if n == 0 {
                break;
            }
            write(ep, STDOUT, &buf[..n]);
        }
        close(ep, fd);
    }

    /// `echo msg > path` — записать строку в файл. Только через shim.
    pub fn echo_to(ep: usize, path: &[u8], msg: &[u8]) {
        let fd = open(ep, path, O_TRUNC);
        if fd != usize::MAX {
            write(ep, fd, msg);
            close(ep, fd);
        }
    }
}
