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

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::ptr::{addr_of, addr_of_mut};

use void_abi::{Cap, ContentId, Rights};

use crate::arch::{self, Context, FaultKind, TrapFrame, UserTrap};
use crate::sync::SpinLock;
use crate::{cap, elf, frame, println, timer};

// ─── адресное пространство процесса ────────────────────────────────────────────
/// Стек процесса живёт в незанятом ядром регионе VPN[2]=1 (0x4000_0000..0x8000_0000),
/// растёт вниз от 0x8000_0000. Это гарантирует, что маппинг стека не заденет общие
/// подтаблицы ядра (VPN[2]=0 и 2) — см. [`arch::clone_kernel_root`].
const USER_STACK_TOP_VA: usize = 0x8000_0000;
const USER_STACK_PAGES: usize = 4;
const PAGE: usize = 4096;
/// Веха 22.1: куча процесса растёт вверх отсюда (код/данные ELF ниже, стек — выше, у
/// 0x8000_0000). `SYS_MAP` только резервирует диапазон [heap_base, heap_brk); страницы
/// выделяются ЛЕНИВО — по page fault ([`handle_user_fault`]).
const USER_HEAP_BASE_VA: usize = 0x6000_0000;
/// Начало региона VPN[2]=1 — весь тот же незанятый ядром диапазон, где живёт стек процесса, но
/// теперь ещё и код/данные ELF-программ (Веха 19, [`spawn_elf`]). Совпадает с базой линковки
/// `programs/*/linker.ld`; [`crate::elf::load`] отвергает сегменты ниже этого адреса.
pub const USER_REGION_START: usize = 0x4000_0000;

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
    /// Веха 22.1: ленивая куча процесса — зарезервированный `SYS_MAP` диапазон
    /// [`USER_HEAP_BASE_VA`, heap_brk). Фолт внутри — выделить страницу; вне — гибель процесса.
    heap_brk: usize,
}

struct Table {
    procs: Vec<Proc>,
    current: usize,
    /// Недоставленные запросы IPC: (отправитель, получатель, сообщение).
    mailbox: Vec<(usize, usize, usize)>,
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
    SpinLock::new(Table { procs: Vec::new(), current: 0, mailbox: Vec::new() });

/// Контекст ядра, в который возвращаемся, когда все процессы завершились.
static mut RETURN_CTX: Context = Context::EMPTY;

// ─── создание и запуск ────────────────────────────────────────────────────────

/// Новое адресное пространство процесса: клон корня ядра (ядро отображено без флага U — нужно
/// trap-обработчику при satp процесса) + приватный стек в незанятом регионе VPN[2]=1. Код и
/// данные добавит [`crate::elf::load`]: с Вехи 23 процессы приходят ТОЛЬКО из ELF в store.
fn new_address_space() -> usize {
    let root = arch::clone_kernel_root();
    // Приватный стек в VPN[2]=1: несколько страниц из свежих фреймов.
    for i in 1..=USER_STACK_PAGES {
        let va = USER_STACK_TOP_VA - i * PAGE;
        let pa = frame::alloc().expect("нет фрейма под стек процесса");
        unsafe { arch::map(root, va, pa, arch::MAP_R | arch::MAP_W | arch::MAP_U) };
    }
    root
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
    t.procs.push(Proc {
        space: arch::space_token(root),
        frame,
        state: State::Runnable,
        domain,
        recv_buf: 0,
        recv_cap: 0,
        send_buf: 0,
        send_len: 0,
        send_cap: usize::MAX,
        heap_brk: USER_HEAP_BASE_VA, // куча пуста, пока процесс не попросит SYS_MAP
    });
    t.procs.len() - 1
}

/// Веха 19.2/19.3 — создать процесс из статического ELF64/RISC-V (с Вехи 23 — единственный
/// способ): новое адресное пространство, [`crate::elf::load`] разбирает `elf` и маппит его `PT_LOAD`-
/// сегменты по правам `p_flags` (W^X), точка входа — `e_entry` файла, а не адрес функции в
/// образе ядра. `elf` может быть чем угодно (в т.ч. байтами, прочитанными [[object-model|из
/// store]] по content-id, см. `main::exec_demo`) — загрузчик не предполагает, что они лежат
/// где-то конкретно, копирует их в свежие фреймы процесса. Отказ парсинга/раскладки ELF не
/// заводит процесс и не трогает таблицу — вызывающий получает [`elf::ElfError`].
pub fn spawn_elf(name: &'static str, elf_bytes: &[u8], arg: usize) -> Result<usize, elf::ElfError> {
    let root = new_address_space();
    // Верхняя граница адресов ELF — начало региона кучи (Веха 22): раскладка процесса —
    // код/данные ELF ниже USER_HEAP_BASE_VA, куча над ними, стек у самого верха.
    let entry = elf::load(root, elf_bytes, USER_HEAP_BASE_VA)?;
    Ok(create_process(name, root, entry, arg))
}

/// Домен защиты (c-space) процесса — сюда ядро минтит его начальные capability до [`run`].
pub fn domain(pid: usize) -> cap::DomainId {
    TABLE.lock().procs[pid].domain
}

/// Задать стартовый аргумент (`a0`) процесса до запуска — например, дескриптор capability,
/// который процесс предъявит в первом syscall'е.
pub fn set_arg(pid: usize, a0: usize) {
    TABLE.lock().procs[pid].frame.set_ret_at(0, a0);
}

/// Задать второй стартовый аргумент (`a1`) — когда начальных capability у процесса два
/// (Веха 20: vsh получает эндпоинт персоналии в `a0` и exec-cap на store в `a1`).
pub fn set_arg2(pid: usize, a1: usize) {
    TABLE.lock().procs[pid].frame.set_ret_at(1, a1);
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
    let waiting: Vec<usize> = {
        let t = TABLE.lock();
        (0..t.procs.len()).filter(|&i| t.procs[i].state == State::StdinWait).collect()
    };
    if waiting.is_empty() {
        return false;
    }
    // Только внешние прерывания (SEIE): исполнять некого, таймер (STIE) не нужен.
    arch::irq_mask_stdin(saved_sie);
    while !arch::console_has_input() {
        // Спать до прерывания: проснёмся и от PENDING-прерывания при выключенном SIE.
        arch::wait_for_interrupt();
        // Короткое окно с прерываниями — принять trap: контроллер → консоль → кольцевой буфер.
        arch::enable_interrupts();
        arch::irq_save_disable();
    }
    let mut t = TABLE.lock();
    for pid in waiting {
        t.procs[pid].state = State::Runnable;
    }
    true
}

/// Разбудить процессы, ждущие в `SYS_EXEC` завершения ребёнка `child` (Веха 20.3): вернуть им
/// код выхода `code`, продвинуть sepc (их ecall завершён) и сделать готовыми.
fn wake_exec_waiters(t: &mut Table, child: usize, code: usize) {
    for i in 0..t.procs.len() {
        if t.procs[i].state == State::ExecWait(child) {
            let f = &mut t.procs[i].frame;
            f.set_ret(code);
            f.advance();
            t.procs[i].state = State::Runnable;
        }
    }
}

/// Веха 22.2: page fault из U-mode. Фолт чтения/записи в ленивом диапазоне кучи
/// [`USER_HEAP_BASE_VA`, heap_brk) — выделить обнулённый фрейм, замапить `U|R|W` и повторить
/// инструкцию (sepc не двигаем). Любой другой фолт — включая исполнение кучи (W^X живёт и
/// здесь) и исчерпание фреймов — гибель ПРОЦЕССА, а не ядра: родителю в `SYS_EXEC` уходит MAX.
fn handle_user_fault(t: &mut Table, cur: usize, va: usize, kind: FaultKind) {
    let (heap_brk, space) = (t.procs[cur].heap_brk, t.procs[cur].space);
    let lazy = va >= USER_HEAP_BASE_VA && va < heap_brk && kind != FaultKind::Exec;
    if lazy {
        if let Some(pa) = frame::alloc() {
            let page_va = va & !(PAGE - 1);
            // SAFETY: пространство процесса сейчас активно — после map сбрасываем TLB,
            // иначе повтор инструкции мог бы увидеть старую (пустую) трансляцию.
            unsafe { arch::map(arch::space_root(space), page_va, pa, arch::MAP_R | arch::MAP_W | arch::MAP_U) };
            arch::flush_tlb();
            println!("  [mm] P{} +страница {:#x} (ленивый фолт кучи)", cur, page_va);
            return; // sepc не тронут — инструкция повторится по замапленной странице
        }
        println!("  [mm] P{} фолт кучи {:#x}: фреймы кончились — процесс убит", cur, va);
    } else {
        println!(
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
    if len == 0 || va < USER_HEAP_BASE_VA || va.saturating_add(len) > t.procs[pid].heap_brk {
        return true; // не куча — обычные (уже отображённые) страницы
    }
    let root = arch::space_root(t.procs[pid].space);
    let mut page = va & !(PAGE - 1);
    while page < va + len {
        if arch::translate(root, page).is_none() {
            let Some(pa) = frame::alloc() else { return false };
            unsafe { arch::map(root, page, pa, arch::MAP_R | arch::MAP_W | arch::MAP_U) };
            arch::flush_tlb();
            println!("  [mm] P{} +страница {:#x} (доотображение под шлюз)", pid, page);
        }
        page += PAGE;
    }
    true
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
        t.procs[cur].frame = *frame; // сохранить состояние текущего процесса
        match trap {
            UserTrap::Syscall => syscall(&mut t, cur),
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
                println!("  [proc] неожиданный trap из U (код {:#x}) — процесс завершён", code);
                t.procs[cur].state = State::Finished;
                wake_exec_waiters(&mut t, cur, usize::MAX); // упавший ребёнок = MAX родителю
                if let Some(n) = t.next_runnable(cur) {
                    t.current = n;
                }
            }
        }
    }
    resume();
}

/// Возобновить текущий процесс (или, если он не готов, следующий готовый). Если готовых нет
/// (все завершены или заблокированы) — вернуться в ядро (в [`run`]). Не возвращается.
fn resume() -> ! {
    let mut t = TABLE.lock();
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
            drop(t);
            unsafe { arch::enter_user(&frame, space, trap_top()) }
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
            println!("  [proc] P{} SYS_EXIT({})", cur, code);
            t.procs[cur].state = State::Finished;
            wake_exec_waiters(t, cur, code);
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
        // SYS_RECV(recv_buf, recv_cap) -> (a0=op, a1=отправитель, a2=длина запроса, a3=принятое
        // право|MAX — Веха 21.1). Приняв запрос, копируем его полезную нагрузку из буфера клиента
        // в recv_buf; если клиент передал capability — она уже скопирована в домен сервера
        // (deliver_request), в a3 — её дескриптор. Нет запроса — блокировка (RecvWait);
        // recv_buf/cap сохранены, чтобы доставка позже скопировала в них.
        4 => {
            let (rbuf, rcap) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1))
            };
            t.procs[cur].recv_buf = rbuf;
            t.procs[cur].recv_cap = rcap;
            if let Some(pos) = t.mailbox.iter().position(|&(_, to, _)| to == cur) {
                let (from, _to, op) = t.mailbox.remove(pos);
                let (n, tcap) = deliver_request(t, from, cur);
                let rc = cap::mint(t.procs[cur].domain, cap::Target::Reply(from), Rights::SEND);
                let f = &mut t.procs[cur].frame;
                f.set_ret(op);
                f.set_ret_at(1, rc.bits() as usize);
                f.set_ret_at(2, n);
                f.set_ret_at(3, tcap);
                f.advance();
            } else {
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
                    println!("  [cap] P{} CALL отклонён: нет права GRANT на передаваемую capability", cur);
                    let f = &mut t.procs[cur].frame;
                    f.set_ret(usize::MAX);
                    f.advance();
                    return;
                }
            }
            match cap::endpoint(dom, Cap::from_bits(ecap as u64)) {
                Ok(dest) => {
                    println!("  [ipc] P{} CALL P{} (по cap) op={} ({} байт)", cur, dest, op, slen);
                    t.procs[cur].recv_buf = rbuf;
                    t.procs[cur].recv_cap = rcap;
                    t.procs[cur].send_buf = sbuf;
                    t.procs[cur].send_len = slen;
                    t.procs[cur].send_cap = scap;
                    if dest < t.procs.len() && t.procs[dest].state == State::RecvWait {
                        // получатель ждёт в RECV — доставить нагрузку в его буфер и разбудить.
                        // Выдать серверу одноразовый reply-cap на этого клиента (см. [[reply-capability]]).
                        let (n, tcap) = deliver_request(t, cur, dest);
                        let rc = cap::mint(t.procs[dest].domain, cap::Target::Reply(cur), Rights::SEND);
                        let df = &mut t.procs[dest].frame;
                        df.set_ret(op);
                        df.set_ret_at(1, rc.bits() as usize);
                        df.set_ret_at(2, n);
                        df.set_ret_at(3, tcap);
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
                    println!("  [ipc] P{} CALL отклонён: {:?}  ← нет capability на эндпоинт", cur, e);
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
                    println!("  [cap] P{} REPLY отклонён: нет права GRANT на передаваемую capability", cur);
                    let f = &mut t.procs[cur].frame;
                    f.set_ret(usize::MAX);
                    f.advance();
                    return;
                }
            }
            let result = match cap::reply_endpoint(dom, Cap::from_bits(rcap as u64)) {
                Ok(dest) => {
                    println!("  [ipc] P{} REPLY P{} ({} байт)", cur, dest, len);
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
                                println!(
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
                        df.advance();
                        t.procs[dest].state = State::Runnable;
                    }
                    let _ = cap::revoke(dom, Cap::from_bits(rcap as u64)); // одноразовость
                    0
                }
                Err(e) => {
                    println!("  [ipc] P{} REPLY отклонён: {:?}  ← нет reply-capability", cur, e);
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
                    println!("  [blk] P{} SYS_BLK_READ сектор {} (по cap)", cur, sector);
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
                    println!("  [blk] P{} SYS_BLK_READ отклонён: {:?}  ← нет capability на устройство", cur, e);
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
                    let id = crate::object::put(bytes);
                    let out = unsafe { core::slice::from_raw_parts_mut(idout as *mut u8, 32) };
                    out.copy_from_slice(&id.0);
                    println!("  [obj] P{} OBJ_PUT {} байт → content-id (по cap)", cur, len);
                    0
                }
                Ok(()) => usize::MAX, // куча есть, а фреймов нет
                Err(e) => {
                    println!("  [obj] P{} OBJ_PUT отклонён: {:?}  ← нет capability на store", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_OBJ_GET(store_cap, id_ptr, out_buf, out_cap) -> длина (0 — нет; MAX — отказ):
        // прочитать значение по 32-байтному content-id (нужен cap на store с правом READ).
        9 => {
            let (scap, idp, obuf, ocap) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::READ) {
                // Веха 22.2: приёмный буфер (и id_ptr — Веха 23) может лежать в ленивой куче —
                // доотобразить до записи ядром (весь ocap: лениво он выделился бы всё равно).
                Ok(()) if ensure_heap_range(t, cur, obuf, ocap)
                    && ensure_heap_range(t, cur, idp, 32) => {
                    let mut id = [0u8; 32];
                    let src = unsafe { core::slice::from_raw_parts(idp as *const u8, 32) };
                    id.copy_from_slice(src);
                    let n = crate::object::with(&ContentId(id), |b| match b {
                        Some(bytes) => {
                            let m = bytes.len().min(ocap);
                            let out = unsafe { core::slice::from_raw_parts_mut(obuf as *mut u8, m) };
                            out.copy_from_slice(&bytes[..m]);
                            m
                        }
                        None => 0,
                    });
                    println!("  [obj] P{} OBJ_GET → {} байт (по cap)", cur, n);
                    n
                }
                Ok(()) => usize::MAX, // куча есть, а фреймов нет
                Err(e) => {
                    println!("  [obj] P{} OBJ_GET отклонён: {:?}  ← нет capability на store", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
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
                            // Веха 20: смена корня из userspace = атомарный чекпойнт (A/B-индекс,
                            // [[persistent-store]]). Иначе файлы интерактивной сессии жили бы
                            // только до выключения QEMU (kmain-commit к этому моменту уже прошёл).
                            crate::object::commit();
                            println!("  [obj] P{} OBJ_SET_ROOT '{}' (по cap, чекпойнт)", cur, name);
                            0
                        }
                        Err(_) => usize::MAX,
                    }
                }
                Ok(()) => usize::MAX, // куча есть, а фреймов нет
                Err(e) => {
                    println!("  [obj] P{} OBJ_SET_ROOT отклонён: {:?}  ← нет capability на store", cur, e);
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
                                println!("  [obj] P{} OBJ_GET_ROOT '{}' → есть (по cap)", cur, name);
                                32
                            }
                            None => {
                                println!("  [obj] P{} OBJ_GET_ROOT '{}' → нет (по cap)", cur, name);
                                0
                            }
                        },
                        Err(_) => usize::MAX,
                    }
                }
                Ok(()) => usize::MAX, // куча есть, а фреймов нет
                Err(e) => {
                    println!("  [obj] P{} OBJ_GET_ROOT отклонён: {:?}  ← нет capability на store", cur, e);
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
                    println!("  [blk] P{} SYS_BLK_WRITE сектор {} ({} байт, по cap)", cur, sector, n);
                    if ok { 0 } else { usize::MAX }
                }
                Ok(_) => usize::MAX, // право есть, а фреймов под ленивый буфер нет
                Err(e) => {
                    println!("  [blk] P{} SYS_BLK_WRITE отклонён: {:?}  ← нет capability (WRITE) на устройство", cur, e);
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
                            let existed = crate::object::del_root(name);
                            if existed {
                                crate::object::commit(); // Веха 20: снятие корня — тоже чекпойнт
                            }
                            println!("  [obj] P{} OBJ_DEL_ROOT '{}' → {} (по cap)", cur, name, if existed { "снят" } else { "не было" });
                            if existed { 0 } else { 1 }
                        }
                        Err(_) => usize::MAX,
                    }
                }
                Ok(()) => usize::MAX, // куча есть, а фреймов нет
                Err(e) => {
                    println!("  [obj] P{} OBJ_DEL_ROOT отклонён: {:?}  ← нет capability на store", cur, e);
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
            let len = t.procs[cur].frame.arg(0);
            let start = t.procs[cur].heap_brk;
            let end = start.saturating_add(len.div_ceil(PAGE) * PAGE);
            let limit = USER_STACK_TOP_VA - USER_STACK_PAGES * PAGE;
            let result = if len == 0 || end > limit {
                usize::MAX
            } else {
                t.procs[cur].heap_brk = end;
                println!(
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
                    println!(
                        "  [cap] P{} CAP_DERIVE → копия с правами [{}] (аттенуация)",
                        cur,
                        cap::rights_str(cap::rights(dom, nc).unwrap_or(Rights::NONE)),
                    );
                    cap::persist();
                    nc.bits() as usize
                }
                Err(e) => {
                    println!("  [cap] P{} CAP_DERIVE отклонён: {:?}", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_READ(buf, cap) -> n: прочитать доступный ввод консоли (stdin) в буфер процесса —
        // хотя бы один байт. Ввода нет — процесс блокируется (StdinWait), sepc НЕ двигаем:
        // когда [`wait_stdin`] разбудит его по прерыванию UART, `ecall` РЕСТАРТУЕТ и на этот
        // раз заберёт байты из кольцевого буфера (Веха 20.2).
        14 => {
            let (buf, cap_len) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1))
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
            if n > 0 {
                let f = &mut t.procs[cur].frame;
                f.set_ret(n);
                f.advance();
            } else {
                t.procs[cur].state = State::StdinWait;
                if let Some(nx) = t.next_runnable(cur) {
                    t.current = nx;
                }
            }
        }
        // SYS_EXEC(store_cap, name_ptr, name_len) -> код выхода ребёнка / MAX: запустить программу
        // из store ПО ИМЕНИ КОРНЯ и ждать её завершения (foreground, Веха 20.3). Требует права
        // `EXEC` на store — ОТДЕЛЬНОГО от READ/WRITE: обладатель может запускать программы, не
        // умея читать или менять объекты (аттенуация «только запуск»). Путь тот же, что в
        // `exec_demo` ([[exec-from-store]]): корень → content-id → байты ELF → [`elf::load`].
        15 => {
            let (scap, nptr, nlen) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            let dom = t.procs[cur].domain;
            let mut spawned = false;
            match cap::store(dom, Cap::from_bits(scap as u64), Rights::EXEC) {
                // Веха 23: имя может лежать в ленивой куче — доотобразить до чтения ядром.
                Ok(()) if ensure_heap_range(t, cur, nptr, nlen) => {
                    let name_bytes = unsafe { core::slice::from_raw_parts(nptr as *const u8, nlen) };
                    if let Ok(name) = core::str::from_utf8(name_bytes) {
                        // Байты ELF копируем из store и сразу отпускаем его замок.
                        let elf_bytes = crate::object::root(name)
                            .and_then(|id| crate::object::with(&id, |b| b.map(Vec::from)));
                        match elf_bytes {
                            Some(bytes) => {
                                let root = new_address_space();
                                match elf::load(root, &bytes, USER_HEAP_BASE_VA) {
                                    Ok(entry) => {
                                        // Имя процесса обязано жить дольше таблицы — утекает
                                        // (запусков за сессию единицы, приемлемо до Вехи 22).
                                        let pname: &'static str =
                                            Box::leak(String::from(name).into_boxed_str());
                                        let child = create_process_locked(t, pname, root, entry, 0);
                                        println!(
                                            "  [exec] P{} SYS_EXEC '{}' → P{} (по cap, ждёт завершения)",
                                            cur, name, child,
                                        );
                                        // Родитель ждёт ребёнка; sepc/a0 выставит wake_exec_waiters.
                                        t.procs[cur].state = State::ExecWait(child);
                                        t.current = child;
                                        spawned = true;
                                    }
                                    Err(e) => println!(
                                        "  [exec] P{} SYS_EXEC '{}': негодный ELF: {:?}",
                                        cur, name, e,
                                    ),
                                }
                            }
                            None => println!("  [exec] P{} SYS_EXEC: корня '{}' нет в store", cur, name),
                        }
                    }
                }
                Ok(()) => println!("  [exec] P{} SYS_EXEC: фреймы кончились под ленивый буфер имени", cur),
                Err(e) => println!(
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
        other => {
            let f = &mut t.procs[cur].frame;
            println!("  [proc] неизвестный syscall {}", other);
            f.set_ret(usize::MAX);
            f.advance();
        }
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
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr().add(off), pa as *mut u8, n) };
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
        unsafe { core::ptr::copy_nonoverlapping(spa as *const u8, dpa as *mut u8, n) };
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
            println!(
                "  [cap] P{} → P{}: право [{}] передано в сообщении (grant по IPC)",
                from, to, cap::rights_str(cap::rights(t.procs[to].domain, nc).unwrap_or(Rights::NONE)),
            );
            cap::persist();
        }
    }
    (n, tcap)
}
