//! Обработка trap'ов: установка вектора и Rust-диспетчер.
//!
//! Низкоуровневое сохранение/восстановление регистров делает ассемблерный трамплин
//! `trap_entry` (см. trap_entry.s). Он зовёт отсюда [`trap_handler`] с указателем на
//! [`TrapFrame`] — снимок всех регистров на момент trap'а. Диспетчер смотрит на
//! `scause` и решает, что это было: таймер, breakpoint или что-то фатальное.

use super::{csr, plic};
use crate::arch::{FaultKind, UserTrap};
use crate::{println, proc, timer};

core::arch::global_asm!(include_str!("trap_entry.s"));

extern "C" {
    /// Ассемблерная точка входа в обработчик (адрес кладём в `stvec`).
    fn trap_entry();
}

/// Снимок состояния процессора на момент trap'а. Раскладка строго совпадает с
/// порядком сохранения в trap_entry.s (поэтому `repr(C)` и фиксированный порядок).
/// `Copy`/`Default` — чтобы сохранять его как состояние процесса ([[user-mode|proc]]).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct TrapFrame {
    /// Регистры x0..x31 (x0 всегда 0; x2 — исходный sp).
    pub regs: [usize; 32],
    /// Адрес инструкции, вызвавшей trap (он же адрес возврата).
    pub sepc: usize,
    /// Снимок sstatus.
    pub sstatus: usize,
    /// Регистры f0..f31 (Веха 32: uutils считают во float — FP-контекст процесса
    /// сохраняется честно). Заполняются только на trap'е ИЗ U-mode: ядро своих
    /// float'ов не имеет, а sstatus.FS процесса всегда ≥ Initial (см. new_user).
    pub fregs: [usize; 32],
    /// fcsr (флаги исключений и режим округления FPU).
    pub fcsr: usize,
}

/// Методы контракта [`crate::arch`]: общий код (`proc`) работает с кадром только через них —
/// какие регистры несут номер syscall'а/аргументы/результаты, знает лишь арх (RISC-V ABI:
/// номер в a7 = x17, аргументы/результаты в a0.. = x10..).
impl TrapFrame {
    /// Стартовый кадр процесса: вход `entry`, стек `sp`, первый аргумент `arg` (a0).
    /// `sstatus`: SPP=0 (возврат в U), SPIE=0 (прерывания в U выключены), SUM=1
    /// (ядро читает U-память в шлюзах syscall'ов), FS=Initial (FPU включён:
    /// иначе первая FP-инструкция — illegal instruction; Веха 32, uutils).
    pub fn new_user(entry: usize, sp: usize, arg: usize) -> Self {
        let mut f = Self::default();
        f.sepc = entry;
        f.regs[2] = sp; // sp = x2
        f.regs[10] = arg; // a0
        f.sstatus = (1 << 18) | (1 << 13); // SUM | FS=Initial
        f
    }

    /// Номер системного вызова (a7).
    pub fn syscall_num(&self) -> usize {
        self.regs[17]
    }

    /// `i`-й аргумент syscall'а (a0..a6).
    pub fn arg(&self, i: usize) -> usize {
        self.regs[10 + i]
    }

    /// Результат syscall'а (a0).
    pub fn set_ret(&mut self, v: usize) {
        self.regs[10] = v;
    }

    /// `i`-й регистр результата (a0..) — многозначные возвраты (RECV/CALL).
    pub fn set_ret_at(&mut self, i: usize, v: usize) {
        self.regs[10 + i] = v;
    }

    /// `i`-й СТАРТОВЫЙ аргумент процесса (до запуска). На RISC-V аргументы и возвраты —
    /// одни регистры (a0..), но контракт различает роли: на x86 это разные регистры.
    pub fn set_start_arg(&mut self, i: usize, v: usize) {
        self.regs[10 + i] = v;
    }

    /// Завершить инструкцию syscall'а — сдвинуть адрес возврата за `ecall` (4 байта).
    /// НЕ вызывается для блокирующих рестартующих syscall'ов (ecall повторится).
    pub fn advance(&mut self) {
        self.sepc += 4;
    }

    /// Перевзвести syscall на рестарт (блокирующий `SYS_READ`): здесь делать нечего —
    /// sepc не двигали, `ecall` повторится сам. На x86 это явный откат rip.
    pub fn restart(&mut self) {}

    /// Веха 35 — TLS-указатель нити. На RISC-V это регистр `tp` (x4): он часть кадра,
    /// сохраняется/восстанавливается trap_entry.s/enter_user.s как любой GPR, поэтому
    /// установленное значение живёт через trap'ы само (на x86 — MSR, восстанавливается
    /// ядром на входе в U). `#[thread_local]`-доступы адресуются относительно `tp`.
    pub fn set_thread_ptr(&mut self, tp: usize) {
        self.regs[4] = tp;
    }

    /// Веха 35 — перенос TLS-указателя между кадрами: на RISC-V `tp` (x4) спасается стабом
    /// как обычный GPR, во «свежем» кадре он уже верный — переносить нечего (no-op; парный
    /// x86, где `fsbase` — MSR вне кадра).
    pub fn carry_tls_from(&mut self, _prev: &TrapFrame) {}

    /// Веха 36 — FP-контекст: на RISC-V f0..f31+fcsr — часть кадра, их спасает сам стаб
    /// trap_entry.s (Веха 32) — обе стороны no-op (парный x86, где XMM снимает ядро
    /// fxsave64-областью кадра).
    pub fn save_fp(&mut self) {}
    pub fn restore_fp(&self) {}
}

/// Классифицировать trap из U-mode в арх-нейтральный [`UserTrap`] для `proc` (Веха 24).
fn classify_user(scause: usize) -> UserTrap {
    if scause == csr::INTERRUPT_BIT | csr::IRQ_S_TIMER {
        return UserTrap::TimerTick;
    }
    match scause {
        csr::EXC_ECALL_FROM_U => UserTrap::Syscall,
        csr::EXC_PF_LOAD => UserTrap::PageFault { va: csr::read_stval(), kind: FaultKind::Load },
        csr::EXC_PF_STORE => UserTrap::PageFault { va: csr::read_stval(), kind: FaultKind::Store },
        csr::EXC_PF_INSN => UserTrap::PageFault { va: csr::read_stval(), kind: FaultKind::Exec },
        other => UserTrap::Unknown(other),
    }
}

/// ABI-имена регистров для читаемого дампа.
const REG_NAMES: [&str; 32] = [
    "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "s0", "s1", "a0", "a1", "a2", "a3", "a4",
    "a5", "a6", "a7", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "t3", "t4",
    "t5", "t6",
];

/// Установить вектор обработки trap'ов (режим Direct). Вызвать один раз при старте.
pub fn init() {
    // Адрес функции -> сначала в указатель, потом в usize (так требует линт).
    csr::write_stvec(trap_entry as *const () as usize);
    // Инвариант переключения стека (Веха 10): в ядре sscratch = 0. trap_entry.s опирается
    // на это, чтобы отличить trap из ядра (S) от trap'а из пользователя (U).
    csr::write_sscratch(0);
    // Открыть U-mode счётчики cycle/time/instret (rdtime — замеры бенчей, Веха 28).
    csr::write_scounteren(0b111);
}

/// Rust-сторона обработчика. Вызывается из trap_entry.s; `frame` указывает на
/// сохранённые регистры на стеке. Менять `frame.sepc` здесь = менять адрес возврата.
#[no_mangle]
pub extern "C" fn trap_handler(frame: &mut TrapFrame) {
    let scause = csr::read_scause();

    // trap из U-mode (SPP=0): системный вызов (ecall), page fault, вытеснение… Причина
    // классифицируется в арх-нейтральный UserTrap (Веха 24); управление уходит в планировщик
    // процессов и сюда НЕ возвращается (возобновляется процесс).
    if frame.sstatus & (1 << 8) == 0 {
        proc::handle_user_trap(frame, classify_user(scause));
    }

    // trap из ядра (S-mode) — как раньше.
    let is_interrupt = scause & csr::INTERRUPT_BIT != 0;
    let code = scause & !csr::INTERRUPT_BIT;

    if is_interrupt {
        match code {
            csr::IRQ_S_TIMER => timer::on_tick(),
            csr::IRQ_S_EXTERNAL => plic::handle_external(), // устройства (диск)
            other => println!("  [trap] неизвестное прерывание, код={}", other),
        }
    } else {
        match code {
            csr::EXC_BREAKPOINT => {
                // `ebreak`: сообщаем и перешагиваем инструкцию, иначе зациклимся на ней.
                // Длина зависит от сжатия (расширение C): 2 байта (c.ebreak) или 4.
                let insn_lo = unsafe { core::ptr::read(frame.sepc as *const u16) };
                let len = if insn_lo & 0b11 == 0b11 { 4 } else { 2 };
                println!("  [trap] breakpoint @ {:#x} → перешагиваем {} байт", frame.sepc, len);
                frame.sepc += len;
            }
            _ => fatal(frame, scause),
        }
    }
}

/// Необработанный (фатальный) trap: печатаем полный контекст и останавливаемся.
/// Это наша «трассировка» Вехи 2 — дамп scause/sepc/stval и всех регистров.
/// (Полный backtrace по кадрам стека добавим, когда появится раскрутка.)
fn fatal(frame: &TrapFrame, scause: usize) -> ! {
    println!();
    println!("  ╔═ FATAL TRAP ═══════════════════════════════════");
    println!("  ║ scause : {:#x}  ({})", scause, cause_name(scause));
    println!("  ║ sepc   : {:#018x}", frame.sepc);
    println!("  ║ stval  : {:#018x}", csr::read_stval());
    println!("  ║ sstatus: {:#018x}", frame.sstatus);
    println!("  ╟─ регистры ─────────────────────────────────────");
    let mut i = 0;
    while i < 32 {
        println!(
            "  ║ {:>4}={:#018x}   {:>4}={:#018x}",
            REG_NAMES[i], frame.regs[i],
            REG_NAMES[i + 1], frame.regs[i + 1],
        );
        i += 2;
    }
    println!("  ╚════════════════════════════════════════════════");
    loop {
        unsafe { core::arch::asm!("wfi") }
    }
}

/// Человекочитаемое имя причины trap'а.
fn cause_name(scause: usize) -> &'static str {
    let code = scause & !csr::INTERRUPT_BIT;
    if scause & csr::INTERRUPT_BIT != 0 {
        match code {
            1 => "S-software interrupt",
            5 => "S-timer interrupt",
            9 => "S-external interrupt",
            _ => "неизвестное прерывание",
        }
    } else {
        match code {
            0 => "instruction address misaligned",
            1 => "instruction access fault",
            2 => "illegal instruction",
            3 => "breakpoint",
            4 => "load address misaligned",
            5 => "load access fault",
            6 => "store address misaligned",
            7 => "store access fault",
            8 => "ecall from U-mode",
            9 => "ecall from S-mode",
            12 => "instruction page fault",
            13 => "load page fault",
            15 => "store page fault",
            _ => "неизвестное исключение",
        }
    }
}
