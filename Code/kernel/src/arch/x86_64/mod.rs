//! Реализация контракта [`crate::arch`] для x86_64 (Веха 25 — ядро ожило).
//!
//! Живое: PVH direct boot (QEMU `-kernel`, трамплин 32→64 в entry.s), консоль COM1
//! (вывод), GDT/IDT + полный дамп фатальных trap'ов, 4-уровневый пейджинг с W^X
//! (paging.rs), LAPIC-таймер (lapic.rs), переключение контекстов ядерных задач
//! (switch.s) — ядерная половина демо (store, sched, async, GC) работает.
//!
//! Ещё заглушки (Веха 26+ — userspace): вход в U-mode (нужны TSS + сегменты ring3 +
//! syscall/sysret), маски прерываний сессий процессов, приём консоли (IOAPIC → IRQ4),
//! virtio-pci (диск → персистентность). До тех пор [`USERSPACE_READY`] = false —
//! kmain пропускает процессные демо.

use core::fmt;

mod lapic;
mod paging;
mod trap;

// Точка входа: PVH-нота + трамплин 32→64 (см. entry.s).
core::arch::global_asm!(include_str!("entry.s"));
// Переключение контекстов ядерных задач.
core::arch::global_asm!(include_str!("switch.s"));

pub use trap::{init as trap_init, TrapFrame};

const STUB: &str = "x86_64: userspace — Веха 26+";

/// Процессы/U-mode на этой архитектуре ещё в bring-up — kmain пропускает их демо.
pub const USERSPACE_READY: bool = false;

/// Конец RAM: QEMU q35 `-m 128M` — [0, 128 МиБ) (дыру BIOS < 1 МиБ ядро не трогает:
/// образ грузится с 1 МиБ, арена фреймов — за ним).
pub const RAM_LIMIT: usize = 128 * 1024 * 1024;

// ─── консоль (COM1, вывод; приём — с IOAPIC, Веха 26+) ──────────────────────

const COM1: u16 = 0x3f8;

/// IRQ COM1 в классической маршрутизации — пригодится при подключении IOAPIC.
pub const CONSOLE_IRQ: u32 = 4;

#[inline]
fn outb(port: u16, v: u8) {
    unsafe { core::arch::asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack)) }
}

/// Zero-sized хэндл последовательной консоли (пишем в THR COM1 без инициализации линии —
/// QEMU этого достаточно; делитель/FIFO настроит приёмная часть при bring-up ввода).
pub struct Console;

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            if b == b'\n' {
                outb(COM1, b'\r');
            }
            outb(COM1, b);
        }
        Ok(())
    }
}

/// Приём с консоли — вместе с IOAPIC (Веха 26+): пока ввода нет.
pub fn console_drain() {}
pub fn console_has_input() -> bool {
    false
}
pub fn console_getc() -> Option<u8> {
    None
}

// ─── прерывания ─────────────────────────────────────────────────────────────

/// Выключить прерывания, вернув прежнее состояние rflags.IF.
pub fn irq_save_disable() -> bool {
    let rflags: usize;
    unsafe {
        core::arch::asm!("pushfq", "pop {0}", "cli", out(reg) rflags, options(nomem));
    }
    rflags & (1 << 9) != 0 // IF
}

/// Восстановить состояние IF из [`irq_save_disable`].
pub fn irq_restore(enabled: bool) {
    if enabled {
        unsafe { core::arch::asm!("sti", options(nomem, nostack)) }
    }
}

/// Глобально включить прерывания.
pub fn enable_interrupts() {
    unsafe { core::arch::asm!("sti", options(nomem, nostack)) }
}

/// Спать до прерывания.
pub fn wait_for_interrupt() {
    unsafe { core::arch::asm!("hlt", options(nomem, nostack)) }
}

// Маски сессий процессов лягут на LVT/IOAPIC, когда появятся процессы (Веха 26+).
pub fn irq_mask_read() -> usize {
    0
}
pub fn irq_mask_write(_mask: usize) {}
pub fn irq_mask_preempt(_saved: usize) {
    unimplemented!("{STUB}: маска сессии процессов (LAPIC-таймер вкл, устройства выкл)")
}
pub fn irq_mask_stdin(_saved: usize) {
    unimplemented!("{STUB}: маска сна до ввода (устройства вкл, таймер выкл)")
}
pub fn mark_in_kernel() {}

/// Маршрутизация прерываний устройств — IOAPIC/MSI + virtio-pci (Веха 26+). Пока
/// устройств нет: virtio-mmio-пробы честно не находят диска, консоль работает выводом.
pub fn init_device_interrupts() {}

// ─── таймер (LAPIC) ─────────────────────────────────────────────────────────

/// Включить LAPIC (+ spurious), one-shot LVT-таймер и глобально прерывания.
pub fn timer_hw_init() {
    lapic::init();
    enable_interrupts();
}

/// Перевзвести квант вытеснения (one-shot: запись initial count = старт отсчёта).
pub fn timer_arm() {
    lapic::arm();
}

// ─── память (4-уровневый пейджинг) ──────────────────────────────────────────

/// Имя схемы трансляции — для баннера загрузки.
pub const MM_NAME: &str = "x86_64 4-level";

pub use paging::{clone_kernel_root, translate};

/// Флаги [`map`] — арх-нейтральные биты; в PTE их переводит сам `map` (x86 наоборот
/// ЗАПРЕЩАЕТ исполнение битом NX — см. paging.rs).
pub const MAP_R: usize = 1 << 0;
pub const MAP_W: usize = 1 << 1;
pub const MAP_X: usize = 1 << 2;
pub const MAP_U: usize = 1 << 3;

/// Построить таблицы ядра (direct map + W^X + MMIO) и вернуть корень (PML4).
pub fn mm_init() -> usize {
    paging::init()
}

/// Включить трансляцию по корню (CR3). До этого работали ВРЕМЕННЫЕ идентичные
/// таблицы трамплина (entry.s) — как «до paging::init» на RISC-V.
///
/// # Safety
/// См. paging::enable.
pub unsafe fn mm_enable(root: usize) {
    paging::enable(root)
}

/// Отобразить страницу `va → pa` с флагами `MAP_*` (перевод в биты PTE — внутри).
///
/// # Safety
/// См. paging::map.
pub unsafe fn map(root: usize, va: usize, pa: usize, flags: usize) {
    let mut pte = 0u64;
    if flags & MAP_W != 0 {
        pte |= paging::PTE_W;
    }
    if flags & MAP_U != 0 {
        pte |= paging::PTE_U;
    }
    if flags & MAP_X == 0 {
        pte |= paging::PTE_NX; // x86: исполнение ЗАПРЕЩАЕТСЯ, а не разрешается
    }
    paging::map(root, va, pa, pte)
}

/// Сбросить TLB после смены отображений активного пространства (перезагрузка CR3).
pub fn flush_tlb() {
    unsafe {
        core::arch::asm!(
            "mov {tmp}, cr3",
            "mov cr3, {tmp}",
            tmp = out(reg) _,
            options(nostack),
        );
    }
}

/// Токен адресного пространства — на x86 это значение CR3 (низ = флаги, нулевые).
pub fn space_token(root: usize) -> usize {
    root
}

/// Корень таблиц из токена.
pub fn space_root(token: usize) -> usize {
    token & !0xfff
}

// ─── вход в процесс (Веха 26+) ──────────────────────────────────────────────

pub unsafe fn enter_user(_frame: &TrapFrame, _space: usize, _trap_top: usize) -> ! {
    unimplemented!("{STUB}: TSS + сегменты ring3 + iretq/sysret")
}

// ─── контексты ядерных задач ────────────────────────────────────────────────

/// Callee-saved x86_64. Раскладка строго совпадает со switch.s; адрес возврата не
/// хранится — он на стеке задачи (финальный `ret` switch.s возобновляет её).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Context {
    rsp: usize,
    rbx: usize,
    rbp: usize,
    r12: usize,
    r13: usize,
    r14: usize,
    r15: usize,
}

impl Context {
    /// Пустой контекст — для статиков и «заполнится при первом переключении».
    pub const EMPTY: Context =
        Context { rsp: 0, rbx: 0, rbp: 0, r12: 0, r13: 0, r14: 0, r15: 0 };

    /// Контекст новой ядерной ЗАДАЧИ: на дно стека кладётся адрес трамплина (его
    /// возьмёт `ret` в switch.s), функция задачи — в rbx (её вызовет трамплин; по
    /// возврату — `sched::task_exit`). Выравнивание: после `ret` rsp кратен 16 —
    /// SysV-состояние «как после call».
    pub fn new_task(entry: fn(), sp: usize) -> Context {
        extern "C" {
            fn x86_task_trampoline();
        }
        let top = (sp & !0xf) - 8;
        unsafe { *(top as *mut usize) = x86_task_trampoline as *const () as usize };
        let mut c = Context::EMPTY;
        c.rsp = top;
        c.rbx = entry as usize;
        c
    }

    /// Контекст прямого входа в функцию ядра на заданном стеке (лончер сессии
    /// процессов): на дно стека — адрес самой функции.
    pub fn new_kernel(entry: extern "C" fn() -> !, sp: usize) -> Context {
        let top = (sp & !0xf) - 8;
        unsafe { *(top as *mut usize) = entry as usize };
        let mut c = Context::EMPTY;
        c.rsp = top;
        c
    }
}

extern "C" {
    /// Сохранить текущий контекст в `*old`, загрузить `*new` и продолжить в нём
    /// (см. switch.s).
    pub fn context_switch(old: *mut Context, new: *const Context);
}

// ─── разное ─────────────────────────────────────────────────────────────────

/// `e_machine` программ этого ядра (EM_X86_64). Программы для x86 появятся с
/// арх-измерением корней `bin/<arch>/<имя>` (Веха 26+); сеяные ELF — RISC-V,
/// загрузчик их честно отвергнет (BadMachine).
pub const ELF_MACHINE: u16 = 62;

#[allow(dead_code)]
pub fn power_off() -> ! {
    // isa-debug-exit/ACPI — вместе с автотестами; пока честная остановка.
    loop {
        unsafe { core::arch::asm!("cli", "hlt", options(nomem, nostack)) }
    }
}

/// ОБЯЗАТЕЛЬСТВА пути процессов (Веха 26+) — хуки общего кода, которые обязан звать
/// обработчик trap'ов из U-mode, когда появится вход в ring3 (сегодня U-mode нет, и
/// x86_trap_handler зовёт только timer::on_tick). Держит общий код живым для
/// dead-code-анализа и перечисляет точки сращивания.
#[allow(dead_code)]
fn user_path_obligations(frame: &mut TrapFrame) -> ! {
    use crate::arch::{FaultKind, UserTrap};
    crate::virtio_blk::on_irq(); // IRQ диска (IOAPIC/MSI → virtio-pci)
    let _ = UserTrap::PageFault { va: 0, kind: FaultKind::Load }; // #PF, errcode.W=0
    let _ = UserTrap::PageFault { va: 0, kind: FaultKind::Store }; // #PF, errcode.W=1
    let _ = UserTrap::PageFault { va: 0, kind: FaultKind::Exec }; // #PF, errcode.I/D
    let _ = UserTrap::TimerTick; // тик LAPIC из ring3 → вытеснение
    let _ = UserTrap::Unknown(0); // прочие вектора (#GP, #UD…)
    crate::proc::handle_user_trap(frame, UserTrap::Syscall)
}
