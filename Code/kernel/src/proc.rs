//! Веха 10.2 — процессы: своё адресное пространство (satp) + кооперативное планирование.
//!
//! Процесс = изолированная единица исполнения в U-mode: собственная таблица страниц
//! ([[user-mode]], [[sv39-paging]]) и сохранённый trap-кадр. Ядро возобновляет «текущий»
//! процесс через [`enter_user_frame`] (переключение `satp` → восстановление регистров → `sret`).
//! Один hart, планирование **кооперативное**: процесс уступает через `SYS_YIELD` или завершается
//! через `SYS_EXIT` (прерывания в U-mode пока выключены).
//!
//! Модель без ядерных нитей на процесс: каждый trap из U обрабатывается на общем trap-стеке,
//! после чего ядро возобновляет тот процесс, что стал текущим ([`handle_user_trap`]).

use alloc::vec::Vec;
use core::ptr::{addr_of, addr_of_mut};

use void_abi::{Cap, ContentId, Rights};

use crate::context::{context_switch, Context};
use crate::sync::SpinLock;
use crate::trap::TrapFrame;
use crate::{cap, csr, frame, paging, println};

// Ассемблерная функция входа в процесс: satp + восстановление регистров из кадра + sret.
core::arch::global_asm!(include_str!("enter_user.s"));

extern "C" {
    /// Переключить `satp`, загрузить регистры из `*frame`, выставить `sscratch`=trap-стек и `sret`.
    fn enter_user_frame(frame: *const TrapFrame, satp: usize, trap_top: usize) -> !;
}

// ─── адресное пространство процесса ────────────────────────────────────────────
const SATP_SV39: usize = 8 << 60;
/// Стек процесса живёт в незанятом ядром регионе VPN[2]=1 (0x4000_0000..0x8000_0000),
/// растёт вниз от 0x8000_0000. Это гарантирует, что маппинг стека не заденет общие
/// подтаблицы ядра (VPN[2]=0 и 2) — см. [`paging::clone_kernel_root`].
const USER_STACK_TOP_VA: usize = 0x8000_0000;
const USER_STACK_PAGES: usize = 4;
const PAGE: usize = 4096;

// ─── ядерный trap-стек для trap'ов из U-mode ──────────────────────────────────
// 64 КиБ — как загрузочный стек ядра (linker.ld): syscall'ы делают настоящую работу
// (`object::put` → BLAKE3 + куча + `println!`), а в debug-сборке кадры крупные. С 16 КиБ
// стек переполнялся ВНИЗ в read-only секцию `.user` (store page fault на записи локали).
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
    Finished,
}

struct Proc {
    satp: usize,
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
static mut RETURN_CTX: Context = Context { ra: 0, sp: 0, s: [0; 12] };

// ─── создание и запуск ────────────────────────────────────────────────────────

/// Создать процесс: свой домен защиты (c-space) + адресное пространство (код `.user` общий,
/// стек приватный) + стартовый кадр (вход `entry`, `a0`=`arg`). Возвращает id процесса; он же —
/// адрес эндпоинта для [`cap::Target::Endpoint`]. Пока Runnable. Начальные capability ядро
/// минтит в его [`domain`] и передаёт дескриптор через [`set_arg`] ДО [`run`].
pub fn spawn(name: &'static str, entry: usize, arg: usize) -> usize {
    let root = paging::clone_kernel_root();
    // Приватный стек в VPN[2]=1: несколько страниц из свежих фреймов.
    for i in 1..=USER_STACK_PAGES {
        let va = USER_STACK_TOP_VA - i * PAGE;
        let pa = frame::alloc().expect("нет фрейма под стек процесса");
        unsafe { paging::map(root, va, pa, paging::PTE_R | paging::PTE_W | paging::PTE_U) };
    }
    let mut frame = TrapFrame::default();
    frame.sepc = entry;
    frame.regs[2] = USER_STACK_TOP_VA; // sp
    frame.regs[10] = arg; // a0
    frame.sstatus = 1 << 18; // SUM=1, SPP=0 (→U), SPIE=0 (прерывания в U выключены)
    let domain = cap::create_domain(name);
    let mut t = TABLE.lock();
    t.procs.push(Proc {
        satp: SATP_SV39 | (root >> 12),
        frame,
        state: State::Runnable,
        domain,
        recv_buf: 0,
        recv_cap: 0,
        send_buf: 0,
        send_len: 0,
    });
    t.procs.len() - 1
}

/// Домен защиты (c-space) процесса — сюда ядро минтит его начальные capability до [`run`].
pub fn domain(pid: usize) -> cap::DomainId {
    TABLE.lock().procs[pid].domain
}

/// Задать стартовый аргумент (`a0`) процесса до запуска — например, дескриптор capability,
/// который процесс предъявит в первом syscall'е.
pub fn set_arg(pid: usize, a0: usize) {
    TABLE.lock().procs[pid].frame.regs[10] = a0;
}

/// Запустить процессы и вернуться сюда, когда все завершатся. Сохраняем контекст ядра в
/// RETURN_CTX и уходим в лончер (как в [[scheduling|context_switch]]-переключении нитей).
pub fn run() {
    // Первый готовый процесс (может быть не индекс 0: после прошлой сессии часть процессов
    // остаётся Finished/заблокированными). Нет готовых — выходим сразу.
    let first = {
        let t = TABLE.lock();
        (0..t.procs.len()).find(|&i| t.procs[i].state == State::Runnable)
    };
    let Some(first) = first else { return };
    let sstatus_sie = csr::irq_save_disable(); // S-mode SIE (для ядерной стороны)
    // Замаскировать таймер/внешние в `sie`: иначе они прервут ПРОЦЕСС в U-mode (там sstatus.SIE
    // не действует) и наш обработчик примет их за неожиданный trap. Процессы кооперативные.
    let saved_sie = csr::read_sie();
    csr::write_sie(saved_sie & !((1 << 5) | (1 << 9))); // сбросить STIE и SEIE
    TABLE.lock().current = first;
    unsafe {
        let mut launch = Context::default();
        launch.ra = proc_enter as *const () as usize;
        launch.sp = trap_top();
        context_switch(addr_of_mut!(RETURN_CTX), addr_of!(launch));
    }
    // ── сюда возвращаемся, когда процессов не осталось ──
    csr::write_sscratch(0);
    csr::write_sie(saved_sie); // вернуть прежние разрешения прерываний
    csr::irq_restore(sstatus_sie);
}

/// Лончер: возобновить текущий (первый) процесс.
extern "C" fn proc_enter() -> ! {
    let (frame, satp) = {
        let t = TABLE.lock();
        let c = t.current;
        (t.procs[c].frame, t.procs[c].satp)
    };
    unsafe { enter_user_frame(&frame, satp, trap_top()) }
}

// ─── обработка trap'ов из U-mode ──────────────────────────────────────────────

/// Обработать trap из U-mode (системный вызов) и возобновить нужный процесс. Не возвращается.
pub fn handle_user_trap(frame: &mut TrapFrame, scause: usize) -> ! {
    {
        let mut t = TABLE.lock();
        let cur = t.current;
        t.procs[cur].frame = *frame; // сохранить состояние текущего процесса
        if scause == csr::EXC_ECALL_FROM_U {
            syscall(&mut t, cur);
        } else {
            println!("  [proc] неожиданный trap из U (scause={:#x}) — процесс завершён", scause);
            t.procs[cur].state = State::Finished;
            if let Some(n) = t.next_runnable(cur) {
                t.current = n;
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
            let satp = t.procs[n].satp;
            drop(t);
            unsafe { enter_user_frame(&frame, satp, trap_top()) }
        }
        None => {
            drop(t);
            unsafe {
                let mut discard = Context::default();
                context_switch(addr_of_mut!(discard), addr_of!(RETURN_CTX));
            }
            loop {} // не достигается
        }
    }
}

/// Диспетчер syscall'ов. Номер в `a7`, аргументы в `a0..`, результат в `a0`. Работает прямо
/// с таблицей: IPC-вызовы затрагивают состояния/кадры ДРУГИХ процессов и выбор `current`.
fn syscall(t: &mut Table, cur: usize) {
    let num = t.procs[cur].frame.regs[17]; // a7
    match num {
        // SYS_WRITE(ptr, len): напечатать буфер процесса (ядро читает U-память, SUM=1).
        1 => {
            let f = &mut t.procs[cur].frame;
            let (ptr, len) = (f.regs[10], f.regs[11]);
            let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
            crate::print!("{}", core::str::from_utf8(bytes).unwrap_or("<?>"));
            f.regs[10] = len;
            f.sepc += 4;
        }
        // SYS_EXIT(code): завершить процесс, уступить следующему готовому.
        2 => {
            println!("  [proc] P{} SYS_EXIT({})", cur, t.procs[cur].frame.regs[10]);
            t.procs[cur].state = State::Finished;
            if let Some(n) = t.next_runnable(cur) {
                t.current = n;
            }
        }
        // SYS_YIELD: уступить следующему готовому.
        3 => {
            t.procs[cur].frame.sepc += 4;
            if let Some(n) = t.next_runnable(cur) {
                t.current = n;
            }
        }
        // SYS_RECV(recv_buf, recv_cap) -> (a0=op, a1=отправитель, a2=длина запроса). Приняв
        // запрос, копируем его полезную нагрузку из буфера клиента в recv_buf. Нет запроса —
        // блокировка (RecvWait); recv_buf/cap сохранены, чтобы доставка позже скопировала в них.
        4 => {
            let (rbuf, rcap) = {
                let f = &t.procs[cur].frame;
                (f.regs[10], f.regs[11])
            };
            t.procs[cur].recv_buf = rbuf;
            t.procs[cur].recv_cap = rcap;
            if let Some(pos) = t.mailbox.iter().position(|&(_, to, _)| to == cur) {
                let (from, _to, op) = t.mailbox.remove(pos);
                let n = deliver_request(t, from, cur);
                let f = &mut t.procs[cur].frame;
                f.regs[10] = op;
                f.regs[11] = from;
                f.regs[12] = n;
                f.sepc += 4;
            } else {
                t.procs[cur].state = State::RecvWait; // sepc не двигаем: доставка сделает это
                if let Some(n) = t.next_runnable(cur) {
                    t.current = n;
                }
            }
        }
        // SYS_CALL(ep_cap, op, send_buf, send_len, recv_buf, recv_cap) -> a0 = число байт ответа
        // (или MAX, если cap не даёт права слать). `ep_cap` — дескриптор эндпоинта в c-space
        // процесса; ядро резолвит его в id сервера. `send_buf`/`send_len` — полезная нагрузка
        // запроса (копируется серверу при доставке). Отправить и ждать ответа (блокируется).
        5 => {
            let (ecap, op, sbuf, slen, rbuf, rcap) = {
                let f = &t.procs[cur].frame;
                (f.regs[10], f.regs[11], f.regs[12], f.regs[13], f.regs[14], f.regs[15])
            };
            let dom = t.procs[cur].domain;
            match cap::endpoint(dom, Cap::from_bits(ecap as u64)) {
                Ok(dest) => {
                    println!("  [ipc] P{} CALL P{} (по cap) op={} ({} байт)", cur, dest, op, slen);
                    t.procs[cur].recv_buf = rbuf;
                    t.procs[cur].recv_cap = rcap;
                    t.procs[cur].send_buf = sbuf;
                    t.procs[cur].send_len = slen;
                    if dest < t.procs.len() && t.procs[dest].state == State::RecvWait {
                        // получатель ждёт в RECV — доставить нагрузку в его буфер и разбудить.
                        let n = deliver_request(t, cur, dest);
                        let df = &mut t.procs[dest].frame;
                        df.regs[10] = op;
                        df.regs[11] = cur;
                        df.regs[12] = n;
                        df.sepc += 4;
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
                    f.regs[10] = usize::MAX;
                    f.sepc += 4;
                }
            }
        }
        // SYS_REPLY(dest, src_buf, len): ответить клиенту, передав `len` байт из своего буфера
        // в его приёмный буфер (копирование между адресными пространствами), и разбудить его.
        6 => {
            let f = &t.procs[cur].frame;
            let (dest, src, len) = (f.regs[10], f.regs[11], f.regs[12]);
            println!("  [ipc] P{} REPLY P{} ({} байт)", cur, dest, len);
            if dest < t.procs.len() && t.procs[dest].state == State::ReplyWait {
                let n = len.min(t.procs[dest].recv_cap);
                // Читаем из текущего (сервера) по SUM=1; пишем в адресное пространство клиента
                // через трансляцию его таблицы (физический адрес отображён в ядре идентично).
                let src_slice = unsafe { core::slice::from_raw_parts(src as *const u8, n) };
                let droot = root_of(t.procs[dest].satp);
                let dbuf = t.procs[dest].recv_buf;
                copy_to_space(droot, dbuf, src_slice);
                let df = &mut t.procs[dest].frame;
                df.regs[10] = n; // клиентский CALL вернёт число принятых байт
                df.sepc += 4;
                t.procs[dest].state = State::Runnable;
            }
            t.procs[cur].frame.sepc += 4; // сервер продолжает (остаётся current)
        }
        // SYS_BLK_READ(dev_cap, sector, buf): шлюз к диску ПОД ЗАЩИТОЙ capability. Без валидного
        // cap на устройство (право READ) — отказ, даже если процесс знает номер сектора. DMA идёт
        // в ЯДЕРНЫЙ буфер (страницы процесса не identity-mapped), затем копируем вызывающему (SUM=1).
        7 => {
            let (dcap, sector, ubuf) = {
                let f = &t.procs[cur].frame;
                (f.regs[10], f.regs[11], f.regs[12])
            };
            let dom = t.procs[cur].domain;
            let result = match cap::device(dom, Cap::from_bits(dcap as u64), Rights::READ) {
                Ok(cap::Device::Block) => {
                    println!("  [blk] P{} SYS_BLK_READ сектор {} (по cap)", cur, sector);
                    let mut tmp = [0u8; 512];
                    let ok = crate::virtio_blk::read(sector as u64, &mut tmp);
                    if ok {
                        let dst = unsafe { core::slice::from_raw_parts_mut(ubuf as *mut u8, 512) };
                        dst.copy_from_slice(&tmp);
                    }
                    if ok { 0 } else { usize::MAX }
                }
                Err(e) => {
                    println!("  [blk] P{} SYS_BLK_READ отклонён: {:?}  ← нет capability на устройство", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.regs[10] = result;
            f.sepc += 4;
        }
        // SYS_OBJ_PUT(store_cap, buf, len, id_out) -> 0/MAX: сохранить значение в объектный
        // [[object-model|store]] (нужен cap на store с правом WRITE) и записать 32-байтный
        // content-id в id_out. Буферы читаются/пишутся в пространстве вызывающего (он current, SUM=1).
        8 => {
            let (scap, buf, len, idout) = {
                let f = &t.procs[cur].frame;
                (f.regs[10], f.regs[11], f.regs[12], f.regs[13])
            };
            let dom = t.procs[cur].domain;
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::WRITE) {
                Ok(()) => {
                    let bytes = unsafe { core::slice::from_raw_parts(buf as *const u8, len) };
                    let id = crate::object::put(bytes);
                    let out = unsafe { core::slice::from_raw_parts_mut(idout as *mut u8, 32) };
                    out.copy_from_slice(&id.0);
                    println!("  [obj] P{} OBJ_PUT {} байт → content-id (по cap)", cur, len);
                    0
                }
                Err(e) => {
                    println!("  [obj] P{} OBJ_PUT отклонён: {:?}  ← нет capability на store", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.regs[10] = result;
            f.sepc += 4;
        }
        // SYS_OBJ_GET(store_cap, id_ptr, out_buf, out_cap) -> длина (0 — нет; MAX — отказ):
        // прочитать значение по 32-байтному content-id (нужен cap на store с правом READ).
        9 => {
            let (scap, idp, obuf, ocap) = {
                let f = &t.procs[cur].frame;
                (f.regs[10], f.regs[11], f.regs[12], f.regs[13])
            };
            let dom = t.procs[cur].domain;
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::READ) {
                Ok(()) => {
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
                Err(e) => {
                    println!("  [obj] P{} OBJ_GET отклонён: {:?}  ← нет capability на store", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.regs[10] = result;
            f.sepc += 4;
        }
        // SYS_OBJ_SET_ROOT(store_cap, name_ptr, name_len, id_ptr) -> 0/MAX: привязать именованный
        // корень к значению (нужен `WRITE`). Так объект переживает перезагрузку ([[persistent-store]]).
        10 => {
            let (scap, nptr, nlen, idp) = {
                let f = &t.procs[cur].frame;
                (f.regs[10], f.regs[11], f.regs[12], f.regs[13])
            };
            let dom = t.procs[cur].domain;
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::WRITE) {
                Ok(()) => {
                    let name_bytes = unsafe { core::slice::from_raw_parts(nptr as *const u8, nlen) };
                    let mut id = [0u8; 32];
                    let src = unsafe { core::slice::from_raw_parts(idp as *const u8, 32) };
                    id.copy_from_slice(src);
                    match core::str::from_utf8(name_bytes) {
                        Ok(name) => {
                            crate::object::set_root(name, ContentId(id));
                            println!("  [obj] P{} OBJ_SET_ROOT '{}' (по cap)", cur, name);
                            0
                        }
                        Err(_) => usize::MAX,
                    }
                }
                Err(e) => {
                    println!("  [obj] P{} OBJ_SET_ROOT отклонён: {:?}  ← нет capability на store", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.regs[10] = result;
            f.sepc += 4;
        }
        // SYS_OBJ_GET_ROOT(store_cap, name_ptr, name_len, id_out) -> 32 (есть) / 0 (нет) / MAX
        // (отказ): узнать content-id именованного корня (нужен `READ`).
        11 => {
            let (scap, nptr, nlen, idout) = {
                let f = &t.procs[cur].frame;
                (f.regs[10], f.regs[11], f.regs[12], f.regs[13])
            };
            let dom = t.procs[cur].domain;
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::READ) {
                Ok(()) => {
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
                Err(e) => {
                    println!("  [obj] P{} OBJ_GET_ROOT отклонён: {:?}  ← нет capability на store", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.regs[10] = result;
            f.sepc += 4;
        }
        other => {
            let f = &mut t.procs[cur].frame;
            println!("  [proc] неизвестный syscall {}", other);
            f.regs[10] = usize::MAX;
            f.sepc += 4;
        }
    }
}

/// Физический адрес корня таблицы страниц из значения `satp` (PPN → байтовый адрес).
fn root_of(satp: usize) -> usize {
    (satp & ((1usize << 44) - 1)) << 12
}

/// Скопировать `src` в адресное пространство с корнем `root` по виртуальному адресу `dst_va`,
/// постранично транслируя (страницы процесса не отображены идентично). Физический адрес назначения
/// доступен ядру через идентичное отображение RAM, поэтому переключать `satp` не нужно.
fn copy_to_space(root: usize, mut dst_va: usize, src: &[u8]) {
    let mut off = 0;
    while off < src.len() {
        let Some(pa) = paging::translate(root, dst_va) else { return };
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
            (paging::translate(src_root, src_va), paging::translate(dst_root, dst_va))
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
/// Возвращает число скопированных байт. Клиент в этот момент заблокирован — его память стабильна.
fn deliver_request(t: &Table, from: usize, to: usize) -> usize {
    let n = t.procs[from].send_len.min(t.procs[to].recv_cap);
    if n > 0 {
        copy_between_spaces(
            root_of(t.procs[from].satp),
            t.procs[from].send_buf,
            root_of(t.procs[to].satp),
            t.procs[to].recv_buf,
            n,
        );
    }
    n
}
