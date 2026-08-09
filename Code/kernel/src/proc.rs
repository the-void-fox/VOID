//! Веха 10.2 — процессы: своё адресное пространство (satp) + кооперативное планирование.
//!
//! Процесс = изолированная единица исполнения в U-mode: собственное адресное пространство
//! ([[user-mode]], [[sv39-paging]]) и сохранённый trap-кадр. Ядро возобновляет «текущий»
//! процесс через [`arch::enter_user`] (активация пространства → регистры из кадра → возврат в U).
//! Один hart, планирование **кооперативное**: процесс уступает через `SYS_YIELD` или завершается
//! через `SYS_EXIT` (прерывания в U-mode пока выключены).
//!
//! Модель без ядерных нитей на процесс: каждый trap из U обрабатывается на общем trap-стеке,
//! после чего ядро возобновляет тот процесс, что стал текущим ([`handle_user_trap`]).
//!
//! Веха 20 — **интерактивность**: `SYS_READ` блокирует процесс до ввода с консоли
//! ([[uart|кольцевой буфер UART]]), планировщик умеет спать в ожидании ввода ([`wait_stdin`]),
//! `SYS_EXEC` запускает программу из store по имени корня (право `EXEC`) и ждёт её завершения.
//!
//! Веха 22 — **куча процесса и честные фолты**: `SYS_MAP` лениво резервирует диапазон
//! [USER_HEAP_BASE_VA, heap_brk); страницы выделяются по page fault из U-mode
//! ([`handle_user_fault`]) или доотображением перед доступом ядра в шлюзах
//! ([`ensure_heap_range`] — фолт из S-mode фатален). Фолт вне кучи убивает ПРОЦЕСС, не ядро.
//!
//! Веха 30 — **контракт запуска** (ABI v2, [[process-contract]]): `SYS_EXEC` несёт argv;
//! env и таблица стартовых capability наследуются от родителя ([`cap::endow`] — наделение
//! потомка, не grant); процесс читает своё наследство через `SYS_ARGS` (argv/env) и
//! `SYS_STARTCAP` (преоткрытые права, как preopen'ы WASI). То, что Linux кладёт на стек
//! при execve, у нас спрашивают у ядра — раскладка стека остаётся делом программы.
//!
//! Веха 37 — **checkpoint процессов** ([[checkpoint]], [`crate::checkpoint`]):
//! `SYS_CHECKPOINT` морозит текущий процесс в store (setjmp-семантика: живому 0,
//! размороженному 1), `SYS_RESTORE` поднимает образ как `SYS_EXEC` — вычисления
//! переживают перезагрузку.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicBool, Ordering};

use void_abi::{Cap, ContentId, Rights};

use crate::arch::{self, Context, FaultKind, TrapFrame, UserTrap};
use crate::sync::SpinLock;
use crate::{cap, elf, frame, println, timer};

/// Болтливость шлюзов syscall'ов ([ipc]/[obj]/[blk]/[mm]/[exec]-строки на каждый вызов).
/// Демо живут этой трассировкой, но бенчи (Веха 28) она бы утопила — и в шуме, и в
/// ЦЕНЕ (println дороже самого syscall'а): на время замеров ядро замолкает.
static VERBOSE: AtomicBool = AtomicBool::new(true);

/// Включить/выключить трассировку шлюзов (бенч-демо глушит её на свою сессию).
pub fn set_verbose(on: bool) {
    VERBOSE.store(on, Ordering::Relaxed);
}

/// Включена ли трассировка. Нужна СОСЕДНИМ модулям: макрос `vprintln!` ниже текстовый и
/// виден только внутри этого файла, а глушить отладочный вывод должно всё ядро, а не proc.rs.
pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

/// println! шлюза: печатает только при включённой трассировке.
macro_rules! vprintln {
    ($($arg:tt)*) => {
        if VERBOSE.load(Ordering::Relaxed) {
            println!($($arg)*);
        }
    };
}

// ─── адресное пространство процесса ────────────────────────────────────────────
/// Стек процесса живёт в незанятом ядром регионе VPN[2]=1 (0x4000_0000..0x8000_0000),
/// растёт вниз от 0x8000_0000. Это гарантирует, что маппинг стека не заденет общие
/// подтаблицы ядра (VPN[2]=0 и 2) — см. [`arch::clone_kernel_root`].
pub const USER_STACK_TOP_VA: usize = 0x8000_0000;
/// Веха 31: 16 страниц (64 КиБ) — std-программы (fmt, sort) прожорливее к стеку,
/// чем наши no_std-бинари; переполнение = фолт ниже стека = гибель процесса, не порча.
/// Веха 32: 64 страницы (256 КиБ) — uutils кладут на стек буферы по 64 КиБ (cat/wc).
const USER_STACK_PAGES: usize = 64;
const PAGE: usize = 4096;
/// Веха 22.1: куча процесса растёт вверх отсюда (код/данные ELF ниже, стек — выше, у
/// 0x8000_0000). `SYS_MAP` только резервирует диапазон [heap_base, heap_brk); страницы
/// выделяются ЛЕНИВО — по page fault ([`handle_user_fault`]).
const USER_HEAP_BASE_VA: usize = 0x6000_0000;
/// Начало региона VPN[2]=1 — весь тот же незанятый ядром диапазон, где живёт стек процесса, но
/// теперь ещё и код/данные ELF-программ (Веха 19, [`spawn_elf`]). Совпадает с базой линковки
/// `programs/*/linker.ld`; [`crate::elf::load`] отвергает сегменты ниже этого адреса.
pub const USER_REGION_START: usize = 0x4000_0000;

/// Веха 108.4 — база, по которой ложится ДИНАМИЧЕСКИЙ ЗАГРУЗЧИК (`ld.so`) linux-процесса.
/// Ровно посередине между образом программы ([`USER_REGION_START`]) и кучей: и тот и другая
/// заведомо меньше 256 МиБ, так что пересечься им негде.
const INTERP_BASE_VA: usize = 0x5000_0000;

/// Веха 30: потолок доп. аргументов `SYS_EXEC` (NUL-разделённый блоб). Столько же, сколько
/// буфер IPC-запроса, — аргументы длиннее пусть едут объектом store.
/// Веха 98 — ответ `SYS_WAIT(nonblock)`, когда ребёнок ещё жив. Отличается от `usize::MAX`
/// («нет такого ребёнка») намеренно: реактору надо различать «подожди» и «ошибка».
pub const WOULD_BLOCK: usize = usize::MAX - 1;

const ARGS_MAX: usize = 512;

// ─── ядерный trap-стек для trap'ов из U-mode ──────────────────────────────────
// 64 КиБ — как загрузочный стек ядра (linker.ld): syscall'ы делают настоящую работу
// (`object::put` → BLAKE3 + куча + `println!`), а в -O0-сборках кадры были крупные: с 16 КиБ
// стек однажды переполнялся ВНИЗ в соседнюю read-only секцию (store page fault на записи).
const TRAP_STACK_SIZE: usize = 64 * 1024;

#[repr(align(16))]
struct TrapStack(#[allow(dead_code)] [u8; TRAP_STACK_SIZE]);
static mut TRAP_STACK: TrapStack = TrapStack([0; TRAP_STACK_SIZE]);

fn trap_top() -> usize {
    addr_of!(TRAP_STACK) as usize + TRAP_STACK_SIZE
}

// ─── таблица процессов ────────────────────────────────────────────────────────
#[derive(PartialEq, Clone, Copy)]
enum State {
    Runnable,
    RecvWait,  // заблокирован в RECV (ждёт сообщения)
    ReplyWait, // заблокирован в CALL (ждёт ответа сервера)
    StdinWait, // заблокирован в READ (ждёт ввода с консоли, Веха 20.2)
    /// Заблокирован в EXEC: ждёт завершения процесса-ребёнка с этим id (Веха 20.3).
    ExecWait(usize),
    /// Веха 35: заблокирован в THREAD_JOIN — ждёт завершения нити с этим id.
    JoinWait(usize),
    /// Веха 35: заблокирован в FUTEX_WAIT (ключ — `futex_addr` + пространство нити);
    /// будит FUTEX_WAKE по тому же адресу или истёкший `futex_deadline`.
    FutexWait,
    /// Веха 52: userspace-драйвер заблокирован в SYS_IRQ_WAIT — ждёт прерывания своего устройства;
    /// будит [`drain_userdrv_irq`] по флагу [`USERDRV_IRQ_PENDING`], выставленному обработчиком IRQ.
    IrqWait,
    /// Веха 114: заблокирован в SYS_SLEEP до срока в `futex_deadline`. Отдельное состояние, а не
    /// `FutexWait` с выдуманным адресом: спящий по времени НЕ должен просыпаться от чужого
    /// `FUTEX_WAKE`, случайно назвавшего тот же адрес.
    Sleeping,
    Finished,
}

struct Proc {
    /// Токен адресного пространства ([`arch::space_token`]; на RISC-V — значение satp).
    space: usize,
    frame: TrapFrame,
    state: State,
    /// Домен защиты процесса — его личный c-space ([[capabilities]]). Начальные права
    /// (эндпоинты, устройства, store) ядро минтит сюда при spawn; syscall'ы проверяют их отсюда.
    domain: cap::DomainId,
    /// Приёмный буфер (VA в своём пространстве) + его размер. Двойного назначения, но НЕ
    /// одновременно: у клиента в `ReplyWait` — куда лёг бы ответ (`REPLY`); у сервера в
    /// `RecvWait` — куда лечь входящему запросу (`RECV`). Задаётся в `CALL`/`RECV`.
    recv_buf: usize,
    recv_cap: usize,
    /// Буфер запроса клиента (VA + длина) — задаётся в `CALL`, копируется в приёмный буфер
    /// сервера при доставке (`RECV`/прямая доставка). Передача буфера клиент→сервер через IPC.
    send_buf: usize,
    send_len: usize,
    /// Веха 21.1: capability, передаваемая с текущим `CALL` (дескриптор в c-space отправителя;
    /// `usize::MAX` — нет). Проверяется (`GRANT`) при отправке, копируется в домен получателя
    /// при доставке — как `send_buf`, только для прав.
    send_cap: usize,
    /// Веха 91 — процесс спит в `SYS_RECV` и просил будить его ещё и ПРИХОДОМ КАДРА
    /// (режим 3): так сетевой сервер просыпается от карты, а не от таймера.
    wake_on_net: bool,
    /// Веха 103 — тот же приём для КЛАВИАТУРЫ: процесс спит в `SYS_RECV`, но просыпается и от
    /// ввода с консоли. Без этого реактор терминала обязан был крутиться по таймеру: ждать
    /// сообщения и клавишу ОДНОВРЕМЕННО было нечем.
    wake_on_key: bool,
    /// Веха 89 — сколько ФРЕЙМОВ куча этой группы реально заняла (лениво, по фолтам).
    /// Живёт у лидера группы, как и `heap_brk`; сверяется с [`page_quota`].
    pages: usize,
    /// Веха 22.1: ленивая куча процесса — зарезервированный `SYS_MAP` диапазон
    /// [`USER_HEAP_BASE_VA`, heap_brk). Фолт внутри — выделить страницу; вне — гибель процесса.
    heap_brk: usize,
    /// Веха 30 — контракт запуска: argv процесса, NUL-разделённые записи, `[0]` — имя
    /// программы. Читается через `SYS_ARGS(0)`; заполняется при `SYS_EXEC` (имя + доп.
    /// аргументы вызывающего).
    args: Vec<u8>,
    /// Веха 30: окружение (`KEY=VAL\0…`). Читается через `SYS_ARGS(1)`; при `SYS_EXEC`
    /// наследуется от родителя как есть.
    env: Vec<u8>,
    /// Веха 30: таблица стартовых capability (биты дескрипторов В ДОМЕНЕ процесса) —
    /// «преоткрытые» права, как preopen'ы WASI. Читается через `SYS_STARTCAP(i)`; при
    /// `SYS_EXEC` наследуется копиями ([`cap::endow`]). Первые два права ядро по традиции
    /// дублирует в `a0`/`a1` при spawn'е серверов.
    start_caps: Vec<usize>,
    /// Веха 35 — группа нитей: индекс ГЛАВНОЙ нити (лидера) процесса. У самого лидера
    /// `group == собственный индекс`. Нити одной группы делят `space` (адресное
    /// пространство), `domain` (c-space) и КУЧУ лидера (`heap_brk`, args/env/start_caps
    /// читаются у него). Свои у нити: кадр, стек, состояние, TLS.
    group: usize,
    /// Веха 35 — значение, с которым нить завершилась (THREAD_EXIT); его получает
    /// присоединяющийся в THREAD_JOIN. Для процесса (SYS_EXIT) роль играет код выхода.
    retval: usize,
    /// Веха 35 — адрес futex-слова (в пространстве нити), на котором она спит в
    /// FUTEX_WAIT; ключ пробуждения = (`space`, `futex_addr`). Валиден лишь в состоянии
    /// [`State::FutexWait`].
    futex_addr: usize,
    /// Веха 35 — дедлайн futex-ожидания в тиках [`arch`]-счётчика (`None` — бессрочно).
    /// Истёкший дедлайн будит нить с «таймаутом» (проверяется в [`resume`]).
    futex_deadline: Option<u64>,
    /// Веха 38 — личность linux-abi: процесс — неизменённый static-PIE musl-бинарь из nixpkgs,
    /// его `ecall`/`syscall` уходит в трансля́тор [`crate::linux`], а не в ABI VOID. Ставится
    /// при exec'е ET_DYN-образа ([`spawn_linux_locked`]); нити наследуют (у linux их пока нет).
    linux: bool,
    /// Веха 98 — процесс запущен через `SYS_SPAWN` и его кода выхода ЖДЁТ родитель: слот
    /// нельзя переиспользовать, пока код не забрали (`SYS_WAIT`). Без этого номер процесса
    /// достался бы новому запуску, и ожидающий получил бы чужой результат.
    zombie: bool,
    /// Веха 98 — кто запустил (для `SYS_WAIT`: ждать чужих детей нельзя). `usize::MAX` — никто.
    parent: usize,
    /// Веха 98 — код выхода, сохранённый до `SYS_WAIT`. `None` — процесс ещё жив.
    exit_code: Option<usize>,
    /// Веха 108.3 — открытые файлы личности Linux (дескрипторы с 3; 0/1/2 — консоль). Читаются
    /// прямо из объектного store ([`crate::lxfs`]), потому что ходить из syscall'а по IPC к
    /// файловому серверу нечем. Пусто у всех, кроме linux-процессов.
    lx_fds: Vec<Option<LxFd>>,
}

/// Открытый файл личности Linux: что читать, откуда и где мы в нём находимся.
#[derive(Clone)]
struct LxFd {
    meta: crate::lxfs::Meta,
    /// Позиция чтения (`lseek`/`read`).
    off: u64,
    /// Путь — нужен `getdents64` (перечисление `/nix/store` идёт по корням, а не по узлу) и
    /// диагностике.
    path: Vec<u8>,
    /// Сколько записей каталога уже отдано `getdents64`.
    dpos: usize,
}

struct Table {
    procs: Vec<Proc>,
    current: usize,
    /// Недоставленные запросы IPC: (отправитель, получатель, сообщение).
    mailbox: Vec<(usize, usize, usize)>,
    /// Веха 89 — номера слотов полностью утилизированных групп, готовые к переиспользованию.
    /// До этого `procs` только рос: завершённые процессы оставались навсегда, планировщик
    /// обходил их линейно на каждом переключении, а сотни `exec` (пакетный менеджер — это
    /// сотни) пухли впустую. Класть сюда слот можно ТОЛЬКО после [`cap::revoke_process`].
    free_slots: Vec<usize>,
}

impl Table {
    /// Следующий готовый процесс по кругу от `from` (включая сам `from`, если он готов).
    fn next_runnable(&self, from: usize) -> Option<usize> {
        let n = self.procs.len();
        for i in 1..=n {
            let idx = (from + i) % n;
            if self.procs[idx].state == State::Runnable {
                return Some(idx);
            }
        }
        None
    }
}

static TABLE: SpinLock<Table> =
    SpinLock::new(Table { procs: Vec::new(), current: 0, mailbox: Vec::new(), free_slots: Vec::new() });

/// Контекст ядра, в который возвращаемся, когда все процессы завершились.
static mut RETURN_CTX: Context = Context::EMPTY;

/// Веха 52 — «пришло прерывание userspace-драйвера» (вектор `VEC_USERDRV`). Обработчик IRQ
/// выставляет флаг БЕЗ замка таблицы процессов (иначе дедлок с прерванным контекстом), а
/// планировщик снимает его и будит спящих в `SYS_IRQ_WAIT` ([`drain_userdrv_irq`]).
static USERDRV_IRQ_PENDING: AtomicBool = AtomicBool::new(false);

/// Веха 52 — из обработчика прерывания (trap): просто отметить, что IRQ пришёл. Пробуждение —
/// на планировщике, где замок таблицы берётся законно. Зовётся из x86-обработчика VEC_USERDRV;
/// на riscv userspace-драйверов с IRQ пока нет (USB/e1000 — x86), поэтому там это мёртвый код.
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
pub fn on_userdrv_irq() {
    USERDRV_IRQ_PENDING.store(true, Ordering::Relaxed);
}

/// Веха 52 — если пришёл IRQ драйвера, разбудить всех в `IrqWait` (сделать `Runnable`). Драйвер
/// сам сверится с состоянием устройства (ICR) при пробуждении — ложное пробуждение безвредно.
fn drain_userdrv_irq(t: &mut Table) {
    if USERDRV_IRQ_PENDING.swap(false, Ordering::Relaxed) {
        for p in &mut t.procs {
            if p.state == State::IrqWait {
                p.state = State::Runnable;
            }
        }
    }
}

/// Веха 52 — есть ли процессы, спящие в `SYS_IRQ_WAIT` (нужно `wait_stdin`: не завершать сессию,
/// пока драйвер ждёт прерывания).
fn any_irq_waiting(t: &Table) -> bool {
    t.procs.iter().any(|p| p.state == State::IrqWait)
}

// ─── создание и запуск ────────────────────────────────────────────────────────

/// Новое адресное пространство процесса: клон корня ядра (ядро отображено без флага U — нужно
/// trap-обработчику при satp процесса) + приватный стек в незанятом регионе VPN[2]=1. Код и
/// данные добавит [`crate::elf::load`]: с Вехи 23 процессы приходят ТОЛЬКО из ELF в store.
/// Веха 89 — `None`, если памяти не хватило. Раньше здесь стояла паника: программа, которую
/// нечем запустить, роняла ЯДРО. Частично построенное пространство сносим целиком, чтобы фреймы
/// не утекли (`free_address_space` умеет неполные деревья — общие с ядром узлы он пропускает).
fn new_address_space() -> Option<usize> {
    let root = arch::clone_kernel_root()?;
    // Приватный стек в VPN[2]=1: несколько страниц из свежих фреймов.
    for i in 1..=USER_STACK_PAGES {
        let va = USER_STACK_TOP_VA - i * PAGE;
        let ok = frame::alloc().is_some_and(|pa| unsafe {
            arch::map(root, va, pa, arch::MAP_R | arch::MAP_W | arch::MAP_U)
        });
        if !ok {
            unsafe { arch::free_address_space(root) };
            return None;
        }
    }
    Some(root)
}

/// Завести запись в таблице процессов: домен защиты (c-space) + стартовый кадр (вход `entry`,
/// `a0`=`arg`, `sp`=верх стека). Возвращает id процесса; он же — адрес эндпоинта для
/// [`cap::Target::Endpoint`]. Пока Runnable. Начальные capability ядро минтит в [`domain`] и
/// передаёт дескриптор через [`set_arg`] ДО [`run`].
fn create_process(name: &'static str, root: usize, entry: usize, arg: usize) -> usize {
    let mut t = TABLE.lock();
    create_process_locked(&mut t, name, root, entry, arg)
}

/// То же, что [`create_process`], но под УЖЕ взятым замком таблицы — для `SYS_EXEC` (Веха 20.3),
/// который создаёт процесс прямо из диспетчера syscall'ов (замок там уже держится).
fn create_process_locked(
    t: &mut Table,
    name: &'static str,
    root: usize,
    entry: usize,
    arg: usize,
) -> usize {
    let frame = TrapFrame::new_user(entry, USER_STACK_TOP_VA, arg);
    let domain = cap::create_domain(name);
    // Веха 89: занять освободившийся слот, если он есть (права на него уже отозваны при
    // утилизации группы), иначе вырасти. Индекс — он же лидер собственной группы.
    let idx = t.free_slots.pop().unwrap_or(t.procs.len());
    let proc = Proc {
        space: arch::space_token(root),
        frame,
        state: State::Runnable,
        domain,
        recv_buf: 0,
        recv_cap: 0,
        send_buf: 0,
        send_len: 0,
        send_cap: usize::MAX,
        wake_on_net: false,
        wake_on_key: false,
        pages: 0,
        heap_brk: USER_HEAP_BASE_VA, // куча пуста, пока процесс не попросит SYS_MAP
        args: {
            // argv по умолчанию — только имя программы; SYS_EXEC добавит аргументы вызывающего.
            let mut a = Vec::from(name.as_bytes());
            a.push(0);
            a
        },
        env: Vec::new(),
        start_caps: Vec::new(),
        group: idx, // новый процесс — лидер собственной группы нитей
        retval: 0,
        futex_addr: 0,
        futex_deadline: None,
        linux: false, // по умолчанию — родная личность VOID; spawn_linux_locked поставит true
        // Веха 98: обычный процесс никем не ожидается — слот освободится сразу. `SYS_SPAWN`
        // пометит своего ребёнка зомби и проставит родителя.
        zombie: false,
        parent: usize::MAX,
        exit_code: None,
        lx_fds: Vec::new(),
    };
    if idx == t.procs.len() {
        t.procs.push(proc);
    } else {
        t.procs[idx] = proc; // переиспользуем освободившийся слот (Веха 89)
    }
    idx
}

/// Веха 35 — завести НИТЬ в существующем процессе `leader`: новая запись в таблице,
/// делящая его `space` (адресное пространство → все страницы, включая кучу, общие) и
/// `domain` (c-space → те же capability). Своё у нити — кадр (вход `entry`, стек
/// `stack_top`, `a0`=`arg`), состояние и TLS. НЕ клонирует пространство и НЕ создаёт
/// домен: нити процесса — одна единица защиты (модель POSIX-нитей). Возвращает id нити.
fn create_thread_locked(t: &mut Table, leader: usize, entry: usize, arg: usize, stack_top: usize) -> usize {
    let frame = TrapFrame::new_user(entry, stack_top, arg);
    let (space, domain, group) =
        (t.procs[leader].space, t.procs[leader].domain, t.procs[leader].group);
    let idx = t.procs.len();
    t.procs.push(Proc {
        space,
        frame,
        state: State::Runnable,
        domain,
        recv_buf: 0,
        recv_cap: 0,
        send_buf: 0,
        send_len: 0,
        send_cap: usize::MAX,
        wake_on_net: false,
        wake_on_key: false,
        pages: 0, // не используется у нити: учёт ведёт лидер
        heap_brk: USER_HEAP_BASE_VA, // не используется у нити: куча резолвится у лидера
        args: Vec::new(),
        env: Vec::new(),
        start_caps: Vec::new(),
        group, // та же группа, что у лидера (group лидера == его индекс)
        retval: 0,
        futex_addr: 0,
        futex_deadline: None,
        linux: t.procs[leader].linux, // нить наследует личность лидера (у linux нитей пока нет)
        zombie: false, // ждут НИТЬ через THREAD_JOIN, а не через SYS_WAIT — зомби не нужен
        parent: usize::MAX,
        exit_code: None,
        lx_fds: Vec::new(),
    });
    idx
}

/// Веха 19.2/19.3 — создать процесс из статического ELF64/RISC-V (с Вехи 23 — единственный
/// способ): новое адресное пространство, [`crate::elf::load`] разбирает `elf` и маппит его `PT_LOAD`-
/// сегменты по правам `p_flags` (W^X), точка входа — `e_entry` файла, а не адрес функции в
/// образе ядра. `elf` может быть чем угодно (в т.ч. байтами, прочитанными [[object-model|из
/// store]] по content-id, см. `main::exec_demo`) — загрузчик не предполагает, что они лежат
/// где-то конкретно, копирует их в свежие фреймы процесса. Отказ парсинга/раскладки ELF не
/// заводит процесс и не трогает таблицу — вызывающий получает [`elf::ElfError`].
pub fn spawn_elf(name: &'static str, elf_bytes: &[u8], arg: usize) -> Result<usize, elf::ElfError> {
    let root = new_address_space().ok_or(elf::ElfError::OutOfMemory)?;
    // Верхняя граница адресов ELF — начало региона кучи (Веха 22): раскладка процесса —
    // код/данные ELF ниже USER_HEAP_BASE_VA, куча над ними, стек у самого верха.
    let entry = elf::load(root, elf_bytes, USER_HEAP_BASE_VA)?;
    Ok(create_process(name, root, entry, arg))
}

/// Веха 38 — завести LINUX-процесс из static-PIE ELF (ET_DYN) под УЖЕ взятым замком таблицы.
/// В отличие от [`spawn_elf`] (наш ET_EXEC): образ грузится по базе [`USER_REGION_START`]
/// ([`elf::load_pie`], musl само-релоцируется), а вместо регистров-аргументов строится
/// стартовый стек Linux — `argc/argv/envp/`**`auxv`** ([`crate::linux::build_init_stack`]),
/// по которому musl находит себя, канарейку и (для многопоточных) TLS. Процесс помечается
/// `linux` — его syscall'ы поедут в трансля́тор [`crate::linux`]. `root` — уже созданное
/// адресное пространство (клон ядра + стек). `None` — негодный образ или нет памяти.
fn spawn_linux_locked(
    t: &mut Table,
    pname: &'static str,
    bytes: &[u8],
    root: usize,
    args_blob: Vec<u8>,
) -> Option<usize> {
    let pie = match elf::load_pie(root, bytes, USER_REGION_START, USER_HEAP_BASE_VA) {
        Ok(p) => p,
        Err(e) => {
            vprintln!("  [linux] негодный PIE-образ: {:?}", e);
            return None;
        }
    };

    // Веха 108.4 — ДИНАМИЧЕСКИЙ бинарь: у него в `PT_INTERP` записан абсолютный путь загрузчика
    // (`/nix/store/…/ld-linux-…so`), то есть ровно в тот пакет, который мы уже умеем прочитать.
    // Грузим загрузчик вторым образом и передаём управление ЕМУ: релокации, поиск библиотек и
    // их отображение он делает сам — ядру остаётся дать ему mmap файлов и верный auxv.
    let mut interp_base = 0usize;
    let mut entry = pie.entry;
    if let Some(ipath) = elf::interp_path(bytes) {
        let Some(meta) = crate::lxfs::lookup(ipath) else {
            println!("  [linux] загрузчик не найден: {}", core::str::from_utf8(ipath).unwrap_or("?"));
            return None;
        };
        let Some(idata) = crate::lxfs::read_all(&meta) else {
            println!("  [linux] загрузчик не читается");
            return None;
        };
        match elf::load_pie(root, &idata, INTERP_BASE_VA, USER_HEAP_BASE_VA) {
            Ok(ip) => {
                interp_base = INTERP_BASE_VA;
                entry = ip.entry;
                vprintln!(
                    "  [linux] загрузчик {} → база {:#x}, вход {:#x}",
                    core::str::from_utf8(ipath).unwrap_or("?"),
                    INTERP_BASE_VA,
                    entry
                );
            }
            Err(e) => {
                println!("  [linux] загрузчик негоден: {:?}", e);
                return None;
            }
        }
    }
    // Минимальное окружение Linux — musl это устраивает (PATH/TERM/HOME на будущее для busybox).
    let env: &[u8] = b"PATH=/bin:/usr/bin\0TERM=linux\0HOME=/\0";
    // 16 байт AT_RANDOM (канарейка/ГПСЧ musl) — из счётчика тиков через splitmix64.
    let mut rnd = [0u8; 16];
    let mut seed = arch::now_ticks();
    for chunk in rnd.chunks_mut(8) {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let bytes = seed.to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
    let (block, sp) =
        crate::linux::build_init_stack(USER_STACK_TOP_VA, &args_blob, env, &pie, rnd, interp_base);

    let child = create_process_locked(t, pname, root, entry, 0);
    // Стартовый кадр: вход = pie.entry, sp = вершина построенного стека (там argc). Аргументы
    // Linux читает со стека, а не из регистров — a0/rdi обнулены (musl `_start` их игнорирует).
    t.procs[child].frame = TrapFrame::new_user(entry, sp, 0);
    t.procs[child].linux = true;
    t.procs[child].args = args_blob;
    t.procs[child].env = Vec::from(env);
    copy_to_space(arch::space_root(t.procs[child].space), sp, &block);
    Some(child)
}

/// Домен защиты (c-space) процесса — сюда ядро минтит его начальные capability до [`run`].
pub fn domain(pid: usize) -> cap::DomainId {
    TABLE.lock().procs[pid].domain
}

/// Задать стартовый аргумент (`a0`) процесса до запуска — например, дескриптор capability,
/// который процесс предъявит в первом syscall'е.
pub fn set_arg(pid: usize, a0: usize) {
    TABLE.lock().procs[pid].frame.set_start_arg(0, a0);
}

/// Задать второй стартовый аргумент (`a1`) — когда начальных capability у процесса два
/// (Веха 20: vsh получает эндпоинт персоналии в `a0` и exec-cap на store в `a1`).
pub fn set_arg2(pid: usize, a1: usize) {
    TABLE.lock().procs[pid].frame.set_start_arg(1, a1);
}

/// Веха 30: окружение процесса (блоб `KEY=VAL\0…`) — процесс читает его `SYS_ARGS(1)`,
/// дети наследуют при `SYS_EXEC`.
pub fn set_env(pid: usize, env: &[u8]) {
    TABLE.lock().procs[pid].env = Vec::from(env);
}

/// Веха 92: дописать аргумент в argv процесса до запуска (`SYS_ARGS(0)` его увидит).
/// Так конфиг системы задаёт сервису НАСТРОЙКИ, а не только права: `arg:ip=10.0.2.15/24`.
/// `false` — не влез в [`ARGS_MAX`] (вызывающий предупредит; argv остаётся целым).
pub fn push_arg(pid: usize, arg: &str) -> bool {
    let mut t = TABLE.lock();
    let p = &mut t.procs[pid];
    if p.args.len() + arg.len() + 1 > ARGS_MAX {
        return false;
    }
    p.args.extend_from_slice(arg.as_bytes());
    p.args.push(0);
    true
}

/// Веха 30: добавить стартовый capability (биты дескриптора, уже смещённого В ДОМЕН процесса)
/// в таблицу преоткрытых прав. Процесс перечисляет её `SYS_STARTCAP(i)`, дети наследуют
/// копиями при `SYS_EXEC`. Регистры `a0`/`a1` остаются быстрым путём для первых двух прав.
pub fn push_start_cap(pid: usize, bits: usize) {
    TABLE.lock().procs[pid].start_caps.push(bits);
}

/// Запустить процессы и вернуться сюда, когда все завершатся. Сохраняем контекст ядра в
/// RETURN_CTX и уходим в лончер (как в [[scheduling|context_switch]]-переключении нитей).
///
/// Веха 20.2: сессия стала циклом. Когда готовых процессов не осталось, но кто-то заблокирован
/// в `SYS_READ` (StdinWait) — ядро НЕ выходит, а спит в [`wait_stdin`] до прерывания UART,
/// будит читающих и продолжает сессию. Выход — только когда нет ни готовых, ни ждущих ввода.
pub fn run() {
    let sstatus_sie = arch::irq_save_disable(); // S-mode SIE (ядро НЕ вытесняется: SIE=0 в S-mode)
    let saved_sie = arch::irq_mask_read();
    loop {
        // Первый готовый процесс (может быть не индекс 0: после прошлой сессии часть процессов
        // остаётся Finished/заблокированными).
        let first = {
            let t = TABLE.lock();
            (0..t.procs.len()).find(|&i| t.procs[i].state == State::Runnable)
        };
        if let Some(first) = first {
            // Вытеснение процессов (Веха 16): разрешить ТАЙМЕР (STIE) — он прервёт процесс в
            // U-mode (там sstatus.SIE не действует, гейтит только `sie`), и `handle_user_trap`
            // переключит на следующего. Устройства (SEIE) на время сессии выключаем — их шлюзы
            // работают опросом, а ввод UART скапливается в PLIC как pending до [`wait_stdin`].
            arch::irq_mask_preempt(saved_sie); // таймер вкл, устройства выкл
            timer::arm(); // вооружить первое вытеснение этой сессии
            TABLE.lock().current = first;
            unsafe {
                let launch = Context::new_kernel(proc_enter, trap_top());
                arch::context_switch(addr_of_mut!(RETURN_CTX), addr_of!(launch));
            }
            // ── сюда возвращаемся, когда готовых не осталось ──
            arch::mark_in_kernel();
        }
        // Готовых нет: если кто-то ждёт ввода — поспать до него и продолжить, иначе сессия окончена.
        if !wait_stdin(saved_sie) {
            break;
        }
    }
    arch::irq_mask_write(saved_sie); // вернуть прежние разрешения прерываний
    arch::irq_restore(sstatus_sie);
}

/// Idle-ожидание ввода (Веха 20.2). Если есть процессы в StdinWait — спать (`wfi`), пока
/// прерывание UART не наполнит [[uart|кольцевой буфер]], затем разбудить ждущих и вернуть
/// `true` (их `SYS_READ` рестартует: sepc не двигали). Ждущих нет — `false`.
///
/// Здесь `sstatus.SIE = 0`, поэтому «потерянного пробуждения» нет: `wfi` просыпается от
/// PENDING прерывания независимо от SIE, а сам обработчик мы пускаем коротким окном с SIE=1.
fn wait_stdin(saved_sie: usize) -> bool {
    let (waiting, irq_waiting, deadline): (Vec<usize>, bool, Option<u64>) = {
        let t = TABLE.lock();
        let w = (0..t.procs.len()).filter(|&i| t.procs[i].state == State::StdinWait).collect();
        // Веха 91: ближайший дедлайн спящих по времени (SYS_RECV с таймаутом, futex_wait).
        // Без него простой был бы «до ввода с консоли», и проснуться по времени было бы нечем.
        let d = t.procs.iter().filter_map(|p| p.futex_deadline).min();
        let wake = any_irq_waiting(&t)
            || t.procs.iter().any(|p| p.wake_on_net || p.wake_on_key);
        (w, wake, d)
    };
    if waiting.is_empty() && !irq_waiting && deadline.is_none() {
        return false;
    }
    // Веха 33: уход в простой — естественная точка синка group commit. Под нагрузкой
    // пачки собирает maybe_commit (порог/период), а здесь фиксируется хвост: «echo и
    // ушёл пить чай» не ждёт следующего ввода. Прерывания выключены — virtio опросом.
    crate::object::commit_if_dirty();
    // Только внешние прерывания (SEIE): исполнять некого, таймер (STIE) не нужен. IRQ устройств
    // (консоль, а с Вехи 52 — и userspace-драйвера через IOAPIC) ходят через LAPIC и разбудят HLT.
    //
    // Веха 91 — ИСКЛЮЧЕНИЕ: если кто-то спит ДО СРОКА, таймер нужен, иначе этот срок некому
    // заметить — процесс проспал бы до случайного нажатия клавиши. Это чинит и давний тихий
    // изъян `futex_wait` с таймаутом: на полностью холостой системе он не просыпался вовсе.
    if deadline.is_some() {
        // Веха 91: и таймер (заметить срок), и устройства (кадр разбудит сетевой сервер).
        arch::irq_mask_idle(saved_sie);
    } else {
        arch::irq_mask_stdin(saved_sie);
    }
    while !arch::console_has_input()
        && !USERDRV_IRQ_PENDING.load(Ordering::Relaxed)
        && !NET_IRQ_PENDING.load(Ordering::Relaxed) // Веха 91: кадр разбудит сетевой сервер
        && !deadline.is_some_and(|d| arch::now_ticks() >= d)
    {
        // Спать до прерывания: проснёмся и от PENDING-прерывания при выключенном SIE.
        arch::wait_for_interrupt();
        // Короткое окно с прерываниями — принять trap: контроллер → консоль → кольцевой буфер;
        // IRQ userspace-драйвера выставит USERDRV_IRQ_PENDING (обработчик VEC_USERDRV).
        arch::enable_interrupts();
        arch::irq_save_disable();
    }
    let mut t = TABLE.lock();
    if arch::console_has_input() {
        // Веха 86: момент пробуждения от ввода — хороший источник джиттера (интервалы между
        // нажатиями непредсказуемы для программ). Подмешиваем в пул энтропии.
        crate::random::stir(1);
        for pid in waiting {
            t.procs[pid].state = State::Runnable; // ввод пришёл — будим ждущих READ
        }
        // Веха 103 — и тех, кто спит в `SYS_RECV` с пробуждением по клавише: сообщения не было,
        // но событие есть. Ответ тот же, что при истёкшем сроке, — «запроса нет», и реактор идёт
        // читать клавиатуру сам.
        for i in 0..t.procs.len() {
            if t.procs[i].state == State::RecvWait && t.procs[i].wake_on_key {
                let f = &mut t.procs[i].frame;
                f.set_ret(usize::MAX);
                f.set_ret_at(2, 0);
                f.advance();
                t.procs[i].state = State::Runnable;
                t.procs[i].futex_deadline = None;
                t.procs[i].wake_on_key = false;
            }
        }
    }
    drain_userdrv_irq(&mut t); // Веха 52: пришёл IRQ драйвера — будим ждущих SYS_IRQ_WAIT
    drain_net_irq(&mut t); // Веха 91: приехал кадр — будим сетевой сервер
    wake_futex_timeouts(&mut t); // Веха 91: срок вышел — разбудить спавших по времени
    true
}

/// Разбудить процессы, ждущие в `SYS_EXEC` завершения ребёнка `child` (Веха 20.3): вернуть им
/// код выхода `code`, продвинуть sepc (их ecall завершён) и сделать готовыми.
fn wake_exec_waiters(t: &mut Table, child: usize, code: usize) {
    // Веха 98: код выхода нужен и тем, кто спросит ПОЗЖЕ (`SYS_WAIT` после завершения ребёнка),
    // поэтому он сохраняется, а не только раздаётся ждущим сейчас.
    if child < t.procs.len() {
        t.procs[child].exit_code = Some(code);
    }
    for i in 0..t.procs.len() {
        if t.procs[i].state == State::ExecWait(child) {
            let f = &mut t.procs[i].frame;
            f.set_ret(code);
            f.advance();
            t.procs[i].state = State::Runnable;
        }
    }
}

/// Веха 35 — разбудить присоединяющихся к нити `thread` (THREAD_JOIN), вернув им её
/// `retval`. Зеркало [`wake_exec_waiters`] для нитей вместо процессов.
fn wake_join_waiters(t: &mut Table, thread: usize, retval: usize) {
    for i in 0..t.procs.len() {
        if t.procs[i].state == State::JoinWait(thread) {
            let f = &mut t.procs[i].frame;
            f.set_ret(retval);
            f.advance();
            t.procs[i].state = State::Runnable;
        }
    }
}

/// Веха 35 — разбудить до `count` нитей, спящих в FUTEX_WAIT на слове `uaddr` в
/// пространстве `space`. Пробуждённой нити syscall вернёт 0 (обычное пробуждение).
/// Возвращает число разбуженных (результат FUTEX_WAKE).
fn wake_futex(t: &mut Table, space: usize, uaddr: usize, count: usize) -> usize {
    let mut woken = 0;
    for i in 0..t.procs.len() {
        if woken >= count {
            break;
        }
        if t.procs[i].state == State::FutexWait
            && t.procs[i].futex_addr == uaddr
            && t.procs[i].space == space
        {
            let f = &mut t.procs[i].frame;
            f.set_ret(0); // 0 — разбужены (не таймаут)
            f.advance();
            t.procs[i].state = State::Runnable;
            t.procs[i].futex_deadline = None;
            woken += 1;
        }
    }
    woken
}

/// Веха 35 — разбудить нити, у которых истёк futex-дедлайн (вернуть им «таймаут» = 1).
/// Зовётся из [`resume`] на каждом trap'е из U (гранулярность ~ квант вытеснения);
/// дешёвая проверка при малом числе процессов.
/// Веха 91 - "приехал сетевой кадр". Ставится ИЗ ОБРАБОТЧИКА ПРЕРЫВАНИЯ, поэтому только атомик:
/// замок таблицы процессов там брать нельзя (Веха 89, п.1 - прерывание посреди удержания замка
/// даёт дедлок на одном ядре). Разбирают флаг обычные пути - `resume` и простой.
static NET_IRQ_PENDING: AtomicBool = AtomicBool::new(false);

/// Отметить приход кадра (зовёт драйвер из обработчика прерывания).
pub fn on_net_irq() {
    NET_IRQ_PENDING.store(true, Ordering::Relaxed);
}

/// Веха 91 - разбудить тех, кто спал в `SYS_RECV`, ожидая кадра. `true`, если кадр приходил.
fn drain_net_irq(t: &mut Table) -> bool {
    if !NET_IRQ_PENDING.swap(false, Ordering::Relaxed) {
        return false;
    }
    for i in 0..t.procs.len() {
        if t.procs[i].state == State::RecvWait && t.procs[i].wake_on_net {
            let f = &mut t.procs[i].frame;
            f.set_ret(usize::MAX); // запроса не было - сервер пойдёт качать стек
            f.set_ret_at(2, 0);
            f.advance();
            t.procs[i].state = State::Runnable;
            t.procs[i].futex_deadline = None;
            t.procs[i].wake_on_net = false;
        }
    }
    true
}

fn wake_futex_timeouts(t: &mut Table) {
    let now = arch::now_ticks();
    for i in 0..t.procs.len() {
        let Some(deadline) = t.procs[i].futex_deadline else { continue };
        if now < deadline {
            continue;
        }
        match t.procs[i].state {
            State::FutexWait => {
                let f = &mut t.procs[i].frame;
                f.set_ret(1); // 1 — истёк таймаут (futex_wait вернёт «не разбужен»)
                f.advance();
            }
            // Веха 91: `SYS_RECV` с дедлайном — время вышло, запроса не было.
            State::RecvWait => {
                let f = &mut t.procs[i].frame;
                f.set_ret(usize::MAX);
                f.set_ret_at(2, 0);
                f.advance();
            }
            // Веха 114: `SYS_SLEEP` — срок вышел, это и есть успех.
            State::Sleeping => {
                let f = &mut t.procs[i].frame;
                f.set_ret(0);
                f.advance();
            }
            _ => continue,
        }
        t.procs[i].state = State::Runnable;
        t.procs[i].futex_deadline = None;
    }
}

/// Веха 22.2: page fault из U-mode. Фолт чтения/записи в ленивом диапазоне кучи
/// [`USER_HEAP_BASE_VA`, heap_brk) — выделить обнулённый фрейм, замапить `U|R|W` и повторить
/// инструкцию (sepc не двигаем). Любой другой фолт — включая исполнение кучи (W^X живёт и
/// здесь) и исчерпание фреймов — гибель ПРОЦЕССА, а не ядра: родителю в `SYS_EXEC` уходит MAX.
fn handle_user_fault(t: &mut Table, cur: usize, va: usize, kind: FaultKind) {
    // Веха 35: куча — общая на группу нитей, её граница живёт у лидера (стек нити тоже
    // ленив и лежит в куче процесса, так что фолт стека любой нити резолвится отсюда).
    let (heap_brk, space) = (t.procs[t.procs[cur].group].heap_brk, t.procs[cur].space);
    let lazy = va >= USER_HEAP_BASE_VA && va < heap_brk && kind != FaultKind::Exec;
    let leader = t.procs[cur].group;
    if lazy && t.procs[leader].pages >= page_quota() {
        // Веха 89: аппетит исчерпал квоту — гибнет ИМЕННО этот процесс, соседи и ядро целы.
        println!(
            "  [mm] P{} превысил квоту памяти ({} страниц) — процесс убит (ядро живо)",
            cur,
            page_quota(),
        );
    } else if lazy {
        if let Some(pa) = frame::alloc() {
            let page_va = va & !(PAGE - 1);
            // SAFETY: пространство процесса сейчас активно — после map сбрасываем TLB,
            // иначе повтор инструкции мог бы увидеть старую (пустую) трансляцию.
            let ok = unsafe {
                arch::map(arch::space_root(space), page_va, pa, arch::MAP_R | arch::MAP_W | arch::MAP_U)
            };
            if ok {
                t.procs[leader].pages += 1;
                arch::flush_tlb();
                vprintln!("  [mm] P{} +страница {:#x} (ленивый фолт кучи)", cur, page_va);
                return; // sepc не тронут — инструкция повторится по замапленной странице
            }
            // Веха 89: памяти не хватило под ТАБЛИЦУ — фрейм назад и вниз, к общему пути
            // «процесс убит»: гибнет программа, которой не хватило памяти, а не ядро.
            frame::free(pa);
        }
        vprintln!("  [mm] P{} фолт кучи {:#x}: памяти не хватило — процесс убит", cur, va);
    } else {
        vprintln!(
            "  [mm] P{} page fault ({}) @ {:#x} вне кучи — процесс убит (ядро живо)",
            cur, kind.name(), va,
        );
    }
    t.procs[cur].state = State::Finished;
    wake_exec_waiters(t, cur, usize::MAX);
    if let Some(n) = t.next_runnable(cur) {
        t.current = n;
    }
}

/// Веха 22.2: доотобразить ленивые страницы кучи ПЕРЕД тем, как ядро само тронет буфер
/// процесса в шлюзе (`OBJ_GET`/`OBJ_PUT`): фолт из S-mode мы не переживаем (fatal), поэтому
/// «ленивость» для ядра снимается заранее. Буферы вне кучи (стек, данные ELF) замаплены и так.
/// `false` — диапазон в куче, но фреймы кончились (шлюзу следует отказать).
fn ensure_heap_range(t: &Table, pid: usize, va: usize, len: usize) -> bool {
    // Веха 35: граница кучи — у лидера группы (нити делят кучу процесса).
    if len == 0 || va < USER_HEAP_BASE_VA || va.saturating_add(len) > t.procs[t.procs[pid].group].heap_brk {
        return true; // не куча — обычные (уже отображённые) страницы
    }
    let root = arch::space_root(t.procs[pid].space);
    let mut page = va & !(PAGE - 1);
    while page < va + len {
        if arch::translate(root, page).is_none() {
            if t.procs[t.procs[pid].group].pages >= page_quota() {
                return false; // Веха 89: квота исчерпана — шлюз откажет, процесс жив
            }
            let Some(pa) = frame::alloc() else { return false };
            if !unsafe { arch::map(root, page, pa, arch::MAP_R | arch::MAP_W | arch::MAP_U) } {
                frame::free(pa);
                return false; // Веха 89: нет памяти под таблицу — шлюз честно откажет
            }
            arch::flush_tlb();
            vprintln!("  [mm] P{} +страница {:#x} (доотображение под шлюз)", pid, page);
        }
        page += PAGE;
    }
    true
}

/// Веха 89 — **КВОТА СТРАНИЦ на группу нитей**. Раньше её не было вовсе: программа, которая
/// в цикле трогает новые страницы кучи, забирала всю RAM машины, и следующим падал не автор
/// аппетита, а тот, кому не досталось, — вплоть до ядра.
///
/// Квота относительная (четверть рабочей RAM, но не меньше 8 МиБ): на 128-МиБ QEMU это 32 МиБ,
/// на реальной машине с гигабайтами — гигабайты. Абсолютная константа тут врала бы в обе
/// стороны. Считается один раз: карта памяти после загрузки не меняется.
fn page_quota() -> usize {
    const MIN: usize = 8 * 1024 * 1024;
    (crate::frame::usable_bytes() / 4).max(MIN) / PAGE
}

/// Лончер: возобновить текущий (первый) процесс.
extern "C" fn proc_enter() -> ! {
    let (frame, space) = {
        let t = TABLE.lock();
        let c = t.current;
        (t.procs[c].frame, t.procs[c].space)
    };
    unsafe { arch::enter_user(&frame, space, trap_top()) }
}

// ─── обработка trap'ов из U-mode ──────────────────────────────────────────────

/// Обработать trap из U-mode (уже классифицированный архом в [`UserTrap`], Веха 24) и
/// возобновить нужный процесс. Не возвращается.
pub fn handle_user_trap(frame: &mut TrapFrame, trap: UserTrap) -> ! {
    {
        let mut t = TABLE.lock();
        let cur = t.current;
        // Веха 35: TLS-указатель нити (x86 fsbase) НЕ спасается стабом trap'а — во «свежем»
        // кадре он мусор. Переносим его из прошлого кадра ДО перезаписи (riscv — no-op: tp
        // в GPR). SYS_SET_TLS, если это он, перепишет уже верным новым значением.
        let prev = t.procs[cur].frame;
        t.procs[cur].frame = *frame; // сохранить состояние текущего процесса
        t.procs[cur].frame.carry_tls_from(&prev);
        // Веха 36: FP/SSE-состояние стаб тоже не спасает (x86: fx-слот — мусор со стека) —
        // снять живые регистры CPU, они принадлежат затрапившему (riscv — no-op: f-регистры
        // в кадре со времён Вехи 32).
        t.procs[cur].frame.save_fp();
        match trap {
            // Веха 38: linux-процесс — его ecall (riscv) уходит в трансля́тор [`crate::linux`],
            // а не в ABI VOID (на x86 linux зовёт ядро `syscall`'ом → ветка Unknown ниже).
            UserTrap::Syscall => {
                if t.procs[cur].linux {
                    linux_syscall(&mut t, cur)
                } else {
                    syscall(&mut t, cur)
                }
            }
            // Веха 22.2: page fault из U-mode — ленивая страница кучи или гибель процесса.
            UserTrap::PageFault { va, kind } => handle_user_fault(&mut t, cur, va, kind),
            UserTrap::TimerTick => {
                // Вытеснение по таймеру: перевзвести и уступить следующему готовому. Текущий
                // остаётся Runnable, его кадр сохранён; PC НЕ двигаем — продолжит с прерванного.
                timer::preempt_tick();
                if let Some(n) = t.next_runnable(cur) {
                    t.current = n;
                }
            }
            UserTrap::Unknown(code) => {
                // Веха 38: musl x86-64 зовёт ядро инструкцией `syscall` (0F 05); мы её НЕ
                // включили (EFER.SCE=0), поэтому она приходит как #UD (вектор 6). Для
                // linux-процесса распознаём опкод и обслуживаем как syscall (rip на инструкции;
                // linux_syscall перешагнёт её skip_syscall_insn'ом при завершении).
                if t.procs[cur].linux && is_linux_syscall_insn(&t, cur) {
                    linux_syscall(&mut t, cur);
                } else {
                    println!(
                        "  [proc] неожиданный trap из U (код {:#x}) @ pc={:#x} — процесс завершён",
                        code,
                        t.procs[cur].frame.user_pc()
                    );
                    t.procs[cur].state = State::Finished;
                    wake_exec_waiters(&mut t, cur, usize::MAX); // упавший ребёнок = MAX родителю
                    if let Some(n) = t.next_runnable(cur) {
                        t.current = n;
                    }
                }
            }
        }
    }
    resume();
}

/// Веха 46 — маркер «пространство этой группы уже освобождено» в поле `space`. Валидным
/// токеном (satp/CR3) `usize::MAX` быть не может, поэтому годится как часовой.
const RECLAIMED: usize = usize::MAX;

/// Веха 46 — собрать корни адресных пространств тех групп, где ВСЕ нити уже `Finished`, и
/// пометить их `space = RECLAIMED` (чтобы не освободить дважды и не тронуть устаревший корень).
/// Единая точка для всех путей гибели (SYS_EXIT, linux exit_group, page fault, лишняя нить):
/// группа освобождается ровно тогда, когда в ней не осталось живых нитей. Возвращает корни —
/// САМО освобождение делает [`resume`] уже под живым пространством (рушить таблицы под
/// активным satp/CR3 нельзя).
fn reclaim_dead_spaces(t: &mut Table) -> Vec<usize> {
    let mut roots = Vec::new();
    let n = t.procs.len();
    for leader in 0..n {
        if t.procs[leader].group != leader || t.procs[leader].space == RECLAIMED {
            continue; // только лидеры групп и только ещё не освобождённые
        }
        let all_dead =
            (0..n).all(|i| t.procs[i].group != leader || t.procs[i].state == State::Finished);
        if !all_dead {
            continue;
        }
        roots.push(arch::space_root(t.procs[leader].space));
        for i in 0..n {
            if t.procs[i].group == leader {
                t.procs[i].space = RECLAIMED;
                // Веха 89 — сперва ОТОЗВАТЬ права, указывающие на этот номер (эндпоинты и
                // reply), и только потом отдать слот под переиспользование: иначе устаревший
                // cap начал бы адресовать чужой, новый процесс.
                cap::revoke_process(i);
                // Веха 97 — умер владелец ЭКРАНА: вернуть экран ядру. Без этого терминал,
                // упавший или вышедший, оставлял бы систему немой — ядро продолжало бы считать
                // экран занятым и печатать в один serial.
                if arch::video_owner() == Some(i) {
                    arch::video_take_back();
                    println!("  [видео] владелец экрана P{} завершился — экран вернулся ядру", i);
                }
                // Веха 98 — слот ЗОМБИ придержан: родитель ещё не забрал код выхода
                // (`SYS_WAIT`). Отдать его сейчас значит подсунуть ожидающему чужой процесс.
                // Текущий слот не отдаём по другой причине: `resume` ещё читает из него кадр.
                if i != t.current && !t.procs[i].zombie {
                    t.free_slots.push(i);
                }
            }
        }
    }
    roots
}

/// Возобновить текущий процесс (или, если он не готов, следующий готовый). Если готовых нет
/// (все завершены или заблокированы) — вернуться в ядро (в [`run`]). Не возвращается.
fn resume() -> ! {
    // Веха 33: политика group commit живёт здесь — каждый trap из U (включая
    // вытеснение таймером каждые ~10 мс) проходит через resume, замки в этот
    // момент не держатся. Пока store чист — это одна проверка счётчика.
    crate::object::maybe_commit();
    let mut t = TABLE.lock();
    // Веха 35: разбудить futex-ждунов с истёкшим дедлайном (проверка на каждом trap'е
    // из U — гранулярность ~кванта вытеснения; для wait_timeout/park_timeout этого хватает).
    wake_futex_timeouts(&mut t);
    // Веха 52: пришёл IRQ userspace-драйвера — разбудить спящих в SYS_IRQ_WAIT.
    drain_userdrv_irq(&mut t);
    drain_net_irq(&mut t); // Веха 91: приехал кадр — разбудить сетевой сервер
    let c = t.current;
    let chosen = if t.procs[c].state == State::Runnable {
        Some(c)
    } else {
        t.next_runnable(c)
    };
    match chosen {
        Some(n) => {
            t.current = n;
            let frame = t.procs[n].frame;
            let space = t.procs[n].space;
            // Веха 46: вернуть фреймы групп, что полностью завершились (страницы, таблицы, корень).
            let dead = reclaim_dead_spaces(&mut t);
            drop(t);
            unsafe {
                if !dead.is_empty() {
                    // Переключиться на ЖИВОЕ пространство ПЕРЕД сносом мёртвых: их таблицы нельзя
                    // рушить под активным satp/CR3 (MMU потом читала бы их из списка свободных).
                    arch::mm_enable(arch::space_root(space));
                    for root in dead {
                        arch::free_address_space(root);
                    }
                }
                arch::enter_user(&frame, space, trap_top())
            }
        }
        None => {
            drop(t);
            unsafe {
                let mut discard = Context::default();
                arch::context_switch(addr_of_mut!(discard), addr_of!(RETURN_CTX));
            }
            loop {} // не достигается
        }
    }
}

/// Диспетчер syscall'ов. Номер в `a7`, аргументы в `a0..`, результат в `a0`. Работает прямо
/// с таблицей: IPC-вызовы затрагивают состояния/кадры ДРУГИХ процессов и выбор `current`.
fn syscall(t: &mut Table, cur: usize) {
    let num = t.procs[cur].frame.syscall_num();
    match num {
        // SYS_WRITE(ptr, len): напечатать буфер процесса (ядро читает U-память, SUM=1).
        1 => {
            let (ptr, len) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1))
            };
            // Веха 23: буфер может лежать в ленивой куче — доотобразить до чтения ядром.
            let result = if ensure_heap_range(t, cur, ptr, len) {
                let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
                crate::print!("{}", core::str::from_utf8(bytes).unwrap_or("<?>"));
                len
            } else {
                usize::MAX
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_EXIT(code): завершить процесс, уступить следующему готовому. Если кто-то ждёт
        // этот процесс в SYS_EXEC (Веха 20.3) — разбудить, вернув ему код выхода.
        2 => {
            let code = t.procs[cur].frame.arg(0);
            // Веха 35: процесс уходит ЦЕЛИКОМ — все нити группы становятся Finished
            // (семантика exit()/возврата из main: прочие нити не переживают процесс).
            // Родитель ждал в SYS_EXEC ЛИДЕРА (его вернул SYS_EXEC) — будим по лидеру.
            let leader = t.procs[cur].group;
            vprintln!("  [proc] P{} SYS_EXIT({}) — процесс P{} (все нити группы)", cur, code, leader);
            for i in 0..t.procs.len() {
                if t.procs[i].group == leader {
                    t.procs[i].state = State::Finished;
                }
            }
            wake_exec_waiters(t, leader, code);
            if let Some(n) = t.next_runnable(cur) {
                t.current = n;
            }
        }
        // SYS_YIELD: уступить следующему готовому.
        3 => {
            t.procs[cur].frame.advance();
            if let Some(n) = t.next_runnable(cur) {
                t.current = n;
            }
        }
        // SYS_RECV(recv_buf, recv_cap, nonblock) -> (a0=op, a1=reply-право, a2=длина запроса,
        // a3=право из сообщения, a4=НОМЕР ОТПРАВИТЕЛЯ — Веха 99). Комментарий выше долго
        // утверждал, что отправитель в a1, а там всегда было reply-право; теперь отправитель
        // есть на самом деле. Он нужен серверу, который ведёт по клиенту СОСТОЯНИЕ: мультиплексор
        // обязан понять, в какую панель лёг вывод, а reply-право для этого не годится — оно
        // одноразовое и у каждого запроса своё.
        // a3=принятое право|MAX — Веха 21.1). Приняв запрос, копируем его полезную нагрузку из
        // буфера клиента в recv_buf; если клиент передал capability — она уже скопирована в домен
        // сервера (deliver_request), в a3 — её дескриптор.
        //
        // Нет запроса, три режима (arg2): 0 — блокировка (RecvWait), recv_buf/cap сохранены, чтобы
        // доставка позже скопировала в них; 1 — немедленный возврат с `op == usize::MAX`
        // (Веха 90); 2 — блокировка ДО ДЕДЛАЙНА (arg3, тики), Веха 91: возврат с `op == MAX`,
        // когда время вышло. Третий режим и есть «сон вместо опроса» для реактора: он спит,
        // пока не придёт запрос или не настанет момент, который назвал сам стек (`poll_at`).
        //
        // Зачем неблокирующий приём. Сетевому серверу нужно ОДНОВРЕМЕННО прокачивать стек
        // (входящие кадры, таймеры ретрансмиссии) и отвечать клиентам. Пока `SYS_RECV` умел
        // только блокировать, сервер стоял в нём и стек не тикал. Альтернативой были две нити с
        // общим состоянием стека под мьютексом — но smoltcp насквозь `&mut`, и такой мьютекс
        // сериализовал бы всё равно всё, добавив лишь способы ошибиться. Один реактор проще и
        // честнее; ждать СРАЗУ кадра и IPC-сообщения (вместо опроса) научит Веха 91.
        4 => {
            let (rbuf, rcap, mode, timeout) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3))
            };
            t.procs[cur].recv_buf = rbuf;
            t.procs[cur].recv_cap = rcap;
            if let Some(pos) = t.mailbox.iter().position(|&(_, to, _)| to == cur) {
                let (from, _to, op) = t.mailbox.remove(pos);
                let (n, tcap) = deliver_request(t, from, cur);
                // Веха 101 — сколько байт запроса ДОШЛО, узнаёт отправитель (третье значение его
                // `SYS_CALL`). Он ещё спит в ReplyWait; ответ выставит a0/a1 и двинет sepc, a2
                // при этом сохранится.
                t.procs[from].frame.set_ret_at(2, n);
                let rc = cap::mint(t.procs[cur].domain, cap::Target::Reply(from), Rights::SEND);
                let f = &mut t.procs[cur].frame;
                f.set_ret(op);
                f.set_ret_at(1, rc.bits() as usize);
                f.set_ret_at(2, n);
                f.set_ret_at(3, tcap);
                f.set_ret_at(4, from);
                f.advance();
            } else if mode == 1 {
                let f = &mut t.procs[cur].frame;
                f.set_ret(usize::MAX); // «запросов нет» — вызывающий занимается своими делами
                f.set_ret_at(2, 0);
                f.advance();
            } else {
                // Веха 91: дедлайн живёт в том же поле, что у futex - механика пробуждения по
                // времени уже есть (`wake_futex_timeouts`), заводить вторую незачем.
                t.procs[cur].futex_deadline =
                    (mode >= 2).then(|| arch::now_ticks().wrapping_add(timeout as u64));
                // Режим 3: разбудить ещё и приходом кадра - тогда сервер реагирует на сеть
                // мгновенно, а не на ближайшем тике таймера.
                t.procs[cur].wake_on_net = mode == 3;
                // Веха 103 — режим 4: разбудить и по клавише (реактор терминала).
                t.procs[cur].wake_on_key = mode == 4;
                t.procs[cur].state = State::RecvWait; // sepc не двигаем: доставка сделает это
                if let Some(n) = t.next_runnable(cur) {
                    t.current = n;
                }
            }
        }
        // SYS_CALL(ep_cap, op, send_buf, send_len, recv_buf, recv_cap, a6=cap|MAX) ->
        // (a0 = число байт ответа | MAX, a1 = право из ответа | MAX). `ep_cap` — дескриптор
        // эндпоинта в c-space процесса; ядро резолвит его в id сервера. `send_buf`/`send_len` —
        // полезная нагрузка запроса (копируется серверу при доставке). Веха 21.1: `a6` —
        // capability, передаваемая в сообщении (нужен `GRANT` на неё — проверяется ЗДЕСЬ,
        // до отправки); сервер получит её копию в своём домене (a3 его RECV). Ответ сервера
        // тоже может нести право — его дескриптор вернётся в a1. Отправить и ждать (блокируется).
        5 => {
            let (ecap, op, sbuf, slen, rbuf, rcap) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3), f.arg(4), f.arg(5))
            };
            let scap = t.procs[cur].frame.arg(6); // право в сообщении (MAX — нет)
            let dom = t.procs[cur].domain;
            // Передаваемое право проверяем ДО отправки: нет GRANT — весь CALL отклонён.
            if scap != usize::MAX {
                let ok = cap::rights(dom, Cap::from_bits(scap as u64))
                    .map_or(false, |r| r.contains(Rights::GRANT));
                if !ok {
                    vprintln!("  [cap] P{} CALL отклонён: нет права GRANT на передаваемую capability", cur);
                    let f = &mut t.procs[cur].frame;
                    f.set_ret(usize::MAX);
                    f.advance();
                    return;
                }
            }
            match cap::endpoint(dom, Cap::from_bits(ecap as u64)) {
                Ok(dest) => {
                    vprintln!("  [ipc] P{} CALL P{} (по cap) op={} ({} байт)", cur, dest, op, slen);
                    t.procs[cur].recv_buf = rbuf;
                    t.procs[cur].recv_cap = rcap;
                    t.procs[cur].send_buf = sbuf;
                    t.procs[cur].send_len = slen;
                    t.procs[cur].send_cap = scap;
                    if dest < t.procs.len() && t.procs[dest].state == State::RecvWait {
                        // получатель ждёт в RECV — доставить нагрузку в его буфер и разбудить.
                        // Выдать серверу одноразовый reply-cap на этого клиента (см. [[reply-capability]]).
                        let (n, tcap) = deliver_request(t, cur, dest);
                        t.procs[cur].frame.set_ret_at(2, n); // Веха 101: доставлено байт запроса
                        let rc = cap::mint(t.procs[dest].domain, cap::Target::Reply(cur), Rights::SEND);
                        let df = &mut t.procs[dest].frame;
                        df.set_ret(op);
                        df.set_ret_at(1, rc.bits() as usize);
                        df.set_ret_at(2, n);
                        df.set_ret_at(3, tcap);
                        df.set_ret_at(4, cur);
                        df.advance();
                        t.procs[dest].state = State::Runnable;
                    } else {
                        t.mailbox.push((cur, dest, op)); // нагрузку скопируют при его RECV
                    }
                    t.procs[cur].state = State::ReplyWait; // sepc двинет доставка ответа
                    if let Some(n) = t.next_runnable(cur) {
                        t.current = n;
                    }
                }
                Err(e) => {
                    // Нет валидного cap на эндпоинт — отказ. Процесс не блокируется, продолжает.
                    vprintln!("  [ipc] P{} CALL отклонён: {:?}  ← нет capability на эндпоинт", cur, e);
                    let f = &mut t.procs[cur].frame;
                    f.set_ret(usize::MAX);
                    f.advance();
                }
            }
        }
        // SYS_REPLY(reply_cap, src_buf, len, a3=cap|MAX) -> 0/MAX: ответить вызвавшему клиенту,
        // передав `len` байт из своего буфера в его приёмный буфер, и разбудить его. `reply_cap` —
        // одноразовый cap на клиента, выданный при `RECV`; ядро резолвит его в id клиента и по
        // исполнении отзывает. Подделать/переиспользовать нельзя (см. [[reply-capability]]).
        // Веха 21.1: `a3` — право, передаваемое С ОТВЕТОМ (нужен `GRANT`); клиент получит его
        // дескриптор в a1 своего CALL. Паттерн «сервер-раздатчик»: клиент просит доступ,
        // сервер отвечает УРЕЗАННОЙ копией своего права (CAP_DERIVE → REPLY).
        6 => {
            let (rcap, src, len) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            let scap = t.procs[cur].frame.arg(3); // право в ответе (MAX — нет)
            let dom = t.procs[cur].domain;
            // Как в CALL: передаваемое право проверяем до доставки — нет GRANT, нет REPLY.
            if scap != usize::MAX {
                let ok = cap::rights(dom, Cap::from_bits(scap as u64))
                    .map_or(false, |r| r.contains(Rights::GRANT));
                if !ok {
                    vprintln!("  [cap] P{} REPLY отклонён: нет права GRANT на передаваемую capability", cur);
                    let f = &mut t.procs[cur].frame;
                    f.set_ret(usize::MAX);
                    f.advance();
                    return;
                }
            }
            let result = match cap::reply_endpoint(dom, Cap::from_bits(rcap as u64)) {
                Ok(dest) => {
                    vprintln!("  [ipc] P{} REPLY P{} ({} байт)", cur, dest, len);
                    if dest < t.procs.len() && t.procs[dest].state == State::ReplyWait {
                        let mut n = len.min(t.procs[dest].recv_cap);
                        // Веха 23: оба конца могут лежать в ленивых кучах — доотобразить: свой
                        // буфер ядро читает напрямую (S-фолт фатален), приёмник клиента
                        // транслируется постранично (немапленное молча пропало бы).
                        if n > 0
                            && !(ensure_heap_range(t, cur, src, n)
                                && ensure_heap_range(t, dest, t.procs[dest].recv_buf, n))
                        {
                            n = 0; // фреймы кончились — честнее не доставить ничего
                        }
                        // Читаем из текущего (сервера) по SUM=1; пишем в пространство клиента через
                        // трансляцию его таблицы (физ. адрес отображён в ядре идентично).
                        // Пустой ответ (n=0, напр. только право — Веха 21) не строит слайс:
                        // from_raw_parts из нулевого указателя — UB даже при нулевой длине.
                        if n > 0 {
                            let src_slice = unsafe { core::slice::from_raw_parts(src as *const u8, n) };
                            let droot = arch::space_root(t.procs[dest].space);
                            let dbuf = t.procs[dest].recv_buf;
                            copy_to_space(droot, dbuf, src_slice);
                        }
                        // Право в ответе: скопировать в домен клиента; его дескриптор — в a1 CALL.
                        let mut tcap = usize::MAX;
                        if scap != usize::MAX {
                            if let Ok(nc) = cap::grant(
                                dom,
                                Cap::from_bits(scap as u64),
                                t.procs[dest].domain,
                                Rights(u32::MAX),
                            ) {
                                tcap = nc.bits() as usize;
                                vprintln!(
                                    "  [cap] P{} → P{}: право [{}] передано в ответе (grant по IPC)",
                                    cur, dest,
                                    cap::rights_str(cap::rights(t.procs[dest].domain, nc).unwrap_or(Rights::NONE)),
                                );
                                cap::persist(); // передача права = чекпойнт c-space (Веха 21.3)
                            }
                        }
                        let df = &mut t.procs[dest].frame;
                        df.set_ret(n); // клиентский CALL вернёт число принятых байт
                        df.set_ret_at(1, tcap); // и дескриптор полученного права (MAX — не было)
                        // Веха 101 — и сколько байт сервер ХОТЕЛ отдать: иначе «ответ ровно такой»
                        // и «мой буфер оказался мал» с виду одно и то же (та же слепота, что была
                        // у запроса). Четвёртым значением, чтобы не трогать прежние два.
                        df.set_ret_at(3, len);
                        df.advance();
                        t.procs[dest].state = State::Runnable;
                    }
                    let _ = cap::revoke(dom, Cap::from_bits(rcap as u64)); // одноразовость
                    0
                }
                Err(e) => {
                    vprintln!("  [ipc] P{} REPLY отклонён: {:?}  ← нет reply-capability", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance(); // сервер продолжает (остаётся current)
        }
        // SYS_BLK_READ(dev_cap, sector, buf): шлюз к диску ПОД ЗАЩИТОЙ capability. Без валидного
        // cap на устройство (право READ) — отказ, даже если процесс знает номер сектора. DMA идёт
        // в ЯДЕРНЫЙ буфер (страницы процесса не identity-mapped), затем копируем вызывающему (SUM=1).
        7 => {
            let (dcap, sector, ubuf) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::device(dom, Cap::from_bits(dcap as u64), Rights::READ) {
                // Веха 23: приёмный буфер может лежать в ленивой куче — доотобразить.
                Ok(cap::Device::Block) if ensure_heap_range(t, cur, ubuf, 512) => {
                    vprintln!("  [blk] P{} SYS_BLK_READ сектор {} (по cap)", cur, sector);
                    let mut tmp = [0u8; 512];
                    let ok = crate::virtio_blk::read(sector as u64, &mut tmp);
                    if ok {
                        let dst = unsafe { core::slice::from_raw_parts_mut(ubuf as *mut u8, 512) };
                        dst.copy_from_slice(&tmp);
                    }
                    if ok { 0 } else { usize::MAX }
                }
                Ok(_) => usize::MAX, // право есть, а фреймов под ленивый буфер нет
                Err(e) => {
                    vprintln!("  [blk] P{} SYS_BLK_READ отклонён: {:?}  ← нет capability на устройство", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_OBJ_PUT(store_cap, buf, len, id_out) -> 0/MAX: сохранить значение в объектный
        // [[object-model|store]] (нужен cap на store с правом WRITE) и записать 32-байтный
        // content-id в id_out. Буферы читаются/пишутся в пространстве вызывающего (он current, SUM=1).
        8 => {
            let (scap, buf, len, idout) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::WRITE) {
                // Веха 22.2: буфер (и id_out — Веха 23) может лежать в ленивой куче —
                // доотобразить до того, как ядро его тронет.
                Ok(()) if ensure_heap_range(t, cur, buf, len)
                    && ensure_heap_range(t, cur, idout, 32) => {
                    let bytes = unsafe { core::slice::from_raw_parts(buf as *const u8, len) };
                    // Веха 104 — нехватка памяти ядра здесь ОТКАЗ, а не паника: размер задаёт
                    // программа (а в пакетной фазе — сеть и чужой архив), и падать всей системой
                    // на чужой цифре недопустимо.
                    match crate::object::try_put(bytes) {
                        Some(id) => {
                            let out =
                                unsafe { core::slice::from_raw_parts_mut(idout as *mut u8, 32) };
                            out.copy_from_slice(&id.0);
                            vprintln!("  [obj] P{} OBJ_PUT {} байт → content-id (по cap)", cur, len);
                            0
                        }
                        None => {
                            println!("  [obj] P{} OBJ_PUT {} байт: НЕ ХВАТИЛО памяти ядра", cur, len);
                            usize::MAX
                        }
                    }
                }
                Ok(()) => usize::MAX, // куча есть, а фреймов нет
                Err(e) => {
                    vprintln!("  [obj] P{} OBJ_PUT отклонён: {:?}  ← нет capability на store", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_OBJ_GET(store_cap, id_ptr, out_buf, out_cap) -> длина (0 — нет; MAX — отказ):
        // прочитать значение по 32-байтному content-id (нужен cap на store с правом READ).
        //
        // Веха 114 — ВТОРЫМ значением возвращается НАСТОЯЩАЯ длина объекта. Без неё «объект ровно
        // с буфер» и «объект не влез» выглядели одинаково, и читатели росли удвоением буфера,
        // перечитывая объект по нескольку раз. Второе значение прежних читателей не задевает
        // (они берут только первое) — та же уловка, которой Веха 101 добавила «сколько хотели
        // отдать» к IPC.
        9 => {
            let (scap, idp, obuf, ocap) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3))
            };
            let dom = t.procs[cur].domain;
            let (result, whole) = match cap::store(dom, Cap::from_bits(scap as u64), Rights::READ) {
                // Веха 22.2: приёмный буфер (и id_ptr — Веха 23) может лежать в ленивой куче —
                // доотобразить до записи ядром (весь ocap: лениво он выделился бы всё равно).
                Ok(()) if ensure_heap_range(t, cur, obuf, ocap)
                    && ensure_heap_range(t, cur, idp, 32) => {
                    let mut id = [0u8; 32];
                    let src = unsafe { core::slice::from_raw_parts(idp as *const u8, 32) };
                    id.copy_from_slice(src);
                    let (n, whole) = crate::object::with(&ContentId(id), |b| match b {
                        Some(bytes) => {
                            let m = bytes.len().min(ocap);
                            let out = unsafe { core::slice::from_raw_parts_mut(obuf as *mut u8, m) };
                            out.copy_from_slice(&bytes[..m]);
                            (m, bytes.len())
                        }
                        None => (0, 0),
                    });
                    vprintln!("  [obj] P{} OBJ_GET → {} байт из {} (по cap)", cur, n, whole);
                    (n, whole)
                }
                Ok(()) => (usize::MAX, 0), // куча есть, а фреймов нет
                Err(e) => {
                    vprintln!("  [obj] P{} OBJ_GET отклонён: {:?}  ← нет capability на store", cur, e);
                    (usize::MAX, 0)
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.set_ret_at(1, whole);
            f.advance();
        }
        // SYS_OBJ_SET_ROOT(store_cap, name_ptr, name_len, id_ptr) -> 0/MAX: привязать именованный
        // корень к значению (нужен `WRITE`). Так объект переживает перезагрузку ([[persistent-store]]).
        10 => {
            let (scap, nptr, nlen, idp) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::WRITE) {
                // Веха 23: имя и id могут лежать в ленивой куче — доотобразить до чтения ядром.
                Ok(()) if ensure_heap_range(t, cur, nptr, nlen)
                    && ensure_heap_range(t, cur, idp, 32) => {
                    let name_bytes = unsafe { core::slice::from_raw_parts(nptr as *const u8, nlen) };
                    let mut id = [0u8; 32];
                    let src = unsafe { core::slice::from_raw_parts(idp as *const u8, 32) };
                    id.copy_from_slice(src);
                    match core::str::from_utf8(name_bytes) {
                        Ok(name) => {
                            crate::object::set_root(name, ContentId(id));
                            // Веха 33: чекпойнт-на-каждый-чих сменился group commit —
                            // операция лишь копит счётчик, фиксацию делает политика
                            // ([`object::maybe_commit`] в resume(): порог или ~2 с).
                            vprintln!("  [obj] P{} OBJ_SET_ROOT '{}' (по cap, в пачку)", cur, name);
                            0
                        }
                        Err(_) => usize::MAX,
                    }
                }
                Ok(()) => usize::MAX, // куча есть, а фреймов нет
                Err(e) => {
                    vprintln!("  [obj] P{} OBJ_SET_ROOT отклонён: {:?}  ← нет capability на store", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_OBJ_GET_ROOT(store_cap, name_ptr, name_len, id_out) -> 32 (есть) / 0 (нет) / MAX
        // (отказ): узнать content-id именованного корня (нужен `READ`).
        11 => {
            let (scap, nptr, nlen, idout) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::READ) {
                // Веха 23: имя и id_out могут лежать в ленивой куче — доотобразить.
                Ok(()) if ensure_heap_range(t, cur, nptr, nlen)
                    && ensure_heap_range(t, cur, idout, 32) => {
                    let name_bytes = unsafe { core::slice::from_raw_parts(nptr as *const u8, nlen) };
                    match core::str::from_utf8(name_bytes) {
                        Ok(name) => match crate::object::root(name) {
                            Some(id) => {
                                let out = unsafe { core::slice::from_raw_parts_mut(idout as *mut u8, 32) };
                                out.copy_from_slice(&id.0);
                                vprintln!("  [obj] P{} OBJ_GET_ROOT '{}' → есть (по cap)", cur, name);
                                32
                            }
                            None => {
                                vprintln!("  [obj] P{} OBJ_GET_ROOT '{}' → нет (по cap)", cur, name);
                                0
                            }
                        },
                        Err(_) => usize::MAX,
                    }
                }
                Ok(()) => usize::MAX, // куча есть, а фреймов нет
                Err(e) => {
                    vprintln!("  [obj] P{} OBJ_GET_ROOT отклонён: {:?}  ← нет capability на store", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_BLK_WRITE(dev_cap, sector, buf, len) -> 0/MAX: записать сектор ПОД ЗАЩИТОЙ capability
        // (нужен `WRITE` на устройство). Данные копируем из буфера вызывающего (SUM=1) в ЯДЕРНЫЙ
        // буфер (страницы процесса не identity-mapped для DMA), недостающее до сектора — нулями.
        12 => {
            let (dcap, sector, ubuf, len) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::device(dom, Cap::from_bits(dcap as u64), Rights::WRITE) {
                // Веха 23: буфер данных может лежать в ленивой куче — доотобразить.
                Ok(cap::Device::Block) if ensure_heap_range(t, cur, ubuf, len.min(512)) => {
                    let mut tmp = [0u8; 512];
                    let n = len.min(512);
                    let src = unsafe { core::slice::from_raw_parts(ubuf as *const u8, n) };
                    tmp[..n].copy_from_slice(src);
                    let ok = crate::virtio_blk::write(sector as u64, &tmp);
                    vprintln!("  [blk] P{} SYS_BLK_WRITE сектор {} ({} байт, по cap)", cur, sector, n);
                    if ok { 0 } else { usize::MAX }
                }
                Ok(_) => usize::MAX, // право есть, а фреймов под ленивый буфер нет
                Err(e) => {
                    vprintln!("  [blk] P{} SYS_BLK_WRITE отклонён: {:?}  ← нет capability (WRITE) на устройство", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_OBJ_DEL_ROOT(store_cap, name_ptr, name_len) -> 0 (снят) / 1 (не было) / MAX (отказ):
        // отвязать именованный корень (нужен `WRITE`). Объект уходит в GC, если больше ни на что не
        // сослан — это делает `unlink` в персоналии честным (Веха 18.3).
        13 => {
            let (scap, nptr, nlen) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::WRITE) {
                // Веха 23: имя может лежать в ленивой куче — доотобразить до чтения ядром.
                Ok(()) if ensure_heap_range(t, cur, nptr, nlen) => {
                    let name_bytes = unsafe { core::slice::from_raw_parts(nptr as *const u8, nlen) };
                    match core::str::from_utf8(name_bytes) {
                        Ok(name) => {
                            // Веха 33: снятие корня тоже едет пачкой (group commit).
                            let existed = crate::object::del_root(name);
                            vprintln!("  [obj] P{} OBJ_DEL_ROOT '{}' → {} (по cap)", cur, name, if existed { "снят" } else { "не было" });
                            if existed { 0 } else { 1 }
                        }
                        Err(_) => usize::MAX,
                    }
                }
                Ok(()) => usize::MAX, // куча есть, а фреймов нет
                Err(e) => {
                    vprintln!("  [obj] P{} OBJ_DEL_ROOT отклонён: {:?}  ← нет capability на store", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_MAP(len) -> VA | MAX (Веха 22.1): зарезервировать len байт кучи ЛЕНИВО — ни один
        // фрейм не выделяется сейчас; страницы придут по page fault ([`handle_user_fault`]) или
        // доотображением под шлюз ([`ensure_heap_range`]). Куча растёт вверх от
        // USER_HEAP_BASE_VA и не смеет дорасти до стека. Без capability: память — свой ресурс
        // процесса (квоты — отдельная история).
        17 => {
            // Веха 35: куча общая на группу — резервируем у лидера (замок таблицы
            // сериализует SYS_MAP разных нитей, гонки за heap_brk нет).
            let leader = t.procs[cur].group;
            let len = t.procs[cur].frame.arg(0);
            let start = t.procs[leader].heap_brk;
            let end = start.saturating_add(len.div_ceil(PAGE) * PAGE);
            let limit = USER_STACK_TOP_VA - USER_STACK_PAGES * PAGE;
            let result = if len == 0 || end > limit {
                usize::MAX
            } else {
                t.procs[leader].heap_brk = end;
                vprintln!(
                    "  [mm] P{} SYS_MAP {} байт → {:#x}..{:#x} (лениво, 0 фреймов)",
                    cur, len, start, end,
                );
                start
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_CAP_DERIVE(cap, mask) -> новый дескриптор / MAX (Веха 21.1): урезанная копия
        // СВОЕГО права в СВОЁМ домене (права ∩ mask). GRANT не нужен — сужать то, чем владеешь,
        // безопасно всегда; передавать другим (CALL/REPLY с cap) — вот что требует GRANT.
        // Тоже чекпойнт: c-space меняется из userspace → фиксируем на диск.
        16 => {
            let (c, mask) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::derive(dom, Cap::from_bits(c as u64), Rights(mask as u32)) {
                Ok(nc) => {
                    vprintln!(
                        "  [cap] P{} CAP_DERIVE → копия с правами [{}] (аттенуация)",
                        cur,
                        cap::rights_str(cap::rights(dom, nc).unwrap_or(Rights::NONE)),
                    );
                    cap::persist();
                    nc.bits() as usize
                }
                Err(e) => {
                    vprintln!("  [cap] P{} CAP_DERIVE отклонён: {:?}", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_READ(buf, cap, nonblock) -> n: прочитать доступный ввод консоли (stdin) в буфер
        // процесса — хотя бы один байт. Ввода нет — процесс блокируется (StdinWait), sepc НЕ
        // двигаем: когда [`wait_stdin`] разбудит его по прерыванию UART, `ecall` РЕСТАРТУЕТ и на
        // этот раз заберёт байты из кольцевого буфера (Веха 20.2).
        //
        // Веха 99 — третий аргумент `nonblock`: вернуть 0 вместо сна. Нужен РЕАКТОРУ: хост чужих
        // процессов не может уснуть на клавиатуре, пока дети шлют ему вывод, — он обязан
        // обслуживать оба источника. Старые вызовы передают 0 и работают как прежде.
        14 => {
            let (buf, cap_len, nonblock) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            // Веха 23: приёмный буфер может лежать в ленивой куче — доотобразить до записи ядром.
            if !ensure_heap_range(t, cur, buf, cap_len) {
                let f = &mut t.procs[cur].frame;
                f.set_ret(usize::MAX);
                f.advance();
                return;
            }
            let mut n = 0usize;
            while n < cap_len {
                let Some(b) = arch::console_getc() else { break };
                // Пишем в U-память вызывающего напрямую: он current, SUM=1 (как в SYS_WRITE).
                unsafe { *((buf + n) as *mut u8) = b };
                n += 1;
            }
            // Веха 101 — сказать вслух, если кольцо консоли переполнилось и ввод пропал. Место
            // выбрано здесь, а не в обработчике прерывания: печатать из него дорого и небезопасно,
            // а чтение — ровно тот момент, когда человек смотрит на результат набора.
            let lost = arch::console_take_lost();
            if lost > 0 {
                println!("  [tty] потеряно {} байт ввода — кольцо консоли переполнено", lost);
            }
            if n > 0 {
                let f = &mut t.procs[cur].frame;
                f.set_ret(n);
                f.advance();
            } else if nonblock != 0 {
                // Веха 99: ввода нет — честный ноль, без сна.
                let f = &mut t.procs[cur].frame;
                f.set_ret(0);
                f.advance();
            } else {
                // Блокировка до ввода с РЕСТАРТОМ: при пробуждении инструкция syscall'а
                // повторится (Веха 26: на riscv sepc и так на ecall, на x86 — откат rip).
                t.procs[cur].frame.restart();
                t.procs[cur].state = State::StdinWait;
                if let Some(nx) = t.next_runnable(cur) {
                    t.current = nx;
                }
            }
        }
        // SYS_EXEC(store_cap, name_ptr, name_len, args_ptr, args_len) -> код выхода ребёнка /
        // MAX: запустить программу из store ПО ИМЕНИ КОРНЯ и ждать её завершения (foreground,
        // Веха 20.3). Требует права `EXEC` на store — ОТДЕЛЬНОГО от READ/WRITE: обладатель
        // может запускать программы, не умея читать или менять объекты (аттенуация «только
        // запуск»). Путь тот же, что в `exec_demo` ([[exec-from-store]]): корень → content-id →
        // байты ELF → [`elf::load`].
        //
        // Веха 30 — контракт запуска: `args` (NUL-разделённые записи, ≤ [`ARGS_MAX`]) станут
        // argv ребёнка после имени; env и таблица стартовых capability НАСЛЕДУЮТСЯ от
        // родителя (права — копиями через [`cap::endow`]: наделение потомка, не grant).
        // Веха 98 — `SYS_SPAWN` (41) — ТОТ ЖЕ путь запуска, но БЕЗ ожидания: родитель получает
        // номер ребёнка и продолжает работать. Одна ветка на оба вызова специально: расхождение
        // между «запустить» и «запустить и подождать» — источник тонких различий в наследовании
        // прав и окружения, а разница между ними ровно одна строка ниже.
        15 | 41 => {
            let wait_child = num == 15;
            let (scap, nptr, nlen, aptr, alen) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3), f.arg(4))
            };
            // Веха 98 — 6-й аргумент SPAWN: право, которое родитель ДОПОЛНИТЕЛЬНО отдаёт ребёнку
            // (обычно свой эндпоинт под stdio). `MAX` — нет такого.
            let extra_cap = if wait_child { usize::MAX } else { t.procs[cur].frame.arg(5) };
            let dom = t.procs[cur].domain;
            let mut spawned = false;
            match cap::store(dom, Cap::from_bits(scap as u64), Rights::EXEC) {
                _ if alen > ARGS_MAX => {
                    vprintln!("  [exec] P{} SYS_EXEC: аргументы длиннее {} — отказ", cur, ARGS_MAX)
                }
                // Веха 23: имя (и аргументы) могут лежать в ленивой куче — доотобразить до чтения.
                Ok(()) if ensure_heap_range(t, cur, nptr, nlen)
                    && (alen == 0 || ensure_heap_range(t, cur, aptr, alen)) =>
                {
                    let name_bytes = unsafe { core::slice::from_raw_parts(nptr as *const u8, nlen) };
                    if let Ok(name) = core::str::from_utf8(name_bytes) {
                        // Веха 26: `bin/<имя>` расширяется в арх-корень `bin/<arch>/<имя>` —
                        // процессы говорят «bin/hello», не зная архитектуры под собой.
                        // Веха 109 — АБСОЛЮТНЫЙ путь запускается из дерева пакета: так работает
                        // PATH профиля (`/nix/store/<путь>/bin/<имя>`). Всё прочее — по-прежнему
                        // корень store `bin/<arch>/<имя>`.
                        let elf_bytes = if name.starts_with('/') {
                            crate::lxfs::lookup(name.as_bytes())
                                .and_then(|m| crate::lxfs::read_all(&m))
                        } else {
                            let full = crate::prog_root(name);
                            // Байты ELF копируем из store и сразу отпускаем его замок.
                            crate::object::root(&full)
                                .and_then(|id| crate::object::with(&id, |b| b.map(Vec::from)))
                        };
                        match elf_bytes {
                            Some(bytes) => {
                                // Веха 89: памяти под новое пространство нет — отказ вызывающему
                                // (программа не запустилась), а не паника ядра.
                                let Some(root) = new_address_space() else {
                                    t.procs[cur].frame.set_ret(usize::MAX);
                                    return;
                                };
                                // argv ребёнка: имя + доп. аргументы вызывающего (общее для обоих путей).
                                let mut args = Vec::from(name.as_bytes());
                                args.push(0);
                                if alen > 0 {
                                    args.extend_from_slice(unsafe {
                                        core::slice::from_raw_parts(aptr as *const u8, alen)
                                    });
                                    if *args.last().unwrap() != 0 {
                                        args.push(0);
                                    }
                                }
                                // Имя процесса обязано жить дольше таблицы — утекает
                                // (запусков за сессию единицы, приемлемо до Вехи 22).
                                let pname: &'static str =
                                    Box::leak(String::from(name).into_boxed_str());
                                // Веха 38: тип ELF решает путь. Наш ET_EXEC — родной запуск
                                // (argv/env/старт-права через контракт); чужой static-PIE
                                // ET_DYN — linux-личность (стек Linux + трансля́тор syscall'ов).
                                let child = if elf::is_pie(&bytes) {
                                    spawn_linux_locked(t, pname, &bytes, root, args)
                                } else {
                                    match elf::load(root, &bytes, USER_HEAP_BASE_VA) {
                                        Ok(entry) => {
                                            let c = create_process_locked(t, pname, root, entry, 0);
                                            let parent_env = t.procs[cur].env.clone();
                                            t.procs[c].args = args;
                                            t.procs[c].env = parent_env;
                                            Some(c)
                                        }
                                        Err(e) => {
                                            vprintln!(
                                                "  [exec] P{} SYS_EXEC '{}': негодный ELF: {:?}",
                                                cur, name, e,
                                            );
                                            None
                                        }
                                    }
                                };
                                if let Some(child) = child {
                                    // Стартовые capability наследуются копиями (`cap::endow`) —
                                    // и родному ребёнку, и linux-процессу (тому — на будущее,
                                    // под файловую персоналию; stdio он шлёт напрямую в консоль).
                                    let cdom = t.procs[child].domain;
                                    for bits in t.procs[cur].start_caps.clone() {
                                        if let Ok(c) =
                                            cap::endow(dom, Cap::from_bits(bits as u64), cdom)
                                        {
                                            t.procs[child].start_caps.push(c.bits() as usize);
                                        }
                                    }
                                    vprintln!(
                                        "  [exec] P{} SYS_EXEC '{}' → P{} ({}; ждёт завершения; env {} Б, старт-прав {})",
                                        cur, name, child,
                                        if t.procs[child].linux { "linux-abi" } else { "native" },
                                        t.procs[child].env.len(), t.procs[child].start_caps.len(),
                                    );
                                    // Веха 98 — наделить ребёнка ДОПОЛНИТЕЛЬНЫМ правом и назвать
                                    // его в окружении. Индекс кладёт ЯДРО, потому что только оно
                                    // знает, сколько прав ребёнок унаследовал; выдумывать его на
                                    // стороне родителя значило бы дублировать эту арифметику и
                                    // разъезжаться с ней при первом же изменении.
                                    //
                                    // Ядро при этом НЕ узнаёт, что такое stdio: оно кладёт
                                    // строку в окружение — ровно как уже кладёт имя программы в
                                    // argv ([[process-contract]]). Смысл строки — дело userspace.
                                    if extra_cap != usize::MAX {
                                        if let Ok(c) =
                                            cap::endow(dom, Cap::from_bits(extra_cap as u64), cdom)
                                        {
                                            let idx = t.procs[child].start_caps.len();
                                            t.procs[child].start_caps.push(c.bits() as usize);
                                            let mut line = alloc::format!("STDIO={}\0", idx);
                                            let env = &mut t.procs[child].env;
                                            // Окружение — блоб `KEY=VAL\0…`; хвостовой NUL уже есть.
                                            unsafe { env.append(line.as_mut_vec()) };
                                        }
                                    }
                                    // Родство записывается в ОБОИХ случаях (Веха 114). Раньше его
                                    // ставил только SPAWN, потому что нужно оно было лишь для
                                    // `SYS_WAIT`/`SYS_KILL`; из-за этого дерево процессов
                                    // обрывалось на каждом `run`, и мультиплексор не мог понять,
                                    // чей вывод к нему пришёл (см. `SYS_PARENT`).
                                    t.procs[child].parent = cur;
                                    if wait_child {
                                        // Родитель ждёт ребёнка; sepc/a0 выставит wake_exec_waiters.
                                        t.procs[cur].state = State::ExecWait(child);
                                        t.current = child;
                                    } else {
                                        // Веха 98 — SPAWN: родителю сразу отдаём номер ребёнка и
                                        // ПРОДОЛЖАЕМ его. Ребёнок помечается зомби — его слот не
                                        // переиспользуется, пока родитель не заберёт код выхода.
                                        t.procs[child].zombie = true;
                                        let f = &mut t.procs[cur].frame;
                                        f.set_ret(child);
                                        f.advance();
                                    }
                                    spawned = true;
                                }
                            }
                            None => vprintln!("  [exec] P{} SYS_EXEC: корня '{}' нет в store", cur, name),
                        }
                    }
                }
                Ok(()) => vprintln!("  [exec] P{} SYS_EXEC: фреймы кончились под ленивый буфер имени", cur),
                Err(e) => vprintln!(
                    "  [exec] P{} SYS_EXEC отклонён: {:?}  ← нет capability (EXEC) на store",
                    cur, e,
                ),
            }
            if !spawned {
                let f = &mut t.procs[cur].frame;
                f.set_ret(usize::MAX);
                f.advance();
            }
        }
        // SYS_ARGS(sel, buf, len) -> полная длина блоба (в buf скопировано min(len, полная)):
        // sel 0 — argv (NUL-разделённые записи, [0] — имя программы), 1 — env (`KEY=VAL\0…`).
        // Контракт запуска Вехи 30: то, что Linux кладёт на стек при execve, у нас процесс
        // спрашивает у ядра — раскладка стека остаётся целиком делом программы.
        18 => {
            let (sel, ptr, len) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            // Веха 35: argv/env — свойство процесса, живут у лидера группы нитей.
            let leader = t.procs[cur].group;
            let blob = match sel {
                0 => Some(t.procs[leader].args.clone()),
                1 => Some(t.procs[leader].env.clone()),
                _ => None,
            };
            let result = match blob {
                None => usize::MAX,
                Some(b) => {
                    let n = len.min(b.len());
                    if n == 0 || ensure_heap_range(t, cur, ptr, n) {
                        if n > 0 {
                            unsafe {
                                core::ptr::copy_nonoverlapping(b.as_ptr(), ptr as *mut u8, n)
                            };
                        }
                        b.len()
                    } else {
                        usize::MAX
                    }
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_STARTCAP(i) -> биты i-го стартового capability | MAX (конец таблицы).
        // Преоткрытые права процесса (как preopen'ы WASI): выданы ядром при spawn'е или
        // унаследованы от родителя при SYS_EXEC. Дескрипторы валидны в СВОЁМ домене.
        19 => {
            let i = t.procs[cur].frame.arg(0);
            // Веха 35: стартовые capability — у лидера группы (нить делит домен процесса).
            let leader = t.procs[cur].group;
            let bits = t.procs[leader].start_caps.get(i).copied().unwrap_or(usize::MAX);
            let f = &mut t.procs[cur].frame;
            f.set_ret(bits);
            f.advance();
        }
        // SYS_NET_SEND(dev_cap, buf, len) -> 0/MAX (Веха 34): отправить сырой Ethernet-кадр.
        // Нужен cap на сетевое устройство (право WRITE). Кадр читается из U-памяти (SUM=1),
        // копируется в ядерный TX-буфер драйвера (страницы процесса не identity-mapped).
        20 => {
            let (dcap, buf, len) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::device(dom, Cap::from_bits(dcap as u64), Rights::WRITE) {
                Ok(cap::Device::Net) if len <= 2048 && ensure_heap_range(t, cur, buf, len) => {
                    let mut tmp = [0u8; 2048];
                    let src = unsafe { core::slice::from_raw_parts(buf as *const u8, len) };
                    tmp[..len].copy_from_slice(src);
                    vprintln!("  [net] P{} SYS_NET_SEND {} байт (по cap)", cur, len);
                    if crate::net::send(&tmp[..len]) { 0 } else { usize::MAX }
                }
                Ok(_) => usize::MAX,
                Err(e) => {
                    vprintln!("  [net] P{} SYS_NET_SEND отклонён: {:?}", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_NET_RECV(dev_cap, buf, buflen) -> длина кадра (0 — пусто; MAX — отказ).
        // Неблокирующий опрос приёмного кольца (нужен cap на устройство, право READ).
        21 => {
            let (dcap, buf, buflen) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::device(dom, Cap::from_bits(dcap as u64), Rights::READ) {
                Ok(cap::Device::Net) if ensure_heap_range(t, cur, buf, buflen.min(2048)) => {
                    let mut tmp = [0u8; 2048];
                    let cap_len = buflen.min(2048);
                    let n = crate::net::recv(&mut tmp[..cap_len]);
                    if n > 0 {
                        let dst = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, n) };
                        dst.copy_from_slice(&tmp[..n]);
                        vprintln!("  [net] P{} SYS_NET_RECV {} байт (по cap)", cur, n);
                    }
                    n
                }
                Ok(_) => usize::MAX,
                Err(_) => usize::MAX,
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_NET_MAC(dev_cap, buf6) -> 0/MAX (Веха 34): записать MAC карты (6 байт).
        22 => {
            let (dcap, buf) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::device(dom, Cap::from_bits(dcap as u64), Rights::READ) {
                Ok(cap::Device::Net) if ensure_heap_range(t, cur, buf, 6) => {
                    let mac = crate::net::mac();
                    let dst = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, 6) };
                    dst.copy_from_slice(&mac);
                    0
                }
                Ok(_) => usize::MAX,
                Err(_) => usize::MAX,
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_THREAD_SPAWN(entry, arg, stack_top) -> tid | MAX (Веха 35): завести НИТЬ в
        // текущем процессе — контекст в ТОМ ЖЕ адресном пространстве и домене, со своим
        // стеком (`stack_top` — вершина, userspace выделяет его из кучи процесса лениво).
        // Возвращает id нити (для THREAD_JOIN). Прав не требует: нить — та же единица
        // защиты, что процесс (не расширяет полномочий).
        23 => {
            let (entry, arg, stack_top) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            let leader = t.procs[cur].group;
            let tid = create_thread_locked(t, leader, entry, arg, stack_top);
            vprintln!(
                "  [thread] P{} SYS_THREAD_SPAWN → нить P{} (вход {:#x}, стек {:#x})",
                cur, tid, entry, stack_top,
            );
            let f = &mut t.procs[cur].frame;
            f.set_ret(tid);
            f.advance();
        }
        // SYS_THREAD_EXIT(retval) (Веха 35): завершить ТЕКУЩУЮ нить (не процесс), отдать
        // `retval` присоединяющимся (THREAD_JOIN). Не возвращается в вызывающего. Возврат
        // из main или std::process::exit идут через SYS_EXIT — тот кладёт всю группу.
        24 => {
            let retval = t.procs[cur].frame.arg(0);
            vprintln!("  [thread] P{} SYS_THREAD_EXIT({})", cur, retval);
            t.procs[cur].state = State::Finished;
            t.procs[cur].retval = retval;
            wake_join_waiters(t, cur, retval);
            if let Some(n) = t.next_runnable(cur) {
                t.current = n;
            }
        }
        // SYS_THREAD_JOIN(tid) -> retval | MAX (Веха 35): дождаться завершения нити `tid`
        // своей группы и забрать её `retval`. MAX — нет такой нити / чужая группа / это мы
        // сами. Уже завершилась — вернуть сразу; иначе блок (JoinWait), пробуждение выставит
        // a0/advance (как ExecWait: не рестарт).
        25 => {
            let tid = t.procs[cur].frame.arg(0);
            let joinable =
                tid < t.procs.len() && tid != cur && t.procs[tid].group == t.procs[cur].group;
            if !joinable {
                let f = &mut t.procs[cur].frame;
                f.set_ret(usize::MAX);
                f.advance();
            } else if t.procs[tid].state == State::Finished {
                let rv = t.procs[tid].retval;
                let f = &mut t.procs[cur].frame;
                f.set_ret(rv);
                f.advance();
            } else {
                t.procs[cur].state = State::JoinWait(tid);
                if let Some(n) = t.next_runnable(cur) {
                    t.current = n;
                }
            }
        }
        // SYS_FUTEX(op, uaddr, val, timeout) (Веха 35): примитив блокировки для Mutex/Condvar/
        // Parker в std. op 0 — WAIT(uaddr, expected, timeout_ticks): уснуть, если *uaddr ещё
        // == expected (иначе сразу 0 — «значение сменилось»); timeout в тиках [`arch::now_ticks`]
        // (0 — бессрочно). op 1 — WAKE(uaddr, count): разбудить до count спящих на слове,
        // вернуть число. Ключ ожидания — (адресное пространство, uaddr): futex-слова процесса
        // общие для его нитей. WAIT возвращает 0 (разбужен) либо 1 (истёк таймаут).
        26 => {
            let (op, uaddr, val, timeout) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3))
            };
            match op {
                0 => {
                    // futex-слово обычно в куче (Arc/Box) — доотобразить до чтения ядром.
                    let read = if ensure_heap_range(t, cur, uaddr, 4) {
                        Some(unsafe { core::ptr::read_volatile(uaddr as *const u32) })
                    } else {
                        None
                    };
                    match read {
                        Some(v) if v == val as u32 => {
                            let deadline = if timeout == 0 {
                                None
                            } else {
                                Some(arch::now_ticks().wrapping_add(timeout as u64))
                            };
                            t.procs[cur].state = State::FutexWait;
                            t.procs[cur].futex_addr = uaddr;
                            t.procs[cur].futex_deadline = deadline;
                            if let Some(n) = t.next_runnable(cur) {
                                t.current = n;
                            }
                        }
                        _ => {
                            // Значение уже иное (или недоступно) — не спать (EAGAIN): 0.
                            let f = &mut t.procs[cur].frame;
                            f.set_ret(0);
                            f.advance();
                        }
                    }
                }
                _ => {
                    let space = t.procs[cur].space;
                    let woken = wake_futex(t, space, uaddr, val);
                    let f = &mut t.procs[cur].frame;
                    f.set_ret(woken);
                    f.advance();
                }
            }
        }
        // SYS_SET_TLS(ptr) (Веха 35): задать TLS-указатель нити (tp на riscv / база %fs на
        // x86). Userspace строит per-thread TLS-блок и сообщает его базу; ядро восстанавливает
        // указатель на каждом входе в U ([`arch::TrapFrame::set_thread_ptr`]).
        27 => {
            let tp = t.procs[cur].frame.arg(0);
            t.procs[cur].frame.set_thread_ptr(tp);
            vprintln!("  [thread] P{} SYS_SET_TLS {:#x}", cur, tp);
            let f = &mut t.procs[cur].frame;
            f.set_ret(0);
            f.advance();
        }
        // SYS_CHECKPOINT(scap, name, len) — Веха 37: заморозить СЕБЯ в store (право WRITE
        // на store: чекпойнт ПИШЕТ объекты). Образ — под корнем `proc/<arch>/<имя>`.
        // Семантика setjmp: живому возвращается 0 (образ снят, работает дальше),
        // РАЗМОРОЖЕННОМУ из образа — 1 («возврат из прошлой жизни»); MAX — отказ.
        // Морозится только лидер группы без других живых нитей (кадр один).
        28 => {
            let (scap, nptr, nlen) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            let dom = t.procs[cur].domain;
            let leader = t.procs[cur].group;
            let solo = cur == leader
                && (0..t.procs.len()).all(|i| {
                    i == cur || t.procs[i].group != leader || t.procs[i].state == State::Finished
                });
            let mut ret = usize::MAX;
            match cap::store(dom, Cap::from_bits(scap as u64), Rights::WRITE) {
                Ok(()) if solo && nlen > 0 && nlen <= 64 && ensure_heap_range(t, cur, nptr, nlen) => {
                    let name_bytes = unsafe { core::slice::from_raw_parts(nptr as *const u8, nlen) };
                    if let Ok(name) = core::str::from_utf8(name_bytes) {
                        // Кадр образа: результат 1 и продвинутый pc — размороженный
                        // очнётся РОВНО в возврате из этого syscall'а.
                        let mut ff = t.procs[cur].frame;
                        ff.set_ret(1);
                        ff.advance();
                        let root_name = alloc::format!("proc/{}/{}", arch::ARCH_NAME, name);
                        let (space, brk) = (t.procs[cur].space, t.procs[cur].heap_brk);
                        let (args, env) = (t.procs[cur].args.clone(), t.procs[cur].env.clone());
                        let pages = crate::checkpoint::freeze(
                            &root_name, space, &ff, brk, &args, &env,
                            USER_REGION_START, USER_STACK_TOP_VA,
                        );
                        // Чекпойнт обязан быть НА ДИСКЕ к возврату syscall'а — иначе
                        // «образ» жил бы в RAM до ближайшего простоя (Веха 33).
                        crate::object::commit_if_dirty();
                        vprintln!(
                            "  [ckpt] P{} SYS_CHECKPOINT '{}' — {} страниц, коммит (по cap)",
                            cur, root_name, pages,
                        );
                        ret = 0;
                    }
                }
                Ok(()) => vprintln!(
                    "  [ckpt] P{} SYS_CHECKPOINT: отказ (другие нити живы / не лидер / имя негодно)",
                    cur,
                ),
                Err(e) => vprintln!(
                    "  [ckpt] P{} SYS_CHECKPOINT отклонён: {:?}  ← нет capability (WRITE) на store",
                    cur, e,
                ),
            }
            let f = &mut t.procs[cur].frame;
            f.set_ret(ret);
            f.advance();
        }
        // SYS_RESTORE(scap, name, len) — Веха 37: разморозить процесс из образа
        // `proc/<arch>/<имя>` (право EXEC — это запуск процесса, как SYS_EXEC, и ждём
        // так же). args/env приезжают ИЗ ОБРАЗА (программа их уже прочла), стартовые
        // capability — свежее наследство размораживающего (права не консервируются:
        // дескрипторы прошлой жизни умерли вместе с ней — модель exec, не пленение).
        29 => {
            let (scap, nptr, nlen) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            let dom = t.procs[cur].domain;
            let mut spawned = false;
            match cap::store(dom, Cap::from_bits(scap as u64), Rights::EXEC) {
                Ok(()) if nlen > 0 && nlen <= 64 && ensure_heap_range(t, cur, nptr, nlen) => {
                    let name_bytes = unsafe { core::slice::from_raw_parts(nptr as *const u8, nlen) };
                    if let Ok(name) = core::str::from_utf8(name_bytes) {
                        let root_name = alloc::format!("proc/{}/{}", arch::ARCH_NAME, name);
                        match crate::checkpoint::thaw(&root_name) {
                            Some(img) => {
                                let parent_scaps = t.procs[cur].start_caps.clone();
                                let pname: &'static str = Box::leak(
                                    alloc::format!("thaw:{}", name).into_boxed_str(),
                                );
                                let child = create_process_locked(t, pname, img.root, 0, 0);
                                let cdom = t.procs[child].domain;
                                t.procs[child].frame = img.frame;
                                t.procs[child].heap_brk = img.heap_brk;
                                t.procs[child].args = img.args;
                                t.procs[child].env = img.env;
                                for bits in parent_scaps {
                                    if let Ok(c) =
                                        cap::endow(dom, Cap::from_bits(bits as u64), cdom)
                                    {
                                        t.procs[child].start_caps.push(c.bits() as usize);
                                    }
                                }
                                vprintln!(
                                    "  [ckpt] P{} SYS_RESTORE '{}' → P{} ({} страниц; ждёт завершения, права — наследство размораживающего)",
                                    cur, root_name, child, img.pages,
                                );
                                t.procs[cur].state = State::ExecWait(child);
                                t.current = child;
                                spawned = true;
                            }
                            None => vprintln!(
                                "  [ckpt] P{} SYS_RESTORE: образа '{}' нет, он чужой архитектуры или негоден",
                                cur, root_name,
                            ),
                        }
                    }
                }
                Ok(()) => vprintln!("  [ckpt] P{} SYS_RESTORE: имя негодно", cur),
                Err(e) => vprintln!(
                    "  [ckpt] P{} SYS_RESTORE отклонён: {:?}  ← нет capability (EXEC) на store",
                    cur, e,
                ),
            }
            if !spawned {
                let f = &mut t.procs[cur].frame;
                f.set_ret(usize::MAX);
                f.advance();
            }
        }
        // SYS_INSTALL(store_cap) -> p2_start | MAX (Веха 48): установить VOID на AHCI-диск из
        // загрузочного модуля multiboot2 (образ с USB). Нужен store-cap с правом WRITE — тот же,
        // что у shell'а (gen1: store:xw): установка меняет содержимое store целиком, право по силе
        // равно записи. ДИСК СТИРАЕТСЯ. После успеха store заморожен — дальше только ребут.
        30 => {
            let scap = t.procs[cur].frame.arg(0);
            let dom = t.procs[cur].domain;
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::WRITE) {
                Ok(()) => match crate::install::run() {
                    Ok(p2) => {
                        crate::println!("  [install] VOID установлен на диск (store с сектора {}); заморожен — перезагрузись без USB", p2);
                        p2 as usize
                    }
                    Err(e) => {
                        crate::println!("  [install] отказ: {}", e);
                        usize::MAX
                    }
                },
                Err(e) => {
                    vprintln!("  [install] P{} отклонён: {:?}  ← нет capability (WRITE) на store", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_MMIO_MAP(mmio_cap, va) -> 0 | MAX (Веха 51): замапить окно MMIO устройства (из cap
        // база+длина) в адресное пространство userspace-драйвера по адресу `va`. Так драйвер в
        // userspace получает регистры железа — без cap доступа нет. `va` — в USER-регионе, вне
        // стека (драйвер сам выбирает окно). Пер-страничное отображение U|R|W.
        31 => {
            let (mcap, va) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::mmio(dom, Cap::from_bits(mcap as u64), Rights::WRITE) {
                Ok((base, len)) => {
                    let pages = len.div_ceil(PAGE);
                    let limit = USER_STACK_TOP_VA - USER_STACK_PAGES * PAGE;
                    if va >= USER_REGION_START && va + pages * PAGE <= limit && base % PAGE == 0 {
                        let root = arch::space_root(t.procs[cur].space);
                        let mut ok = true;
                        for i in 0..pages {
                            ok &= unsafe {
                                arch::map(root, va + i * PAGE, base + i * PAGE,
                                    arch::MAP_R | arch::MAP_W | arch::MAP_U)
                            };
                            if !ok {
                                break; // Веха 89: нет памяти под таблицы — отказ драйверу
                            }
                        }
                        arch::flush_tlb();
                        if !ok {
                            usize::MAX
                        } else {
                            // Веха 97: замаплен ЭКРАН — ядро уступает его и уходит в serial.
                            // Единственная точка передачи владения: раньше отдавать нечего
                            // (окно не отображено), позже — некому.
                            if arch::video_window() == Some((base, len)) {
                                arch::video_give_to_user(cur);
                                println!("  [видео] экран отдан процессу P{} — вывод ядра уходит в serial", cur);
                            }
                            vprintln!("  [drv] P{} SYS_MMIO_MAP {:#x} ({} стр.) → {:#x}", cur, base, pages, va);
                            0
                        }
                    } else {
                        usize::MAX
                    }
                }
                Err(e) => {
                    vprintln!("  [drv] P{} SYS_MMIO_MAP отклонён: {:?}  ← нет cap на MMIO", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_DMA_ALLOC(dma_cap, va) -> физ-адрес | MAX (Веха 51): выделить один обнулённый фрейм,
        // замапить его в драйвер по `va` (U|R|W) и вернуть его ФИЗИЧЕСКИЙ адрес — им драйвер
        // программирует DMA устройства. Без IOMMU это доверенное право (dma-cap только у драйверов).
        32 => {
            let (dcap, va) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::dma(dom, Cap::from_bits(dcap as u64), Rights::WRITE) {
                Ok(()) => {
                    let limit = USER_STACK_TOP_VA - USER_STACK_PAGES * PAGE;
                    if va >= USER_REGION_START && va + PAGE <= limit {
                        match frame::alloc() {
                            Some(pa) => {
                                let root = arch::space_root(t.procs[cur].space);
                                let ok = unsafe {
                                    arch::map(root, va, pa, arch::MAP_R | arch::MAP_W | arch::MAP_U)
                                };
                                arch::flush_tlb();
                                if ok {
                                    pa // физ-адрес фрейма (драйверу нужен именно физический)
                                } else {
                                    frame::free(pa); // Веха 89: нет памяти под таблицу — отказ
                                    usize::MAX
                                }
                            }
                            None => usize::MAX,
                        }
                    } else {
                        usize::MAX
                    }
                }
                Err(e) => {
                    vprintln!("  [drv] P{} SYS_DMA_ALLOC отклонён: {:?}  ← нет cap на DMA", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_IRQ_WAIT(irq_cap) -> 0 | MAX (Веха 52): усыпить userspace-драйвер до прерывания его
        // устройства (нужен Irq-cap). Кадр продвигаем СЕЙЧАС (вернётся 0 при пробуждении); процесс
        // уходит в IrqWait, планировщик даёт ход другим. Разбудит drain_userdrv_irq по флагу от
        // обработчика VEC_USERDRV. Уже пришедший IRQ поймает drain в resume() сразу — потери нет.
        33 => {
            let icap = t.procs[cur].frame.arg(0);
            let dom = t.procs[cur].domain;
            match cap::irq(dom, Cap::from_bits(icap as u64), Rights::READ) {
                Ok(_vector) => {
                    let f = &mut t.procs[cur].frame;
                    f.set_ret(0);
                    f.advance();
                    t.procs[cur].state = State::IrqWait;
                    // Веха 52 — «взвести» линию (размаскировать в IOAPIC): если причина уже
                    // висит на карте, прерывание доставится сразу; обработчик снова замаскирует.
                    arch::userdrv_irq_arm();
                    if let Some(n) = t.next_runnable(cur) {
                        t.current = n;
                    }
                }
                Err(e) => {
                    vprintln!("  [drv] P{} SYS_IRQ_WAIT отклонён: {:?}  ← нет cap на IRQ", cur, e);
                    let f = &mut t.procs[cur].frame;
                    f.set_ret(usize::MAX);
                    f.advance();
                }
            }
        }
        // SYS_OBJ_LIST_ROOTS(store_cap, buf_ptr, buf_len) -> ПОЛНАЯ длина текста | MAX:
        // перечислить СЫРЫЕ корни store текстом («короткий id + имя» на строку) — vsh `roots`,
        // как `ls` для объектов store. Гейт: store-cap с READ ИЛИ WRITE (любой из
        // привилегированных доступов к store позволяет узнать имена корней; у shell'а cap
        // store:xw — есть WRITE).
        //
        // Веха 107: возвращается длина ВСЕГО текста, а не записанного. Раньше отдавалось
        // `min(длина, буфер)` — и «корней ровно столько» было не отличить от «буфер мал», причём
        // обрезание приходилось на середину строки: имя корня доезжало покалеченным. На этом
        // стоит нумерация поколений (`system/gen<N>`, `pkg/profile/*/gen<N>`), а `pkg` заводит
        // по два корня на каждый путь замыкания — недосчитаться поколения значило бы ЗАТЕРЕТЬ
        // существующее. Соглашение то же, что у SYS_OBJ_CHILDREN и readdir персоналии.
        34 => {
            let (scap, bptr, blen) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            let dom = t.procs[cur].domain;
            let cap = Cap::from_bits(scap as u64);
            let allowed = cap::store(dom, cap, Rights::READ).is_ok()
                || cap::store(dom, cap, Rights::WRITE).is_ok();
            let result = if !allowed {
                vprintln!("  [obj] P{} OBJ_LIST_ROOTS отклонён ← нет capability (READ/WRITE) на store", cur);
                usize::MAX
            } else if ensure_heap_range(t, cur, bptr, blen) {
                let text = crate::object::list_roots_text();
                let bytes = text.as_bytes();
                let n = bytes.len().min(blen);
                let dst = unsafe { core::slice::from_raw_parts_mut(bptr as *mut u8, n) };
                dst.copy_from_slice(&bytes[..n]);
                vprintln!(
                    "  [obj] P{} OBJ_LIST_ROOTS → {} Б из {} ({} корней)",
                    cur, n, bytes.len(), text.lines().count()
                );
                bytes.len()
            } else {
                usize::MAX // куча под буфер не доотобразилась
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_OBJ_GC(store_cap) -> собрано объектов | MAX: сборка мусора store по достижимости
        // от корней (Веха 109). Нужен store-cap с WRITE: это операция, меняющая store.
        //
        // Наружу она понадобилась пакетам: `pkg gc` снимает корни путей, выпавших из всех
        // поколений профиля, — но пока никто не пройдёт по графу, место занято по-прежнему.
        // Раньше сборка случалась только на загрузке, то есть «удалил — перезагрузись».
        46 => {
            let scap = t.procs[cur].frame.arg(0);
            let dom = t.procs[cur].domain;
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::WRITE) {
                Ok(()) => {
                    let (kept, collected) = crate::object::gc();
                    println!(
                        "  [gc] P{} по запросу: достижимо {}, собрано {}",
                        cur, kept, collected
                    );
                    collected
                }
                Err(e) => {
                    vprintln!("  [obj] P{} SYS_OBJ_GC отклонён: {:?}", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_SLEEP(ns) -> 0 (Веха 114): уснуть на указанное время. Прав не требует — спящий
        // ничего не делает и ничего не узнаёт; отказать ему значило бы заставить программу
        // крутить `yield` в пустом цикле, чем она до сих пор и занималась.
        //
        // Срок хранится в ТИКАХ, как у `futex_wait` и `recv_timeout`, и будит его та же
        // [`wake_futex_timeouts`]; наносекунды переводятся ЗДЕСЬ, чтобы программе не надо было
        // знать таймбазу своей архитектуры. `ns == 0` — не спать, просто уступить процессор.
        47 => {
            let ns = t.procs[cur].frame.arg(0) as u64;
            if ns == 0 {
                // Возврат оформляем сами: спать не будем, а значит и будить некому.
                let f = &mut t.procs[cur].frame;
                f.set_ret(0);
                f.advance();
            } else {
                // sepc НЕ двигаем и ret не ставим — это сделает пробуждение по сроку
                // ([`wake_futex_timeouts`]), ровно как у `futex_wait`.
                let deadline = arch::now_ticks().wrapping_add(crate::clock::ns_to_ticks(ns));
                t.procs[cur].state = State::Sleeping;
                t.procs[cur].futex_deadline = Some(deadline);
            }
            if let Some(n) = t.next_runnable(cur) {
                t.current = n;
            }
        }
        // SYS_PARENT(pid) -> ppid | MAX (Веха 114): чей это ребёнок.
        //
        // Понадобилось мультиплексору. Он раздаёт панелям своё право на stdio, ребёнок панели
        // (шелл) наследует его дальше — и внук пишет НАМ, но под своим номером процесса. Пока
        // вывод раскладывался по панелям сравнением «отправитель == ребёнок панели», всё, что
        // шелл запускал, печаталось В НИКУДА: `pkg update` честно отработал десять минут и не
        // показал ни строки. Теперь хост поднимается по родителям и находит владельца.
        //
        // Гейта прав нет намеренно: номер процесса и так не тайна (его возвращает `SYS_SPAWN`,
        // он приходит в каждом сообщении), а родство — то же самое знание, только на шаг выше.
        // Изменить оно ничего не даёт: убить и дождаться по-прежнему можно лишь СВОЕГО ребёнка.
        48 => {
            let pid = t.procs[cur].frame.arg(0);
            let ppid = t
                .procs
                .get(pid)
                .map(|p| p.parent)
                .filter(|&pp| pp != usize::MAX)
                .unwrap_or(usize::MAX);
            let f = &mut t.procs[cur].frame;
            f.set_ret(ppid);
            f.advance();
        }
        // SYS_LOG(on) -> 0: вкл/выкл подробный трейс ядра (vprintln — [ipc]/[obj]/[mm]/[exec]/…).
        // Отладочная удобность, не привилегия (гейта нет): по умолчанию интерактивная сессия тихая,
        // чтобы трейс не сбивал вывод команд; `log on` в шелле включает обратно.
        35 => {
            let on = t.procs[cur].frame.arg(0) != 0;
            set_verbose(on);
            let f = &mut t.procs[cur].frame;
            f.set_ret(0);
            f.advance();
        }
        // SYS_TIME(kind) -> наносекунды (Веха 86). kind: 0 = настенное время Unix (UTC),
        // 1 = монотонное с загрузки. Гейта прав нет — время не секрет и ничего не меняет
        // (как SYS_LOG). Наносекунды влезают в usize: обе арх 64-битные (u64 хватит до 2554 года).
        36 => {
            let kind = t.procs[cur].frame.arg(0);
            let ns = match kind {
                1 => crate::clock::uptime_ns(),
                _ => crate::clock::realtime_ns(),
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(ns as usize);
            f.advance();
        }
        // SYS_RANDOM(buf, len) -> len | MAX (Веха 86): заполнить буфер процесса случайными
        // байтами (аппаратный ГСЧ + пул событий, см. [`crate::random`]). Буфер может лежать в
        // ленивой куче — доотображаем, как в SYS_WRITE.
        37 => {
            let (ptr, len, kind) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            // Веха 95: `kind == 1` — не выдача байт, а ВОПРОС «есть ли сильный источник».
            // Нужен TLS: строить ключи на пуле джиттера без подтверждённого источника нельзя,
            // и решать это должен потребитель, а не молча ядро.
            if kind == 1 {
                let strong = crate::random::has_strong_source();
                let f = &mut t.procs[cur].frame;
                f.set_ret(strong as usize);
                f.advance();
                return;
            }
            let result = if len == 0 {
                0
            } else if ensure_heap_range(t, cur, ptr, len) {
                let out = unsafe { core::slice::from_raw_parts_mut(ptr as *mut u8, len) };
                crate::random::fill(out);
                len
            } else {
                usize::MAX
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_OBJ_PUT_NODE(store_cap, buf, len, kids_ptr, nkids, idout) -> 0|MAX (Веха 94):
        // положить УЗЕЛ — значение плюс список исходящих ссылок (по 32 байта каждая).
        //
        // Зачем отдельно от `SYS_OBJ_PUT`: большой файл не кладётся одним слайсом — ни в кучу
        // процесса, ни в кучу ядра. Он кладётся КУСКАМИ (каждый — обычный объект), а узел
        // связывает их в целое. Дедуп при этом достаётся даром: одинаковый кусок в двух
        // загрузках — один объект. GC уже умеет ходить по детям (checkpoint строит такое же
        // дерево с Вехи 37), так что новой машинерии не появляется — только доступ из userspace.
        38 => {
            let (scap, buf, len, kids, nkids, idout) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3), f.arg(4), f.arg(5))
            };
            let dom = t.procs[cur].domain;
            let kbytes = nkids.saturating_mul(32);
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::WRITE) {
                Ok(()) if ensure_heap_range(t, cur, buf, len)
                    && (nkids == 0 || ensure_heap_range(t, cur, kids, kbytes))
                    && ensure_heap_range(t, cur, idout, 32) =>
                {
                    let bytes = unsafe { core::slice::from_raw_parts(buf as *const u8, len) };
                    let mut children = Vec::with_capacity(nkids);
                    for i in 0..nkids {
                        let mut id = [0u8; 32];
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                (kids + i * 32) as *const u8, id.as_mut_ptr(), 32,
                            )
                        };
                        children.push(void_abi::ContentId(id));
                    }
                    // Веха 104 — нехватка памяти ядра: отказ, а не паника (см. OBJ_PUT).
                    match crate::object::try_put_node(bytes, &children) {
                        Some(id) => {
                            let out =
                                unsafe { core::slice::from_raw_parts_mut(idout as *mut u8, 32) };
                            out.copy_from_slice(&id.0);
                            vprintln!(
                                "  [obj] P{} OBJ_PUT_NODE {} байт + {} детей → content-id (по cap)",
                                cur, len, nkids,
                            );
                            0
                        }
                        None => {
                            println!(
                                "  [obj] P{} OBJ_PUT_NODE {} байт: НЕ ХВАТИЛО памяти ядра",
                                cur, len,
                            );
                            usize::MAX
                        }
                    }
                }
                Ok(()) => usize::MAX,
                Err(e) => {
                    vprintln!("  [obj] P{} OBJ_PUT_NODE отклонён: {:?}", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_OBJ_CHILDREN(store_cap, id_ptr, out_buf, out_cap) -> число детей | MAX (Веха 94):
        // выписать ссылки узла (по 32 байта). Без этого положенное деревом нельзя прочитать
        // обратно: `SYS_OBJ_GET` отдаёт только полезную нагрузку узла, а не его детей.
        39 => {
            let (scap, idp, obuf, ocap) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::READ) {
                Ok(()) if ensure_heap_range(t, cur, idp, 32)
                    && (ocap == 0 || ensure_heap_range(t, cur, obuf, ocap)) =>
                {
                    let mut id = [0u8; 32];
                    unsafe { core::ptr::copy_nonoverlapping(idp as *const u8, id.as_mut_ptr(), 32) };
                    let kids = crate::object::children(&void_abi::ContentId(id));
                    let n = kids.len().min(ocap / 32);
                    for (i, c) in kids.iter().take(n).enumerate() {
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                c.0.as_ptr(), (obuf + i * 32) as *mut u8, 32,
                            )
                        };
                    }
                    // Возвращаем ПОЛНОЕ число детей, а не сколько влезло: иначе вызывающий не
                    // отличил бы «детей ровно столько» от «буфер мал» и потерял бы хвост.
                    kids.len()
                }
                Ok(()) => usize::MAX,
                Err(e) => {
                    vprintln!("  [obj] P{} OBJ_CHILDREN отклонён: {:?}", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_VIDEO_INFO(mmio_cap, out) -> 0 | MAX (Веха 97): описание видеорежима в буфер
        // процесса — 10 × u32: ширина, высота, шаг строки, бит/пиксель и по паре
        // (позиция, ширина маски) на R, G, B.
        //
        // Права те же, что на само окно: числа сами по себе безобидны, но отдавать их отдельно
        // от права рисовать незачем — так геометрия неотделима от capability, а не висит
        // «общедоступной справкой» рядом с ней.
        40 => {
            let (mcap, out) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1))
            };
            let dom = t.procs[cur].domain;
            const N: usize = 10;
            let result = match cap::mmio(dom, Cap::from_bits(mcap as u64), Rights::READ) {
                Ok(_) if ensure_heap_range(t, cur, out, N * 4) => {
                    let (w, h, pitch, bpp, rgb) = arch::video_info();
                    let vals: [u32; N] = [
                        w as u32, h as u32, pitch as u32, bpp as u32,
                        rgb[0].0 as u32, rgb[0].1 as u32,
                        rgb[1].0 as u32, rgb[1].1 as u32,
                        rgb[2].0 as u32, rgb[2].1 as u32,
                    ];
                    for (i, v) in vals.iter().enumerate() {
                        unsafe { core::ptr::write_unaligned((out + i * 4) as *mut u32, *v) };
                    }
                    0
                }
                Ok(_) => usize::MAX,
                Err(e) => {
                    vprintln!("  [видео] P{} VIDEO_INFO отклонён: {:?}", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_SELF_ENDPOINT() -> cap (Веха 98): право ВЫЗЫВАТЬ этот процесс, чтобы отдать его
        // детям. Не расширение полномочий: принимать сообщения процесс может и так (`SYS_RECV`),
        // а кому раздать право на себя — его собственное дело. Без этого хост чужого stdio
        // невозможен: ребёнку некуда слать вывод, потому что сослаться на родителя нечем.
        //
        // Права SEND — только «позвать»; ни принимать за нас, ни раздавать дальше (нет GRANT).
        43 => {
            let dom = t.procs[cur].domain;
            let leader = t.procs[cur].group; // эндпоинт принадлежит ПРОЦЕССУ, не нити
            let cap = cap::mint(dom, cap::Target::Endpoint(leader), Rights::SEND);
            let f = &mut t.procs[cur].frame;
            f.set_ret(cap.bits() as usize);
            f.advance();
        }
        // SYS_KILL(pid) (Веха 103) — завершить СВОЕГО ребёнка, запущенного `SYS_SPAWN`.
        //
        // Право берётся оттуда же, откуда его берёт `SYS_WAIT`: из РОДИТЕЛЬСТВА. Отдельной
        // capability заводить не стали — она бы дублировала уже существующее отношение: кто
        // процесс создал, тот им и распоряжается, чужого не тронуть. Ровно этого не хватало,
        // чтобы закрытая панель мультиплексора не оставляла сироту ([[multiplexer]]).
        //
        // Код выхода — 137 (128+9), как принято для «убит», чтобы родитель отличал его от
        // обычного возврата.
        45 => {
            let pid = t.procs[cur].frame.arg(0);
            let ok = pid < t.procs.len()
                && pid != cur
                && t.procs[pid].parent == cur
                && t.procs[pid].state != State::Finished;
            if !ok {
                let f = &mut t.procs[cur].frame;
                f.set_ret(usize::MAX);
                f.advance();
                return;
            }
            let leader = t.procs[pid].group;
            vprintln!("  [proc] P{} SYS_KILL P{} (свой ребёнок)", cur, leader);
            for i in 0..t.procs.len() {
                if t.procs[i].group == leader {
                    t.procs[i].state = State::Finished;
                }
            }
            // Ждущие узнают код выхода тем же путём, что и при обычном завершении; права на
            // мертвеца отзовёт `reclaim_dead_spaces` (Веха 89), когда освободит его слот.
            wake_exec_waiters(t, leader, 137);
            let f = &mut t.procs[cur].frame;
            f.set_ret(0);
            f.advance();
        }
        // SYS_POWEROFF(cap) (Веха 101) — выключить машину. Право отдельное (`Target::Power`,
        // токен `power` в конфиге): выключение — одностороннее действие над ВСЕЙ системой, и
        // «может любой процесс» здесь было бы дырой ровно того сорта, который capability-модель
        // и должна закрывать.
        //
        // Раньше выключения не было вовсе: `exit` в шелле лишь заканчивал программу, а машина
        // продолжала работать — сессия в нынешнем виде не кончается никогда (после ухода шелла
        // остаются сервисы, и `run()` честно крутит их дальше).
        44 => {
            let (dom, ccap) = (t.procs[cur].domain, t.procs[cur].frame.arg(0));
            if !cap::may_power_off(dom, Cap::from_bits(ccap as u64)) {
                let f = &mut t.procs[cur].frame;
                f.set_ret(usize::MAX);
                f.advance();
                return;
            }
            println!("  [power] выключение по запросу процесса");
            // Синк ПЕРЕД снятием питания: иначе выключение съело бы хвост несинхронизированных
            // операций (окно group commit ~2 с).
            crate::object::commit();
            println!(
                "  [store] финальный синк: поколение {} · записано за сессию: {} КиБ",
                crate::object::generation(),
                crate::object::bytes_written() / 1024,
            );
            arch::power_off();
        }
        // SYS_WAIT(pid, nonblock) -> код выхода | WOULD_BLOCK | MAX (Веха 98): забрать результат
        // СВОЕГО ребёнка, запущенного `SYS_SPAWN`. Чужих детей ждать нельзя — иначе один процесс
        // мог бы наблюдать за жизнью другого, ничего на него не имея.
        //
        // `nonblock` — не удобство, а необходимость: реактор мультиплексора не может замереть на
        // одном ребёнке, пока остальные панели ждут отрисовки.
        42 => {
            let (pid, nonblock) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1))
            };
            let mine = pid < t.procs.len() && t.procs[pid].parent == cur && t.procs[pid].zombie;
            let mut blocked = false;
            let result = if !mine {
                usize::MAX
            } else if let Some(code) = t.procs[pid].exit_code {
                // Ребёнок уже закончил: отдать код и ОТПУСТИТЬ слот — зомби больше не нужен.
                t.procs[pid].zombie = false;
                if t.procs[pid].state == State::Finished && pid != t.current {
                    t.free_slots.push(pid);
                }
                code
            } else if nonblock != 0 {
                WOULD_BLOCK
            } else {
                // Блокирующая форма переиспользует механизм `SYS_EXEC`: пробуждение и доставку
                // кода уже делает `wake_exec_waiters`, второго такого пути заводить незачем.
                t.procs[cur].state = State::ExecWait(pid);
                t.procs[pid].zombie = false; // код придёт напрямую, придерживать слот больше не надо
                blocked = true;
                0
            };
            if !blocked {
                let f = &mut t.procs[cur].frame;
                f.set_ret(result);
                f.advance();
            }
        }
        other => {
            let f = &mut t.procs[cur].frame;
            vprintln!("  [proc] неизвестный syscall {}", other);
            f.set_ret(usize::MAX);
            f.advance();
        }
    }
}

// ─── linux-abi: трансля́тор syscall'ов (Веха 38) ───────────────────────────────

/// Прочитать 2 байта по `user_pc` и проверить, что это `syscall` (0F 05) — распознаёт
/// #UD от linux-процесса на x86-64 (см. [`handle_user_trap`]). Читаем побайтно с трансляцией:
/// инструкция теоретически может лежать на стыке страниц.
fn is_linux_syscall_insn(t: &Table, cur: usize) -> bool {
    let pc = t.procs[cur].frame.user_pc();
    let root = arch::space_root(t.procs[cur].space);
    let byte = |va: usize| -> Option<u8> {
        // Веха 87: translate отдаёт ФИЗИЧЕСКИЙ адрес — читаем его через direct-map.
        arch::translate(root, va).map(|pa| unsafe { *(crate::frame::ptr(pa) as *const u8) })
    };
    byte(pc) == Some(0x0f) && byte(pc + 1) == Some(0x05)
}

/// Записать `data` в буфер процесса `cur` по VA `va` (доотобразив ленивую кучу). `false` —
/// адрес недоступен (фреймы кончились). Процесс — current, поэтому пишем прямой ссылкой
/// (riscv: SUM=1; x86: ring0 пишет U-страницы, SMAP не включён).
fn lx_put(t: &mut Table, cur: usize, va: usize, data: &[u8]) -> bool {
    if data.is_empty() {
        return true;
    }
    if !ensure_heap_range(t, cur, va, data.len()) {
        return false;
    }
    unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), va as *mut u8, data.len()) };
    true
}

/// Прочитать срез памяти процесса `cur` длиной `len` по VA `va` для ядра. Возвращает `None`,
/// если диапазон не удалось обеспечить. Процесс — current (см. [`lx_put`]).
fn lx_get<'a>(t: &mut Table, cur: usize, va: usize, len: usize) -> Option<&'a [u8]> {
    if len == 0 {
        return Some(&[]);
    }
    if !ensure_heap_range(t, cur, va, len) {
        return None;
    }
    Some(unsafe { core::slice::from_raw_parts(va as *const u8, len) })
}

/// Веха 108.3 — прочитать NUL-терминированную строку (путь) из памяти процесса. Потолок стоит
/// затем, что длину задаёт чужая программа, а искать нуль до конца адресного пространства нельзя.
fn lx_cstr(t: &mut Table, cur: usize, va: usize, max: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for i in 0..max {
        let b = lx_get(t, cur, va + i, 1)?[0];
        if b == 0 {
            return Some(out);
        }
        out.push(b);
    }
    None
}

/// Открыть найденный узел: занять слот в таблице дескрипторов процесса, вернуть номер fd.
fn lx_fd_alloc(t: &mut Table, cur: usize, meta: crate::lxfs::Meta, path: Vec<u8>) -> usize {
    let fd = LxFd { meta, off: 0, path, dpos: 0 };
    let tbl = &mut t.procs[cur].lx_fds;
    let i = match tbl.iter().position(|s| s.is_none()) {
        Some(i) => {
            tbl[i] = Some(fd);
            i
        }
        None => {
            tbl.push(Some(fd));
            tbl.len() - 1
        }
    };
    i + LX_FD_BASE
}

/// Первый номер файлового дескриптора Linux-процесса: 0/1/2 заняты консолью.
const LX_FD_BASE: usize = 3;

/// Права страницы из `prot` линуксового `mmap`/`mprotect` (PROT_READ=1, WRITE=2, EXEC=4).
///
/// `PROT_NONE` мы отображаем как «читаемо»: ld.so резервирует им дыры между сегментами и потом
/// перекрывает их FIXED-отображениями. Настоящая защита от чтения потребовала бы отдельного
/// состояния «страница есть, но недоступна», а пользы для запуска бинаря не даёт.
fn lx_page_flags(prot: usize) -> usize {
    let mut f = arch::MAP_U | arch::MAP_R;
    if prot & 2 != 0 {
        f |= arch::MAP_W;
    }
    if prot & 4 != 0 {
        f |= arch::MAP_X;
    }
    f
}

/// Материализовать диапазон страниц процесса с правами на ЗАПИСЬ (в них ещё предстоит копировать).
/// `false` — не хватило памяти или квоты.
fn lx_map_range(t: &mut Table, cur: usize, start: usize, size: usize, _prot: usize) -> bool {
    let leader = t.procs[cur].group;
    let root = arch::space_root(t.procs[cur].space);
    let mut va = start;
    while va < start + size {
        // Страница могла остаться от ПРЕДЫДУЩЕГО отображения этого же диапазона (ld.so сперва
        // резервирует весь файл, потом кладёт в него сегменты) — и остаться без права записи.
        // Копировать в такую нечем, поэтому права возвращаем на запись независимо от того,
        // была она отображена или нет.
        if let Some(pa) = arch::translate(root, va) {
            let _ = unsafe {
                arch::map(root, va, pa & !(PAGE - 1), arch::MAP_R | arch::MAP_W | arch::MAP_U)
            };
        }
        if arch::translate(root, va).is_none() {
            if t.procs[leader].pages >= page_quota() {
                return false;
            }
            let Some(pa) = frame::alloc() else { return false };
            if !unsafe { arch::map(root, va, pa, arch::MAP_R | arch::MAP_W | arch::MAP_U) } {
                frame::free(pa);
                return false;
            }
            t.procs[leader].pages += 1;
        }
        va += PAGE;
    }
    arch::flush_tlb();
    true
}

/// Поставить диапазону страниц права `prot` (уже отображённым — не трогая их содержимого).
fn lx_protect_range(t: &mut Table, cur: usize, start: usize, size: usize, prot: usize) {
    let root = arch::space_root(t.procs[cur].space);
    let flags = lx_page_flags(prot);
    let mut va = start;
    while va < start + size {
        if let Some(pa) = arch::translate(root, va) {
            let _ = unsafe { arch::map(root, va, pa & !(PAGE - 1), flags) };
        }
        va += PAGE;
    }
    arch::flush_tlb();
}

/// Веха 38 — трансля́тор Linux-syscall'ов для процессов личности `linux` ([`crate::linux`]).
/// Зеркало VOID-диспетчера [`syscall`], но номера/семантика — Linux; завершённый вызов
/// перешагивает свою инструкцию `skip_syscall_insn` (на riscv это sepc+4, на x86 rip+2),
/// блокирующий (чтение stdin) — оставляет PC на месте для рестарта, как VOID `SYS_READ`.
fn linux_syscall(t: &mut Table, cur: usize) {
    use crate::linux::{self, Lx};
    let nr = t.procs[cur].frame.syscall_num();
    let a = |t: &Table, i: usize| t.procs[cur].frame.arg(i);
    let (a0, a1, a2) = (a(t, 0), a(t, 1), a(t, 2));

    let decoded = linux::decode(nr);
    // Значение результата вычисляем в `ret`; блокирующие/завершающие ветки ставят `done=false`
    // и сами разбираются с состоянием/PC (тогда общий эпилог не трогает кадр).
    let mut ret: usize = 0;
    let mut done = true;

    match decoded {
        // ── вывод ──────────────────────────────────────────────────────────────
        Some(Lx::Write) => {
            let (fd, buf, len) = (a0, a1, a2);
            if fd == 1 || fd == 2 {
                ret = match lx_get(t, cur, buf, len) {
                    Some(b) => {
                        crate::print!("{}", core::str::from_utf8(b).unwrap_or("<?>"));
                        len
                    }
                    None => linux::err(linux::EFAULT),
                };
            } else {
                ret = linux::err(linux::EBADF);
            }
        }
        Some(Lx::Writev) => {
            // iov: массив из iovcnt структур { base: u64, len: u64 }.
            let (fd, iov, iovcnt) = (a0, a1, a2);
            if fd == 1 || fd == 2 {
                let mut total = 0usize;
                let mut ok = true;
                for i in 0..iovcnt {
                    let ent = match lx_get(t, cur, iov + i * 16, 16) {
                        Some(e) => e,
                        None => {
                            ok = false;
                            break;
                        }
                    };
                    let base = usize::from_le_bytes(ent[0..8].try_into().unwrap());
                    let len = usize::from_le_bytes(ent[8..16].try_into().unwrap());
                    match lx_get(t, cur, base, len) {
                        Some(b) => {
                            crate::print!("{}", core::str::from_utf8(b).unwrap_or("<?>"));
                            total += len;
                        }
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                ret = if ok { total } else { linux::err(linux::EFAULT) };
            } else {
                ret = linux::err(linux::EBADF);
            }
        }
        // ── ввод (stdin с консоли, блокирующе) ──────────────────────────────────
        Some(Lx::Read) => {
            let (fd, buf, len) = (a0, a1, a2);
            if fd >= LX_FD_BASE {
                // Файл из store (Веха 108.3). Чтение короткое: за раз отдаём не больше остатка
                // текущего куска блоба — так же ведёт себя `read` на трубе, и musl дочитает.
                let slot = t.procs[cur].lx_fds.get(fd - LX_FD_BASE).cloned().flatten();
                ret = match slot {
                    None => linux::err(linux::EBADF),
                    Some(f) => {
                        let mut tmp = alloc::vec![0u8; len.min(64 * 1024)];
                        let n = crate::lxfs::read_at(&f.meta, f.off, &mut tmp);
                        if lx_put(t, cur, buf, &tmp[..n]) {
                            if let Some(Some(sl)) = t.procs[cur].lx_fds.get_mut(fd - LX_FD_BASE) {
                                sl.off += n as u64;
                            }
                            n
                        } else {
                            linux::err(linux::EFAULT)
                        }
                    }
                };
            } else if fd != 0 {
                ret = linux::err(linux::EBADF);
            } else if !ensure_heap_range(t, cur, buf, len) {
                ret = linux::err(linux::EFAULT);
            } else {
                let mut n = 0usize;
                while n < len {
                    let Some(b) = arch::console_getc() else { break };
                    unsafe { *((buf + n) as *mut u8) = b };
                    n += 1;
                }
                if n > 0 || len == 0 {
                    ret = n;
                } else {
                    // Ввода нет — заблокироваться с рестартом (PC на инструкции syscall'а).
                    t.procs[cur].state = State::StdinWait;
                    if let Some(nx) = t.next_runnable(cur) {
                        t.current = nx;
                    }
                    done = false;
                }
            }
        }
        Some(Lx::Pread64) => {
            // pread64(fd, buf, len, off) — им `ld.so` читает заголовки ELF, не двигая позицию.
            let (fd, buf, len, off) = (a0, a1, a2, a(t, 3));
            let slot = fd
                .checked_sub(LX_FD_BASE)
                .and_then(|i| t.procs[cur].lx_fds.get(i).cloned())
                .flatten();
            ret = match slot {
                None => linux::err(linux::EBADF),
                Some(f) => {
                    let mut tmp = alloc::vec![0u8; len.min(64 * 1024)];
                    let n = crate::lxfs::read_at(&f.meta, off as u64, &mut tmp);
                    if lx_put(t, cur, buf, &tmp[..n]) { n } else { linux::err(linux::EFAULT) }
                }
            };
        }
        Some(Lx::Readv) => {
            // Для stdin достаточно наполнить первый непустой iov (короткое чтение допустимо).
            let (fd, iov, iovcnt) = (a0, a1, a2);
            if fd != 0 {
                ret = linux::err(linux::EBADF);
            } else {
                let mut got = 0usize;
                for i in 0..iovcnt {
                    let ent = match lx_get(t, cur, iov + i * 16, 16) {
                        Some(e) => e,
                        None => break,
                    };
                    let base = usize::from_le_bytes(ent[0..8].try_into().unwrap());
                    let len = usize::from_le_bytes(ent[8..16].try_into().unwrap());
                    if len == 0 || !ensure_heap_range(t, cur, base, len) {
                        continue;
                    }
                    while got < len {
                        let Some(b) = arch::console_getc() else { break };
                        unsafe { *((base + got) as *mut u8) = b };
                        got += 1;
                    }
                    if got > 0 {
                        break;
                    }
                }
                ret = got; // 0 = EOF-подобно (не блокируем readv — им пользуются реже)
            }
        }
        // ── память: brk/mmap поверх ленивой кучи процесса ───────────────────────
        Some(Lx::Brk) => {
            let leader = t.procs[cur].group;
            let cur_brk = t.procs[leader].heap_brk;
            let limit = USER_STACK_TOP_VA - USER_STACK_PAGES * PAGE;
            ret = if a0 == 0 {
                cur_brk
            } else if a0 >= USER_HEAP_BASE_VA && a0 <= limit {
                t.procs[leader].heap_brk = a0; // растёт/сжимается лениво (страницы по фолту)
                a0
            } else {
                cur_brk // за пределами — не двигаем (Linux: возврат старого = «не удалось»)
            };
        }
        Some(Lx::Mmap) => {
            // Веха 108.4 — три случая, и все три нужны динамическому загрузчику:
            //   1) анонимное без адреса — хвост ленивой кучи (страницы придут по фолту);
            //   2) MAP_FIXED — материализовать страницы ПО ЗАДАННОМУ адресу с правами `prot`
            //      (ld.so сперва резервирует диапазон, потом кладёт в него сегменты);
            //   3) файловое — то же плюс копирование содержимого из store.
            let (addr, len, prot, flags, fd, off) =
                (a0, a1, a2, a(t, 3), a(t, 4) as isize, a(t, 5));
            const MAP_ANONYMOUS: usize = 0x20;
            const MAP_FIXED: usize = 0x10;
            let anon = flags & MAP_ANONYMOUS != 0 || fd < 0;
            let leader = t.procs[cur].group;
            let limit = USER_STACK_TOP_VA - USER_STACK_PAGES * PAGE;
            let size = (len + PAGE - 1) & !(PAGE - 1);

            // Куда ложимся: заданный адрес (MAP_FIXED) либо хвост кучи.
            let start = if flags & MAP_FIXED != 0 { addr & !(PAGE - 1) } else { t.procs[leader].heap_brk };
            let end = start.saturating_add(size);
            if len == 0 || end > limit || start < USER_HEAP_BASE_VA {
                ret = linux::err(linux::ENOMEM);
            } else {
                // Диапазон обязан числиться кучей: по нему пойдут фолты и проверки шлюзов.
                if end > t.procs[leader].heap_brk {
                    t.procs[leader].heap_brk = end;
                }
                let need_data = !anon;
                // Анонимное без FIXED оставляем ленивым (так было и раньше — дёшево); всё
                // остальное материализуем сразу: под копирование содержимого страницы нужны.
                let ok = if !need_data && flags & MAP_FIXED == 0 {
                    true
                } else {
                    lx_map_range(t, cur, start, size, prot)
                };
                if !ok {
                    ret = linux::err(linux::ENOMEM);
                } else if anon && flags & MAP_FIXED != 0 {
                    // MAP_ANONYMOUS обязано быть НУЛЯМИ, а страницы тут — не обязательно свежие:
                    // ld.so сперва отображает файлом ВЕСЬ образ библиотеки (включая будущий bss),
                    // и только потом накрывает хвост анонимным отображением. Без этого зануления
                    // в bss оставались байты файла — glibc видел там «занятый» замок и вставал
                    // намертво в futex, а printf брал оттуда указатель и падал в #GP.
                    let zero = alloc::vec![0u8; PAGE];
                    let mut done = 0usize;
                    while done < size {
                        let n = (size - done).min(PAGE);
                        if !lx_put(t, cur, start + done, &zero[..n]) {
                            break;
                        }
                        done += n;
                    }
                    lx_protect_range(t, cur, start, size, prot);
                    vprintln!("  [linux] P{} mmap анонимно {:#x}+{:#x} — обнулено", cur, start, size);
                    ret = start;
                } else if need_data {
                    let slot = fd
                        .try_into()
                        .ok()
                        .and_then(|f: usize| f.checked_sub(LX_FD_BASE))
                        .and_then(|i| t.procs[cur].lx_fds.get(i).cloned())
                        .flatten();
                    match slot {
                        None => ret = linux::err(linux::EBADF),
                        Some(f) => {
                            // Копируем файловую часть; хвост до конца страниц остаётся нулевым
                            // (фреймы приходят обнулёнными) — это и есть .bss сегмента.
                            let mut done = 0usize;
                            let mut buf = alloc::vec![0u8; 64 * 1024];
                            while done < len {
                                let take = (len - done).min(buf.len());
                                let n = crate::lxfs::read_at(
                                    &f.meta,
                                    (off + done) as u64,
                                    &mut buf[..take],
                                );
                                if n == 0 {
                                    break;
                                }
                                if !lx_put(t, cur, start + done, &buf[..n]) {
                                    break;
                                }
                                done += n;
                            }
                            // Права ставим ПОСЛЕ копирования: сегмент кода приходит без W, а
                            // писать в него нам было надо.
                            lx_protect_range(t, cur, start, size, prot);
                            vprintln!(
                                "  [linux] P{} mmap файла fd{} off={:#x} len={:#x} → {:#x} prot={} скопировано {:#x}",
                                cur, fd, off, len, start, prot, done
                            );
                            ret = start;
                        }
                    }
                } else {
                    ret = start;
                }
            }
        }
        Some(Lx::Munmap) => ret = 0, // bump-куча не освобождает — утечка допустима (демо)
        Some(Lx::Mprotect) => {
            // Веха 108.4 — теперь по-настоящему: ld.so переводит RELRO в read-only и ставит
            // права сегментам, а страницы у нас появляются с правами из mmap.
            let (addr, len, prot) = (a0, a1, a2);
            lx_protect_range(t, cur, addr & !(PAGE - 1), (len + PAGE - 1) & !(PAGE - 1), prot);
            ret = 0;
        }
        Some(Lx::Madvise) => ret = 0,
        Some(Lx::Mremap) => ret = linux::err(linux::ENOSYS),
        // ── TLS и потоковые заглушки ────────────────────────────────────────────
        Some(Lx::ArchPrctl) => {
            // x86-64: ARCH_SET_FS(0x1002) — musl кладёт сюда базу TLS; ставим fsbase кадра.
            const ARCH_SET_FS: usize = 0x1002;
            if a0 == ARCH_SET_FS {
                t.procs[cur].frame.set_thread_ptr(a1);
                ret = 0;
            } else {
                ret = linux::err(linux::EINVAL);
            }
        }
        Some(Lx::Futex) => {
            // Веха 108.4 — futex(uaddr, op, val, …). glibc берёт его на КАЖДЫЙ внутренний
            // замок, и без него запуск обрывался на «The futex facility returned an unexpected
            // error code» — то есть на ENOSYS, а не на самой блокировке.
            //
            // Кладём на механизм Вехи 35, которым живут нити VOID: FUTEX_WAIT засыпает, если
            // слово ещё то самое, FUTEX_WAKE будит. Флаг PRIVATE (128) нам безразличен — futex
            // и так живёт в адресном пространстве процесса, а CLOCK_REALTIME (256) значим лишь
            // для таймаутов, которых мы пока не различаем.
            const FUTEX_WAIT: usize = 0;
            const FUTEX_WAKE: usize = 1;
            let (uaddr, op, val) = (a0, a1 & 0x7f, a2);
            match op {
                FUTEX_WAIT => {
                    let read = if ensure_heap_range(t, cur, uaddr, 4) {
                        Some(unsafe { core::ptr::read_volatile(uaddr as *const u32) })
                    } else {
                        None
                    };
                    match read {
                        Some(v) if v == val as u32 => {
                            // Ждём бессрочно: единственная нить linux-процесса разбудить себя не
                            // может, но и попасть сюда при своей же незанятой блокировке — тоже.
                            t.procs[cur].state = State::FutexWait;
                            t.procs[cur].futex_addr = uaddr;
                            t.procs[cur].futex_deadline = None;
                            if let Some(n) = t.next_runnable(cur) {
                                t.current = n;
                            }
                            done = false; // спим: кадр и PC не трогаем (проснёмся — повторим)
                        }
                        // Значение уже иное — EAGAIN, как и положено futex'у.
                        _ => ret = linux::err(11),
                    }
                }
                FUTEX_WAKE => {
                    let space = t.procs[cur].space;
                    ret = wake_futex(t, space, uaddr, val);
                }
                _ => ret = linux::err(linux::ENOSYS),
            }
        }
        Some(Lx::SetTidAddress) => ret = cur + 1, // «tid» = индекс процесса + 1
        Some(Lx::SetRobustList) => ret = 0,
        Some(Lx::RtSigprocmask) => ret = 0,
        Some(Lx::RtSigaction) => ret = 0, // обработчики сигналов игнорируем (однопоточный CLI)
        Some(Lx::Rseq) => ret = linux::err(linux::ENOSYS),
        Some(Lx::Prlimit64) => ret = linux::err(linux::ENOSYS),
        // ── информация ──────────────────────────────────────────────────────────
        Some(Lx::Getpid) | Some(Lx::Gettid) => ret = cur + 1,
        Some(Lx::Getppid) => ret = 1,
        Some(Lx::Getuid) | Some(Lx::Geteuid) | Some(Lx::Getgid) | Some(Lx::Getegid) => ret = 0,
        // Мы «root» (uid/gid 0) — сброс привилегий busybox'а на старте no-op (успех).
        Some(Lx::Setuid) | Some(Lx::Setgid) | Some(Lx::Setgroups) => ret = 0,
        Some(Lx::Uname) => {
            let mut buf = [0u8; linux::UTSNAME_SIZE];
            linux::fill_utsname(&mut buf);
            ret = if lx_put(t, cur, a0, &buf) { 0 } else { linux::err(linux::EFAULT) };
        }
        Some(Lx::Sysinfo) => {
            let zero = [0u8; 112]; // struct sysinfo — нулями (демо не читает поля критично)
            ret = if lx_put(t, cur, a0, &zero) { 0 } else { linux::err(linux::EFAULT) };
        }
        Some(Lx::Getrandom) => {
            // Веха 86: был линейный конгруэнтный генератор от счётчика — теперь общий источник
            // ядра (аппаратный ГСЧ + пул событий, [`crate::random`]), тот же, что у SYS_RANDOM.
            let (buf, len) = (a0, a1);
            let mut tmp = alloc::vec![0u8; len];
            crate::random::fill(&mut tmp);
            ret = if lx_put(t, cur, buf, &tmp) { len } else { linux::err(linux::EFAULT) };
        }
        Some(Lx::Getcwd) => {
            // Корневой каталог: "/". Linux getcwd возвращает длину включая NUL.
            ret = if lx_put(t, cur, a0, b"/\0") { 2 } else { linux::err(linux::EFAULT) };
        }
        // ── время ────────────────────────────────────────────────────────────────
        // Веха 86: часы стали настоящими, поэтому REALTIME и MONOTONIC наконец РАЗНЫЕ.
        // clockid: 0 = CLOCK_REALTIME, 1 = CLOCK_MONOTONIC (прочие сводим к монотонному —
        // BOOTTIME/MONOTONIC_RAW у нас совпадают с ним, а CPU-таймеров процесса нет).
        Some(Lx::ClockGettime) => {
            let ns = match a0 {
                0 => crate::clock::realtime_ns(),
                _ => crate::clock::uptime_ns(),
            };
            let ts = [(ns / 1_000_000_000), (ns % 1_000_000_000)];
            let mut buf = [0u8; 16];
            buf[0..8].copy_from_slice(&ts[0].to_le_bytes());
            buf[8..16].copy_from_slice(&ts[1].to_le_bytes());
            ret = if lx_put(t, cur, a1, &buf) { 0 } else { linux::err(linux::EFAULT) };
        }
        Some(Lx::Gettimeofday) => {
            // gettimeofday — всегда настенное время (у него нет clockid).
            let us = crate::clock::realtime_ns() / 1000;
            let tv = [(us / 1_000_000), (us % 1_000_000)];
            let mut buf = [0u8; 16];
            buf[0..8].copy_from_slice(&tv[0].to_le_bytes());
            buf[8..16].copy_from_slice(&tv[1].to_le_bytes());
            ret = if a0 == 0 || lx_put(t, cur, a0, &buf) { 0 } else { linux::err(linux::EFAULT) };
        }
        // Веха 114 — сон стал НАСТОЯЩИМ. Раньше обе эти заглушки возвращали 0 не поспав, и
        // программа, честно попросившая подождать, получала busy-loop: «спит» — а машина занята.
        // Теперь кладём на тот же механизм срока, что `futex_wait` и `SYS_SLEEP`.
        //
        // `nanosleep(req, rem)` и `clock_nanosleep(clockid, flags, req, rem)` — timespec из двух
        // 64-битных полей. Абсолютный срок (TIMER_ABSTIME=1) пересчитываем в относительный по
        // своим часам; `rem` не заполняем — просыпаться раньше срока у нас нечему (сигналов нет),
        // а значит остатка не бывает.
        Some(Lx::Nanosleep) | Some(Lx::ClockNanosleep) => {
            let a3 = t.procs[cur].frame.arg(3);
            let _ = a3; // `rem` не заполняем — просыпаться раньше срока у нас нечему
            let (req_va, abstime, clockid) = match decoded {
                Some(Lx::Nanosleep) => (a0, false, 1),
                _ => (a2, a1 & 1 != 0, a0),
            };
            match lx_get(t, cur, req_va, 16) {
                Some(ts) => {
                    let secs = u64::from_le_bytes(ts[0..8].try_into().unwrap());
                    let nsec = u64::from_le_bytes(ts[8..16].try_into().unwrap());
                    let want = secs.saturating_mul(1_000_000_000).saturating_add(nsec);
                    let ns = if abstime {
                        let now = if clockid == 0 {
                            crate::clock::realtime_ns()
                        } else {
                            crate::clock::uptime_ns()
                        };
                        want.saturating_sub(now)
                    } else {
                        want
                    };
                    if ns == 0 {
                        ret = 0;
                    } else {
                        let deadline =
                            arch::now_ticks().wrapping_add(crate::clock::ns_to_ticks(ns));
                        t.procs[cur].state = State::Sleeping;
                        t.procs[cur].futex_deadline = Some(deadline);
                        if let Some(n) = t.next_runnable(cur) {
                            t.current = n;
                        }
                        done = false; // спим: кадр и PC не трогаем, возврат оформит пробуждение
                    }
                }
                None => ret = linux::err(linux::EFAULT),
            }
        }
        Some(Lx::SchedYield) => {
            ret = 0;
            // мягко уступить: пометим ret и дадим общему эпилогу продвинуть; переключение
            // сделает следующий тик — для CLI этого достаточно.
        }
        // ── файловые (пока без ФС: заглушки, что не роняют однопоточный CLI) ─────
        Some(Lx::Ioctl) => ret = linux::err(linux::ENOTTY), // isatty/TIOCGWINSZ → «не терминал»
        Some(Lx::Fcntl) => ret = 0,
        Some(Lx::Close) => {
            // fd 0/1/2 — «закрыты», реальных ресурсов нет; файловые — освободить слот.
            if a0 >= LX_FD_BASE {
                if let Some(sl) = t.procs[cur].lx_fds.get_mut(a0 - LX_FD_BASE) {
                    *sl = None;
                }
            }
            ret = 0;
        }
        // ── файлы (Веха 108.3): читаются ПРЯМО ИЗ STORE, см. [`crate::lxfs`] ──────
        Some(Lx::Openat) | Some(Lx::Open) => {
            // openat(dirfd, path, flags, mode) либо legacy open(path, flags, mode) — разница
            // только в том, где лежит путь. Относительных путей у нас нет: cwd Linux-процесса
            // всегда `/`, а пакеты адресуются абсолютно — этого хватает и ld.so, и applet'ам.
            const O_WRONLY: usize = 0o1;
            const O_RDWR: usize = 0o2;
            const O_CREAT: usize = 0o100;
            let legacy = decoded == Some(Lx::Open);
            let (path_va, flags) = if legacy { (a0, a1) } else { (a1, a2) };
            ret = match lx_cstr(t, cur, path_va, 4096) {
                None => linux::err(linux::EFAULT),
                Some(path) if flags & (O_WRONLY | O_RDWR | O_CREAT) != 0 => {
                    // Только чтение — и сказать об этом надо честно, а не «нет файла».
                    vprintln!("  [linux] P{} openat на запись — EROFS", cur);
                    let _ = path;
                    linux::err(linux::EROFS)
                }
                Some(path) => match crate::lxfs::lookup(&path) {
                    Some(meta) => {
                        let fd = lx_fd_alloc(t, cur, meta, path);
                        vprintln!("  [linux] P{} openat → fd {}", cur, fd);
                        fd
                    }
                    None => linux::err(linux::ENOENT),
                },
            };
        }
        Some(Lx::Faccessat) | Some(Lx::Access) => {
            let path_va = if decoded == Some(Lx::Access) { a0 } else { a1 };
            ret = match lx_cstr(t, cur, path_va, 4096) {
                Some(path) if crate::lxfs::lookup(&path).is_some() => 0,
                Some(_) => linux::err(linux::ENOENT),
                None => linux::err(linux::EFAULT),
            };
        }
        Some(Lx::Readlinkat) | Some(Lx::Readlink) => {
            // legacy readlink(path, buf, size) — путь в ПЕРВОМ аргументе, dirfd'а нет.
            let (path_va, buf, len) = if decoded == Some(Lx::Readlink) {
                (a0, a1, a2)
            } else {
                (a1, a2, a(t, 3))
            };
            ret = match lx_cstr(t, cur, path_va, 4096) {
                None => linux::err(linux::EFAULT),
                Some(path) => match crate::lxfs::lookup(&path).and_then(|m| crate::lxfs::readlink(&m))
                {
                    Some(target) => {
                        let n = target.len().min(len);
                        // readlink НЕ дописывает нуль — так в Linux, и musl на это рассчитывает.
                        if lx_put(t, cur, buf, &target[..n]) {
                            n
                        } else {
                            linux::err(linux::EFAULT)
                        }
                    }
                    None => linux::err(linux::EINVAL), // не ссылка либо нет пути
                },
            };
        }
        Some(Lx::Getdents64) => {
            // Записи каталога в linux-формате: d_ino(8) d_off(8) d_reclen(2) d_type(1) имя+NUL.
            let (fd, buf, len) = (a0, a1, a2);
            let slot = fd.checked_sub(LX_FD_BASE).and_then(|i| t.procs[cur].lx_fds.get(i).cloned());
            match slot.flatten() {
                None => ret = linux::err(linux::EBADF),
                Some(f) => {
                    let entries = crate::lxfs::dir_entries(&f.path, &f.meta);
                    let mut out: Vec<u8> = Vec::new();
                    let mut pos = f.dpos;
                    while pos < entries.len() {
                        let (ty, name) = &entries[pos];
                        let reclen = (19 + name.len() + 1 + 7) & !7; // выравнивание на 8
                        if out.len() + reclen > len {
                            break;
                        }
                        // d_ino/d_off — не несут смысла в content-адресуемом сторе; ставим номер
                        // записи: musl требует лишь монотонности и ненулевого d_ino.
                        out.extend_from_slice(&((pos as u64) + 1).to_le_bytes());
                        out.extend_from_slice(&((pos as u64) + 1).to_le_bytes());
                        out.extend_from_slice(&(reclen as u16).to_le_bytes());
                        out.push(if void_tree::is_dir(*ty) {
                            4 // DT_DIR
                        } else if void_tree::is_link(*ty) {
                            10 // DT_LNK
                        } else {
                            8 // DT_REG
                        });
                        out.extend_from_slice(name.as_bytes());
                        out.push(0);
                        while out.len() % 8 != 0 {
                            out.push(0);
                        }
                        pos += 1;
                    }
                    if let Some(Some(sl)) = t.procs[cur].lx_fds.get_mut(fd - LX_FD_BASE) {
                        sl.dpos = pos;
                    }
                    ret = if lx_put(t, cur, buf, &out) {
                        out.len()
                    } else {
                        linux::err(linux::EFAULT)
                    };
                }
            }
        }
        Some(Lx::Lseek) => {
            let (fd, off, whence) = (a0, a1 as i64, a2);
            if fd < LX_FD_BASE {
                ret = linux::err(linux::ESPIPE); // консоль не позиционируется
            } else {
                match t.procs[cur].lx_fds.get_mut(fd - LX_FD_BASE).and_then(|s| s.as_mut()) {
                    None => ret = linux::err(linux::EBADF),
                    Some(f) => {
                        let base = match whence {
                            1 => f.off as i64,
                            2 => f.meta.size as i64,
                            _ => 0,
                        };
                        let p = (base + off).clamp(0, f.meta.size as i64) as u64;
                        f.off = p;
                        ret = p as usize;
                    }
                }
            }
        }
        Some(Lx::Dup) | Some(Lx::Dup3) => ret = linux::err(linux::ENOSYS),
        Some(Lx::Ppoll) => ret = linux::err(linux::ENOSYS),
        Some(Lx::Fstat) if a0 >= LX_FD_BASE => {
            // fstat файла из store: тип и размер знает узел дерева.
            let (fd, buf) = (a0, a1);
            let slot = t.procs[cur].lx_fds.get(fd - LX_FD_BASE).cloned().flatten();
            ret = match slot {
                None => linux::err(linux::EBADF),
                Some(f) => {
                    let mut st = alloc::vec![0u8; linux::STAT_SIZE];
                    linux::fill_stat_file(&mut st, f.meta.size, f.meta.ty, crate::lxfs::ino(&f.meta));
                    if lx_put(t, cur, buf, &st) {
                        0
                    } else {
                        linux::err(linux::EFAULT)
                    }
                }
            };
        }
        Some(Lx::Fstat) => {
            // fstat(fd, buf): только символьные устройства stdin/out/err (fd 0/1/2).
            let (fd, buf) = (a0 as isize, a1);
            if (0..=2).contains(&fd) {
                let mut st = alloc::vec![0u8; linux::STAT_SIZE];
                linux::fill_stat_chr(&mut st);
                ret = if lx_put(t, cur, buf, &st) { 0 } else { linux::err(linux::EFAULT) };
            } else {
                ret = linux::err(linux::EBADF);
            }
        }
        Some(Lx::Stat) | Some(Lx::Lstat) => {
            // legacy stat(path, buf) / lstat(path, buf). Разницы между ними у нас нет: путь мы и
            // так не разыменовываем, а тип записи отдаём настоящий (см. долги вехи).
            let (path_va, buf) = (a0, a1);
            ret = match lx_cstr(t, cur, path_va, 4096) {
                None => linux::err(linux::EFAULT),
                Some(p) => match crate::lxfs::lookup(&p) {
                    Some(meta) => {
                        let mut st = alloc::vec![0u8; linux::STAT_SIZE];
                        linux::fill_stat_file(&mut st, meta.size, meta.ty, crate::lxfs::ino(&meta));
                        if lx_put(t, cur, buf, &st) { 0 } else { linux::err(linux::EFAULT) }
                    }
                    None => linux::err(linux::ENOENT),
                },
            };
        }
        Some(Lx::Newfstatat) => {
            // newfstatat(dirfd, path, buf, flags): без пути (AT_EMPTY_PATH) и fd 0/1/2 —
            // символьное устройство; с путём — ФС ещё нет, значит файла нет (ENOENT).
            const AT_EMPTY_PATH: usize = 0x1000;
            let (dirfd, path, buf, flags) = (a0 as isize, a1, a2, a(t, 3));
            let empty_path = flags & AT_EMPTY_PATH != 0
                || lx_get(t, cur, path, 1).map_or(true, |b| b.first() == Some(&0));
            if empty_path && (0..=2).contains(&dirfd) {
                let mut st = alloc::vec![0u8; linux::STAT_SIZE];
                linux::fill_stat_chr(&mut st);
                ret = if lx_put(t, cur, buf, &st) { 0 } else { linux::err(linux::EFAULT) };
            } else if empty_path && dirfd >= LX_FD_BASE as isize {
                // AT_EMPTY_PATH на открытом файле = fstat.
                let slot = t.procs[cur].lx_fds.get(dirfd as usize - LX_FD_BASE).cloned().flatten();
                ret = match slot {
                    None => linux::err(linux::EBADF),
                    Some(f) => {
                        let mut st = alloc::vec![0u8; linux::STAT_SIZE];
                        linux::fill_stat_file(&mut st, f.meta.size, f.meta.ty, crate::lxfs::ino(&f.meta));
                        if lx_put(t, cur, buf, &st) { 0 } else { linux::err(linux::EFAULT) }
                    }
                };
            } else {
                // Веха 108.3 — путь ищется в store. AT_SYMLINK_NOFOLLOW нам безразличен: путь
                // и так не разыменовывается (см. долги вехи), а тип записи мы отдаём настоящий.
                ret = match lx_cstr(t, cur, path, 4096) {
                    None => linux::err(linux::EFAULT),
                    Some(p) => match crate::lxfs::lookup(&p) {
                        Some(meta) => {
                            let mut st = alloc::vec![0u8; linux::STAT_SIZE];
                            linux::fill_stat_file(&mut st, meta.size, meta.ty, crate::lxfs::ino(&meta));
                            if lx_put(t, cur, buf, &st) { 0 } else { linux::err(linux::EFAULT) }
                        }
                        None => linux::err(linux::ENOENT),
                    },
                };
            }
        }
        // ── завершение ────────────────────────────────────────────────────────────
        Some(Lx::Exit) | Some(Lx::ExitGroup) => {
            let code = a0 & 0xff;
            let leader = t.procs[cur].group;
            vprintln!("  [linux] P{} exit_group({}) — процесс P{}", cur, code, leader);
            for i in 0..t.procs.len() {
                if t.procs[i].group == leader {
                    t.procs[i].state = State::Finished;
                }
            }
            wake_exec_waiters(t, leader, code);
            if let Some(n) = t.next_runnable(cur) {
                t.current = n;
            }
            done = false; // процесс завершён — PC/кадр не трогаем
        }
        None => {
            vprintln!("  [linux] P{} НЕреализованный syscall #{} — ENOSYS", cur, nr);
            ret = linux::err(linux::ENOSYS);
        }
    }

    if done {
        let f = &mut t.procs[cur].frame;
        f.set_ret(ret);
        f.skip_syscall_insn(); // riscv: sepc+4; x86: rip+2 (пройти `syscall`)
    }
}

/// Скопировать `src` в адресное пространство с корнем `root` по виртуальному адресу `dst_va`,
/// постранично транслируя (страницы процесса не отображены идентично). Физический адрес назначения
/// доступен ядру через идентичное отображение RAM, поэтому переключать `satp` не нужно.
fn copy_to_space(root: usize, mut dst_va: usize, src: &[u8]) {
    let mut off = 0;
    while off < src.len() {
        let Some(pa) = arch::translate(root, dst_va) else { return };
        let page_off = dst_va & (PAGE - 1);
        let n = (src.len() - off).min(PAGE - page_off);
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr().add(off), crate::frame::ptr(pa), n) };
        off += n;
        dst_va += n;
    }
}

/// Скопировать `len` байт МЕЖДУ двумя адресными пространствами: из `src_va` (корень `src_root`)
/// в `dst_va` (корень `dst_root`). Оба конца транслируем постранично в физические адреса (RAM
/// идентично отображена в ядре → переключать `satp` не нужно); шаг ограничен границей страницы
/// с обеих сторон, т.к. буферы могут пересекать страницы независимо.
fn copy_between_spaces(
    src_root: usize,
    mut src_va: usize,
    dst_root: usize,
    mut dst_va: usize,
    len: usize,
) {
    let mut off = 0;
    while off < len {
        let (Some(spa), Some(dpa)) =
            (arch::translate(src_root, src_va), arch::translate(dst_root, dst_va))
        else {
            return;
        };
        let s_off = src_va & (PAGE - 1);
        let d_off = dst_va & (PAGE - 1);
        let n = (len - off).min(PAGE - s_off).min(PAGE - d_off);
        // Оба конца — физические адреса из translate; ходим по ним через direct-map.
        unsafe {
            core::ptr::copy_nonoverlapping(
                crate::frame::ptr(spa) as *const u8,
                crate::frame::ptr(dpa),
                n,
            )
        };
        off += n;
        src_va += n;
        dst_va += n;
    }
}

/// Доставить полезную нагрузку запроса: скопировать буфер отправителя `from` (`send_buf`/`send_len`)
/// в приёмный буфер получателя `to` (`recv_buf`/`recv_cap`), усекая по размеру приёмника.
/// Клиент в этот момент заблокирован — его память стабильна.
///
/// Веха 21.1: если отправитель передаёт capability (`send_cap` != MAX) — скопировать право в
/// домен получателя ([`cap::grant`], права как есть: аттенуация делается ЗАРАНЕЕ через
/// `CAP_DERIVE`) и зафиксировать c-space на диск ([`cap::persist`] — передача права = чекпойнт).
/// Возвращает (скопировано байт, дескриптор права у получателя | MAX).
fn deliver_request(t: &Table, from: usize, to: usize) -> (usize, usize) {
    let mut n = t.procs[from].send_len.min(t.procs[to].recv_cap);
    // Веха 101 — усечение по приёмнику остаётся (менять семантику на живой системе дороже, чем
    // она стоит), но перестаёт быть НЕВИДИМЫМ: доставленную длину получает и лог, и сам
    // отправитель (третьим значением `SYS_CALL`, см. места вызова). До этого запрос молча
    // обрезался, а вызов рапортовал успех — так пропала половина сеянного `terminal.vv`.
    if n < t.procs[from].send_len {
        vprintln!(
            "  [ipc] P{} → P{}: запрос УСЕЧЁН {} → {} байт (буфер приёмника мал)",
            from, to, t.procs[from].send_len, n,
        );
    }
    // Веха 23: буферы обеих сторон могут лежать в ленивых кучах — доотобразить, иначе
    // постраничная трансляция молча пропустила бы немапленные страницы.
    if n > 0
        && !(ensure_heap_range(t, from, t.procs[from].send_buf, n)
            && ensure_heap_range(t, to, t.procs[to].recv_buf, n))
    {
        n = 0; // фреймы кончились — честнее не доставить ничего
    }
    if n > 0 {
        copy_between_spaces(
            arch::space_root(t.procs[from].space),
            t.procs[from].send_buf,
            arch::space_root(t.procs[to].space),
            t.procs[to].recv_buf,
            n,
        );
    }
    let mut tcap = usize::MAX;
    if t.procs[from].send_cap != usize::MAX {
        let c = Cap::from_bits(t.procs[from].send_cap as u64);
        // GRANT проверен при отправке (`CALL`); маска без сужения — копия прав как есть.
        if let Ok(nc) = cap::grant(t.procs[from].domain, c, t.procs[to].domain, Rights(u32::MAX)) {
            tcap = nc.bits() as usize;
            vprintln!(
                "  [cap] P{} → P{}: право [{}] передано в сообщении (grant по IPC)",
                from, to, cap::rights_str(cap::rights(t.procs[to].domain, nc).unwrap_or(Rights::NONE)),
            );
            cap::persist();
        }
    }
    (n, tcap)
}
