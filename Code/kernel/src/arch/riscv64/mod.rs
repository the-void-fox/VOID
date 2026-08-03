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
pub fn console_init(_a0: usize, _a1: usize) {}

/// Веха 96 — пиксельной консоли на riscv нет (в QEMU `virt` дисплея нет вовсе; понадобится
/// virtio-gpu или ramfb). Заглушка арх-контракта, парная x86-версии.
pub fn video_mode() -> Option<(usize, usize, usize)> {
    None
}

/// Веха 97 — фреймбуфера нет, отдавать процессу нечего.
pub fn video_window() -> Option<(usize, usize)> {
    None
}
pub fn video_info() -> (usize, usize, usize, usize, [(u8, u8); 3]) {
    (0, 0, 0, 0, [(0, 0); 3])
}
pub fn video_give_to_user() {}
pub fn video_take_back() {}

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

/// Веха 91 — политика сна СО СРОКОМ: нужны И устройства (кадр разбудит сетевой сервер), И таймер
/// (иначе срок некому заметить). Прежние две политики были взаимоисключающими, и это стоило
/// целого захода: с включённым таймером прерывание карты копилось в PLIC как pending и не
/// доставлялось никогда — pending-бит стоял, а claim его не видел, потому что SEIE был выключен.
pub fn irq_mask_idle(saved: usize) {
    csr::write_sie(saved | (1 << 5) | (1 << 9));
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
    // Веха 91 - прерывание приёма сетевой карты: сервер спит до кадра, а не опрашивает.
    let net = crate::virtio_net::irq();
    if net != 0 {
        plic::init(net); // порог контекста мог быть не выставлен, если диска нет
        plic::enable(net);
        csr::enable_external_interrupt();
        // Снять линию, поднятую ещё во время работы опросом, — иначе PLIC не увидит фронта.
        crate::virtio_net::ack_pending();
    }
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
                irq: slot as u32 + 1, // у QEMU virt источник PLIC = номер слота + 1
            });
        }
    }
    None
}

/// Найти virtio-rng (device id **4**) среди слотов virtio-mmio. Закрывает долг Вехи 86:
/// своего аппаратного ГСЧ у QEMU virt нет (`Zkr` не гарантирован), и без этого устройства
/// вся энтропия riscv держалась на джиттере прерываний.
pub fn probe_virtio_rng() -> Option<crate::arch::BlkTransport> {
    const MMIO_BASE: usize = 0x1000_1000;
    const MMIO_STRIDE: usize = 0x1000;
    for slot in 0..8 {
        let base = MMIO_BASE + slot * MMIO_STRIDE;
        let r = |off: usize| unsafe { core::ptr::read_volatile((base + off) as *const u32) };
        if r(0x000) == 0x7472_6976 && r(0x004) == 2 && r(0x008) == 4 {
            return Some(crate::arch::BlkTransport::Mmio { base });
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

/// Счётчик ТАКТОВ для джиттер-источника энтропии. `None` — прошивка не дала `rdcycle`
/// из S-mode (`mcounteren.CY`), и джиттер собирать не на чем: `rdtime` тикает 10 МГц, в такой
/// сетке разброс задержек памяти не виден. Доступность ПРОВЕРЯЕТСЯ, а не предполагается —
/// иначе первая же плата с другой прошивкой встретила бы нас фатальным трапом.
pub fn now_cycles() -> Option<u64> {
    static AVAILABLE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
    use core::sync::atomic::Ordering;
    // 0 — ещё не пробовали, 1 — есть, 2 — запрещено.
    match AVAILABLE.load(Ordering::Relaxed) {
        1 => return Some(csr::read_cycle()),
        2 => return None,
        _ => {}
    }
    let ok = trap::probe_illegal(|| {
        core::hint::black_box(csr::read_cycle());
    });
    AVAILABLE.store(if ok { 1 } else { 2 }, Ordering::Relaxed);
    ok.then(csr::read_cycle)
}

// ─── память (Sv39) ──────────────────────────────────────────────────────────

/// Веха 87 — смещение direct-map: `VA = PA + KERNEL_OFFSET` ([[0010-address-space-layout]]).
///
/// Это НАЧАЛО верхней половины Sv39: физический ноль ложится ровно туда, поэтому
/// `phys_to_virt` осмыслен для ЛЮБОГО физического адреса (и RAM, и MMIO), а потолок RAM —
/// вся половина, 256 ГиБ. Тем же смещением сдвинуты VMA секций ядра (linker.ld): образ живёт
/// внутри direct-map, отдельного окна ему не нужно (medany адресует всё PC-относительно).
///
/// Взять вместо этого «VMA образа − LMA» было бы ошибкой: тогда физические адреса выше 4 ГиБ
/// переполняли бы 64-битный VA и потолок RAM снова стал бы 2 ГиБ — ровно то, от чего уходим.
pub const KERNEL_OFFSET: usize = 0xFFFF_FFC0_0000_0000;

/// Физический адрес → указатель ядра на него (через direct-map).
#[inline(always)]
pub fn phys_to_virt(pa: usize) -> usize {
    pa.wrapping_add(KERNEL_OFFSET)
}

/// Указатель ядра в direct-map → физический адрес (обратная к [`phys_to_virt`]).
/// Только для адресов ИЗ direct-map: к образу ядра и MMIO-окнам не применять.
#[inline(always)]
pub fn virt_to_phys(va: usize) -> usize {
    va.wrapping_sub(KERNEL_OFFSET)
}

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

/// Отобразить страницу `va → pa` с флагами `MAP_*`. Веха 89 — `false`, если не хватило памяти
/// под промежуточную таблицу: отображение НЕ создано, решать вызывающему (обычно — убить
/// процесс, который его просил, а не ядро).
///
/// # Safety
/// См. `paging::map`: валидный корень; таблицы доступны по VA == PA.
#[must_use]
pub unsafe fn map(root: usize, va: usize, pa: usize, flags: usize) -> bool {
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

/// Веха 87 — единственный оставшийся потолок RAM: сколько физической памяти помещается в
/// direct-map. Он равен размеру верхней половины Sv39 (256 ГиБ), потому что `KERNEL_OFFSET`
/// кладёт физический ноль ровно в её начало. Прежний искусственный CAP (2 ГиБ, Веха 85) снят:
/// он существовал только потому, что ядро жило в нижней половине рядом с процессами.
const DIRECT_MAP_LIMIT: usize = 256 * 1024 * 1024 * 1024;

/// Обнаруженный размер RAM (байты). До разбора DTB — дефолт QEMU virt 128 МиБ.
static RAM_SIZE_CELL: AtomicUsize = AtomicUsize::new(128 * 1024 * 1024);

/// Полная обнаруженная RAM машины (байты) — для отчёта.
pub fn ram_total() -> usize {
    RAM_SIZE_CELL.load(Ordering::Relaxed)
}

/// Веха 85/88 — платформенная инициализация: разобрать `/memory` из device tree (a1 = dtb), чтобы
/// direct-map и аллокатор фреймов взяли реальную карту RAM, а не зашитые 128 МиБ.
/// DTB нет/не разобрать — остаётся дефолт 128 МиБ (контракт QEMU virt по умолчанию).
pub fn platform_init(_hartid: usize, dtb: usize) {
    // Веха 87: a1 от OpenSBI — ФИЗИЧЕСКИЙ адрес блоба, а ядро уже работает в верхней половине
    // (трамплин включил трансляцию) — читаем DTB через direct-map, а не по сырой физике.
    let dtb = if dtb == 0 { 0 } else { phys_to_virt(dtb) };
    let total = dtb_ram_regions(dtb);
    if total > 0 {
        RAM_SIZE_CELL.store(total, Ordering::Relaxed);
    } else {
        crate::frame::add_region(RAM_BASE, RAM_BASE + RAM_SIZE_CELL.load(Ordering::Relaxed));
    }
    // Веха 86 — часы: адрес RTC берём из того же DTB (у QEMU virt это `google,goldfish-rtc`
    // по 0x101000). Отобразит страницу `paging::init`, который идёт следом за platform_init.
    if let Some(base) = dtb_find_compatible(dtb, b"google,goldfish-rtc") {
        RTC_BASE_CELL.store(base, Ordering::Relaxed);
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

/// Веха 88 — разобрать DTB (`dtb` = a1 от OpenSBI) и зарегистрировать КАЖДЫЙ банк RAM из узлов
/// `/memory*` в [`crate::frame::add_region`]. Возвращает суммарный объём (0 — нет/битый DTB или
/// узла нет). Предполагаем #address-cells = #size-cells = 2 (QEMU virt и типовые riscv-платы):
/// `reg` = массив пар [адрес(8) размер(8)] big-endian — узлов и пар может быть несколько (у платы
/// бывает несколько банков). Обход ограничен `totalsize`.
fn dtb_ram_regions(dtb: usize) -> usize {
    if dtb == 0 {
        return 0;
    }
    let mut total = 0usize;
    unsafe {
        if fdt_be32(dtb) != FDT_MAGIC {
            return 0;
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
                            // reg = массив пар [адрес(2 ячейки=8) размер(2 ячейки=8)], big-endian.
                            let mut off = 0usize;
                            while off + 16 <= len {
                                let base = u64::from_be(core::ptr::read_unaligned(
                                    (val + off) as *const u64,
                                )) as usize;
                                let size = u64::from_be(core::ptr::read_unaligned(
                                    (val + off + 8) as *const u64,
                                )) as usize;
                                total = total.saturating_add(size);
                                // Подрезать по досягаемости direct-map: раздать можно только то,
                                // до чего ядро дотянется через `phys_to_virt` (Веха 87).
                                crate::frame::add_region(
                                    base,
                                    base.saturating_add(size).min(DIRECT_MAP_LIMIT),
                                );
                                off += 16;
                            }
                        }
                    }
                }
                FDT_NOP => {}
                FDT_END => break,
                _ => break, // неизвестный токен — прекращаем разбор
            }
        }
    }
    total
}

/// Найти в DTB узел с данным `compatible` и вернуть базовый адрес из его `reg`.
/// `None` — нет DTB или узла. Обход тот же, что у [`dtb_ram_size`]: узел считается найденным,
/// когда у ОДНОГО узла встретились и совпавший `compatible`, и `reg` (проверяем на выходе из узла).
fn dtb_find_compatible(dtb: usize, want: &[u8]) -> Option<usize> {
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
        // Состояние ТЕКУЩЕГО узла: совпал ли compatible и какой у него reg.
        let mut matched = false;
        let mut reg: Option<usize> = None;
        while p + 4 <= end {
            let tok = fdt_be32(p);
            p += 4;
            match tok {
                FDT_BEGIN_NODE => {
                    // Новый узел — состояние прошлого не наследуем (ищем лист с обоими свойствами).
                    matched = false;
                    reg = None;
                    let name_ptr = p;
                    let mut q = name_ptr;
                    while q < end && *(q as *const u8) != 0 {
                        q += 1;
                    }
                    p += ((q - name_ptr) + 1 + 3) & !3;
                }
                FDT_END_NODE => {
                    if matched {
                        if let Some(base) = reg {
                            return Some(base);
                        }
                    }
                    matched = false;
                    reg = None;
                }
                FDT_PROP => {
                    let len = fdt_be32(p) as usize;
                    let nameoff = fdt_be32(p + 4) as usize;
                    let val = p + 8;
                    p += 8 + ((len + 3) & !3);
                    let nptr = strings_base + nameoff;
                    let mut q = nptr;
                    while q < end && *(q as *const u8) != 0 {
                        q += 1;
                    }
                    let pname = core::slice::from_raw_parts(nptr as *const u8, q - nptr);
                    if pname == b"compatible" && len > 0 {
                        // `compatible` — список строк через '\0'; ищем нужную среди них.
                        let bytes = core::slice::from_raw_parts(val as *const u8, len);
                        matched = bytes.split(|b| *b == 0).any(|s| s == want);
                    } else if pname == b"reg" && len >= 8 {
                        // #address-cells = 2 у QEMU virt: первые 8 байт — базовый адрес.
                        let addr = u64::from_be(core::ptr::read_unaligned(val as *const u64));
                        reg = Some(addr as usize);
                    }
                }
                FDT_NOP => {}
                FDT_END => break,
                _ => break,
            }
        }
    }
    None
}

// ─── часы и случайность (Веха 86) ───────────────────────────────────────────

/// База MMIO часов реального времени (`google,goldfish-rtc` у QEMU virt; 0 — часов нет).
/// Заполняется в [`platform_init`] из DTB, ДО включения трансляции; страницу отображает
/// `paging::init` (см. [`rtc_base`]).
static RTC_BASE_CELL: AtomicUsize = AtomicUsize::new(0);

/// Регистры goldfish-rtc: младшие/старшие 32 бита наносекунд Unix.
const RTC_TIME_LOW: usize = 0x00;
const RTC_TIME_HIGH: usize = 0x04;

/// База RTC для отображения MMIO-страницы в `paging::init` (0 — нечего отображать).
pub fn rtc_base() -> usize {
    RTC_BASE_CELL.load(Ordering::Relaxed)
}

/// Настенное время от платформы — goldfish-rtc, наносекунды Unix. `None` — часов нет.
/// Порядок чтения важен: чтение `TIME_LOW` защёлкивает старшую половину (иначе можно поймать
/// перенос между двумя чтениями).
pub fn wall_clock_unix_ns() -> Option<u64> {
    let base = rtc_base();
    if base == 0 {
        return None;
    }
    unsafe {
        let lo = core::ptr::read_volatile((base + RTC_TIME_LOW) as *const u32) as u64;
        let hi = core::ptr::read_volatile((base + RTC_TIME_HIGH) as *const u32) as u64;
        let ns = hi << 32 | lo;
        // 0 — часы есть, но не идут (или мы читаем не то устройство): лучше признать, что часов нет.
        (ns > 0).then_some(ns)
    }
}

/// Аппаратной случайности на QEMU virt нет (расширение Zkr не гарантировано, virtio-rng не
/// подключён) — общий код [`crate::random`] замешивает энтропию сам.
pub fn hw_random_u64() -> Option<u64> {
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
