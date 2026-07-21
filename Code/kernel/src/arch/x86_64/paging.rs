//! 4-уровневый пейджинг x86_64 (Веха 25) — брат-близнец riscv64/paging.rs (Sv39).
//!
//! Та же стратегия: **direct map** — вся RAM идентично (VA == PA) как RW+NX, затем
//! ужесточение прав ядра (код R+X, константы R+NX) — W^X; окна MMIO (virtio-mmio-пробы,
//! LAPIC) — RW+NX. Отличия от Sv39 — только форма PTE: 4 уровня по 512 записей,
//! исполнение управляется битом **NX** (запрет, а не разрешение — поэтому EFER.NXE
//! включён трамплином), права U требуются на ВСЕХ уровнях пути.

use crate::frame::{self, PAGE_SIZE};

// ─── Биты PTE ────────────────────────────────────────────────────────────────
pub const PTE_P: u64 = 1 << 0; // Present
pub const PTE_W: u64 = 1 << 1; // Writable
pub const PTE_U: u64 = 1 << 2; // User
pub const PTE_NX: u64 = 1 << 63; // No-eXecute (требует EFER.NXE)

const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000; // физ. адрес внутри PTE

use core::sync::atomic::{AtomicUsize, Ordering};

/// Корень таблиц ЯДРА (PML4) — основа адресных пространств процессов.
static KERNEL_ROOT: AtomicUsize = AtomicUsize::new(0);

extern "C" {
    static _text_start: u8;
    static _text_end: u8;
    static _rodata_start: u8;
    static _rodata_end: u8;
}

/// Построить таблицы ядра: direct map RAM (RW+NX) → W^X кода/констант → окна MMIO.
/// Возвращает физический адрес PML4.
pub fn init() -> usize {
    frame::init();
    let root = frame::alloc().expect("нет фрейма под PML4");

    let text_s = &raw const _text_start as usize;
    let text_e = &raw const _text_end as usize;
    let ro_s = &raw const _rodata_start as usize;
    let ro_e = &raw const _rodata_end as usize;

    unsafe {
        // 1) direct map всей RAM как RW+NX (данные не исполняются).
        map_range(root, 0, super::RAM_LIMIT, PTE_W | PTE_NX);
        // 2) W^X: код R+X (без W и без NX), константы R+NX.
        map_range(root, text_s, text_e, 0);
        map_range(root, ro_s, ro_e, PTE_NX);
        // 3) MMIO контроллеров прерываний: LAPIC + IOAPIC (BAR'ы PCI отобразит
        //    map_mmio, когда их найдёт pci::probe_virtio_blk — они известны в рантайме).
        map_range(root, super::lapic::LAPIC_BASE, super::lapic::LAPIC_BASE + PAGE_SIZE, PTE_W | PTE_NX);
        map_range(root, super::ioapic::IOAPIC_BASE, super::ioapic::IOAPIC_BASE + PAGE_SIZE, PTE_W | PTE_NX);
    }
    KERNEL_ROOT.store(root, Ordering::Relaxed);
    root
}

/// Включить трансляцию по нашим таблицам: загрузить CR3 (заодно полный сброс TLB).
///
/// # Safety
/// Таблицы обязаны идентично отображать текущие PC/SP/стек (см. riscv64::paging::enable).
pub unsafe fn enable(root_pa: usize) {
    core::arch::asm!("mov cr3, {0}", in(reg) root_pa, options(nostack));
}

/// Создать корневую таблицу процесса: копия PML4 ядра (ядро отображено без U — нужно
/// обработчику trap'ов при CR3 процесса), слоты процесса добавит elf::load/proc.
///
/// ВАЖНОЕ отличие от Sv39 (Веха 26): там user-регион (VPN[2]=1) — отдельный слот
/// КОРНЯ, и копии корня достаточно. Здесь PML4[0] покрывает 0..512 ГиБ — и ядро,
/// и user-регион (1..2 ГиБ) живут под ОДНОЙ записью; разделять её PDPT нельзя —
/// маппинги процессов попали бы в общие таблицы и перепутались между собой.
/// Поэтому дополнительно копируем PDPT из PML4[0]: его запись [1] (1..2 ГиБ — весь
/// user: ELF+куча+стек < 0x8000_0000) у ядра пуста, под ней вырастут приватные
/// таблицы процесса; PD/PT ядра (запись [0], [3]) разделяются как раньше.
pub fn clone_kernel_root() -> usize {
    let kroot = KERNEL_ROOT.load(Ordering::Relaxed);
    let new = frame::alloc().expect("нет фрейма под PML4 процесса");
    let pdpt = frame::alloc().expect("нет фрейма под PDPT процесса");
    unsafe {
        let src = kroot as *const u64;
        let dst = new as *mut u64;
        for i in 0..512 {
            *dst.add(i) = *src.add(i);
        }
        let kpdpt = (*src & ADDR_MASK) as *const u64;
        let dpdpt = pdpt as *mut u64;
        for i in 0..512 {
            *dpdpt.add(i) = *kpdpt.add(i);
        }
        *dst = pdpt as u64 | (*src & !ADDR_MASK); // PML4[0] → приватный PDPT, флаги те же
    }
    new
}

/// Отобразить MMIO-диапазон [pa, pa+len) идентично (RW+NX) в таблицы ЯДРА уже в
/// рантайме — BAR'ы PCI известны только после поиска устройства. Зваться обязан ДО
/// первого клона пространств: новые записи верхних уровней в копии не попадут
/// (сегодня так и есть: virtio_blk::init идёт раньше первого spawn'а).
///
/// # Safety
/// `pa` — настоящий MMIO этой машины; попадание в RAM перетёрло бы прямое отображение.
pub unsafe fn map_mmio(pa: usize, len: usize) {
    let root = KERNEL_ROOT.load(Ordering::Relaxed);
    map_range(root, pa, pa + len, PTE_W | PTE_NX);
    super::flush_tlb();
}

/// Отобразить одну 4 КиБ-страницу `va → pa`. `flags` — биты PTE (`PTE_W`/`PTE_U`/`PTE_NX`),
/// P ставится всегда. Промежуточные уровни получают P|W|U: право решает ЛИСТ, но U на
/// пути обязателен для доступа из ring3 (особенность x86 против Sv39).
///
/// # Safety
/// `root_pa` — валидный PML4; таблицы доступны по VA == PA (direct map).
pub unsafe fn map(root_pa: usize, va: usize, pa: usize, flags: u64) {
    let mut table = root_pa;
    let mut level = 3usize; // PML4 → PDPT → PD → PT
    while level >= 1 {
        let idx = (va >> (12 + 9 * level)) & 0x1ff;
        let pte = (table as *mut u64).add(idx);
        if *pte & PTE_P == 0 {
            let next = frame::alloc().expect("нет фрейма под таблицу");
            *pte = next as u64 | PTE_P | PTE_W | PTE_U;
            table = next;
        } else {
            table = (*pte & ADDR_MASK) as usize;
        }
        level -= 1;
    }
    let idx = (va >> 12) & 0x1ff;
    let pte = (table as *mut u64).add(idx);
    *pte = (pa as u64 & ADDR_MASK) | PTE_P | flags;
}

/// Отобразить диапазон [start, end) идентично (VA == PA), постранично.
unsafe fn map_range(root_pa: usize, start: usize, end: usize, flags: u64) {
    let mut va = start & !(PAGE_SIZE - 1);
    let end = (end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    while va < end {
        map(root_pa, va, va, flags);
        va += PAGE_SIZE;
    }
}

/// Веха 37 — обход VA→(PA страницы, сырой листовой PTE) для чекпойнта процессов:
/// перевод PTE-битов в арх-нейтральные MAP_* делает обёртка в mod.rs (как у `map`,
/// только в обратную сторону).
pub fn page_info(root_pa: usize, va: usize) -> Option<(usize, u64)> {
    let mut table = root_pa;
    let mut level = 3i32;
    let mut pte = 0u64;
    while level >= 0 {
        let idx = (va >> (12 + 9 * level as usize)) & 0x1ff;
        pte = unsafe { *(table as *const u64).add(idx) };
        if pte & PTE_P == 0 {
            return None;
        }
        table = (pte & ADDR_MASK) as usize;
        level -= 1;
    }
    Some((table, pte))
}

/// Программный обход VA→PA (то, что аппаратно делает MMU). None — не отображено.
pub fn translate(root_pa: usize, va: usize) -> Option<usize> {
    let mut table = root_pa;
    let mut level = 3i32;
    while level >= 0 {
        let idx = (va >> (12 + 9 * level as usize)) & 0x1ff;
        let pte = unsafe { *(table as *const u64).add(idx) };
        if pte & PTE_P == 0 {
            return None;
        }
        table = (pte & ADDR_MASK) as usize;
        level -= 1;
    }
    Some(table | (va & (PAGE_SIZE - 1)))
}
