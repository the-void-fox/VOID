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

use core::sync::atomic::{AtomicUsize, Ordering};

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

/// Веха 48 — установщик на диск (образ через multiboot2-модуль) — только x86. Заглушка.
pub fn boot_module() -> Option<(usize, usize)> {
    None
}

/// Веха 49 — сетевая карта e1000 (Intel PRO/1000, PCI) — только x86. На riscv/QEMU-virt сеть
/// приходит через virtio-mmio. Заглушка: драйвер e1000 откатится на virtio-net.
pub fn probe_e1000() -> Option<usize> {
    None
}

/// Веха 52 — маршрутизация IRQ e1000 в userspace — только x86 (IOAPIC). Заглушка.
pub fn e1000_irq_setup() -> Option<u8> {
    None
}

/// Веха 52 — «взвести» IRQ userspace-драйвера перед `SYS_IRQ_WAIT`. На riscv таких драйверов
/// пока нет (e1000/USB — x86), делать нечего.
pub fn userdrv_irq_arm() {}

// Веха 50 — xHCI/USB-HID есть только на x86 (probe_xhci/usb_key), поэтому в riscv-контракте их
// нет: `xhci`-модуль ядра на riscv — заглушка, никто эти функции тут не зовёт.

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

/// База RAM на QEMU virt (адрес начала физической памяти).
pub const RAM_BASE: usize = 0x8000_0000;

/// Веха 85 — потолок используемой RAM (2 ГиБ). direct-map кладём huge-страницами (2 МиБ), так
/// что гигабайты стелятся дёшево; 2 ГиБ держит границу ниже MMIO-дыр и внешне-4-ГиБ RAM QEMU virt
/// (RAM там сплошная от 0x8000_0000, устройства — НИЖЕ базы), избегая карты памяти с дырами.
const RAM_CAP: usize = 2 * 1024 * 1024 * 1024;

/// Обнаруженный размер RAM (байты). До разбора DTB — дефолт QEMU virt 128 МиБ.
static RAM_SIZE_CELL: AtomicUsize = AtomicUsize::new(128 * 1024 * 1024);

/// Веха 85 — граница используемой RAM (адрес конца, exclusive): база + min(обнаружено, потолок).
pub fn ram_limit() -> usize {
    RAM_BASE + RAM_SIZE_CELL.load(Ordering::Relaxed).min(RAM_CAP)
}

/// Полная обнаруженная RAM машины (байты) — для отчёта.
pub fn ram_total() -> usize {
    RAM_SIZE_CELL.load(Ordering::Relaxed)
}

/// Веха 85 — платформенная инициализация: разобрать `/memory` из device tree (a1 = dtb), чтобы
/// direct-map и аллокатор фреймов взяли реальный объём RAM (`-m 2G` и т.п.), а не зашитые 128 МиБ.
/// DTB нет/не разобрать — остаётся дефолт 128 МиБ (контракт QEMU virt по умолчанию).
pub fn platform_init(_hartid: usize, dtb: usize) {
    if let Some(size) = dtb_ram_size(dtb) {
        RAM_SIZE_CELL.store(size, Ordering::Relaxed);
    }
}

// ─── разбор device tree (FDT) — только узел /memory (Веха 85) ────────────────
const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

/// Прочитать big-endian u32 по абсолютному адресу (FDT всегда big-endian, даже на LE-хосте).
unsafe fn fdt_be32(addr: usize) -> u32 {
    u32::from_be(core::ptr::read_unaligned(addr as *const u32))
}

/// Разобрать DTB (`dtb` = a1 от OpenSBI) и вернуть размер RAM узла `/memory` (байты). `None` —
/// нет/битый DTB или узел не найден. Предполагаем #address-cells = #size-cells = 2 (QEMU virt):
/// `reg` = [адрес(8) размер(8)] big-endian; берём размер. Обход ограничен `totalsize`.
fn dtb_ram_size(dtb: usize) -> Option<usize> {
    if dtb == 0 {
        return None;
    }
    unsafe {
        if fdt_be32(dtb) != FDT_MAGIC {
            return None;
        }
        let totalsize = fdt_be32(dtb + 4) as usize;
        let off_struct = fdt_be32(dtb + 8) as usize;
        let off_strings = fdt_be32(dtb + 12) as usize;
        let end = dtb + totalsize;
        let strings_base = dtb + off_strings;
        let mut p = dtb + off_struct;
        let mut in_memory = false;
        while p + 4 <= end {
            let tok = fdt_be32(p);
            p += 4;
            match tok {
                FDT_BEGIN_NODE => {
                    let name_ptr = p;
                    let mut q = name_ptr;
                    while q < end && *(q as *const u8) != 0 {
                        q += 1;
                    }
                    let name = core::slice::from_raw_parts(name_ptr as *const u8, q - name_ptr);
                    // Узел RAM — «memory» или «memory@<адрес>».
                    in_memory = name.starts_with(b"memory")
                        && (name.len() == 6 || name[6] == b'@');
                    p += ((q - name_ptr) + 1 + 3) & !3; // имя + '\0', выровнено на 4
                }
                FDT_END_NODE => in_memory = false,
                FDT_PROP => {
                    let len = fdt_be32(p) as usize;
                    let nameoff = fdt_be32(p + 4) as usize;
                    let val = p + 8;
                    p += 8 + ((len + 3) & !3);
                    if in_memory && len >= 16 {
                        let nptr = strings_base + nameoff;
                        let mut q = nptr;
                        while q < end && *(q as *const u8) != 0 {
                            q += 1;
                        }
                        let pname = core::slice::from_raw_parts(nptr as *const u8, q - nptr);
                        if pname == b"reg" {
                            // reg = адрес(2 ячейки=8) размер(2 ячейки=8), big-endian; берём размер.
                            let size = u64::from_be(core::ptr::read_unaligned(
                                (val + 8) as *const u64,
                            ));
                            return Some(size as usize);
                        }
                    }
                }
                FDT_NOP => {}
                FDT_END => break,
                _ => break, // неизвестный токен — прекращаем разбор
            }
        }
    }
    None
}

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
