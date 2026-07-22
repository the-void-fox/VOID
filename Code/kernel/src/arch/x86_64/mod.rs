//! Реализация контракта [`crate::arch`] для x86_64
//! (Веха 25 — ядро, Веха 26 — userspace, Веха 27 — устройства).
//!
//! Живое: PVH direct boot (QEMU `-kernel`, трамплин 32→64 в entry.s), консоль COM1
//! (вывод + приём: IRQ4 через IOAPIC и, в сессиях процессов, опрос LSR на тиках),
//! GDT с ring3 и TSS (gdt.rs), IDT + syscall `int 0x80` + классификация трапов из
//! ring3 (trap.rs), 4-уровневый пейджинг с W^X (paging.rs), LAPIC-таймер (lapic.rs),
//! контексты ядерных задач (switch.s), вход в процессы через iretq (enter_user.s),
//! virtio-blk-pci: поиск на шине + MSI-X (pci.rs) — диск и персистентность работают.

use core::fmt;
use core::sync::atomic::{AtomicUsize, Ordering};

mod gdt;
mod ioapic;
mod lapic;
mod paging;
mod pci;
mod trap;
mod vga;

// Точка входа: PVH-нота + трамплин 32→64 (см. entry.s).
core::arch::global_asm!(include_str!("entry.s"));
// Переключение контекстов ядерных задач.
core::arch::global_asm!(include_str!("switch.s"));
// Вход в процесс: iretq по подготовленному trap-кадру.
core::arch::global_asm!(include_str!("enter_user.s"));

pub use pci::{probe_virtio_blk, probe_virtio_net};
pub use trap::{init as trap_init, TrapFrame};

/// Имя архитектуры — арх-измерение корней программ `bin/<arch>/<имя>` (Веха 26).
pub const ARCH_NAME: &str = "x86_64";

/// Процессы/U-mode работают (Веха 26) — kmain гоняет процессные демо.
pub const USERSPACE_READY: bool = true;

/// Операционная граница RAM (адрес конца, exclusive) — теперь ОБНАРУЖИВАЕТСЯ (Веха 41), а не
/// зашита: `platform_init` читает карту памяти загрузчика (multiboot от GRUB / PVH от QEMU) и
/// зажимает её CAP'ом (direct-map и аллокатор фреймов — этой границей). До discovery — дефолт
/// QEMU q35 128 МиБ.
static RAM_LIMIT_CELL: AtomicUsize = AtomicUsize::new(128 * 1024 * 1024);
/// Полная обнаруженная ёмкость RAM (для отчёта; операционно ограничена [`ram_limit`]).
static RAM_TOTAL_CELL: AtomicUsize = AtomicUsize::new(128 * 1024 * 1024);

/// Верхняя граница используемой RAM (адрес конца) — читают `frame`/`paging`.
pub fn ram_limit() -> usize {
    RAM_LIMIT_CELL.load(Ordering::Relaxed)
}

/// Полная обнаруженная RAM машины (байты) — для отчёта памяти.
pub fn ram_total() -> usize {
    RAM_TOTAL_CELL.load(Ordering::Relaxed)
}

/// Magic multiboot1 в eax при входе от GRUB (у PVH eax не определён → 0).
const MULTIBOOT_MAGIC: usize = 0x2BADB002;

/// Веха 41 — разобрать инфо-структуру загрузчика и выставить границы RAM. `magic` — eax при
/// входе (`0x2BADB002` = multiboot/GRUB), `info` — ebx (указатель на инфо). Direct-map и
/// аллокатор фреймов зажимаются `RAM_CAP`: VOID не нужны гигабайты, а отображать всю память
/// 4-КиБ страницами дорого; полную ёмкость печатаем отдельно ради честности отчёта.
///
/// # Safety
/// `info` — валидный указатель инфо-структуры соответствующего типа (гарантирует загрузчик).
pub fn platform_init(magic: usize, info: usize) {
    const RAM_CAP: usize = 256 * 1024 * 1024;
    let total = discover_ram(magic, info);
    RAM_TOTAL_CELL.store(total, Ordering::Relaxed);
    RAM_LIMIT_CELL.store(total.min(RAM_CAP), Ordering::Relaxed);
}

/// Прочитать полную RAM из карты памяти. multiboot: `flags(u32)@0`, `mem_upper(u32)@8` — КиБ
/// выше 1 МиБ (GRUB/QEMU дают при флаге bit0). PVH/неизвестно — дефолт 128 МиБ (это QEMU).
fn discover_ram(magic: usize, info: usize) -> usize {
    if magic == MULTIBOOT_MAGIC && info != 0 {
        let flags = unsafe { core::ptr::read_volatile(info as *const u32) };
        if flags & 1 != 0 {
            let mem_upper =
                unsafe { core::ptr::read_volatile((info + 8) as *const u32) } as usize;
            return 0x10_0000 + mem_upper * 1024;
        }
    }
    128 * 1024 * 1024
}

// ─── консоль (COM1: вывод напрямую, приём — опросом LSR на тиках таймера) ────

const COM1: u16 = 0x3f8;

/// IRQ COM1 в классической маршрутизации — пригодится при подключении IOAPIC (Веха 27+).
pub const CONSOLE_IRQ: u32 = 4;

#[inline]
fn outb(port: u16, v: u8) {
    unsafe { core::arch::asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack)) }
}

#[inline]
fn inb(port: u16) -> u8 {
    let v: u8;
    unsafe { core::arch::asm!("in al, dx", in("dx") port, out("al") v, options(nomem, nostack)) }
    v
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
            // Веха 41: зеркалим в VGA-текст — на реальной машине (без COM-порта) виден ОН.
            vga::put_byte(b);
        }
        Ok(())
    }
}

/// Веха 41 — ранняя инициализация консоли: очистить VGA-экран от мусора BIOS (на реальной
/// машине это первое, что видно). На riscv — no-op (там консоль — UART).
pub fn console_init() {
    vga::clear();
}

// Кольцевой буфер принятых байт — аналог riscv64/uart.rs. Наполняется двумя путями
// (оба идемпотентны и не пересекаются — прерывания в обработчиках выключены):
// IRQ4 через IOAPIC (Веха 27 — будит сон до ввода) и опрос на тиках таймера
// (политика Вехи 20.1 — подбирает байты в сессиях процессов между прерываниями).
const RX_CAP: usize = 256;
static mut RX_BUF: [u8; RX_CAP] = [0; RX_CAP];
static RX_HEAD: AtomicUsize = AtomicUsize::new(0); // писатель (drain)
static RX_TAIL: AtomicUsize = AtomicUsize::new(0); // читатель (getc)

/// Вычерпать приёмный FIFO COM1 в кольцевой буфер (LSR.DR — «данные готовы»).
/// Зовётся с выключенными прерываниями (из обработчика тика) — гонок нет.
pub fn console_drain() {
    let mut head = RX_HEAD.load(Ordering::Relaxed);
    while inb(COM1 + 5) & 1 != 0 {
        let b = inb(COM1);
        if head - RX_TAIL.load(Ordering::Relaxed) < RX_CAP {
            unsafe { RX_BUF[head % RX_CAP] = b };
            head += 1;
        } // переполнение — байт молча теряется (как в riscv-кольце)
    }
    RX_HEAD.store(head, Ordering::Relaxed);
}

pub fn console_has_input() -> bool {
    RX_HEAD.load(Ordering::Relaxed) != RX_TAIL.load(Ordering::Relaxed)
}

pub fn console_getc() -> Option<u8> {
    let tail = RX_TAIL.load(Ordering::Relaxed);
    if RX_HEAD.load(Ordering::Relaxed) == tail {
        return None;
    }
    let b = unsafe { RX_BUF[tail % RX_CAP] };
    RX_TAIL.store(tail + 1, Ordering::Relaxed);
    Some(b)
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

/// Спать до прерывания. ОТЛИЧИЕ от `wfi`: hlt при IF=0 не просыпается от pending-
/// прерывания — поэтому классический идиом `sti; hlt` (sti вступает в силу ПОСЛЕ
/// следующей инструкции — прерывание не проскочит между ними, а ОБСЛУЖИТСЯ прямо
/// в окне hlt), затем cli возвращает состояние «в ядре прерывания выключены».
/// Семантика для вызывающих та же, что у wfi: вернулись — прерывание случилось
/// (просто обработчик уже отработал здесь, а не в «коротком окне» после).
pub fn wait_for_interrupt() {
    unsafe { core::arch::asm!("sti", "hlt", "cli", options(nomem, nostack)) }
}

// ─── маски прерываний сессий процессов (Веха 26/27) ─────────────────────────
// На riscv это биты sie (таймер/внешние). Здесь прерывания устройств (консоль по
// IOAPIC, диск по MSI-X) обрабатываются trap-диспетчером прозрачно ИЗ ЛЮБОГО кольца
// (короткая работа + EOI + iretq в прерванное) — маскировать их на сессию незачем;
// политики управляют только LVT-таймером LAPIC.

/// Снимок маски: бит 0 = LVT-таймер размаскирован.
pub fn irq_mask_read() -> usize {
    (!lapic::timer_masked()) as usize
}

/// Восстановить маску из снимка [`irq_mask_read`].
pub fn irq_mask_write(mask: usize) {
    lapic::set_timer_masked(mask & 1 == 0);
}

/// Сессия процессов: таймер вкл — вытеснение в ring3 + опрос консоли на тиках.
pub fn irq_mask_preempt(_saved: usize) {
    lapic::set_timer_masked(false);
}

/// Сон до ввода: таймер выкл — исполнять некого, разбудит IRQ4 консоли (Веха 27),
/// как SEIE-путь на riscv.
pub fn irq_mask_stdin(_saved: usize) {
    lapic::set_timer_masked(true);
}

pub fn mark_in_kernel() {}

/// Маршрутизация прерываний устройств (Веха 27): IOAPIC ведёт GSI4 (COM1) на
/// [`trap::VEC_CONSOLE`]; сам UART начинает слать прерывания приёма (IER.DR;
/// OUT2 в MCR — классический «разъём» линии до контроллера). Диск сюда не ходит:
/// его MSI-X взводит pci.rs при поиске устройства.
pub fn init_device_interrupts() {
    ioapic::route(CONSOLE_IRQ, trap::VEC_CONSOLE);
    outb(COM1 + 1, 0x01); // IER: data ready
    outb(COM1 + 4, 0x0b); // MCR: DTR | RTS | OUT2
}

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

/// Веха 37 — VA→(PA страницы, флаги MAP_*): обратный перевод листового PTE
/// (зеркало `map`): R — сам Present, W — PTE_W, X — ОТСУТСТВИЕ PTE_NX, U — PTE_U.
pub fn page_info(root: usize, va: usize) -> Option<(usize, usize)> {
    let (pa, pte) = paging::page_info(root, va)?;
    let mut flags = MAP_R;
    if pte & paging::PTE_W != 0 {
        flags |= MAP_W;
    }
    if pte & paging::PTE_NX == 0 {
        flags |= MAP_X;
    }
    if pte & paging::PTE_U != 0 {
        flags |= MAP_U;
    }
    Some((pa, flags))
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

/// Веха 35 — монотонный счётчик тиков для futex-дедлайнов (`rdtsc`; ~1 ГГц в QEMU TCG,
/// TICK_NS=1). Тот же счётчик, что читает U-mode для замеров и `Instant`.
pub fn now_ticks() -> u64 {
    let (lo, hi): (u32, u32);
    unsafe { core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack)) };
    (hi as u64) << 32 | lo as u64
}

/// MSR базы сегмента `%fs` — Веха 35 несёт в нём TLS-указатель нити (Variant II).
const IA32_FS_BASE: u32 = 0xC000_0100;

/// Записать модельно-специфичный регистр (ring0). `edx:eax = value`, `ecx = msr`.
#[inline]
unsafe fn wrmsr(msr: u32, value: u64) {
    let lo = value as u32;
    let hi = (value >> 32) as u32;
    core::arch::asm!("wrmsr", in("ecx") msr, in("eax") lo, in("edx") hi, options(nostack, nomem));
}

// ─── вход в процесс (Веха 26) ───────────────────────────────────────────────

/// Войти в процесс: стек следующего трапа из ring3 — в TSS.rsp0 (аналог sscratch),
/// адресное пространство — в CR3 (заодно полный сброс TLB, как sfence.vma), кадр —
/// в регистры через iretq (enter_user.s). Селекторы ring3 и IF ставим здесь ВСЕГДА:
/// стартовые кадры их не заполняют, а кадрам из трапов не даём права понизить их.
///
/// # Safety
/// `space` — валидный токен пространства с отображённым ядром; `frame` — стартовый
/// или сохранённый трапом кадр этого процесса.
pub unsafe fn enter_user(frame: &TrapFrame, space: usize, trap_top: usize) -> ! {
    extern "C" {
        fn x86_enter_user(f: *const TrapFrame) -> !;
    }
    gdt::set_rsp0(trap_top);
    let mut f = *frame;
    f.cs = gdt::UCODE_SEL as usize;
    f.ss = gdt::UDATA_SEL as usize;
    f.rflags |= 1 << 9; // IF: в U-mode прерывания всегда включены (вытеснение)
    // Веха 35 — TLS нити: загрузить её базу %fs (0 у нитей без TLS — безвредно).
    // fsbase — глобальный регистр CPU, прошлая нить могла оставить свой → ставим всегда.
    wrmsr(IA32_FS_BASE, f.fsbase as u64);
    // Веха 36 — вернуть FP/SSE-состояние процесса (XMM — глобальные регистры CPU,
    // симметрично save_fp на трапе; ядро между ними их не трогает — soft-float).
    f.restore_fp();
    paging::enable(space_root(space));
    x86_enter_user(&f)
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

/// `e_machine` программ этого ядра (EM_X86_64) — с Вехи 26 программы собираются
/// под обе архитектуры и сеются под арх-корни `bin/<arch>/<имя>`.
pub const ELF_MACHINE: u16 = 62;

#[allow(dead_code)]
pub fn power_off() -> ! {
    // isa-debug-exit/ACPI — вместе с автотестами; пока честная остановка.
    loop {
        unsafe { core::arch::asm!("cli", "hlt", options(nomem, nostack)) }
    }
}

