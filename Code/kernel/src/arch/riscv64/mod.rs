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

/// Веха 41 — ранняя инициализация консоли: на RISC-V консоль — UART (SBI/NS16550), чистить
/// нечего (no-op; парный x86, где очищается VGA-экран от мусора BIOS).
pub fn console_init() {}

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

// ─── устройства ─────────────────────────────────────────────────────────────

/// Найти virtio-blk на шине virtio-mmio QEMU virt (Веха 27: поиск устройства — дело
/// арха, разговор с ним — общего драйвера): 8 слотов по 0x1000 от 0x1000_1000;
/// в слоте ищем magic «virt», версию 2 (modern) и device id 2 (block). Номер
/// прерывания на PLIC у QEMU virt — слот + 1.
pub fn probe_virtio_blk() -> Option<crate::arch::BlkDevice> {
    const MMIO_BASE: usize = 0x1000_1000;
    const MMIO_STRIDE: usize = 0x1000;
    for slot in 0..8 {
        let base = MMIO_BASE + slot * MMIO_STRIDE;
        let r = |off: usize| unsafe { core::ptr::read_volatile((base + off) as *const u32) };
        if r(0x000) == 0x7472_6976 && r(0x004) == 2 && r(0x008) == 2 {
            return Some(crate::arch::BlkDevice {
                transport: crate::arch::BlkTransport::Mmio { base },
                irq: slot as u32 + 1,
            });
        }
    }
    None
}

/// Веха 47 — AHCI (SATA) на этой платформе нет: QEMU `virt` даёт только virtio-mmio, а
/// таргет-железо riscv (VisionFive 2 и т.п.) использует eMMC/NVMe, не SATA. Заглушка: диск
/// приходит через virtio-blk, драйвер AHCI откатится на него.
pub fn probe_ahci() -> Option<(usize, u32)> {
    None
}

/// Найти virtio-net в тех же 8 mmio-слотах (Веха 34): magic «virt», версия 2,
/// device id **1** (network). IRQ не нужен — драйвер опрашивает кольца.
pub fn probe_virtio_net() -> Option<crate::arch::NetDevice> {
    const MMIO_BASE: usize = 0x1000_1000;
    const MMIO_STRIDE: usize = 0x1000;
    for slot in 0..8 {
        let base = MMIO_BASE + slot * MMIO_STRIDE;
        let r = |off: usize| unsafe { core::ptr::read_volatile((base + off) as *const u32) };
        if r(0x000) == 0x7472_6976 && r(0x004) == 2 && r(0x008) == 1 {
            return Some(crate::arch::NetDevice {
                transport: crate::arch::BlkTransport::Mmio { base },
            });
        }
    }
    None
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

/// Веха 35 — монотонный счётчик тиков для futex-дедлайнов (`rdtime`, таймбаза QEMU
/// virt 10 МГц). Тот же счётчик, что читает U-mode для замеров и `Instant` (TICK_NS=100).
pub fn now_ticks() -> u64 {
    csr::read_time()
}

// ─── память (Sv39) ──────────────────────────────────────────────────────────

/// Имя схемы трансляции — для баннера загрузки.
pub const MM_NAME: &str = "Sv39";

pub use paging::{clone_kernel_root, free_address_space, page_info, translate};

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
    // Веха 36 — парность контракта с x86: FP-состояние (f0..f31) здесь часть кадра
    // и восстановится enter_user.s — метод пуст, но точка вызова та же.
    frame.restore_fp();
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
const RAM_LIMIT: usize = 0x8000_0000 + 128 * 1024 * 1024;

/// Веха 41 — граница используемой RAM (адрес конца). На riscv пока константа (QEMU virt
/// 128 МиБ); разбор `/memory` из device tree (a1) — впереди, как x86-memmap.
pub fn ram_limit() -> usize {
    RAM_LIMIT
}

/// Полная RAM машины (байты) — для отчёта (на riscv = размер от базы RAM).
pub fn ram_total() -> usize {
    RAM_LIMIT - 0x8000_0000
}

/// Веха 41 — платформенная инициализация: на x86 разбирает карту памяти загрузчика; на riscv
/// (QEMU virt) пока no-op — RAM/UART/virtio известны по контракту QEMU (DTB-парсинг впереди).
pub fn platform_init(_hartid: usize, _dtb: usize) {}

/// Веха 42 — реальное железо? На riscv у нас пока только QEMU virt (плата VisionFive 2 —
/// впереди), поэтому всегда `false`: демо на загрузке гоняем, как раньше.
pub fn is_real_hardware() -> bool {
    false
}

/// Выключить машину (SBI SRST; в QEMU — завершить процесс). Задел под автотесты.
#[allow(dead_code)]
pub fn power_off() -> ! {
    sbi::shutdown()
}
