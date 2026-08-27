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

/// Веха 94 — HTTP-клиент: GET поверх TCP, тело потоком прямо в объектный store
/// (куски + узел; content-id узла — Merkle-корень над содержимым).
pub mod http;

/// Куча программы (bump + свободный список поверх ленивой кучи процесса). Вынесена из `vvsh`
/// Вехой 95, когда аллокатор понадобился второй программе. Сам модуль `alloc` НЕ тянет —
/// это реализация `core::alloc::GlobalAlloc`, поэтому программы без кучи не страдают.
pub mod heap;

/// Веха 98 — соглашение о чужом stdio (вывод и ввод через IPC к хосту вместо общей консоли).
pub mod stdio;
/// Веха 117 — протокол окон: клиент рисует в свою память, кладёт объектом в store и
/// называет content-id; композитор складывает кадр. Ядро про окна не знает ничего.
pub mod win;
/// Веха 140 — растровый шрифт 8×16 из таблицы ядра: минимум для тех, кто рисует текст, но не
/// является терминалом (бар). Без кучи и без крейтов ereb.
pub mod glyph;
/// Веха 148.5 — аргументы и окружение процесса: разобранные один раз и без ловушки с длиной,
/// на которой панику ловили тринадцать мест, включая сам обработчик паники.
pub mod argv;


// Веха 95 — источник случайности для криптографии. `getrandom` на bare-metal системного
// источника не имеет, поэтому регистрируем свой: он идёт в `SYS_RANDOM`, за которым стоят
// virtio-rng, ГСЧ процессора и джиттер-источник ядра, смешанные через BLAKE3.
//
// Регистрация живёт в БИБЛИОТЕКЕ, а не в TLS-бинаре: символ должен быть ровно один на
// программу, и так он определён для любого потребителя, кто бы им ни оказался.
getrandom::register_custom_getrandom!(sys_getrandom);

fn sys_getrandom(buf: &mut [u8]) -> Result<(), getrandom::Error> {
    if random(buf) == buf.len() {
        Ok(())
    } else {
        // Ядро не отдало запрошенное — для криптографии это отказ, а не повод продолжить
        // с тем, что есть. Код ошибки произвольный из пользовательского диапазона.
        Err(getrandom::Error::from(
            core::num::NonZeroU32::new(getrandom::Error::CUSTOM_START).expect("ненулевой"),
        ))
    }
}

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
/// Веха 129 — разделяемая память: создать область и отобразить существующую.
const SYS_SHM_NEW: usize = 54;
const SYS_SHM_MAP: usize = 55;
const SYS_SHM_UNMAP: usize = 56;
const SYS_RANDOM: usize = 37;
const SYS_OBJ_PUT_NODE: usize = 38;
const SYS_OBJ_CHILDREN: usize = 39;
const SYS_VIDEO_INFO: usize = 40;
const SYS_SPAWN: usize = 41;
const SYS_WAIT: usize = 42;
const SYS_SELF_ENDPOINT: usize = 43;
const SYS_POWEROFF: usize = 44;
const SYS_KILL: usize = 45;
/// Веха 109 — сборка мусора store по требованию (нужна `pkg gc`).
const SYS_OBJ_GC: usize = 46;
const SYS_SLEEP: usize = 47;
const SYS_PARENT: usize = 48;
const SYS_MOUSE_READ: usize = 49;
const SYS_KLOG: usize = 50;
const SYS_KEY_READ: usize = 51;
const SYS_CONSIZE: usize = 52;
const SYS_SETENV: usize = 53;
/// Веха 143 — раскладка клавиатуры: `0` спросить, `1` следующая.
const SYS_KEYMAP: usize = 57;
/// Веха 152.2 — описать дескриптор: вид цели и права (read-only интроспекция).
const SYS_CAP_INFO: usize = 58;
const SYS_PROC_LIST: usize = 59;

/// «Capability отсутствует» — в аргументах и результатах IPC.
pub const NO_CAP: usize = usize::MAX;

/// Паника программы — завершиться ненулевым кодом, не трогая ядро: раскрутки стека нет
/// (panic="abort"), а печатать backtrace — не забота userspace-программы.
/// Веха 143.1 — паника программы ГОВОРИТ, где умерла.
///
/// Раньше здесь стоял молчаливый `exit(101)`, и это дорого обошлось: композитор падал на выходе
/// за границу среза, а снаружи выглядело как «wm умер, паники нет, экран вернулся ядру». Место
/// падения приходилось искать чтением кода вместо чтения журнала.
///
/// Пишем В КОНСОЛЬ ЯДРА, а не в stdio: хоста может не быть (композитор), а если он есть, то он
/// сам может быть тем, кто падает. Буфер на стеке — куча в этот момент уже не заслуживает
/// доверия, да и у библиотеки её нет.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    use core::fmt::Write;
    struct Buf {
        b: [u8; 256],
        n: usize,
    }
    impl Write for Buf {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            for &c in s.as_bytes() {
                if self.n < self.b.len() {
                    self.b[self.n] = c;
                    self.n += 1;
                }
            }
            Ok(())
        }
    }
    let mut out = Buf { b: [0; 256], n: 0 };
    let _ = out.write_str("  [паника] ");
    // Имя программы из argv[0] — иначе по журналу не понять, КТО именно упал.
    //
    // Веха 148.5 — через [`argv::name_into`], и это не украшательство. Здесь стояло
    // `abuf[..n]` при буфере в 64 байта, а `args` возвращает ПОЛНУЮ длину argv: программа с
    // длинными аргументами роняла обработчик паники, и процесс исчезал молча — ни строки о том,
    // что он вообще падал. Место, обязанное объяснить падение, само же его и прятало.
    let mut abuf = [0u8; 64];
    let n = argv::name_into(&mut abuf);
    if let Ok(s) = core::str::from_utf8(&abuf[..n]) {
        let _ = out.write_str(s);
        let _ = out.write_str(": ");
    }
    if let Some(l) = info.location() {
        let _ = write!(out, "{}:{}: ", l.file(), l.line());
    }
    let _ = write!(out, "{}\n", info.message());
    write_console(&out.b[..out.n]);
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

    /// riscv64: то же, но забирает ПЯТЫЙ результат (a4). Отдельная функция, а не расширение
    /// общей: пятый регистр нужен ровно приёму сообщений (там в нём номер отправителя, Веха 99),
    /// а трогать раскладку на всех остальных путях ради одного случая — лишний риск.
    #[cfg(target_arch = "riscv64")]
    #[inline(always)]
    pub fn syscall5(
        num: usize, mut a0: usize, mut a1: usize, mut a2: usize, mut a3: usize, mut a4: usize,
    ) -> (usize, usize, usize, usize, usize) {
        unsafe {
            core::arch::asm!("ecall", in("a7") num,
                inout("a0") a0, inout("a1") a1, inout("a2") a2, inout("a3") a3,
                inout("a4") a4, options(nostack));
        }
        (a0, a1, a2, a3, a4)
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

    /// x86_64: то же, но забирает ПЯТЫЙ результат (r10) — см. riscv-двойник.
    #[cfg(target_arch = "x86_64")]
    #[inline(always)]
    pub fn syscall5(
        num: usize, mut a0: usize, mut a1: usize, mut a2: usize, mut a3: usize, a4: usize,
    ) -> (usize, usize, usize, usize, usize) {
        let r0;
        unsafe {
            core::arch::asm!(
                "int 0x80",
                inout("rax") num => r0,
                inout("rdi") a0, inout("rsi") a1, inout("rdx") a2,
                inout("r10") a3, in("r8") a4,
                options(nostack),
            );
        }
        (r0, a0, a1, a2, a3)
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

/// Напечатать байты. Если процессу назначен ХОСТ ([`stdio`], Веха 98) — вывод уходит ему по IPC;
/// иначе, как раньше, в общую консоль ядра (`SYS_WRITE`).
///
/// Откат на консоль при неудаче — не перестраховка: молча потерять вывод хуже, чем напечатать
/// его не туда, а хост может умереть в любой момент.
pub fn write(buf: &[u8]) {
    if stdio::write(buf) {
        return;
    }
    abi::syscall(SYS_WRITE, buf.as_ptr() as usize, buf.len(), 0, 0, 0, 0, 0);
}

/// Напечатать байты СТРОГО в консоль ядра, минуя хост. Нужна самому хосту и путям, где IPC
/// недоступен или неуместен (паника, отладка транспорта).
pub fn write_console(buf: &[u8]) {
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

/// `SYS_SLEEP(ns)` (Веха 114) — поспать указанное время и не занимать процессор.
///
/// До этого сна не было вовсе: ждать умели только серверы (`recv_timeout`) и нити на футексе, а
/// обычная программа крутила `yield_now` в цикле — то есть «ждала», не отдавая машину никому
/// насовсем. Время в наносекундах, потому что тики — это таймбаза архитектуры, и знать её
/// программе незачем.
pub fn sleep_ns(ns: u64) {
    abi::syscall(SYS_SLEEP, ns as usize, 0, 0, 0, 0, 0, 0);
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
    /// Веха 99 — номер процесса-отправителя. Нужен серверу, который ведёт по клиенту СОСТОЯНИЕ:
    /// мультиплексору — чтобы понять, в какую панель лёг вывод. Reply-право для этого не годится:
    /// оно одноразовое и у каждого запроса своё.
    pub sender: usize,
}

/// `SYS_RECV`: ждать запрос; нагрузка ложится в `buf` (усечённая по его размеру).
pub fn recv(buf: &mut [u8]) -> Message {
    let (op, reply_cap, len, cap, sender) =
        abi::syscall5(SYS_RECV, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0);
    Message { op, reply_cap, len, cap, sender }
}

/// Веха 90 — `SYS_RECV` БЕЗ блокировки: `None`, если запросов нет прямо сейчас.
/// Нужен серверам, которым между запросами есть чем заняться, — прежде всего сетевому:
/// стек обязан тикать (входящие, ретрансмиссии), даже когда клиенты молчат.
pub fn try_recv(buf: &mut [u8]) -> Option<Message> {
    let (op, reply_cap, len, cap, sender) =
        abi::syscall5(SYS_RECV, buf.as_mut_ptr() as usize, buf.len(), 1, 0, 0);
    (op != usize::MAX).then_some(Message { op, reply_cap, len, cap, sender })
}

/// Веха 91 — `SYS_RECV` со СНОМ до дедлайна: `None`, если за `timeout_ns` запроса не было.
/// Это «сон вместо опроса» для серверов-реакторов: пока никто не зовёт и делать нечего, процесс
/// не занимает процессор вовсе, но просыпается к моменту, который назвал сам (у сетевого стека
/// это `poll_at` — ближайший таймер ретрансмиссии).
///
/// Веха 139.3 — срок в НАНОСЕКУНДАХ, как у [`sleep_ns`]. Раньше эти три обёртки брали ТИКИ, то
/// есть таймбазу архитектуры, — и два реактора из трёх её не знали: `wm` и `term` писали `200`,
/// имея в виду миллисекунды, а получали 200 тактов TSC (около 60 нс). Сон вырождался в опрос, и
/// простаивающая графическая сессия жгла ЦЕЛОЕ ЯДРО (замер: 100% при пустом экране). Прятать
/// таймбазу здесь — единственное надёжное место: она нужна только на границе с ядром.
pub fn recv_timeout(buf: &mut [u8], timeout_ns: u64) -> Option<Message> {
    let (op, reply_cap, len, cap, sender) = abi::syscall5(
        SYS_RECV, buf.as_mut_ptr() as usize, buf.len(), 2, ns_to_ticks(timeout_ns) as usize, 0,
    );
    (op != usize::MAX).then_some(Message { op, reply_cap, len, cap, sender })
}

/// Веха 103 — `SYS_RECV` со сном до дедлайна ИЛИ до КЛАВИШИ. То, чего не хватало реактору
/// терминала: он обязан обслуживать два источника — вывод детей и клавиатуру, — а ждать умел
/// только на одном, поэтому крутился по короткому таймеру. Теперь спит до события.
/// `None` — проснулись не из-за сообщения (клавиша или срок): читать клавиатуру и повторять.
/// Срок — в наносекундах (см. [`recv_timeout`]).
pub fn recv_console(buf: &mut [u8], timeout_ns: u64) -> Option<Message> {
    let (op, reply_cap, len, cap, sender) = abi::syscall5(
        SYS_RECV, buf.as_mut_ptr() as usize, buf.len(), 4, ns_to_ticks(timeout_ns) as usize, 0,
    );
    (op != usize::MAX).then_some(Message { op, reply_cap, len, cap, sender })
}

/// Веха 91 - `SYS_RECV` со сном до дедлайна ИЛИ до прихода СЕТЕВОГО КАДРА. То, ради чего веха:
/// сетевой сервер спит, ничего не занимая, и просыпается ровно тогда, когда карта что-то
/// приняла, - а не на ближайшем тике таймера. `None` - проснулись не из-за запроса.
/// Срок — в наносекундах (см. [`recv_timeout`]).
pub fn recv_net(buf: &mut [u8], timeout_ns: u64) -> Option<Message> {
    let (op, reply_cap, len, cap, sender) = abi::syscall5(
        SYS_RECV, buf.as_mut_ptr() as usize, buf.len(), 3, ns_to_ticks(timeout_ns) as usize, 0,
    );
    (op != usize::MAX).then_some(Message { op, reply_cap, len, cap, sender })
}

/// `SYS_CALL` с передачей capability: послать `send` эндпоинту `ep`, ждать ответа в `recv`.
/// Возвращает (байт ответа | MAX, право из ответа | [`NO_CAP`]). На передаваемое право
/// (`cap` != NO_CAP) нужен `GRANT` — иначе ядро отклонит весь вызов.
pub fn call_full(ep: usize, op: usize, send: &[u8], recv: &mut [u8], cap: usize) -> (usize, usize) {
    let r = call_ex(ep, op, send, recv, cap);
    (r.reply_len, r.cap)
}

/// Итог `SYS_CALL` целиком — включая две величины, по которым видно УСЕЧЕНИЕ (Веха 101).
pub struct Call {
    /// Байт ответа принято (или [`usize::MAX`] — вызов не состоялся).
    pub reply_len: usize,
    /// Право, приехавшее в ответе ([`NO_CAP`] — не было).
    pub cap: usize,
    /// Байт ЗАПРОСА доставлено. Меньше отправленного — у сервера маленький приёмный буфер.
    pub sent: usize,
    /// Байт ответа сервер ХОТЕЛ отдать. Больше `reply_len` — мал наш буфер.
    pub reply_want: usize,
}

/// `SYS_CALL` с полным итогом. Нужен там, где усечение — ошибка, а не норма: ядро режет запрос
/// по приёмному буферу сервера и ответ по нашему, и до Вехи 101 обе усечки были НЕВИДИМЫ —
/// вызов возвращал успех, а половина данных исчезала (так пропала часть сеянного `terminal.vv`).
pub fn call_ex(ep: usize, op: usize, send: &[u8], recv: &mut [u8], cap: usize) -> Call {
    let (n, got, sent, want) = abi::syscall(
        SYS_CALL, ep, op,
        send.as_ptr() as usize, send.len(),
        recv.as_mut_ptr() as usize, recv.len(),
        cap,
    );
    Call { reply_len: n, cap: got, sent, reply_want: want }
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
    obj_get_ex(store_cap, id, out).0
}

/// То же, но вторым числом — НАСТОЯЩАЯ длина объекта (Веха 114).
///
/// Без неё «объект ровно с буфер» и «объект не влез» неразличимы, и читатель вынужден гадать:
/// в VOID это выглядело как рост буфера удвоением с перечитыванием объекта по нескольку раз.
/// Теперь размер спрашивается один раз и читается ровно столько, сколько есть.
pub fn obj_get_ex(store_cap: usize, id: &[u8; 32], out: &mut [u8]) -> (usize, usize) {
    let r = abi::syscall(
        SYS_OBJ_GET, store_cap,
        id.as_ptr() as usize,
        out.as_mut_ptr() as usize, out.len(), 0, 0, 0,
    );
    (r.0, r.1)
}

/// `SYS_OBJ_PUT_NODE` (Веха 94): положить УЗЕЛ — значение + список исходящих ссылок.
/// Так кладут то, что не помещается одним слайсом: куски — обычными [`obj_put`], а узел
/// связывает их в целое (и раздаёт дедуп: одинаковый кусок в двух загрузках — один объект).
/// `0` — успех, [`usize::MAX`] — отказ (нет права WRITE / не хватило памяти).
pub fn obj_put_node(
    store_cap: usize,
    data: &[u8],
    children: &[[u8; 32]],
    id_out: &mut [u8; 32],
) -> usize {
    abi::syscall(
        SYS_OBJ_PUT_NODE, store_cap,
        data.as_ptr() as usize, data.len(),
        children.as_ptr() as usize, children.len(),
        id_out.as_mut_ptr() as usize, 0,
    ).0
}

/// `SYS_OBJ_CHILDREN` (Веха 94): выписать ссылки узла в `out` (нужен `READ`). Возвращает ПОЛНОЕ
/// число детей — может быть больше, чем влезло, иначе не отличить «детей ровно столько» от
/// «буфер мал». [`usize::MAX`] — отказ.
pub fn obj_children(store_cap: usize, id: &[u8; 32], out: &mut [[u8; 32]]) -> usize {
    abi::syscall(
        SYS_OBJ_CHILDREN, store_cap,
        id.as_ptr() as usize,
        out.as_mut_ptr() as usize, out.len() * 32, 0, 0, 0,
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

/// Веха 148.6 — разрешить **спецификацию объекта**: то, чем человек и конфиг называют объект в
/// одну строку — либо КОРЕНЬ store (`f/etc/wall.png`), либо сам content-id шестьюдесятью
/// четырьмя шестнадцатеричными знаками. `None` — корня нет.
///
/// Правило «64 hex-знака значат content-id, всё прочее — имя корня» жило в двух местах (`obj` и
/// `net-srv`) и в обоих было набрано заново. Стоит его где-то уточнить — скажем, разрешить
/// сокращённый id по первым шестнадцати знакам, как это делает `git`, — и одна и та же строка
/// станет означать в двух программах разное.
///
/// Корень удобнее (его переназначает новая загрузка), id строже (его нельзя подменить) — и
/// выбор между ними принадлежит тому, кто пишет строку, а не тому, кто её читает.
pub fn obj_resolve(store_cap: usize, spec: &[u8]) -> Option<[u8; 32]> {
    let mut id = [0u8; 32];
    if spec.len() == 64 && spec.iter().all(|b| b.is_ascii_hexdigit()) {
        let hex = |c: u8| match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => c - b'A' + 10,
        };
        for (i, pair) in spec.chunks(2).enumerate() {
            id[i] = hex(pair[0]) << 4 | hex(pair[1]);
        }
        return Some(id);
    }
    (obj_get_root(store_cap, spec, &mut id) == 32).then_some(id)
}

/// `SYS_OBJ_DEL_ROOT`: отвязать корень (объект уйдёт в GC, если недостижим). 0/1/MAX.
pub fn obj_del_root(store_cap: usize, name: &[u8]) -> usize {
    abi::syscall(SYS_OBJ_DEL_ROOT, store_cap, name.as_ptr() as usize, name.len(), 0, 0, 0, 0).0
}

/// `SYS_OBJ_LIST_ROOTS`: заполнить `buf` текстом «короткий id  имя\n» по каждому СЫРОМУ корню
/// store (vsh `roots`). Возвращает `(доехало, хотел отдать)` в байтах — второе больше первого,
/// если буфер мал. Нужен store-cap с READ или WRITE; при отказе — `(0, 0)`.
///
/// Веха 107: длина ВСЕГО списка возвращается отдельно по той же причине, по которой её отдаёт
/// `readdir` персоналии, — иначе «корней ровно столько» не отличить от «буфер мал», а обрезание
/// приходится на середину строки и даёт покалеченное имя корня. На списке корней стоит
/// нумерация поколений: недосчитаться — значит затереть существующее поколение.
pub fn obj_list_roots_ex(store_cap: usize, buf: &mut [u8]) -> Option<(usize, usize)> {
    let r = abi::syscall(SYS_OBJ_LIST_ROOTS, store_cap, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0, 0).0;
    if r == usize::MAX {
        None // отказ по правам — это НЕ то же самое, что «корней нет»
    } else {
        Some((r.min(buf.len()), r))
    }
}

/// То же, но только «сколько байт доехало» — для тех, кому хватает одного буфера.
pub fn obj_list_roots(store_cap: usize, buf: &mut [u8]) -> usize {
    obj_list_roots_ex(store_cap, buf).map_or(0, |(got, _)| got)
}

/// `SYS_OBJ_GC`: собрать мусор store — объекты, недостижимые от корней (Веха 109). Возвращает
/// число собранных (`usize::MAX` — отказ: нужен store-cap с WRITE).
///
/// Снятый корень сам по себе места не возвращает: пока никто не прошёл по графу достижимости,
/// объекты лежат как лежали. До этой операции сборка случалась только на загрузке — то есть
/// «удалил пакет — перезагрузись».
pub fn obj_gc(store_cap: usize) -> usize {
    abi::syscall(SYS_OBJ_GC, store_cap, 0, 0, 0, 0, 0, 0).0
}

/// `SYS_LOG`: вкл/выкл подробный трейс ядра ([ipc]/[obj]/[mm]/…). По умолчанию интерактивная
/// сессия ТИХАЯ (трейс сбивал вывод команд); включить на лету — `log(true)`.
pub fn log(on: bool) {
    abi::syscall(SYS_LOG, on as usize, 0, 0, 0, 0, 0, 0);
}

/// Веха 129 — создать ОБЩУЮ ОБЛАСТЬ памяти на `len` байт и отобразить её по `va` (страничное
/// выравнивание обязательно). Возвращает **право** на область (`None` — не вышло).
///
/// Право передаётся по IPC тому, с кем делятся буфером: без него область не отобразить, а
/// подделать его нельзя. Так пиксели окна перестают ездить объектами store — по IPC остаётся
/// только «строки такие-то изменились» ([[shm]]).
pub fn shm_new(len: usize, va: usize) -> Option<usize> {
    let r = abi::syscall(SYS_SHM_NEW, len, va, 0, 0, 0, 0, 0).0;
    (r != usize::MAX).then_some(r)
}

/// Веха 129 — отобразить по `va` область, право на которую получено. Возвращает её длину.
/// Без права `WRITE` в самом cap область ляжет только на чтение.
pub fn shm_map(shm_cap: usize, va: usize) -> Option<usize> {
    let r = abi::syscall(SYS_SHM_MAP, shm_cap, va, 0, 0, 0, 0, 0).0;
    (r != usize::MAX).then_some(r)
}

/// Веха 129 — отпустить область: снять её со своих адресов (`va` — тот, по которому её
/// отображали). Ушёл последний держатель — страницы вернулись системе.
///
/// Звать обязательно там, где буфер СМЕНИЛСЯ: размер окна в тайлинге меняется при появлении
/// каждого соседа, и не отпущенная область осталась бы висеть до смерти процесса.
pub fn shm_unmap(shm_cap: usize, va: usize) -> bool {
    abi::syscall(SYS_SHM_UNMAP, shm_cap, va, 0, 0, 0, 0, 0).0 == 0
}

/// Веха 153 — размер одной записи `SYS_PROC_LIST` в байтах (см. [`proc_list`]).
pub const PROC_REC: usize = 64;

/// Веха 153 — перечислить ЖИВЫЕ процессы под правом обзора (`sysview_cap`, требует READ).
/// Заполняет `buf` записями по [`PROC_REC`] байт и возвращает ПОЛНОЕ число процессов: если оно
/// больше `buf.len() / PROC_REC`, часть не поместилась — перезапроси бо́льшим буфером. `None` —
/// нет права (ambient-доступа к списку процессов нет, [[task-manager]]).
///
/// Раскладка записи (LE): `pid u16`, `parent u16` (0xFFFF — никто), `flags u16`
/// (bit0 системный, bit1 linux, bit2 есть content-id), `state u8`, `name_len u8`,
/// `image [32]` (content-id образа), `name [24]`.
pub fn proc_list(sysview_cap: usize, buf: &mut [u8]) -> Option<usize> {
    let r = abi::syscall(SYS_PROC_LIST, sysview_cap, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0, 0).0;
    (r != usize::MAX).then_some(r)
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

/// `SYS_TIME(2)` — ТАЙМБАЗА системы: тиков [`now`] в секунду (Веха 136). Спрашивается один раз и
/// запоминается: значение не меняется за время жизни системы, а syscall в горячем пути замера
/// стоил бы дороже самого замера.
///
/// Зачем вообще: счётчик программа читает сама (одна инструкция), но цена тика — свойство МАШИНЫ,
/// а не программы. До этой вехи её знала каждая программа отдельно, константой «x86 ≈ 1 ГГц»; на
/// живом процессоре TSC идёт втрое быстрее, и все замеры и сроки в userspace были втрое мимо.
pub fn tick_hz() -> u64 {
    use core::sync::atomic::{AtomicU64, Ordering};
    static CACHED: AtomicU64 = AtomicU64::new(0);
    let v = CACHED.load(Ordering::Relaxed);
    if v != 0 {
        return v;
    }
    let hz = (abi::syscall(SYS_TIME, 2, 0, 0, 0, 0, 0, 0).0 as u64).max(1);
    CACHED.store(hz, Ordering::Relaxed);
    hz
}

/// Тики [`now`] → наносекунды по таймбазе системы.
pub fn ticks_to_ns(ticks: u64) -> u64 {
    (ticks as u128 * 1_000_000_000 / tick_hz() as u128) as u64
}

/// Наносекунды → тики, вверх (сроки в `SYS_RECV`/`SYS_FUTEX` считаются в тиках).
pub fn ns_to_ticks(ns: u64) -> u64 {
    (ns as u128 * tick_hz() as u128).div_ceil(1_000_000_000) as u64
}

/// `SYS_RANDOM`: заполнить буфер случайными байтами (аппаратный ГСЧ ядра + пул событий).
/// Возвращает число заполненных байт (`usize::MAX` — буфер недоступен).
pub fn random(buf: &mut [u8]) -> usize {
    abi::syscall(SYS_RANDOM, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0, 0, 0).0
}

/// `SYS_RANDOM(kind=1)` (Веха 95) — есть ли у системы КРИПТОГРАФИЧЕСКИЙ источник случайности
/// (virtio-rng, ГСЧ процессора или прошедший проверку джиттер-источник ядра).
///
/// Спрашивать это должен потребитель, а не гадать: `false` означает, что генерировать ключи
/// здесь нельзя, и TLS обязан отказаться, а не «сделать как-нибудь».
pub fn random_is_strong() -> bool {
    abi::syscall(SYS_RANDOM, 0, 0, 1, 0, 0, 0, 0).0 == 1
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

/// Прочитать ввод. При назначенном хосте ([`stdio`]) запрос уходит ему по IPC — и хост волен
/// ответить ОТЛОЖЕННО, тогда мы спим в `SYS_CALL` ровно как спали бы в `SYS_READ`. Без хоста —
/// прежний путь: консоль ядра, блокировка до первого байта.
pub fn read_stdin(buf: &mut [u8]) -> usize {
    if let Some(n) = stdio::read(buf) {
        return n;
    }
    abi::syscall(SYS_READ, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0, 0, 0).0
}

/// Веха 143 — какая раскладка клавиатуры сейчас: 0 — US, 1 — RU. Спросить может кто угодно.
pub fn keymap() -> usize {
    abi::syscall(SYS_KEYMAP, 0, 0, 0, 0, 0, 0, 0).0
}

/// Веха 143 — переключить раскладку на следующую; `None` — экран не наш (переключает тот, кто
/// на этом экране рисует: композитор в окнах, полноэкранный терминал в тексте).
pub fn keymap_next() -> Option<usize> {
    let n = abi::syscall(SYS_KEYMAP, 1, 0, 0, 0, 0, 0, 0).0;
    (n != usize::MAX).then_some(n)
}

/// `SYS_KILL(pid)` (Веха 103) — завершить СВОЕГО ребёнка (запущенного [`spawn`]). `false` —
/// отказ: чужой процесс, свой собственный номер или уже завершившийся. Право — из родительства,
/// как у [`wait`]: кто процесс создал, тот им и распоряжается.
pub fn kill(pid: usize) -> bool {
    abi::syscall(SYS_KILL, pid, 0, 0, 0, 0, 0, 0).0 != usize::MAX
}

/// `SYS_POWEROFF(cap)` (Веха 101) — выключить машину. Нужно право `power` из конфига
/// (`cap::Target::Power` + WRITE): выключение — одностороннее действие над всей системой, и
/// «может любой» здесь было бы дырой ровно того сорта, который capability-модель закрывает.
/// Возвращается только при ОТКАЗЕ (нет права) — иначе питание уже снято.
pub fn power_off(cap: usize) -> bool {
    abi::syscall(SYS_POWEROFF, cap, 0, 0, 0, 0, 0, 0).0 != usize::MAX
}

/// Прочитать ввод КОНСОЛИ, не засыпая: 0 — пока ничего нет (Веха 99). Нужна реактору хоста —
/// он обязан обслуживать и клавиатуру, и вывод детей, а значит не может уснуть ни на одном.
/// Мимо соглашения [`stdio`] сознательно: хост читает настоящую клавиатуру, а не чей-то поток.
pub fn read_console_nonblock(buf: &mut [u8]) -> usize {
    abi::syscall(SYS_READ, buf.as_mut_ptr() as usize, buf.len(), 1, 0, 0, 0, 0).0
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

/// `SYS_SETENV` (Веха 120.1) — заменить СВОЁ окружение (`KEY=VAL\0…`); дети унаследуют его при
/// запуске. `false` — блоб длиннее потолка ядра.
///
/// Прав не раздаёт: окружение — слой ИМЁН над таблицей стартовых capability, а сама таблица
/// наследуется целиком и неизменной (разбор — у обработчика в `proc.rs`).
pub fn set_env(blob: &[u8]) -> bool {
    abi::syscall(SYS_SETENV, blob.as_ptr() as usize, blob.len(), 0, 0, 0, 0, 0).0 != NO_CAP
}

/// Текущий каталог, объявленный шеллом (`CWD=` в окружении) → в `out`; возвращает его длину
/// (0 — каталог не объявляли).
///
/// Текущего каталога у процесса в VOID НЕТ: это состояние шелла, а не ядра, и держать его в
/// ядре значило бы завести общий изменяемый корень имён — ровно то, от чего уходит
/// capability-модель. Поэтому шелл СООБЩАЕТ его детям, а путь собирает [`posix::resolve`].
pub fn cwd(out: &mut [u8]) -> usize {
    argv::env_raw(b"CWD", out).unwrap_or(0)
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

/// Веха 99.1 — найти стартовое право ПО ИМЕНИ (`CAP_POSIXFS`, `CAP_STORE`, `CAP_FB`, …).
///
/// Имена кладёт init рядом с самими правами ([[declarative-init]]). До этого права были только
/// позиционными — «файловый сервер нулевой, store первый», — и это выстрелило ровно так, как и
/// должно было: поменяли порядок токенов в конфиге, и `vvsh` принял фреймбуфер за файловый
/// сервер. Позиция осталась для совместимости, но полагаться на неё больше не нужно.
///
/// `None` — имени нет (старый конфиг или право безымянное); вызывающий откатывается на индекс.
pub fn cap_named(name: &str) -> Option<usize> {
    let mut key = [0u8; 40];
    let pre = b"CAP_";
    if pre.len() + name.len() > key.len() {
        return None;
    }
    key[..pre.len()].copy_from_slice(pre);
    key[pre.len()..pre.len() + name.len()].copy_from_slice(name.as_bytes());
    let idx = argv::env_num(&key[..pre.len() + name.len()])?;
    let c = start_cap(idx);
    (c != NO_CAP).then_some(c)
}

/// Веха 98 — `SYS_SPAWN(exec_cap, имя, аргументы)`: запустить программу из store и **сразу
/// вернуть управление**, отдав номер ребёнка. В отличие от [`exec`], родитель продолжает
/// работать — на этом стоит любой хост чужих процессов (мультиплексор, супервизор).
/// `None` — запустить не удалось.
pub fn spawn(exec_cap: usize, name: &[u8], args: &[u8]) -> Option<usize> {
    spawn_with_stdio(exec_cap, name, args, NO_CAP)
}

/// То же, но ребёнок дополнительно получает право `stdio_cap` — обычно эндпоинт родителя
/// ([`self_endpoint`]). Ядро само назовёт его индекс в окружении ребёнка (`STDIO=<i>`), после
/// чего весь его вывод и ввод пойдут к нам ([`stdio`]) вместо общей консоли.
pub fn spawn_with_stdio(
    exec_cap: usize, name: &[u8], args: &[u8], stdio_cap: usize,
) -> Option<usize> {
    spawn_with_endpoint(exec_cap, name, args, stdio_cap, b"STDIO\0")
}

/// То же, но право объявляется в окружении под ЗАДАННЫМ именем (Веха 117).
///
/// Хостов у процесса может быть несколько и разных: терминал даёт `STDIO`, композитор окон —
/// `WM`. Ядро смысла этих строк по-прежнему не знает: оно кладёт в окружение имя и индекс, а что
/// они значат — дело userspace ([[process-contract]]).
///
/// `key` — ASCII-строка, ЗАВЕРШЁННАЯ нулём (её читает ядро из памяти процесса).
pub fn spawn_with_endpoint(
    exec_cap: usize, name: &[u8], args: &[u8], cap: usize, key: &[u8],
) -> Option<usize> {
    let r = abi::syscall(
        SYS_SPAWN, exec_cap, name.as_ptr() as usize, name.len(),
        args.as_ptr() as usize, args.len(), cap, key.as_ptr() as usize,
    ).0;
    (r != NO_CAP).then_some(r)
}

/// Одно событие мыши (Веха 115): смещение с прошлого события и состояние кнопок.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MouseEvent {
    pub dx: i16,
    pub dy: i16,
    /// бит0 — левая, бит1 — правая, бит2 — средняя; Веха 142 — бит3 и бит4 это четвёртая и
    /// пятая кнопки (боковые; ими же тилт-колесо сообщает наклон влево-вправо — отдельной оси
    /// для наклона у PS/2 нет). Приходят только от мыши, понявшей режим Explorer.
    pub buttons: u8,
    /// Колесо: +1 — крутнули ОТ СЕБЯ (вверх), -1 — на себя, 0 — не крутили (Веха 123.1).
    /// Знак приведён к человеческому в ядре: у самого PS/2 он обратный. Шестой байт события
    /// был запасным с самого начала — расширять протокол не пришлось.
    pub wheel: i8,
}

impl MouseEvent {
    pub fn left(&self) -> bool {
        self.buttons & 1 != 0
    }
    pub fn right(&self) -> bool {
        self.buttons & 2 != 0
    }
    pub fn middle(&self) -> bool {
        self.buttons & 4 != 0
    }
}

/// `SYS_MOUSE_READ` (Веха 115) — забрать накопившиеся события мыши. Доступно только ВЛАДЕЛЬЦУ
/// ЭКРАНА: курсор существует лишь там, где есть чем его нарисовать.
///
/// Ядро отдаёт СОБЫТИЯ, а не положение курсора: где курсор, решает тот, кто рисует, — он знает
/// границы экрана, ускорение и то, во что курсор упирается.
pub fn mouse_read(out: &mut [MouseEvent]) -> usize {
    const REC: usize = 6;
    let mut raw = [0u8; 64 * REC];
    let cap = out.len().min(raw.len() / REC) * REC;
    let n = abi::syscall(SYS_MOUSE_READ, raw.as_mut_ptr() as usize, cap, 0, 0, 0, 0, 0).0;
    if n == NO_CAP || n == 0 {
        return 0;
    }
    let count = n / REC;
    for (i, e) in out.iter_mut().enumerate().take(count) {
        let b = &raw[i * REC..];
        *e = MouseEvent {
            dx: i16::from_le_bytes([b[0], b[1]]),
            dy: i16::from_le_bytes([b[2], b[3]]),
            buttons: b[4],
            wheel: b[5] as i8,
        };
    }
    count
}

/// `SYS_KLOG` (Веха 116) — забрать журнал ядра. Возвращает `(байт, потеряно кольцом)`.
///
/// Нужен там, где увиденное нельзя перечитать: на машине без COM-порта вывод ядра живёт до
/// первого кадра терминала, а потом исчезает навсегда. Если буфер меньше журнала, приезжают
/// ПОСЛЕДНИЕ байты — при разборе неполадки свежее ценнее.
pub fn klog(out: &mut [u8]) -> (usize, usize) {
    let r = abi::syscall(SYS_KLOG, out.as_mut_ptr() as usize, out.len(), 0, 0, 0, 0, 0);
    if r.0 == NO_CAP {
        return (0, 0);
    }
    (r.0, r.1)
}

/// `SYS_CONSIZE` (Веха 120) — размер КОНСОЛИ ЯДРА в знакоместах. `None` — ядро размера не знает
/// (консоль в serial: сколько знакомест у терминала на том конце, оно выяснить не может).
///
/// Спрашивать её приходится тому, у кого нет хоста stdio ([`stdio::win_size`]) — то есть
/// программе в спасательном шелле. Это единственный экран, какой в такой момент есть.
pub fn console_size() -> Option<(u16, u16)> {
    let r = abi::syscall(SYS_CONSIZE, 0, 0, 0, 0, 0, 0, 0);
    (r.0 > 0 && r.1 > 0).then(|| (r.0 as u16, r.1 as u16))
}

/// Событие клавиатуры (Веха 119): что нажали, с какими модификаторами и какой это символ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    /// Код клавиши: печатные — их НЕсдвинутый ASCII, прочие — числа выше 0x100.
    pub sym: u16,
    /// Биты: 1 Shift, 2 Ctrl, 4 Alt, 8 Super.
    pub mods: u8,
    pub down: bool,
    /// Готовый символ КОДОВОЙ ТОЧКОЙ (0 — клавиша непечатная). Раскладку знает ядро, и знает в
    /// одном месте. Точка, а не байт: с раскладкой RU/EN печатается кириллица (Веха 127.1).
    pub ch: u16,
}

impl KeyEvent {
    pub fn shift(&self) -> bool { self.mods & 1 != 0 }
    pub fn ctrl(&self) -> bool { self.mods & 2 != 0 }
    pub fn alt(&self) -> bool { self.mods & 4 != 0 }
    pub fn super_key(&self) -> bool { self.mods & 8 != 0 }
}

/// `SYS_KEY_READ` (Веха 119) — забрать события клавиатуры. Право — ВЛАДЕНИЕ ЭКРАНОМ, как у мыши.
///
/// Байтовый поток консоли никуда не делся: он нужен всем, кто читает «текст», а события нужны
/// тому, кто разбирает АККОРДЫ. Из байта аккорд не восстановить — `Super+L` и `l` это один байт.
pub fn key_read(out: &mut [KeyEvent]) -> usize {
    const REC: usize = 6;
    let mut raw = [0u8; 64 * REC];
    let cap = out.len().min(raw.len() / REC) * REC;
    let n = abi::syscall(SYS_KEY_READ, raw.as_mut_ptr() as usize, cap, 0, 0, 0, 0, 0).0;
    if n == NO_CAP || n == 0 {
        return 0;
    }
    let count = n / REC;
    for (i, e) in out.iter_mut().enumerate().take(count) {
        let b = &raw[i * REC..];
        *e = KeyEvent {
            sym: u16::from_le_bytes([b[0], b[1]]),
            mods: b[2],
            down: b[3] != 0,
            ch: u16::from_le_bytes([b[4], b[5]]),
        };
    }
    count
}

/// `SYS_PARENT(pid)` (Веха 114): чей это ребёнок. `None` — родителя нет или номер неверен.
///
/// Нужен хосту stdio: право на него наследуется вглубь, поэтому писать ему может не только
/// ребёнок, но и внук, — а разложить вывод по панелям надо всё равно. Поднимаясь по родителям,
/// хост находит владельца ([`stdio`]).
pub fn parent_of(pid: usize) -> Option<usize> {
    let p = abi::syscall(SYS_PARENT, pid, 0, 0, 0, 0, 0, 0).0;
    (p != NO_CAP).then_some(p)
}

/// `SYS_SELF_ENDPOINT` (Веха 98): право ВЫЗЫВАТЬ нас, чтобы отдать его детям. Не расширение
/// полномочий — принимать сообщения процесс может и так; кому раздать право на себя, его дело.
pub fn self_endpoint() -> usize {
    abi::syscall(SYS_SELF_ENDPOINT, 0, 0, 0, 0, 0, 0, 0).0
}

/// Результат [`wait`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    /// Ребёнок завершился с этим кодом.
    Exited(usize),
    /// Ещё работает (бывает только при `nonblock`).
    Running,
    /// Не наш ребёнок либо номер неверен.
    NoChild,
}

/// Веха 98 — `SYS_WAIT(pid, nonblock)`: забрать код выхода СВОЕГО ребёнка. Неблокирующая форма
/// нужна реактору: он не может замереть на одном ребёнке, пока остальные ждут обслуживания.
pub fn wait(pid: usize, nonblock: bool) -> Wait {
    match abi::syscall(SYS_WAIT, pid, nonblock as usize, 0, 0, 0, 0, 0).0 {
        NO_CAP => Wait::NoChild,
        r if r == NO_CAP - 1 => Wait::Running,
        code => Wait::Exited(code),
    }
}

/// Описание видеорежима (Веха 97): что за экран нам отдали.
#[derive(Clone, Copy, Debug, Default)]
pub struct VideoInfo {
    pub width: usize,
    pub height: usize,
    /// Байт на строку — НЕ равен `width * bpp/8`: прошивка выравнивает строки.
    pub pitch: usize,
    pub bpp: usize,
    /// По каналам R, G, B: (позиция младшего бита, ширина маски). Раскладку задаёт прошивка,
    /// зашивать `0x00RRGGBB` нельзя — бывают и 16-битные режимы.
    pub rgb: [(u8, u8); 3],
}

/// `SYS_VIDEO_INFO(mmio_cap, out)` (Веха 97): геометрия и раскладка цвета экрана. Право то же,
/// что на окно фреймбуфера, — числа неотделимы от права рисовать. `None` — нет права или экрана.
pub fn video_info(mmio_cap: usize) -> Option<VideoInfo> {
    let mut raw = [0u32; 10];
    let r = abi::syscall(SYS_VIDEO_INFO, mmio_cap, raw.as_mut_ptr() as usize, 0, 0, 0, 0, 0).0;
    if r != 0 || raw[0] == 0 || raw[1] == 0 {
        return None;
    }
    Some(VideoInfo {
        width: raw[0] as usize,
        height: raw[1] as usize,
        pitch: raw[2] as usize,
        bpp: raw[3] as usize,
        rgb: [
            (raw[4] as u8, raw[5] as u8),
            (raw[6] as u8, raw[7] as u8),
            (raw[8] as u8, raw[9] as u8),
        ],
    })
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

/// Веха 152.2 — `SYS_CAP_INFO`: описать дескриптор. `None` — недействителен/устарел; иначе
/// `(вид, права)`. Виды (общий словарь с ядром, `cap::info_kind`): 1 store · 2 root · 3 value ·
/// 4 endpoint · 5 reply · 6 blk · 7 net · 8 mmio · 9 dma · 10 power · 11 shm · 12 irq. Права —
/// битовая маска `Rights` (READ 1 · WRITE 2 · EXEC 4 · SEND 8 · GRANT 16). Побочного эффекта нет.
pub fn cap_info(cap: usize) -> Option<(u8, u32)> {
    let r = abi::syscall(SYS_CAP_INFO, cap, 0, 0, 0, 0, 0, 0).0;
    (r != NO_CAP).then(|| ((r >> 16) as u8, (r & 0xffff) as u32))
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

// Веха 136: константы `TICK_NS` здесь БОЛЬШЕ НЕТ. Она объявляла «x86 = 1 нс на тик» — то есть
// TSC ровно 1 ГГц, как в QEMU TCG. На живом процессоре (и под KVM) TSC идёт с частотой ядра, и
// каждая программа, считавшая по этой константе, ошибалась во столько же раз. Цену тика теперь
// называет ядро: [`tick_hz`], [`ticks_to_ns`], [`ns_to_ticks`].

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
    /// readlink(path) -> цель ссылки (пусто — не ссылка). Веха 108.2: симлинки есть только в
    /// дереве пакета под `/nix/store` — своих персоналия по-прежнему не заводит.
    pub const OP_READLINK: usize = 10;

    /// Тип записи в ответе `stat` (7-й байт, Веха 108.2) — тот же, что в индексе дерева пакета.
    pub const T_FILE: u8 = 0;
    pub const T_DIR: u8 = 1;
    pub const T_LINK: u8 = 2;
    /// Исполняемый бит поверх типа.
    pub const T_EXEC: u8 = 0x08;

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

    /// Путь относительно каталога, объявленного шеллом ([`crate::cwd`]) → АБСОЛЮТНЫЙ, с
    /// схлопнутыми `.`, `..` и `//`; результат в `out`, возвращается его длина (Веха 120.1).
    ///
    /// Здесь, а не в каждой программе: логика уже существовала дважды (`vsh`, `vvsh`), и третья
    /// копия в редакторе разъехалась бы с ними при первой правке. Абсолютный путь переписывается
    /// как есть — начинать разбор с `/` значит игнорировать любой cwd, и это правильно.
    ///
    /// Без кучи (как и вся эта библиотека): компоненты не собираются в список, а дописываются в
    /// `out`, а `..` откусывает от него последний компонент.
    pub fn resolve(path: &[u8], out: &mut [u8]) -> usize {
        fn push(out: &mut [u8], len: usize, c: &[u8]) -> usize {
            if len + 1 + c.len() > out.len() {
                return len; // не влезло — путь усечён, но не испорчен
            }
            out[len] = b'/';
            out[len + 1..len + 1 + c.len()].copy_from_slice(c);
            len + 1 + c.len()
        }
        let mut len = 0usize;
        if path.first() != Some(&b'/') {
            let mut cwdbuf = [0u8; 256];
            let n = crate::cwd(&mut cwdbuf);
            for c in cwdbuf[..n].split(|&b| b == b'/').filter(|c| !c.is_empty()) {
                len = push(out, len, c);
            }
        }
        for c in path.split(|&b| b == b'/') {
            match c {
                b"" | b"." => {}
                // `..` — откусить последний компонент: ищем `/`, с которого он начался.
                b".." => len = out[..len].iter().rposition(|&b| b == b'/').unwrap_or(0),
                _ => len = push(out, len, c),
            }
        }
        if len == 0 && !out.is_empty() {
            out[0] = b'/'; // всё схлопнулось до корня
            len = 1;
        }
        len
    }

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
    ///
    /// В запросе едет РАЗМЕР приёмного буфера (u32 LE), и это не оптимизация (Веха 112). Ядро
    /// доставляет клиенту `min(ответ, его буфер)`, а сервер двигает курсор файла на то, что
    /// ОТПРАВИЛ, — и разница между этими числами пропадала молча: файл длиннее буфера читался
    /// первым куском, после чего `read` возвращал 0, будто файл кончился. Так терялся хвост
    /// всякого `.vv`-модуля длиннее 512 байт: `terminal.vv` доезжал до середины комментария, и
    /// `rebuild` ругался на конфиг, который на диске лежал целым.
    ///
    /// Лечится это только тем, что сервер УЗНАЁТ ёмкость приёмника: правду о ней знает клиент,
    /// и говорить её должен он.
    pub fn read(ep: usize, fd: usize, buf: &mut [u8]) -> usize {
        if fd == STDIN {
            return crate::read_stdin(buf);
        }
        if fd < FD_BASE {
            return 0;
        }
        let want = (buf.len() as u32).to_le_bytes();
        crate::call(ep, OP_READ | ((fd - FD_BASE) << 8), &want, buf)
    }

    /// `write(fd, buf) -> len`. `fd`=1/2 → консоль ядра.
    ///
    /// Пишем КУСКАМИ: приёмный буфер файлового сервера конечен (512 Б в `bin/posixfs`), а
    /// хвост, который в него не влез, доставка отрезает — молча, и `write` при этом бодро
    /// возвращал полную длину. Так пропала половина сеянного `/etc/system/terminal.vv`
    /// (Веха 100): ушло 1.5 КиБ, дошло 512, текст оборвался посреди буквы, и виноватым
    /// выглядел конфиг. Чтение кусками умели давно — запись просто забыли.
    pub fn write(ep: usize, fd: usize, buf: &[u8]) -> usize {
        if fd < FD_BASE {
            crate::write(buf);
            return buf.len();
        }
        const CHUNK: usize = 512; // = размер `req` в bin/posixfs; больше не доедет
        let op = OP_WRITE | ((fd - FD_BASE) << 8);
        if buf.is_empty() {
            crate::call(ep, op, buf, &mut []);
            return 0;
        }
        // Возвращаем то, что ДЕЙСТВИТЕЛЬНО доехало (Веха 101): если приёмный буфер сервера
        // окажется меньше куска, вызывающий узнает об этом по числу, а не по испорченному файлу.
        let mut done = 0usize;
        for part in buf.chunks(CHUNK) {
            let r = crate::call_ex(ep, op, part, &mut [], crate::NO_CAP);
            done += r.sent;
            if r.reply_len == usize::MAX || r.sent < part.len() {
                break;
            }
        }
        done
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
        readdir_ex(ep, path, buf).0
    }

    /// То же, но вторым числом — сколько сервер ХОТЕЛ отдать (Веха 105).
    ///
    /// Разница между «доехало» и «хотел отдать» — единственный способ отличить короткий каталог
    /// от обрезанного списка. Ядро эту длину сообщает с Вехи 101; здесь она наконец доходит до
    /// того, кто печатает список, — иначе `ls` уверенно врёт, показывая первые N имён.
    pub fn readdir_ex(ep: usize, path: &[u8], buf: &mut [u8]) -> (usize, usize) {
        let c = crate::call_ex(ep, OP_READDIR, path, buf, crate::NO_CAP);
        (c.reply_len, c.reply_want)
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
        stat_ex(ep, path).map(|(d, sz, _)| (d, sz))
    }

    /// То же плюс ТИП записи (Веха 108.2): по нему видно симлинк и исполняемый бит — в дереве
    /// пакета это единственный способ их различить, а `ls` без этого показывал бы ссылку файлом.
    pub fn stat_ex(ep: usize, path: &[u8]) -> Option<(bool, usize, u8)> {
        let mut r = [0u8; 7];
        let n = crate::call(ep, OP_STAT, path, &mut r);
        if n < 6 || r[0] == 0 {
            return None;
        }
        let size = u32::from_le_bytes([r[1], r[2], r[3], r[4]]) as usize;
        let ty = if n >= 7 { r[6] } else { r[5] }; // старый сервер: только «каталог?»
        Some((r[5] != 0, size, ty))
    }

    /// `readlink(path, buf) -> длина цели` (0 — не ссылка либо нет такой). Веха 108.2.
    pub fn readlink(ep: usize, path: &[u8], buf: &mut [u8]) -> usize {
        let n = crate::call(ep, OP_READLINK, path, buf);
        if n == usize::MAX {
            0
        } else {
            n
        }
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
    ///
    /// Веха 101 — возвращает, записалось ли ВСЁ. Раньше не возвращала ничего, и запись, дошедшая
    /// наполовину, выглядела как удачная; вызывающему полагается сказать об этом человеку.
    pub fn echo_to(ep: usize, path: &[u8], msg: &[u8]) -> bool {
        let fd = open(ep, path, O_TRUNC);
        if fd == usize::MAX {
            return false;
        }
        let n = write(ep, fd, msg);
        close(ep, fd);
        n == msg.len()
    }
}
