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
    Finished,
}

struct Proc {
    satp: usize,
    frame: TrapFrame,
    state: State,
}

struct Table {
    procs: Vec<Proc>,
    current: usize,
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

static TABLE: SpinLock<Table> = SpinLock::new(Table { procs: Vec::new(), current: 0 });

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
    TABLE.lock().procs.push(Proc { satp: SATP_SV39 | (root >> 12), frame, state: State::Runnable });
}

/// Запустить процессы и вернуться сюда, когда все завершатся. Сохраняем контекст ядра в
/// RETURN_CTX и уходим в лончер (как в [[scheduling|context_switch]]-переключении нитей).
pub fn run() {
    if TABLE.lock().procs.is_empty() {
        return;
    }
    let sie = csr::irq_save_disable(); // на время процессов прерывания не нужны
    TABLE.lock().current = 0;
    unsafe {
        let mut launch = Context::default();
        launch.ra = proc_enter as *const () as usize;
        launch.sp = trap_top();
        context_switch(addr_of_mut!(RETURN_CTX), addr_of!(launch));
    }
    // ── сюда возвращаемся, когда процессов не осталось ──
    csr::write_sscratch(0);
    csr::irq_restore(sie);
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

enum Action {
    Continue, // остаться на том же процессе
    Yield,    // уступить следующему
    Exit,     // завершить процесс
}

/// Обработать trap из U-mode (системный вызов) и возобновить нужный процесс. Не возвращается.
pub fn handle_user_trap(frame: &mut TrapFrame, scause: usize) -> ! {
    let action = if scause == csr::EXC_ECALL_FROM_U {
        dispatch(frame)
    } else {
        println!("  [proc] неожиданный trap из U (scause={:#x}) — процесс завершён", scause);
        Action::Exit
    };

    // Сохранить состояние текущего процесса и выбрать, кого возобновить.
    let (next_frame, next_satp, has_next) = {
        let mut t = TABLE.lock();
        let cur = t.current;
        t.procs[cur].frame = *frame;
        match action {
            Action::Continue => {}
            Action::Yield => {
                if let Some(n) = t.next_runnable(cur) {
                    t.current = n;
                }
            }
            Action::Exit => {
                t.procs[cur].state = State::Finished;
                if let Some(n) = t.next_runnable(cur) {
                    t.current = n;
                }
            }
        }
        let c = t.current;
        if t.procs[c].state == State::Runnable {
            (t.procs[c].frame, t.procs[c].satp, true)
        } else {
            (TrapFrame::default(), 0, false)
        }
    };

    if has_next {
        unsafe { enter_user_frame(&next_frame, next_satp, trap_top()) }
    } else {
        // Все процессы завершились → вернуться в ядро (в run).
        unsafe {
            let mut discard = Context::default();
            context_switch(addr_of_mut!(discard), addr_of!(RETURN_CTX));
        }
        loop {} // не достигается
    }
}

/// Диспетчер syscall'ов. Номер в `a7`, аргументы в `a0..`, результат в `a0`.
fn dispatch(frame: &mut TrapFrame) -> Action {
    let num = frame.regs[17]; // a7
    match num {
        // SYS_WRITE(ptr, len): напечатать буфер процесса (ядро читает U-память, SUM=1).
        1 => {
            let ptr = frame.regs[10];
            let len = frame.regs[11];
            let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
            crate::print!("{}", core::str::from_utf8(bytes).unwrap_or("<?>"));
            frame.regs[10] = len;
            frame.sepc += 4;
            Action::Continue
        }
        // SYS_EXIT(code): завершить процесс.
        2 => {
            println!("  [proc] SYS_EXIT({})", frame.regs[10]);
            Action::Exit
        }
        // SYS_YIELD: уступить процессор следующему готовому процессу.
        3 => {
            frame.sepc += 4;
            Action::Yield
        }
        other => {
            println!("  [proc] неизвестный syscall {}", other);
            frame.regs[10] = usize::MAX;
            frame.sepc += 4;
            Action::Continue
        }
    }
}
