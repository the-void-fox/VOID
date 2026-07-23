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

mod font;
mod gdt;
mod ioapic;
mod lapic;
mod paging;
mod pci;
mod ps2;
mod trap;
mod vga;

// Точка входа: PVH-нота + трамплин 32→64 (см. entry.s).
core::arch::global_asm!(include_str!("entry.s"));
// Переключение контекстов ядерных задач.
core::arch::global_asm!(include_str!("switch.s"));
// Вход в процесс: iretq по подготовленному trap-кадру.
core::arch::global_asm!(include_str!("enter_user.s"));

pub use pci::{probe_ahci, probe_e1000, probe_virtio_blk, probe_virtio_net};
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

/// Magic multiboot2 в eax при входе от GRUB (у PVH eax не определён → 0).
const MULTIBOOT2_MAGIC: usize = 0x36D76289;

/// Как загрузились: `0x36D76289` — реальная машина через GRUB (multiboot2), иначе QEMU (PVH).
static BOOT_MAGIC_CELL: AtomicUsize = AtomicUsize::new(0);

/// Веха 42 — на РЕАЛЬНОМ железе (загрузка GRUB'ом) ли мы? Отличаем эмулятор (QEMU/PVH) от
/// металла (GRUB/multiboot2): на металле kmain НЕ гоняет QEMU-демо (часть виснет — ждут таймер/
/// диск, которых как в QEMU нет), а сразу поднимает систему, как настоящая ОС без «демо на boot».
pub fn is_real_hardware() -> bool {
    BOOT_MAGIC_CELL.load(Ordering::Relaxed) == MULTIBOOT2_MAGIC
}

/// Веха 41 — разобрать инфо-структуру загрузчика и выставить границы RAM. `magic` — eax при
/// входе (`0x2BADB002` = multiboot/GRUB), `info` — ebx (указатель на инфо). Direct-map и
/// аллокатор фреймов зажимаются `RAM_CAP`: VOID не нужны гигабайты, а отображать всю память
/// 4-КиБ страницами дорого; полную ёмкость печатаем отдельно ради честности отчёта.
///
/// # Safety
/// `info` — валидный указатель инфо-структуры соответствующего типа (гарантирует загрузчик).
pub fn platform_init(magic: usize, info: usize) {
    const RAM_CAP: usize = 256 * 1024 * 1024;
    BOOT_MAGIC_CELL.store(magic, Ordering::Relaxed);
    let total = discover_multiboot(magic, info);
    RAM_TOTAL_CELL.store(total, Ordering::Relaxed);
    RAM_LIMIT_CELL.store(total.min(RAM_CAP), Ordering::Relaxed);
}

/// Веха 48 — загрузочный модуль multiboot2 (образ установки VOID, [`boot_module`]): база и
/// длина в RAM, куда GRUB положил его командой `module2`. 0 — модуля нет (обычная загрузка).
static MODULE_BASE: AtomicUsize = AtomicUsize::new(0);
static MODULE_LEN: AtomicUsize = AtomicUsize::new(0);

/// Разобрать карту памяти multiboot2 (от GRUB) И загрузочный модуль (Веха 48) за один проход.
/// Инфо — список тегов (`total_size@0`, теги с `@8`): type 4 «basic meminfo» (`mem_upper@+12` —
/// КиБ выше 1 МиБ) → полная RAM; type 3 «module» (`mod_start@+8`, `mod_end@+12`) → образ установки.
/// PVH/неизвестно — дефолт 128 МиБ RAM (это QEMU, где RAM известна раннеру), модуля нет.
fn discover_multiboot(magic: usize, info: usize) -> usize {
    if magic != MULTIBOOT2_MAGIC || info == 0 {
        return 128 * 1024 * 1024;
    }
    let rd = |off: usize| unsafe { core::ptr::read_volatile((info + off) as *const u32) };
    let mut ram = 128 * 1024 * 1024;
    let mut p = 8usize; // теги начинаются после total_size(u32)+reserved(u32)
    loop {
        let ty = rd(p);
        let size = rd(p + 4) as usize;
        if ty == 0 || size < 8 {
            break; // завершающий тег или мусор
        }
        match ty {
            4 => ram = 0x10_0000 + rd(p + 12) as usize * 1024, // basic meminfo: mem_upper (КиБ)
            3 => {
                let (start, end) = (rd(p + 8) as usize, rd(p + 12) as usize);
                MODULE_BASE.store(start, Ordering::Relaxed);
                MODULE_LEN.store(end.saturating_sub(start), Ordering::Relaxed);
                // GRUB кладёт модуль в свободную RAM (обычно сразу за образом ядра) — уберечь его
                // от bump-аллокатора фреймов: тот стартует с `_kernel_end`, а модуль может быть
                // выше. Резервируем [.._end), frame::init поднимет старт до этой границы.
                crate::frame::reserve_boot_module(end);
            }
            _ => {}
        }
        p += (size + 7) & !7; // следующий тег — с выравниванием на 8
    }
    ram
}

/// Веха 48 — загрузочный модуль (образ установки), переданный GRUB через `module2`:
/// `(база, длина)` в RAM или `None`. Читает установщик [`crate::install`].
pub fn boot_module() -> Option<(usize, usize)> {
    let len = MODULE_LEN.load(Ordering::Relaxed);
    (len != 0).then(|| (MODULE_BASE.load(Ordering::Relaxed), len))
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
        // Идём по СИМВОЛАМ: в COM1 (терминал QEMU) — сырой UTF-8, в VGA — перевод в CP866
        // (там загружен наш шрифт), иначе кириллица на экране машины была бы «?».
        let mut buf = [0u8; 4];
        for c in s.chars() {
            if c == '\n' {
                outb(COM1, b'\r');
            }
            for &b in c.encode_utf8(&mut buf).as_bytes() {
                outb(COM1, b);
            }
            // Веха 41: VGA-текст — на реальной машине (без COM-порта) виден ОН.
            vga::put_char(c);
        }
        // Веха 43: подвинуть аппаратный курсор VGA туда, где мы остановились (иначе висит).
        vga::sync_cursor();
        Ok(())
    }
}

/// Веха 41 — ранняя инициализация консоли: загрузить CP866-шрифт в знакогенератор VGA (чтобы
/// кириллица рисовалась, а не «?») и очистить экран от мусора BIOS — это первое, что видно на
/// реальной машине. На riscv — no-op (там консоль — UART).
pub fn console_init() {
    vga::load_font();
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

/// Положить принятый байт в кольцевой буфер (переполнение — байт теряется, как в riscv-кольце).
/// Единая точка для обоих источников ввода: COM1 (QEMU/serial) и PS/2-клавиатура (реальное
/// железо, Веха 42). Зовётся с выключенными прерываниями (из обработчика тика/IRQ) — гонок нет.
pub(super) fn rx_push(b: u8) {
    let head = RX_HEAD.load(Ordering::Relaxed);
    if head.wrapping_sub(RX_TAIL.load(Ordering::Relaxed)) < RX_CAP {
        unsafe { RX_BUF[head % RX_CAP] = b };
        RX_HEAD.store(head.wrapping_add(1), Ordering::Relaxed);
    }
}

/// Вычерпать приёмные буферы в кольцо: COM1 FIFO (LSR.DR — «данные готовы») И скан-коды
/// PS/2-клавиатуры (Веха 42). На QEMU ввод идёт через COM1 (serial), на реальной машине —
/// через клавиатуру; оба пути наполняют одно кольцо, `console_getc` их не различает.
///
/// Веха 42: у ноутбука НЕТ COM-порта — чтение LSR (0x3F8+5) на открытой шине даёт `0xFF`, где
/// бит DR всегда «1» → наивный `while LSR&DR` крутился бы ВЕЧНО (это и вешало реальную машину
/// сразу после включения прерываний). `LSR == 0xFF` — надёжный признак отсутствия UART (у
/// живого 16550 бит 7 не бывает вместе со всеми): в этом случае COM1 пропускаем. Плюс страховка
/// от флуда — не больше кольца за проход.
pub fn console_drain() {
    let mut n = 0;
    loop {
        let lsr = inb(COM1 + 5);
        if lsr == 0xff || lsr & 1 == 0 || n >= RX_CAP {
            break;
        }
        rx_push(inb(COM1));
        n += 1;
    }
    ps2::drain();
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
///
/// Веха 42: клавиатура — GSI1 на ТОТ ЖЕ вектор консоли (оба источника ввода наполняют одно
/// кольцо, `console_drain` черпает и COM1, и PS/2), плюс инициализация контроллера 8042. Так
/// на реальной машине нажатие клавиши будит систему из сна `wait_stdin` (как IRQ4 в QEMU).
pub fn init_device_interrupts() {
    // Веха 42: COM1 подключаем к прерыванию ТОЛЬКО если порт реально есть (в QEMU есть, на
    // ноутбуке нет: LSR читается как 0xFF). Иначе GSI4 слал бы спурьёзные прерывания в пустоту.
    if inb(COM1 + 5) != 0xff {
        ioapic::route(CONSOLE_IRQ, trap::VEC_CONSOLE);
        outb(COM1 + 1, 0x01); // IER: data ready
        outb(COM1 + 4, 0x0b); // MCR: DTR | RTS | OUT2
    }
    ioapic::route(1, trap::VEC_CONSOLE); // GSI1 — клавиатура PS/2 (общий вектор с COM1)
    ps2::init();
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

pub use paging::{clone_kernel_root, free_address_space, translate};

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

