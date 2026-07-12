//! Заглушка контракта [`crate::arch`] для x86_64 (Веха 24).
//!
//! Цель этой вехи — не работающее x86-ядро, а ГРАНИЦА: общий код собирается под
//! `x86_64-unknown-none` без единого `cfg` вне `arch/`, все точки сращивания перечислены
//! здесь и обозначены `unimplemented!`. Bring-up (Limine → GDT/IDT → 4-уровневый пейджинг →
//! LAPIC → syscall/sysret → virtio-pci) — Вехи 25+; по мере его продвижения заглушки одна
//! за другой превращаются в реализацию, а контракт НЕ меняется.
//!
//! Что уже настоящее: последовательная консоль COM1 (вывод) и примитивы прерываний
//! (rflags.IF / cli / sti / hlt) — их хватит, чтобы первый bring-up печатал.

use core::fmt;

// Точка входа: пока прошивка/загрузчик не выбраны (Limine — Вехи 25+), просто стек → kmain.
core::arch::global_asm!(include_str!("entry.s"));

const STUB: &str = "x86_64: заглушка Вехи 24 — bring-up в Вехах 25+";

// ─── консоль (COM1, только вывод) ───────────────────────────────────────────

const COM1: u16 = 0x3f8;

/// IRQ COM1 в классической маршрутизации (8259/IOAPIC) — пока только для печати баннера.
pub const CONSOLE_IRQ: u32 = 4;

#[inline]
fn outb(port: u16, v: u8) {
    unsafe { core::arch::asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack)) }
}

/// Zero-sized хэндл последовательной консоли (пишем в THR COM1 без инициализации линии —
/// QEMU этого достаточно; делитель/FIFO настроит bring-up).
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

/// Приём с консоли — часть bring-up (IRQ 4 / опрос LSR): пока ввода нет.
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

// Маски сессий процессов: на x86 лягут на маскировку линий LAPIC/IOAPIC (bring-up).
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
pub fn init_device_interrupts() {
    // IOAPIC/MSI + virtio-pci — Вехи 25+; без них ядро печатает и останавливается раньше.
}

// ─── таймер ─────────────────────────────────────────────────────────────────

pub fn timer_hw_init() {
    unimplemented!("{STUB}: LAPIC-таймер")
}
pub fn timer_arm() {
    unimplemented!("{STUB}: LAPIC-таймер (one-shot)")
}

// ─── память (4-уровневый пейджинг — bring-up) ───────────────────────────────

/// Имя схемы трансляции — для баннера загрузки.
pub const MM_NAME: &str = "x86_64 4-level (заглушка)";

/// Флаги [`map`]: значения станут битами PTE x86_64 при bring-up (W=1<<1, U=1<<2, NX…).
pub const MAP_R: usize = 1 << 0;
pub const MAP_W: usize = 1 << 1;
pub const MAP_X: usize = 1 << 2;
pub const MAP_U: usize = 1 << 3;

pub fn mm_init() -> usize {
    crate::frame::init(); // обязательство bring-up: арена фреймов — часть mm_init (как на RISC-V)
    unimplemented!("{STUB}: построение таблиц (PML4) + W^X")
}
pub unsafe fn mm_enable(_root: usize) {
    unimplemented!("{STUB}: cr3")
}
pub fn clone_kernel_root() -> usize {
    unimplemented!("{STUB}: клон PML4 ядра")
}
pub unsafe fn map(_root: usize, _va: usize, _pa: usize, _flags: usize) {
    unimplemented!("{STUB}: map 4 КиБ-страницы")
}
pub fn translate(_root: usize, _va: usize) -> Option<usize> {
    unimplemented!("{STUB}: программный обход таблиц")
}
pub fn flush_tlb() {
    unimplemented!("{STUB}: invlpg / смена cr3")
}
pub fn space_token(_root: usize) -> usize {
    unimplemented!("{STUB}: токен = значение cr3")
}
pub fn space_root(_token: usize) -> usize {
    unimplemented!("{STUB}: корень из cr3")
}

// ─── trap'ы и вход в U-mode ─────────────────────────────────────────────────

pub fn trap_init() {
    unimplemented!("{STUB}: GDT/IDT")
}

/// ОБЯЗАТЕЛЬСТВА будущего обработчика trap'ов x86 — те же хуки общего кода, которые сегодня
/// зовёт riscv64/trap.rs: тик таймера, IRQ диска, классифицированный trap из U-mode.
/// Не вызывается; перечисляет точки сращивания «арх → общий код» (и не даёт dead-code-анализу
/// посчитать общий код мёртвым при сборке заглушки).
#[allow(dead_code)]
fn trap_dispatch_obligations(frame: &mut TrapFrame) -> ! {
    use crate::arch::{FaultKind, UserTrap};
    crate::timer::on_tick(); // прерывание LAPIC-таймера из ядра
    crate::virtio_blk::on_irq(); // IRQ диска (IOAPIC/MSI → virtio)
    // Классификатор (аналог classify_user в riscv64/trap.rs) обязан строить ВСЕ варианты:
    let _ = UserTrap::PageFault { va: 0, kind: FaultKind::Load }; // #PF, бит W errcode = 0
    let _ = UserTrap::PageFault { va: 0, kind: FaultKind::Store }; // #PF, бит W errcode = 1
    let _ = UserTrap::PageFault { va: 0, kind: FaultKind::Exec }; // #PF, бит I/D errcode
    let _ = UserTrap::TimerTick; // вектор LAPIC-таймера из U-mode
    let _ = UserTrap::Unknown(0); // прочие вектора (#GP, #UD…)
    // trap из U-mode: классифицировать и отдать планировщику.
    crate::proc::handle_user_trap(frame, UserTrap::Syscall)
}

/// Снимок регистров процесса. Раскладка и связь с ABI syscall'ов (rax=номер, rdi..r9=аргументы)
/// будут зафиксированы при bring-up (Вехи 25+) вместе с трамплином входа.
#[derive(Clone, Copy, Default)]
pub struct TrapFrame {
    regs: [usize; 16],
    rip: usize,
    rflags: usize,
}

impl TrapFrame {
    pub fn new_user(entry: usize, sp: usize, arg: usize) -> Self {
        let mut f = Self::default();
        f.rip = entry;
        f.regs[7] = sp; // rsp
        f.regs[1] = arg; // rdi — первый аргумент SysV
        f.rflags = 1 << 9; // IF
        f
    }
    pub fn syscall_num(&self) -> usize {
        self.regs[0] // rax
    }
    pub fn arg(&self, i: usize) -> usize {
        self.regs[1 + i] // rdi, rsi, rdx, r10, r8, r9…
    }
    pub fn set_ret(&mut self, v: usize) {
        self.regs[0] = v; // rax
    }
    pub fn set_ret_at(&mut self, i: usize, v: usize) {
        if i == 0 {
            self.regs[0] = v;
        } else {
            self.regs[1 + i] = v;
        }
    }
    pub fn advance(&mut self) {
        // syscall/sysret вернёт на rcx — уточнится при bring-up; для заглушки — ничего.
    }
}

pub unsafe fn enter_user(_frame: &TrapFrame, _space: usize, _trap_top: usize) -> ! {
    unimplemented!("{STUB}: iretq/sysret в U-mode")
}

// ─── контексты ядерных задач ────────────────────────────────────────────────

/// Callee-saved x86_64: rip (возврат), rsp, rbx, rbp, r12..r15.
#[derive(Clone, Copy, Default)]
pub struct Context {
    #[allow(dead_code)]
    rip: usize,
    #[allow(dead_code)]
    rsp: usize,
    #[allow(dead_code)]
    s: [usize; 6],
}

impl Context {
    pub const EMPTY: Context = Context { rip: 0, rsp: 0, s: [0; 6] };

    pub fn new_task(entry: fn(), sp: usize) -> Context {
        let mut c = Context::EMPTY;
        c.rip = entry as usize; // трамплин появится вместе с switch-асм (bring-up)
        c.rsp = sp;
        c
    }

    pub fn new_kernel(entry: extern "C" fn() -> !, sp: usize) -> Context {
        let mut c = Context::EMPTY;
        c.rip = entry as usize;
        c.rsp = sp;
        c
    }
}

/// Переключение контекстов — ассемблер bring-up (аналог switch.s).
///
/// # Safety
/// Не реализовано.
pub unsafe fn context_switch(_old: *mut Context, _new: *const Context) {
    unimplemented!("{STUB}: switch-асм (callee-saved + rsp)")
}

// ─── разное ─────────────────────────────────────────────────────────────────

/// `e_machine` программ этого ядра (EM_X86_64). Программы для x86 появятся с арх-измерением
/// корней `bin/<arch>/<имя>` (Вехи 25+); сегодняшние сеяные ELF — RISC-V, загрузчик их отвергнет.
pub const ELF_MACHINE: u16 = 62;

#[allow(dead_code)]
pub fn power_off() -> ! {
    unimplemented!("{STUB}: ACPI/isa-debug-exit")
}
