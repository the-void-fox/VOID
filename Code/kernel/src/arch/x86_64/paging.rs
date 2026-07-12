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

/// Окно virtio-mmio проб (совместимость с общим драйвером: на x86 устройств там нет,
/// unassigned-чтения вернут 0xFF и magic не совпадёт — драйвер честно скажет «нет диска»).
const VIRTIO_MMIO_START: usize = 0x1000_0000;
const VIRTIO_MMIO_END: usize = 0x1000_9000;

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
        // 3) MMIO: LAPIC + окно virtio-mmio (см. выше).
        map_range(root, super::lapic::LAPIC_BASE, super::lapic::LAPIC_BASE + PAGE_SIZE, PTE_W | PTE_NX);
        map_range(root, VIRTIO_MMIO_START, VIRTIO_MMIO_END, PTE_W | PTE_NX);
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
/// Копируются только записи верхнего уровня — подтаблицы ядра разделяются.
pub fn clone_kernel_root() -> usize {
    let kroot = KERNEL_ROOT.load(Ordering::Relaxed);
    let new = frame::alloc().expect("нет фрейма под PML4 процесса");
    unsafe {
        let src = kroot as *const u64;
        let dst = new as *mut u64;
        for i in 0..512 {
            *dst.add(i) = *src.add(i);
        }
    }
    new
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
