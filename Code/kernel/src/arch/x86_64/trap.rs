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
    /// Таблица адресов стабов: [0..=35] — вектора 0–35 (исключения + таймер +
    /// консоль + диск + userspace-драйвер), [36] — spurious (0xFF), [37] — syscall (0x80).
    static TRAP_STUBS: [usize; 38];
}

/// Вектор LAPIC-таймера (первый свободный после 32 исключений).
pub const VEC_TIMER: u8 = 32;
/// Spurious-вектор LAPIC (EOI не шлётся).
pub const VEC_SPURIOUS: u8 = 0xff;
/// Вектор системных вызовов (`int 0x80`, шлюз с DPL=3 — Веха 26).
pub const VEC_SYSCALL: u8 = 0x80;
/// Вектор консольного ввода: IOAPIC маршрутизирует GSI4 (COM1) сюда (Веха 27).
pub const VEC_CONSOLE: u8 = 33;
/// Вектор завершений virtio-blk: MSI-X-запись устройства указывает сюда (Веха 27).
pub const VEC_BLK: u8 = 34;
/// Веха 52 — вектор прерываний userspace-драйверов: IOAPIC маршрутизирует IRQ их устройств сюда,
/// обработчик будит спящего в `SYS_IRQ_WAIT` (через флаг — без замка таблицы процессов).
pub const VEC_USERDRV: u8 = 35;

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
    /// Веха 35 — база FS-сегмента нити (её TLS-указатель). НЕ часть аппаратного кадра
    /// iretq и не трогается trap_entry.s (стаб пишет только слоты 0..21); ядро грузит
    /// её в `IA32_FS_BASE` на входе в U ([`super::enter_user`]), т.к. `%fs`-относительные
    /// `#[thread_local]`-доступы иначе читали бы TLS чужой нити. На riscv роль играет `tp`.
    pub fsbase: usize,
    /// Веха 36 — FP/SSE-контекст процесса (образ `fxsave64`: x87 + XMM0..15 + MXCSR).
    /// Как и `fsbase`, не трогается стабом: ядро собрано с soft-float и XMM не касается,
    /// поэтому живое состояние FPU на входе в трап принадлежит процессу — его снимает
    /// [`TrapFrame::save_fp`] (proc сразу после копии кадра) и возвращает
    /// [`TrapFrame::restore_fp`] на входе в U. Зеркало riscv, где f0..f31 спасает сам стаб.
    pub fx: FxArea,
}

/// 512-байтная область `fxsave64`. Отдельный тип ради ручного `Default`
/// (`[u64; 64]` его не даёт) с честным стартовым состоянием FPU.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FxArea(pub [u64; 64]);

impl Default for FxArea {
    fn default() -> Self {
        let mut a = [0u64; 64];
        a[0] = 0x037F; // FCW: все исключения x87 замаскированы (как после finit)
        a[3] = 0x1F80; // MXCSR: все исключения SSE замаскированы (reset-состояние)
        FxArea(a)
    }
}

/// `fxsave64`/`fxrstor64` требуют 16-выровненный адрес; кадры в таблице процессов не
/// выровнены — скретч + копия. Один на систему: ядро однопроцессорное, а внутри
/// trap-обработчиков IF=0 (interrupt gate) — реентерабельность исключена.
#[repr(C, align(16))]
struct FxScratch([u64; 64]);
static mut FX_SCRATCH: FxScratch = FxScratch([0; 64]);

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

    /// Веха 35 — TLS-указатель нити: на x86-64 это база сегмента `%fs` (Variant II —
    /// `.tdata`/`.tbss` лежат по ОТРИЦАТЕЛЬНЫМ смещениям от неё). Кладём в кадр; ядро
    /// применит `wrmsr IA32_FS_BASE` на входе в U-mode.
    pub fn set_thread_ptr(&mut self, fsbase: usize) {
        self.fsbase = fsbase;
    }

    /// Веха 35 — перенести TLS-указатель из прошлого кадра. `fsbase` НЕ спасается стабом
    /// trap_entry.s (это не GP-регистр, а MSR), поэтому во «свежем» кадре из трапа слот
    /// `fsbase` — мусор со стека. Восстанавливаем его из сохранённого кадра, иначе `wrmsr`
    /// на входе в U загрузил бы мусор и `%fs`-доступы (thread_local) улетели бы в никуда.
    pub fn carry_tls_from(&mut self, prev: &TrapFrame) {
        self.fsbase = prev.fsbase;
    }

    /// Веха 36 — снять живое состояние FPU/SSE в кадр. Зовётся сразу после копии кадра
    /// из трапа: слот `fx` там — мусор со стека (стаб его не пишет), а живые XMM в CPU —
    /// ровно состояние затрапившего процесса (ядро с soft-float их не меняет).
    pub fn save_fp(&mut self) {
        unsafe {
            let p = core::ptr::addr_of_mut!(FX_SCRATCH);
            core::arch::asm!("fxsave64 [{0}]", in(reg) p, options(nostack));
            self.fx.0 = (*p).0;
        }
    }

    /// Веха 36 — вернуть FPU/SSE-состояние кадра в CPU (вход в U, [`super::enter_user`]).
    pub fn restore_fp(&self) {
        unsafe {
            let p = core::ptr::addr_of_mut!(FX_SCRATCH);
            (*p).0 = self.fx.0;
            core::arch::asm!("fxrstor64 [{0}]", in(reg) p, options(nostack));
        }
    }

    /// Веха 38 — адрес инструкции, вызвавшей trap. Для linux-abi: musl x86-64 зовёт ядро
    /// инструкцией `syscall` (0F 05), а мы её НЕ включили (EFER.SCE=0) → она даёт #UD с
    /// rip НА ней; читаем опкод по этому адресу, чтобы отличить syscall от настоящего #UD.
    pub fn user_pc(&self) -> usize {
        self.rip
    }

    /// Веха 38 — перешагнуть инструкцию системного вызова linux-процесса: на x86-64 `syscall`
    /// (0F 05) — 2 байта. #UD оставил rip на инструкции; двигаем за неё (аналог того, что
    /// `sysret` сделал бы аппаратно).
    pub fn skip_syscall_insn(&mut self) {
        self.rip += 2;
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
        for v in 0..=35 {
            IDT[v] = IdtEntry::gate(TRAP_STUBS[v]);
        }
        IDT[VEC_SPURIOUS as usize] = IdtEntry::gate(TRAP_STUBS[36]);
        IDT[VEC_SYSCALL as usize] = IdtEntry::gate_user(TRAP_STUBS[37]);
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
    // Прерывания устройств (Веха 27) обрабатываются НЕЗАВИСИМО от кольца: стаб
    // полностью сохранил кадр и вернёт его iretq'ом — короткая работа + EOI, и
    // прерванное (хоть ядро, хоть ring3-процесс) продолжится, как ни в чём не бывало.
    match frame.vector as u8 {
        VEC_SPURIOUS => return, // spurious: без EOI по спецификации
        VEC_CONSOLE => {
            super::console_drain();
            lapic::eoi();
            return;
        }
        VEC_BLK => {
            crate::virtio_blk::on_irq();
            lapic::eoi();
            return;
        }
        VEC_USERDRV => {
            // Веха 52 — IRQ устройства userspace-драйвера. Сперва ЗАМАСКИРОВАТЬ линию (oneshot):
            // карта держит level-INTx, пока драйвер не прочитает ICR, — без маски IOAPIC переотправлял
            // бы прерывание штормом. Взведёт заново следующий SYS_IRQ_WAIT. Затем выставить флаг
            // (БЕЗ замка таблицы процессов — иначе дедлок с прерванным контекстом) и EOI; разбудит
            // спящего в SYS_IRQ_WAIT планировщик (resume/wait_stdin) при следующем проходе.
            super::ioapic::set_userdrv_masked(true);
            crate::proc::on_userdrv_irq();
            lapic::eoi();
            return;
        }
        _ => {}
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
