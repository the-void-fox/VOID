//! Веха 10.1 — пользовательский режим (U-mode) и граница системных вызовов.
//!
//! Первый выход из доверенного S-mode в **изолированный** U-mode. Пользовательский код лежит в
//! секции `.user` (страницы `U|R|X`, см. [[user-mode]] и `paging.rs`), исполняется с меньшими
//! правами и общается с ядром только через `ecall` (системные вызовы). Аппаратура запрещает
//! U-mode трогать не-`U` страницы — так ядро защищено от пользователя.
//!
//! Возврат в ядро на `SYS_EXIT` сделан через сохранённый контекст (переиспользуем
//! [`context_switch`] из [[scheduling]]): вход = «переключиться в лончер, который делает `sret`»,
//! выход = «переключиться обратно в сохранённый контекст ядра». Отдельного адресного
//! пространства у процесса пока нет — U-код живёт в таблицах ядра, но видит только `U`-страницы.

use core::arch::asm;
use core::ptr::{addr_of, addr_of_mut};

use crate::context::{context_switch, Context};
use crate::csr;

// ─── номера системных вызовов (граница ABI) ───────────────────────────────────
const SYS_WRITE: usize = 1;
const SYS_EXIT: usize = 2;

// ─── пользовательская программа (исполняется в U-mode) ────────────────────────

/// Сообщение пользователя. В `.user`, поэтому доступно и из U-mode, и ядру при чтении буфера.
#[link_section = ".user"]
static USER_MSG: [u8; 25] = *b"VOID: hello from U-mode!\n";

/// Точка входа пользовательской программы. Делает два системных вызова и завершается.
/// Никаких прямых обращений к ядру — только `ecall`.
#[link_section = ".user"]
extern "C" fn user_main() -> ! {
    unsafe {
        // SYS_WRITE(ptr, len): попросить ядро напечатать наш буфер.
        asm!(
            "ecall",
            in("a7") SYS_WRITE,
            inout("a0") USER_MSG.as_ptr() as usize => _,
            in("a1") USER_MSG.len(),
            options(nostack),
        );
        // SYS_EXIT(0): завершиться. Управление уйдёт в ядро и сюда не вернётся.
        asm!(
            "ecall",
            in("a7") SYS_EXIT,
            in("a0") 0usize,
            options(nostack, noreturn),
        );
    }
}

/// Адрес точки входа пользовательской программы (для `sret`).
pub fn entry() -> usize {
    user_main as *const () as usize
}

// ─── ядерный trap-стек для trap'ов из U-mode ──────────────────────────────────
// trap из U-mode должен строить кадр НЕ на пользовательском стеке. trap_entry.s берёт вершину
// этого стека из sscratch. Отдельный от загрузочного стека ядра, чтобы их не путать.

const TRAP_STACK_SIZE: usize = 16 * 1024;

#[repr(align(16))]
struct TrapStack(#[allow(dead_code)] [u8; TRAP_STACK_SIZE]); // используется по адресу (sscratch)
static mut TRAP_STACK: TrapStack = TrapStack([0; TRAP_STACK_SIZE]);

/// Вершина ядерного trap-стека (для sscratch перед входом в U-mode).
pub fn trap_stack_top() -> usize {
    addr_of!(TRAP_STACK) as usize + TRAP_STACK_SIZE
}

// ─── вход в U-mode / возврат в ядро ───────────────────────────────────────────

/// Контекст ядра, в который вернётся [`sys_exit`] (сохраняется в [`run_user`]).
static mut RETURN_CTX: Context = Context { ra: 0, sp: 0, s: [0; 12] };

// Параметры входа для лончера (передаём через статику — их читает `user_launch`).
static mut U_ENTRY: usize = 0;
static mut U_SP: usize = 0;
static mut U_TRAP_TOP: usize = 0;

/// Запустить пользовательскую программу в U-mode и вернуться сюда после её `SYS_EXIT`.
///
/// Переключаемся в лончер (`user_launch`), сохраняя текущий контекст ядра в `RETURN_CTX`;
/// лончер делает `sret` в U-mode. `sys_exit` позже переключится обратно в `RETURN_CTX`.
pub fn run_user(entry: usize, user_sp: usize, trap_top: usize) {
    // Прерывания выключаем на время «прыжка»: context_switch не трогает sstatus.SIE, а между
    // сохранением контекста и sret таймер нам ни к чему. Восстановим по возвращении.
    let sie = csr::irq_save_disable();
    unsafe {
        U_ENTRY = entry;
        U_SP = user_sp;
        U_TRAP_TOP = trap_top;

        let mut launch = Context::default();
        launch.ra = user_launch as *const () as usize;
        launch.sp = trap_top; // лончер почти не использует стек — сразу sret

        // Сохранить контекст ядра в RETURN_CTX и уйти в лончер.
        context_switch(addr_of_mut!(RETURN_CTX), addr_of!(launch));
        // ── сюда возвращаемся из sys_exit ──
        csr::write_sscratch(0); // снова в ядре: инвариант sscratch = 0
    }
    csr::irq_restore(sie);
}

/// Лончер (S-mode): настроить sstatus/sepc и `sret` в U-mode. Аргументы — из статик.
extern "C" fn user_launch() -> ! {
    unsafe {
        let entry = U_ENTRY;
        let usp = U_SP;
        let ttop = U_TRAP_TOP;
        asm!(
            "csrw sscratch, {ttop}",          // trap из U переключится на ядерный trap-стек
            "csrr t0, sstatus",
            "li   t1, {spp}", "not t1, t1", "and t0, t0, t1",  // SPP=0 → sret уйдёт в U-mode
            "li   t1, {spie}", "not t1, t1", "and t0, t0, t1", // SPIE=0 → прерывания в U выключены
            "li   t1, {sum}", "or  t0, t0, t1",                // SUM=1 → ядро сможет читать U-память
            "csrw sstatus, t0",
            "csrw sepc, {entry}",
            "mv   sp, {usp}",
            "sret",
            entry = in(reg) entry,
            usp = in(reg) usp,
            ttop = in(reg) ttop,
            spp = const 1usize << 8,
            spie = const 1usize << 5,
            sum = const 1usize << 18,
            options(noreturn),
        )
    }
}

/// Завершить пользовательскую программу: вернуться в ядро в точку вызова [`run_user`].
/// Вызывается из диспетчера syscall'ов на `SYS_EXIT`. Переключается в `RETURN_CTX` и не
/// возвращается (кадр trap'а на trap-стеке просто бросаем — процесс завершён).
pub fn sys_exit() {
    unsafe {
        let mut discard = Context::default();
        context_switch(addr_of_mut!(discard), addr_of!(RETURN_CTX));
    }
}
