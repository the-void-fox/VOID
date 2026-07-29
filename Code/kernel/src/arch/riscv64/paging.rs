//! Sv39 — трёхуровневая виртуальная память RISC-V.
//!
//! Виртуальный адрес (39 значащих бит) делится на три 9-битных индекса VPN[2..0]
//! и 12-битное смещение. Перевод идёт по дереву из трёх уровней таблиц по 512
//! записей (PTE) в каждой; лист хранит физический номер страницы (PPN) и флаги.
//!
//! ```text
//!   VA:  | VPN[2] (9) | VPN[1] (9) | VPN[0] (9) | offset (12) |
//!   PTE: |        PPN (44)        | RSW(2) | D A G U X W R V |
//! ```
//!
//! Стратегия Вехи 3: **direct map** — отобразить всю физическую RAM идентично
//! (VA == PA) как RW, затем ужесточить права ядра (код R+X, константы R) — это W^X.
//! Так после включения трансляции исполнение продолжается на тех же адресах, а все
//! структуры (стек, сами таблицы, будущая куча) остаются доступны по VA == PA.

use core::arch::asm;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::frame::{self, PAGE_SIZE};

// ─── Биты флагов PTE ────────────────────────────────────────────────────────
pub const PTE_V: usize = 1 << 0; // Valid — запись действительна
pub const PTE_R: usize = 1 << 1; // Read
pub const PTE_W: usize = 1 << 2; // Write
pub const PTE_X: usize = 1 << 3; // eXecute
pub const PTE_U: usize = 1 << 4; // User — страница доступна из U-mode
pub const PTE_A: usize = 1 << 6; // Accessed — выставляем сами, чтобы не словить fault
pub const PTE_D: usize = 1 << 7; // Dirty    — то же для записи

/// Физический адрес корневой таблицы ЯДРА (сохраняется в [`init`]) — основа для
/// адресных пространств процессов (см. [`clone_kernel_root`]).
static KERNEL_ROOT: AtomicUsize = AtomicUsize::new(0);

/// Создать корневую таблицу процесса: копия корня ядра (ядро отображено в каждом процессе —
/// БЕЗ флага U, процессу оно недоступно, но trap-обработчик должен исполняться при satp
/// процесса), в которую процесс затем добавит СВОИ страницы (код/данные ELF, стек, кучу) в
/// незанятом ядром регионе VPN[2]=1 (VA 0x4000_0000..0x8000_0000). Копируются только записи
/// верхнего уровня: ядерные подтаблицы разделяются, а слот VPN[2]=1 у процесса свой.
pub fn clone_kernel_root() -> usize {
    let kroot = KERNEL_ROOT.load(Ordering::Relaxed);
    let new = frame::alloc().expect("нет фрейма под корень процесса");
    unsafe {
        let src = kroot as *const usize;
        let dst = new as *mut usize;
        for i in 0..512 {
            *dst.add(i) = *src.add(i);
        }
    }
    new
}

/// Маска PPN внутри PTE — 44 бита.
const PPN_MASK: usize = (1 << 44) - 1;

/// Веха 46 — освободить ВСЕ приватные фреймы адресного пространства процесса: листовые
/// страницы (код/данные/стек/куча) + промежуточные таблицы + сам корень. Ядерные подтаблицы
/// (общие для всех пространств) НЕ трогаем: их узнаём сравнением с корнем ядра — записи,
/// скопированные из [`clone_kernel_root`] дословно, идентичны и пропускаются; приватные
/// (слот VPN[2]=1 у процесса свой) — отличаются и уходят под нож рекурсивно.
///
/// # Safety
/// `root_pa` — корень ПРОЦЕССА, который БОЛЬШЕ НЕ АКТИВЕН (satp уже переключён на живое
/// пространство): освобождать таблицы под текущим satp нельзя — MMU читала бы их из
/// списка свободных. Вызывать один раз на пространство.
pub unsafe fn free_address_space(root_pa: usize) {
    let kroot = KERNEL_ROOT.load(Ordering::Relaxed);
    free_private(root_pa, kroot, 2); // Sv39: корень — уровень 2
}

/// Рекурсивно освободить таблицу `tbl` (уровня `level`) и её приватных потомков. `ktbl` —
/// параллельная таблица ЯДРА того же уровня (`0` — у ядра её нет): записи, совпадающие с
/// ядром, — общие, пропускаются; прочие приватны. На уровне 0 записи — листовые страницы.
unsafe fn free_private(tbl: usize, ktbl: usize, level: usize) {
    let t = tbl as *const usize;
    let k = ktbl as *const usize;
    for i in 0..512 {
        let pte = *t.add(i);
        if pte & PTE_V == 0 {
            continue;
        }
        if ktbl != 0 && pte == *k.add(i) {
            continue; // общая с ядром запись — не наша
        }
        let child = ((pte >> 10) & PPN_MASK) << 12;
        let leaf = pte & (PTE_R | PTE_W | PTE_X) != 0;
        if level == 0 || leaf {
            // Веха 51: листья userspace-драйвера могут указывать на MMIO устройства (не RAM) —
            // такие НЕ освобождаем (иначе адрес железа попал бы в список свободных фреймов).
            if frame::is_ram(child) {
                frame::free(child); // листовая страница RAM
            }
        } else {
            // Ядерный потомок того же слота (если у ядра он есть и это подтаблица).
            let kchild = if ktbl != 0 && *k.add(i) & PTE_V != 0 && *k.add(i) & (PTE_R | PTE_W | PTE_X) == 0 {
                ((*k.add(i) >> 10) & PPN_MASK) << 12
            } else {
                0
            };
            free_private(child, kchild, level - 1);
        }
    }
    frame::free(tbl); // сама таблица — после всех детей
}

// MMIO-регион QEMU virt: UART (0x1000_0000) + 8 слотов virtio-mmio (0x1000_1000..0x1000_9000).
const MMIO_START: usize = 0x1000_0000;
const MMIO_END: usize = 0x1000_9000;
// PLIC (контроллер прерываний устройств): до claim/complete контекста 1 включительно.
const PLIC_START: usize = 0x0c00_0000;
const PLIC_END: usize = 0x0c20_3000;
const RAM_START: usize = 0x8000_0000;
/// Мегастраница Sv39 — листовой PTE на СРЕДНЕМ уровне (2 МиБ). Веха 85: ею стелем direct-map,
/// чтобы гигабайты RAM отображались дёшево (одна запись на 2 МиБ вместо 512 листьев по 4 КиБ).
const MEGA: usize = 2 * 1024 * 1024;

extern "C" {
    static _text_start: u8;
    static _text_end: u8;
    static _rodata_start: u8;
    static _rodata_end: u8;
}

/// Построить корневую таблицу ядра и вернуть её физический адрес.
pub fn init() -> usize {
    frame::init();
    let root = frame::alloc().expect("нет фрейма под корневую таблицу");

    let text_s = &raw const _text_start as usize;
    let text_e = &raw const _text_end as usize;
    let ro_s = &raw const _rodata_start as usize;
    let ro_e = &raw const _rodata_end as usize;

    unsafe {
        // 1) direct map всей (обнаруженной) RAM как RW — база, чтобы всё осталось доступно. Веха 85:
        //    мегастраницами (2 МиБ), но чанки, накрывающие образ ядра [text_s, ro_e), кладём
        //    постранично (4 КиБ) — иначе шаг 3 (W^X) не смог бы сузить права страницы кода/констант.
        map_direct(root, RAM_START, super::ram_limit(), text_s, ro_e);
        // 2) MMIO как RW: UART (иначе пропадёт вывод) + слоты virtio-mmio (для диска) + PLIC.
        map_range(root, MMIO_START, MMIO_END, PTE_R | PTE_W);
        map_range(root, PLIC_START, PLIC_END, PTE_R | PTE_W);
        // Веха 86 — страница часов (goldfish-rtc, адрес из DTB; лежит НИЖЕ региона virtio-mmio,
        // поэтому отдельным отображением). 0 — DTB не дал часов, отображать нечего.
        let rtc = super::rtc_base();
        if rtc != 0 {
            map_range(root, rtc, rtc + 0x1000, PTE_R | PTE_W);
        }
        // 3) W^X: перетираем листовые PTE кода и констант более строгими правами (в 4-КиБ вырезе).
        map_range(root, text_s, text_e, PTE_R | PTE_X);
        map_range(root, ro_s, ro_e, PTE_R);
        // Флага U нет НИ У ОДНОЙ страницы ядра (Веха 23: секция `.user` похоронена) — код
        // U-mode приходит только из ELF-программ store и маппится в elf::load / proc.rs.
    }
    KERNEL_ROOT.store(root, Ordering::Relaxed);
    root
}

/// Отобразить одну МЕГАСТРАНИЦУ (2 МиБ) `va → pa`: листовой PTE на среднем уровне (level 1). `va`
/// и `pa` обязаны быть выровнены на 2 МиБ. Промежуточную таблицу верхнего уровня создаёт при нужде.
///
/// # Safety
/// Как [`map`]: `root_pa` валиден, таблицы доступны по VA == PA.
unsafe fn map_mega(root_pa: usize, va: usize, pa: usize, flags: usize) {
    // Уровень 2 (верхний) — промежуточный: спускаемся/создаём таблицу уровня 1.
    let idx2 = (va >> 30) & 0x1ff;
    let pte2 = (root_pa as *mut usize).add(idx2);
    let table = if *pte2 & PTE_V == 0 {
        let next = frame::alloc().expect("нет фрейма под таблицу");
        *pte2 = ((next >> 12) << 10) | PTE_V; // нелистовой
        next
    } else {
        ((*pte2 >> 10) & PPN_MASK) << 12
    };
    // Уровень 1 — ЛИСТ (мегастраница): права живут здесь.
    let idx1 = (va >> 21) & 0x1ff;
    let pte1 = (table as *mut usize).add(idx1);
    *pte1 = ((pa >> 12) << 10) | flags | PTE_V | PTE_A | PTE_D;
}

/// Веха 85 — застелить direct-map [start, end) идентично (VA == PA) мегастраницами RW, но чанки,
/// перекрывающие защищаемый диапазон [prot_s, prot_e) (образ ядра — под W^X), кладём постранично
/// (4 КиБ), чтобы затем можно было сузить права отдельных страниц. Хвост < 2 МиБ — тоже постранично.
unsafe fn map_direct(root_pa: usize, start: usize, end: usize, prot_s: usize, prot_e: usize) {
    let end = (end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let prot_s = prot_s & !(PAGE_SIZE - 1);
    let mut va = start & !(MEGA - 1);
    while va < end {
        let chunk_end = va + MEGA;
        let overlaps_kernel = va < prot_e && chunk_end > prot_s;
        if chunk_end <= end && !overlaps_kernel {
            map_mega(root_pa, va, va, PTE_R | PTE_W);
        } else {
            // Вырез ядра или хвост меньше мегастраницы — постранично.
            let mut p = va;
            let stop = chunk_end.min(end);
            while p < stop {
                map(root_pa, p, p, PTE_R | PTE_W);
                p += PAGE_SIZE;
            }
        }
        va += MEGA;
    }
}

/// Включить трансляцию: `satp = Sv39 | PPN(корень)`, затем сбросить TLB.
///
/// # Safety
/// Вызывать один раз, когда таблицы уже построены и идентично отображают
/// текущие PC/SP/стек, иначе следующая же инструкция уйдёт в page fault.
pub unsafe fn enable(root_pa: usize) {
    let satp = (8usize << 60) | (root_pa >> 12); // MODE=8 (Sv39), ASID=0, PPN корня
    asm!(
        "csrw satp, {satp}",
        "sfence.vma",          // сбросить кэш трансляций (TLB)
        satp = in(reg) satp,
        options(nostack),
    );
}

/// Отобразить одну 4 КиБ-страницу `va → pa` с флагами, создавая промежуточные
/// таблицы по пути.
///
/// # Safety
/// `root_pa` — валидная корневая таблица; вызывать до включения paging либо когда
/// все задействованные таблицы доступны по VA == PA.
pub unsafe fn map(root_pa: usize, va: usize, pa: usize, flags: usize) {
    let mut table = root_pa;

    // Уровни 2 и 1 — промежуточные (нелистовые).
    let mut level = 2usize;
    while level >= 1 {
        let idx = (va >> (12 + 9 * level)) & 0x1ff;
        let pte = (table as *mut usize).add(idx);
        if *pte & PTE_V == 0 {
            // Промежуточной таблицы ещё нет — создаём.
            let next = frame::alloc().expect("нет фрейма под таблицу");
            *pte = ((next >> 12) << 10) | PTE_V; // нелистовой: R=W=X=0
            table = next;
        } else {
            // Спускаемся к существующей.
            table = ((*pte >> 10) & PPN_MASK) << 12;
        }
        level -= 1;
    }

    // Уровень 0 — листовой PTE (тут и живут права доступа).
    let idx = (va >> 12) & 0x1ff;
    let pte = (table as *mut usize).add(idx);
    *pte = ((pa >> 12) << 10) | flags | PTE_V | PTE_A | PTE_D;
}

/// Отобразить диапазон [start, end) идентично (VA == PA), постранично.
unsafe fn map_range(root_pa: usize, start: usize, end: usize, flags: usize) {
    let mut va = start & !(PAGE_SIZE - 1);
    let end = (end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    while va < end {
        map(root_pa, va, va, flags);
        va += PAGE_SIZE;
    }
}

/// Веха 37 — обход VA→(PA страницы, флаги MAP_*) для чекпойнта процессов: ядру нужно
/// не только «куда», но и «с какими правами» страница отображена, чтобы образ восстановил
/// W^X-раскладку буквально. На Sv39 MAP_* == биты PTE — маска без перевода.
pub fn page_info(root_pa: usize, va: usize) -> Option<(usize, usize)> {
    let page = translate(root_pa, va & !(PAGE_SIZE - 1))?;
    // Листовой PTE уже найден translate'ом; флаги достаём повторным спуском к нему.
    let mut table = root_pa;
    let mut level = 2i32;
    loop {
        let idx = (va >> (12 + 9 * level as usize)) & 0x1ff;
        let pte = unsafe { *(table as *const usize).add(idx) };
        if pte & (PTE_R | PTE_X) != 0 || level == 0 {
            return Some((page, pte & (PTE_R | PTE_W | PTE_X | PTE_U)));
        }
        table = ((pte >> 10) & PPN_MASK) << 12;
        level -= 1;
    }
}

/// Программный обход дерева VA→PA — ровно то, что аппаратно делает MMU.
/// Возвращает физический адрес или `None`, если страница не отображена.
#[allow(dead_code)] // отладочный инструмент: пригодится для page fault'ов (Веха 6+)
pub fn translate(root_pa: usize, va: usize) -> Option<usize> {
    let mut table = root_pa;
    let mut level = 2i32;
    while level >= 0 {
        let idx = (va >> (12 + 9 * level as usize)) & 0x1ff;
        let pte = unsafe { *(table as *const usize).add(idx) };
        if pte & PTE_V == 0 {
            return None; // невалидно — отображения нет
        }
        if pte & (PTE_R | PTE_X) != 0 {
            // Листовой PTE: дальше не спускаемся.
            let page = ((pte >> 10) & PPN_MASK) << 12;
            return Some(page | (va & (PAGE_SIZE - 1)));
        }
        // Нелистовой — спускаемся на уровень ниже.
        table = ((pte >> 10) & PPN_MASK) << 12;
        level -= 1;
    }
    None
}
