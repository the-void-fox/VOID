//! Trap'ы x86_64 (Веха 25/26): IDT + ассемблерные стабы + Rust-диспетчер.
//!
//! Аналог riscv64/trap.rs: стабы сохраняют регистры в [`TrapFrame`] на стеке и зовут
//! [`x86_trap_handler`]. Отличия платформы: причин не один регистр (scause), а номер
//! вектора (кладём его в кадр сами) + код ошибки (часть исключений кладёт его аппаратно,
//! остальным стаб подкладывает 0 — кадр всегда одной формы); CS.RPL в кадре говорит,
//! откуда пришли: ring3 — путь процессов (Веха 26), классифицируется в [`UserTrap`]
//! и уходит в `proc::handle_user_trap` (как riscv-ветка `sstatus.SPP == 0`).
//!
//! Syscall — `int 0x80` (шлюз с DPL=3): у нас нет причин экономить наносекунды
//! `syscall/sysret` в bring-up, а int-путь ложится в ту же единую форму кадра.
//! Стек при трапе из ring3 процессор берёт из `TSS.rsp0` ([`super::gdt`]).

use core::ptr::addr_of;

use super::lapic;
use crate::arch::{FaultKind, UserTrap};
use crate::println;

// Стабы всех векторов + общий трамплин сохранения кадра. Порядок push'ей строго
// совпадает с раскладкой [`TrapFrame`] (поэтому repr(C) и комментарии индексов).
core::arch::global_asm!(include_str!("trap_entry.s"));

extern "C" {
    /// Таблица адресов стабов: [0..=32] — вектора 0–32, [33] — spurious (0xFF),
    /// [34] — syscall (0x80).
    static TRAP_STUBS: [usize; 35];
}

/// Вектор LAPIC-таймера (первый свободный после 32 исключений).
pub const VEC_TIMER: u8 = 32;
/// Spurious-вектор LAPIC (EOI не шлётся).
pub const VEC_SPURIOUS: u8 = 0xff;
/// Вектор системных вызовов (`int 0x80`, шлюз с DPL=3 — Веха 26).
pub const VEC_SYSCALL: u8 = 0x80;

/// Снимок состояния процессора на момент trap'а. Раскладка = порядок push'ей в
/// trap_entry.s (адреса растут к концу структуры; регистры — в порядке r15..rax).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct TrapFrame {
    /// r15,r14,r13,r12,r11,r10,r9,r8,rdi,rsi,rbp,rbx,rdx,rcx,rax (индексы 0..14).
    pub regs: [usize; 15],
    /// Номер вектора (кладёт стаб).
    pub vector: usize,
    /// Код ошибки исключения (0, если аппаратно не кладётся).
    pub err: usize,
    // ── аппаратный кадр iretq (в 64-битном режиме — всегда пять слов) ──
    pub rip: usize,
    pub cs: usize,
    pub rflags: usize,
    pub rsp: usize,
    pub ss: usize,
}

// Индексы регистров в `regs` (порядок push'ей: rax первым → верх структуры).
pub const RAX: usize = 14;
pub const RDX: usize = 12;
pub const RBX: usize = 11;
pub const RSI: usize = 9;
pub const RDI: usize = 8;
pub const R8: usize = 7;
pub const R9: usize = 6;
pub const R10: usize = 5;

/// Регистры аргументов будущего syscall-ABI VOID/x86_64 (Веха 26): rax = номер,
/// аргументы — rdi, rsi, rdx, r10, r8, r9 (как SysV/Linux: rcx занят `syscall`ом)
/// + rbx как 7-й (наш ABI несёт до 7 аргументов — a0..a6 на RISC-V).
const ARG_REGS: [usize; 7] = [RDI, RSI, RDX, R10, R8, R9, RBX];

/// Методы контракта [`crate::arch`] — общий код (`proc`) работает с кадром только через них.
impl TrapFrame {
    /// Стартовый кадр процесса: вход `entry`, стек `sp`, первый аргумент `arg` (rdi).
    /// cs/ss не заполняются — [`super::enter_user`] всегда ставит селекторы ring3 сам.
    pub fn new_user(entry: usize, sp: usize, arg: usize) -> Self {
        let mut f = Self::default();
        f.rip = entry;
        f.rsp = sp;
        f.regs[RDI] = arg;
        f.rflags = 1 << 9; // IF
        f
    }

    /// Номер системного вызова (rax).
    pub fn syscall_num(&self) -> usize {
        self.regs[RAX]
    }

    /// `i`-й аргумент syscall'а (rdi, rsi, rdx, r10, r8, r9, rbx).
    pub fn arg(&self, i: usize) -> usize {
        self.regs[ARG_REGS[i]]
    }

    /// Результат syscall'а (rax).
    pub fn set_ret(&mut self, v: usize) {
        self.regs[RAX] = v;
    }

    /// `i`-й регистр результата: 0 — rax, дальше — по регистрам аргументов
    /// (многозначные возвраты RECV/CALL нашего ABI).
    pub fn set_ret_at(&mut self, i: usize, v: usize) {
        if i == 0 {
            self.regs[RAX] = v;
        } else {
            self.regs[ARG_REGS[i - 1]] = v;
        }
    }

    /// `i`-й СТАРТОВЫЙ аргумент процесса (до запуска): rdi, rsi, … На x86 это НЕ регистры
    /// возвратов (rax…) — в отличие от RISC-V, где a0..a1 играют обе роли.
    pub fn set_start_arg(&mut self, i: usize, v: usize) {
        self.regs[ARG_REGS[i]] = v;
    }

    /// Завершить инструкцию syscall'а. На x86 `int` кладёт в кадр адрес УЖЕ следующей
    /// инструкции — двигать нечего.
    pub fn advance(&mut self) {}

    /// Перевзвести syscall на рестарт (блокирующий `SYS_READ`): откатить rip НА
    /// инструкцию `int 0x80` (2 байта: CD 80) — при возобновлении она повторится.
    /// Зеркало riscv-семантики, где sepc и так остаётся на `ecall`.
    pub fn restart(&mut self) {
        self.rip -= 2;
    }
}

/// Дескриптор шлюза IDT (interrupt gate, 16 байт).
#[repr(C)]
#[derive(Clone, Copy)]
struct IdtEntry {
    off_lo: u16,
    selector: u16,
    ist_and_type: u16, // ist(3) | zero(5) | type(4)=0xE | zero(1) | dpl(2) | present(1)
    off_mid: u16,
    off_hi: u32,
    zero: u32,
}

impl IdtEntry {
    const EMPTY: IdtEntry =
        IdtEntry { off_lo: 0, selector: 0, ist_and_type: 0, off_mid: 0, off_hi: 0, zero: 0 };

    fn gate(handler: usize) -> IdtEntry {
        IdtEntry {
            off_lo: handler as u16,
            selector: 0x08, // код ring0 из GDT трамплина
            ist_and_type: 0x8e00, // present | dpl=0 | interrupt gate (IF гасится аппаратно)
            off_mid: (handler >> 16) as u16,
            off_hi: (handler >> 32) as u32,
            zero: 0,
        }
    }

    /// Шлюз, доступный из ring3 (`int 0x80`): DPL=3 — иначе int из U-mode даёт #GP.
    fn gate_user(handler: usize) -> IdtEntry {
        let mut g = Self::gate(handler);
        g.ist_and_type |= 0x6000; // dpl=3
        g
    }
}

static mut IDT: [IdtEntry; 256] = [IdtEntry::EMPTY; 256];

#[repr(C, packed)]
struct IdtPtr {
    limit: u16,
    base: u64,
}

/// Загрузить полную GDT (сегменты ring3 + TSS, [`super::gdt`]), заполнить IDT
/// (исключения 0–31, таймер 32, spurious 0xFF, syscall 0x80) и загрузить её.
/// Заодно замаскировать legacy-PIC: прерывания ходят только через LAPIC.
pub fn init() {
    super::gdt::init();
    unsafe {
        for v in 0..=32 {
            IDT[v] = IdtEntry::gate(TRAP_STUBS[v]);
        }
        IDT[VEC_SPURIOUS as usize] = IdtEntry::gate(TRAP_STUBS[33]);
        IDT[VEC_SYSCALL as usize] = IdtEntry::gate_user(TRAP_STUBS[34]);
        let ptr = IdtPtr {
            limit: (core::mem::size_of::<[IdtEntry; 256]>() - 1) as u16,
            base: addr_of!(IDT) as u64,
        };
        core::arch::asm!("lidt [{0}]", in(reg) &ptr, options(nostack));
    }
    // Замаскировать оба 8259 (иначе legacy-таймер/клавиатура шумят по своим векторам).
    super::outb(0x21, 0xff);
    super::outb(0xa1, 0xff);
}

/// Классифицировать trap из ring3 в арх-нейтральный [`UserTrap`] для `proc` —
/// зеркало riscv `classify_user(scause)`. Причина у x86 размазана: вектор + код
/// ошибки (#PF: бит 1 — запись, бит 4 — instruction fetch) + cr2 (адрес фолта).
fn classify_user(frame: &TrapFrame) -> UserTrap {
    match frame.vector as u8 {
        VEC_SYSCALL => UserTrap::Syscall,
        14 => {
            let va: usize;
            unsafe { core::arch::asm!("mov {0}, cr2", out(reg) va, options(nomem, nostack)) };
            let kind = if frame.err & (1 << 4) != 0 {
                FaultKind::Exec
            } else if frame.err & (1 << 1) != 0 {
                FaultKind::Store
            } else {
                FaultKind::Load
            };
            UserTrap::PageFault { va, kind }
        }
        VEC_TIMER => {
            // handle_user_trap не возвращается (возобновит процесс через iretq) —
            // EOI шлём здесь, иначе LAPIC навсегда сочтёт тик необслуженным.
            lapic::eoi();
            UserTrap::TimerTick
        }
        v => UserTrap::Unknown(v as usize),
    }
}

/// Rust-сторона обработчика. Trap из ring3 (CS.RPL=3) классифицируется в [`UserTrap`]
/// и уходит в планировщик процессов — управление сюда не возвращается (возобновится
/// какой-то процесс). Trap из ядра: таймер тикает, breakpoint перешагивается (int3 —
/// trap, rip уже за инструкцией), остальное — фатальный дамп.
#[no_mangle]
extern "C" fn x86_trap_handler(frame: &mut TrapFrame) {
    if frame.vector as u8 == VEC_SPURIOUS {
        return; // spurious: без EOI по спецификации, откуда бы ни прилетел
    }
    if frame.cs & 3 == 3 {
        let trap = classify_user(frame);
        crate::proc::handle_user_trap(frame, trap);
    }
    match frame.vector as u8 {
        VEC_TIMER => {
            crate::timer::on_tick();
            lapic::eoi();
        }
        3 => println!("  [trap] breakpoint @ {:#x} → продолжаем", frame.rip),
        _ => fatal(frame),
    }
}

/// Необработанный (фатальный) trap: полный контекст и стоп — паритет riscv64/fatal.
fn fatal(frame: &TrapFrame) -> ! {
    let cr2: usize;
    unsafe { core::arch::asm!("mov {0}, cr2", out(reg) cr2, options(nomem, nostack)) };
    println!();
    println!("  ╔═ FATAL TRAP (x86_64) ══════════════════════════");
    println!("  ║ vector : {}  ({})", frame.vector, vector_name(frame.vector));
    println!("  ║ err    : {:#x}", frame.err);
    println!("  ║ rip    : {:#018x}   cs: {:#x}", frame.rip, frame.cs);
    println!("  ║ rflags : {:#018x}", frame.rflags);
    println!("  ║ rsp    : {:#018x}   ss: {:#x}", frame.rsp, frame.ss);
    println!("  ║ cr2    : {:#018x}  (адрес при #PF)", cr2);
    println!("  ╟─ регистры (r15..rax) ──────────────────────────");
    let mut i = 0;
    while i < 15 {
        if i + 1 < 15 {
            println!("  ║ {:#018x}   {:#018x}", frame.regs[i], frame.regs[i + 1]);
        } else {
            println!("  ║ {:#018x}", frame.regs[i]);
        }
        i += 2;
    }
    println!("  ╚════════════════════════════════════════════════");
    loop {
        unsafe { core::arch::asm!("hlt") }
    }
}

/// Человекочитаемое имя вектора.
fn vector_name(v: usize) -> &'static str {
    match v {
        0 => "divide error",
        3 => "breakpoint",
        6 => "invalid opcode",
        8 => "double fault",
        13 => "general protection",
        14 => "page fault",
        32 => "LAPIC timer",
        _ => "исключение/прерывание",
    }
}
