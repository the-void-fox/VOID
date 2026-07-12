//! Веха 24 — граница архитектур: узкий контракт, за которым живёт ВСЁ арх-специфичное.
//!
//! Общий код ядра (процессы, IPC, store, capability, планировщики, драйвер virtio) не знает
//! ни про CSR, ни про satp, ни про PLIC — только про имена ниже. Каждая архитектура реализует
//! контракт своим модулем (`riscv64/`, `x86_64/`), выбор — на этапе компиляции по
//! `target_arch` (`cargo build --target …` — один проект, N образов). Никаких трейтов и
//! динамики: реализация одна на сборку, диспетчеризация не нужна, всё инлайнится.
//!
//! ## Контракт (что обязан отдать каждый arch)
//!
//! - **Загрузка**: ассемблерная точка входа, зовущая `kmain(hartid, boot_info)`.
//! - **Консоль**: [`Console`] (`core::fmt::Write`), приём: `console_init_rx` /
//!   `console_drain` / `console_getc` / `console_has_input`.
//! - **Прерывания**: `irq_save_disable`/`irq_restore` (критические секции),
//!   `enable_interrupts`, маски сессий процессов: `irq_mask_read`/`irq_mask_write` (снимок)
//!   + политики `irq_mask_preempt` (таймер вкл, устройства выкл — сессия процессов) и
//!   `irq_mask_stdin` (устройства вкл, таймер выкл — сон до ввода); `wait_for_interrupt`;
//!   `init_device_interrupts` (контроллер + маршрутизация IRQ диска/консоли);
//!   `mark_in_kernel` (инвариант «trap пришёл из ядра» после возврата из сессии).
//! - **Таймер**: `timer_hw_init` (размаскировать и включить), `timer_arm` (перевзвести квант
//!   вытеснения; величина кванта — дело арха: таймбазы разные).
//! - **Память**: `mm_init() -> корень`, `mm_enable`, `clone_kernel_root`, `map` с флагами
//!   `MAP_R/W/X/U`, `translate`, `flush_tlb`, токены адресных пространств
//!   `space_token(корень)`/`space_root(токен)` (на RISC-V токен = значение satp), `MM_NAME`.
//! - **Trap'ы**: `trap_init`; [`TrapFrame`] — снимок регистров с методами вместо голых
//!   индексов (`syscall_num`, `arg(i)`, `set_ret`/`set_ret_at`, `advance`, `new_user`);
//!   арх сам классифицирует trap из U-mode в [`UserTrap`] и зовёт
//!   `proc::handle_user_trap(frame, trap)`; `enter_user` — вход в процесс.
//! - **Контексты**: [`Context`] (opaque: `EMPTY`/`new_task`/`new_kernel`), `context_switch`.
//! - **Разное**: `ELF_MACHINE` (e_machine загружаемых программ), `RAM_LIMIT` (конец RAM
//!   платформы — для арены фреймов), `USERSPACE_READY` (false на архе в bring-up: kmain
//!   пропускает процессные демо, пока не готовы вход в U-mode и программы), `power_off`.
//!
//! Платформенные константы (адреса RAM/MMIO QEMU virt) пока остаются в общих
//! `frame`/`virtio_blk` — их черёд отделяться придёт с реальным x86-железом (Вехи 25+),
//! когда появится слой «платформа» поверх слоя «архитектура»; см. ADR 0005.

#[cfg(target_arch = "riscv64")]
#[path = "riscv64/mod.rs"]
mod imp;

#[cfg(target_arch = "x86_64")]
#[path = "x86_64/mod.rs"]
mod imp;

pub use imp::{
    // консоль (init приёма — внутри init_device_interrupts)
    console_drain, console_getc, console_has_input, Console, CONSOLE_IRQ,
    // прерывания
    enable_interrupts, init_device_interrupts, irq_mask_preempt, irq_mask_read, irq_mask_stdin,
    irq_mask_write, irq_restore, irq_save_disable, mark_in_kernel, wait_for_interrupt,
    // таймер
    timer_arm, timer_hw_init,
    // память
    clone_kernel_root, flush_tlb, map, mm_enable, mm_init, space_root, space_token, translate,
    MAP_R, MAP_U, MAP_W, MAP_X, MM_NAME,
    // trap'ы и контексты
    context_switch, enter_user, trap_init, Context, TrapFrame,
    // разное
    ELF_MACHINE, RAM_LIMIT, USERSPACE_READY,
};

/// Выключение машины — задел под автотесты (ядро само завершает QEMU); пока не зовётся.
#[allow(unused_imports)]
pub use imp::power_off;

/// Род page fault'а из U-mode — общий язык арха и `proc::handle_user_fault`
/// (ленивая куча обслуживает Load/Store; Exec в куче — гибель процесса, W^X).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FaultKind {
    Load,
    Store,
    Exec,
}

impl FaultKind {
    /// Имя для диагностики («процесс убит»).
    pub fn name(self) -> &'static str {
        match self {
            FaultKind::Load => "load",
            FaultKind::Store => "store",
            FaultKind::Exec => "exec",
        }
    }
}

/// Классифицированный trap из U-mode: арх разбирает свой регистр причины (scause, вектор IDT…)
/// и отдаёт общему коду ([`crate::proc::handle_user_trap`]) уже смысл, а не номера.
#[derive(Clone, Copy)]
pub enum UserTrap {
    /// Системный вызов (ecall / syscall).
    Syscall,
    /// Page fault по адресу `va` (ленивая куча или гибель процесса).
    PageFault { va: usize, kind: FaultKind },
    /// Тик таймера — вытеснение процесса.
    TimerTick,
    /// Всё прочее — фатально для процесса; код причины в арх-кодировке (для печати).
    Unknown(usize),
}
