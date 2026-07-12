//! Реализация контракта [`crate::arch`] для RISC-V 64 (S-mode поверх OpenSBI, QEMU virt).
//!
//! Здесь живёт всё, что до Вехи 24 было «просто модулями ядра»: CSR, SBI, PLIC, NS16550,
//! Sv39, trap-трамплины и переключение контекстов. Наружу торчат только имена контракта;
//! внутренние модули (`csr`, `sbi`, `plic`, …) из общего кода не видны.

mod context;
mod csr;
mod paging;
mod plic;
mod sbi;
mod trap;
mod uart;

// Ассемблерная точка входа `_start`: OpenSBI прыгает на 0x8020_0000 в S-mode → стек → kmain.
core::arch::global_asm!(include_str!("entry.s"));
// Вход в U-mode: satp процесса + восстановление регистров из кадра + sret.
core::arch::global_asm!(include_str!("enter_user.s"));

pub use context::{context_switch, Context};
pub use trap::{init as trap_init, TrapFrame};
pub use uart::Uart as Console;

extern "C" {
    /// Переключить `satp`, загрузить регистры из `*frame`, выставить `sscratch`=trap-стек и `sret`.
    fn enter_user_frame(frame: *const TrapFrame, satp: usize, trap_top: usize) -> !;
}

// ─── консоль ────────────────────────────────────────────────────────────────

/// IRQ консоли в контроллере прерываний (UART0 в PLIC на QEMU virt).
pub const CONSOLE_IRQ: u32 = uart::IRQ;

pub use uart::{drain_rx as console_drain, getc as console_getc, has_input as console_has_input};

// ─── прерывания ─────────────────────────────────────────────────────────────

pub use csr::{enable_interrupts, irq_restore, irq_save_disable};

/// Снимок маски разрешённых типов прерываний (`sie`) — opaque-токен для политик ниже.
pub fn irq_mask_read() -> usize {
    csr::read_sie()
}

/// Восстановить маску из снимка.
pub fn irq_mask_write(mask: usize) {
    csr::write_sie(mask)
}

/// Политика сессии процессов: таймер (STIE) вкл — вытеснение работает даже в U-mode,
/// устройства (SEIE) выкл — их шлюзы работают опросом, а ввод копится в PLIC как pending.
pub fn irq_mask_preempt(saved: usize) {
    csr::write_sie((saved | (1 << 5)) & !(1 << 9));
}

/// Политика сна до ввода: только внешние (SEIE) — исполнять некого, таймер (STIE) не нужен.
pub fn irq_mask_stdin(saved: usize) {
    csr::write_sie((saved & !(1 << 5)) | (1 << 9));
}

/// Инвариант переключения стеков: в ядре `sscratch` = 0 (trap_entry.s по нему отличает
/// trap из ядра от trap'а из процесса). Вызвать после возврата из сессии процессов.
pub fn mark_in_kernel() {
    csr::write_sscratch(0);
}

/// Спать до прерывания (`wfi`). Просыпается и от PENDING-прерывания при SIE=0 —
/// на этом стоит протокол «сон без потерянного пробуждения» в `proc::wait_stdin`.
pub fn wait_for_interrupt() {
    unsafe { core::arch::asm!("wfi") }
}

/// Поднять маршрутизацию прерываний устройств: PLIC (диск, если он есть, + консоль)
/// и разрешить внешние прерывания S-mode. Обработчик — `plic::handle_external`.
pub fn init_device_interrupts() {
    let blk = crate::virtio_blk::irq();
    if blk != 0 {
        plic::init(blk);
        csr::enable_external_interrupt();
    }
    uart::init_rx();
    plic::enable(uart::IRQ);
}

// ─── таймер ─────────────────────────────────────────────────────────────────

/// Квант вытеснения: 200_000 тиков при таймбазе 10 МГц (QEMU virt) = 20 мс.
const TIMER_INTERVAL: u64 = 200_000;

/// Размаскировать таймерные прерывания и глобально включить прерывания S-mode.
pub fn timer_hw_init() {
    csr::enable_timer_interrupt(); // sie.STIE
    csr::enable_interrupts(); // sstatus.SIE
}

/// Перевзвести одноразовый таймер на «сейчас + квант» (SBI TIME). Это же действие
/// сбрасывает pending-бит таймера.
pub fn timer_arm() {
    sbi::set_timer(csr::read_time() + TIMER_INTERVAL);
}

// ─── память (Sv39) ──────────────────────────────────────────────────────────

/// Имя схемы трансляции — для баннера загрузки.
pub const MM_NAME: &str = "Sv39";

pub use paging::{clone_kernel_root, translate};

/// Флаги [`map`] в кодировке этого арха (листовой PTE Sv39).
pub const MAP_R: usize = paging::PTE_R;
pub const MAP_W: usize = paging::PTE_W;
pub const MAP_X: usize = paging::PTE_X;
pub const MAP_U: usize = paging::PTE_U;

/// Построить таблицы ядра (direct map RAM + MMIO, W^X) и вернуть корень.
pub fn mm_init() -> usize {
    paging::init()
}

/// Включить трансляцию по корню.
///
/// # Safety
/// См. `paging::enable`: таблицы обязаны идентично отображать текущие PC/SP/стек.
pub unsafe fn mm_enable(root: usize) {
    paging::enable(root)
}

/// Отобразить страницу `va → pa` с флагами `MAP_*`.
///
/// # Safety
/// См. `paging::map`: валидный корень; таблицы доступны по VA == PA.
pub unsafe fn map(root: usize, va: usize, pa: usize, flags: usize) {
    paging::map(root, va, pa, flags)
}

/// Сбросить TLB после смены отображений АКТИВНОГО пространства (повтор инструкции после
/// ленивого фолта обязан увидеть новую трансляцию).
pub fn flush_tlb() {
    unsafe { core::arch::asm!("sfence.vma") };
}

const SATP_SV39: usize = 8 << 60;
const SATP_PPN_MASK: usize = (1usize << 44) - 1;

/// Токен адресного пространства из корня таблиц — то, что процесс носит в своей записи
/// и что [`enter_user`] грузит одним движением. На RISC-V это значение `satp`.
pub fn space_token(root: usize) -> usize {
    SATP_SV39 | (root >> 12)
}

/// Корень таблиц обратно из токена (PPN → байтовый адрес).
pub fn space_root(token: usize) -> usize {
    (token & SATP_PPN_MASK) << 12
}

// ─── вход в процесс ─────────────────────────────────────────────────────────

/// Войти в U-mode: активировать пространство `space` (токен из [`space_token`]),
/// восстановить регистры из `frame`, выставить `sscratch` = `trap_top` и `sret`.
///
/// # Safety
/// `frame` — валидный стартовый/сохранённый кадр процесса; `trap_top` — вершина ядерного
/// trap-стека. Не возвращается.
pub unsafe fn enter_user(frame: &TrapFrame, space: usize, trap_top: usize) -> ! {
    enter_user_frame(frame, space, trap_top)
}

// ─── разное ─────────────────────────────────────────────────────────────────

/// `e_machine` программ, которые исполняет это ядро (EM_RISCV).
pub const ELF_MACHINE: u16 = 243;

/// Имя архитектуры — арх-измерение корней программ `bin/<arch>/<имя>` (Веха 26).
pub const ARCH_NAME: &str = "riscv64";

/// Процессы/U-mode здесь полностью рабочие с Вехи 10.
pub const USERSPACE_READY: bool = true;

/// Конец RAM: QEMU virt `-m 128M` — [0x8000_0000, 0x8800_0000).
pub const RAM_LIMIT: usize = 0x8000_0000 + 128 * 1024 * 1024;

/// Выключить машину (SBI SRST; в QEMU — завершить процесс). Задел под автотесты.
#[allow(dead_code)]
pub fn power_off() -> ! {
    sbi::shutdown()
}
