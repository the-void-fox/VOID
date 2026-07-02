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

use crate::context::{context_switch, Context};
use crate::sync::SpinLock;
use crate::trap::TrapFrame;
use crate::{csr, frame, paging, println};

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
const TRAP_STACK_SIZE: usize = 16 * 1024;

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
    /// Приёмный буфер клиента для ответа (VA в его адресном пространстве) — задаётся в CALL,
    /// заполняется в REPLY (передача буфера через IPC).
    recv_buf: usize,
    recv_cap: usize,
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

/// Создать процесс: своё адресное пространство (код `.user` общий, стек приватный),
/// стартовый кадр (вход `entry`, `a0`=`arg`). Пока Runnable.
pub fn spawn(entry: usize, arg: usize) {
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
    TABLE.lock().procs.push(Proc {
        satp: SATP_SV39 | (root >> 12),
        frame,
        state: State::Runnable,
        recv_buf: 0,
        recv_cap: 0,
    });
}

/// Запустить процессы и вернуться сюда, когда все завершатся. Сохраняем контекст ядра в
/// RETURN_CTX и уходим в лончер (как в [[scheduling|context_switch]]-переключении нитей).
pub fn run() {
    if TABLE.lock().procs.is_empty() {
        return;
    }
    let sstatus_sie = csr::irq_save_disable(); // S-mode SIE (для ядерной стороны)
    // Замаскировать таймер/внешние в `sie`: иначе они прервут ПРОЦЕСС в U-mode (там sstatus.SIE
    // не действует) и наш обработчик примет их за неожиданный trap. Процессы кооперативные.
    let saved_sie = csr::read_sie();
    csr::write_sie(saved_sie & !((1 << 5) | (1 << 9))); // сбросить STIE и SEIE
    TABLE.lock().current = 0;
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
        // SYS_RECV -> (a0 = сообщение, a1 = отправитель). Блокируется, если запросов нет.
        4 => {
            if let Some(pos) = t.mailbox.iter().position(|&(_, to, _)| to == cur) {
                let (from, _to, msg) = t.mailbox.remove(pos);
                let f = &mut t.procs[cur].frame;
                f.regs[10] = msg;
                f.regs[11] = from;
                f.sepc += 4;
            } else {
                t.procs[cur].state = State::RecvWait; // sepc не двигаем: доставка сделает это
                if let Some(n) = t.next_runnable(cur) {
                    t.current = n;
                }
            }
        }
        // SYS_CALL(dest, msg, recv_buf, recv_cap) -> a0 = число принятых байт. Отправить запрос
        // и ждать ответа (блокируется); recv_buf/cap — куда положить данные ответа.
        5 => {
            let f = &t.procs[cur].frame;
            let (dest, msg, rbuf, rcap) = (f.regs[10], f.regs[11], f.regs[12], f.regs[13]);
            println!("  [ipc] P{} CALL P{} msg={}", cur, dest, msg);
            t.procs[cur].recv_buf = rbuf;
            t.procs[cur].recv_cap = rcap;
            if dest < t.procs.len() && t.procs[dest].state == State::RecvWait {
                // получатель ждёт в RECV — доставить напрямую и разбудить.
                let df = &mut t.procs[dest].frame;
                df.regs[10] = msg;
                df.regs[11] = cur;
                df.sepc += 4;
                t.procs[dest].state = State::Runnable;
            } else {
                t.mailbox.push((cur, dest, msg)); // получит при следующем RECV
            }
            t.procs[cur].state = State::ReplyWait; // sepc двинет доставка ответа
            if let Some(n) = t.next_runnable(cur) {
                t.current = n;
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
                let droot = (t.procs[dest].satp & ((1usize << 44) - 1)) << 12;
                let dbuf = t.procs[dest].recv_buf;
                copy_to_space(droot, dbuf, src_slice);
                let df = &mut t.procs[dest].frame;
                df.regs[10] = n; // клиентский CALL вернёт число принятых байт
                df.sepc += 4;
                t.procs[dest].state = State::Runnable;
            }
            t.procs[cur].frame.sepc += 4; // сервер продолжает (остаётся current)
        }
        // SYS_BLK_READ(sector, buf): привилегированный шлюз к диску. DMA идёт в ЯДЕРНЫЙ буфер
        // (страницы процесса не identity-mapped), затем копируем в буфер вызывающего (SUM=1).
        7 => {
            let sector = t.procs[cur].frame.regs[10];
            let ubuf = t.procs[cur].frame.regs[11];
            println!("  [blk] P{} SYS_BLK_READ сектор {}", cur, sector);
            let mut tmp = [0u8; 512];
            let ok = crate::virtio_blk::read(sector as u64, &mut tmp);
            if ok {
                let dst = unsafe { core::slice::from_raw_parts_mut(ubuf as *mut u8, 512) };
                dst.copy_from_slice(&tmp);
            }
            let f = &mut t.procs[cur].frame;
            f.regs[10] = if ok { 0 } else { usize::MAX };
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
