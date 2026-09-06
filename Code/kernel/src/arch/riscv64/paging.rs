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
/// Веха 129 — **программный** бит (RSW, биты 8–9 Sv39 отданы системе): страница ОБЩАЯ, её фреймы
/// принадлежат области разделяемой памяти, а не этому процессу. Иначе смерть процесса освободила
/// бы чужие страницы: `free_private` отдаёт в список свободных всякий лист в RAM.
pub const PTE_SHARED: usize = 1 << 8;

/// Веха 87 — указатель на таблицу страниц по её ФИЗИЧЕСКОМУ адресу (через direct-map).
#[inline(always)]
fn tbl_ptr(pa: usize) -> *mut usize {
    crate::arch::phys_to_virt(pa) as *mut usize
}

/// Физический адрес корневой таблицы ЯДРА (сохраняется в [`init`]) — основа для
/// адресных пространств процессов (см. [`clone_kernel_root`]).
static KERNEL_ROOT: AtomicUsize = AtomicUsize::new(0);

/// Веха 170 — корень таблиц ЯДРА. Нужен ядру, оставшемуся без процесса: стоять в CR3/satp на
/// пространстве чужой группы, ничего в нём не исполняя, значит держать её таблицы неосвобождаемыми.
pub fn kernel_root() -> usize {
    KERNEL_ROOT.load(Ordering::Relaxed)
}

/// Создать корневую таблицу процесса: копия корня ядра (ядро отображено в каждом процессе —
/// БЕЗ флага U, процессу оно недоступно, но trap-обработчик должен исполняться при satp
/// процесса), в которую процесс затем добавит СВОИ страницы (код/данные ELF, стек, кучу) в
/// незанятом ядром регионе VPN[2]=1 (VA 0x4000_0000..0x8000_0000). Копируются только записи
/// верхнего уровня: ядерные подтаблицы разделяются, а слот VPN[2]=1 у процесса свой.
pub fn clone_kernel_root() -> Option<usize> {
    let kroot = KERNEL_ROOT.load(Ordering::Relaxed);
    let new = frame::alloc()?; // Веха 89: нет памяти — отказ, а не паника ядра
    unsafe {
        let src = tbl_ptr(kroot) as *const usize;
        let dst = tbl_ptr(new);
        for i in 0..512 {
            *dst.add(i) = *src.add(i);
        }
    }
    Some(new)
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
    let t = tbl_ptr(tbl) as *const usize;
    let k = tbl_ptr(ktbl) as *const usize;
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
        // Веха 129 — общая страница: освобождает её область разделяемой памяти по счётчику
        // держателей, а не умирающий процесс.
        if leaf && pte & PTE_SHARED != 0 {
            continue;
        }
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
/// Мегастраница Sv39 — листовой PTE на СРЕДНЕМ уровне (2 МиБ). Веха 85: ею стелем direct-map,
/// чтобы гигабайты RAM отображались дёшево (одна запись на 2 МиБ вместо 512 листьев по 4 КиБ).
const MEGA: usize = 2 * 1024 * 1024;
/// Веха 87 — гигастраница Sv39: листовой PTE в КОРНЕВОЙ таблице (1 ГиБ). Поддерживается всегда
/// (в отличие от x86, где 1-ГиБ страницы — опция процессора). Ею стелется direct-map: десятки
/// гигабайт RAM — десятки записей корня и ни одной подтаблицы.
const GIGA: usize = 1024 * 1024 * 1024;

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
        //    Веха 87: VA = phys_to_virt(PA), образ ядра задан ФИЗИЧЕСКИМИ границами.
        //    Веха 88: по КАРТЕ РЕГИОНОВ. У QEMU virt запись одна, но плата с несколькими банками
        //    памяти (или зарезервированной областью для OpenSBI) даст несколько — и дыры между
        //    ними отображать нельзя: за ними нет памяти. Дыр «как на x86» (VGA/BIOS) тут нет —
        //    устройства живут ниже RAM и отображаются тождественно шагом 2.
        for r in frame::regions() {
            map_direct(
                root,
                r.start,
                r.end,
                super::virt_to_phys(text_s),
                super::virt_to_phys(ro_e),
            );
        }
        // 2) MMIO как RW: UART (иначе пропадёт вывод) + слоты virtio-mmio (для диска) + PLIC.
        //    Веха 87: регистры устройств остаются ТОЖДЕСТВЕННЫМИ (в нижней половине) — драйверы
        //    держат физические адреса и говорят с железом по ним. Регион устройств QEMU virt лежит
        //    ниже RAM и ниже региона процессов (VPN[2]=1), поэтому ничему не мешает.
        map_range_id(root, MMIO_START, MMIO_END, PTE_R | PTE_W);
        map_range_id(root, PLIC_START, PLIC_END, PTE_R | PTE_W);
        // Веха 86 — страница часов (goldfish-rtc, адрес из DTB; лежит НИЖЕ региона virtio-mmio,
        // поэтому отдельным отображением). 0 — DTB не дал часов, отображать нечего.
        let rtc = super::rtc_base();
        if rtc != 0 {
            map_range_id(root, rtc, rtc + 0x1000, PTE_R | PTE_W);
        }
        // 3) W^X: перетираем листовые PTE кода и констант более строгими правами (в 4-КиБ вырезе).
        //    Символы образа — уже высокие VA, их физику даёт virt_to_phys (одно окно).
        map_range_dm(root, text_s, text_e, PTE_R | PTE_X);
        map_range_dm(root, ro_s, ro_e, PTE_R);
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
    let pte2 = tbl_ptr(root_pa).add(idx2);
    let table = if *pte2 & PTE_V == 0 {
        let next = frame::alloc().expect("нет фрейма под таблицу");
        *pte2 = ((next >> 12) << 10) | PTE_V; // нелистовой
        next
    } else {
        ((*pte2 >> 10) & PPN_MASK) << 12
    };
    // Уровень 1 — ЛИСТ (мегастраница): права живут здесь.
    let idx1 = (va >> 21) & 0x1ff;
    let pte1 = tbl_ptr(table).add(idx1);
    *pte1 = ((pa >> 12) << 10) | flags | PTE_V | PTE_A | PTE_D;
}

/// Веха 87 — отобразить ГИГАСТРАНИЦУ (1 ГиБ) `va → pa`: листовой PTE прямо в КОРНЕВОЙ таблице
/// (уровень 2). `va`/`pa` выровнены на 1 ГиБ.
///
/// # Safety
/// Как [`map`]: `root_pa` валиден, таблицы доступны через direct-map.
unsafe fn map_giga(root_pa: usize, va: usize, pa: usize, flags: usize) {
    let idx2 = (va >> 30) & 0x1ff;
    *tbl_ptr(root_pa).add(idx2) = ((pa >> 12) << 10) | flags | PTE_V | PTE_A | PTE_D;
}

/// Веха 85/87 — застелить direct-map физической памяти [start, end) мегастраницами RW по
/// `VA = phys_to_virt(PA)`, но чанки, перекрывающие защищаемый диапазон [prot_s, prot_e)
/// (образ ядра — под W^X; границы ФИЗИЧЕСКИЕ), кладём постранично (4 КиБ), чтобы затем можно
/// было сузить права отдельных страниц. Хвост < 2 МиБ — тоже постранично.
unsafe fn map_direct(root_pa: usize, start: usize, end: usize, prot_s: usize, prot_e: usize) {
    let end = (end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let prot_s = prot_s & !(PAGE_SIZE - 1);
    let mut pa = start & !(MEGA - 1);
    while pa < end {
        // Целый свободный гигабайт — ОДНОЙ записью корневой таблицы. Образ ядра лежит в первом
        // гигабайте RAM, поэтому проверяем КАЖДЫЙ кусок, а не прекращаем цикл на первом занятом.
        if pa & (GIGA - 1) == 0 && pa + GIGA <= end && !(pa < prot_e && pa + GIGA > prot_s) {
            map_giga(root_pa, super::phys_to_virt(pa), pa, PTE_R | PTE_W);
            pa += GIGA;
            continue;
        }
        let chunk_end = pa + MEGA;
        let overlaps_kernel = pa < prot_e && chunk_end > prot_s;
        if chunk_end <= end && !overlaps_kernel {
            map_mega(root_pa, super::phys_to_virt(pa), pa, PTE_R | PTE_W);
        } else {
            // Вырез ядра или хвост меньше мегастраницы — постранично.
            let mut p = pa;
            let stop = chunk_end.min(end);
            while p < stop {
                // Загрузочный путь: фреймов на таблицы всегда хватает (RAM ещё не роздана),
                // а если нет — система всё равно нежизнеспособна.
                assert!(map(root_pa, super::phys_to_virt(p), p, PTE_R | PTE_W), "нет фрейма под таблицу direct-map");
                p += PAGE_SIZE;
            }
        }
        pa += MEGA;
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
/// Веха 89 — `false`, если не хватило фрейма под промежуточную таблицу. Раньше здесь стояла
/// паника, а зовут это по запросу ПРОЦЕССА (карта памяти, рост кучи, загрузка ELF) — то есть
/// программа, которой не хватило памяти, роняла ядро вместо себя.
///
/// # Safety
/// `root_pa` — валидная корневая таблица; вызывать до включения paging либо когда
/// все задействованные таблицы доступны по VA == PA.
#[must_use]
pub unsafe fn map(root_pa: usize, va: usize, pa: usize, flags: usize) -> bool {
    let mut table = root_pa;

    // Уровни 2 и 1 — промежуточные (нелистовые).
    let mut level = 2usize;
    while level >= 1 {
        let idx = (va >> (12 + 9 * level)) & 0x1ff;
        let pte = tbl_ptr(table).add(idx);
        if *pte & PTE_V == 0 {
            // Промежуточной таблицы ещё нет — создаём.
            let Some(next) = frame::alloc() else { return false };
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
    let pte = tbl_ptr(table).add(idx);
    *pte = ((pa >> 12) << 10) | flags | PTE_V | PTE_A | PTE_D;
    true
}

/// Веха 129 — снять отображение ОДНОЙ общей страницы (двойник x86, см. его же комментарий).
///
/// Нарочно узкая: только 4-КиБ лист с [`PTE_SHARED`] и только указывающий на названный фрейм
/// `pa`. Зовут это по просьбе процесса, адрес выбирает он — без проверок «снять отображение»
/// стало бы способом продырявить собственный образ.
///
/// # Safety
/// `root_pa` — валидная корневая таблица; TLB сбрасывает вызывающий.
pub unsafe fn unmap_shared(root_pa: usize, va: usize, pa: usize) -> bool {
    let mut table = root_pa;
    let mut level = 2i32;
    while level >= 0 {
        let idx = (va >> (12 + 9 * level as usize)) & 0x1ff;
        let pte = tbl_ptr(table).add(idx);
        if *pte & PTE_V == 0 {
            return false;
        }
        if *pte & (PTE_R | PTE_X) != 0 {
            // Лист. Мега/гигастраница (уровень выше нулевого) общей быть не может — не наша.
            if level != 0 || *pte & PTE_SHARED == 0 || ((*pte >> 10) & PPN_MASK) << 12 != pa {
                return false;
            }
            *pte = 0;
            return true;
        }
        table = ((*pte >> 10) & PPN_MASK) << 12;
        level -= 1;
    }
    false
}

/// Отобразить диапазон [start, end) ТОЖДЕСТВЕННО (VA == PA), постранично — окна MMIO.
unsafe fn map_range_id(root_pa: usize, start: usize, end: usize, flags: usize) {
    let mut va = start & !(PAGE_SIZE - 1);
    let end = (end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    while va < end {
        assert!(map(root_pa, va, va, flags), "нет фрейма под таблицу MMIO");
        va += PAGE_SIZE;
    }
}

/// Веха 87 — переотобразить диапазон ВЫСОКИХ адресов [start, end) на их физику
/// (`VA → virt_to_phys(VA)`), постранично: так уточняются права образа ядра (W^X) поверх
/// уже застеленного direct-map.
unsafe fn map_range_dm(root_pa: usize, start: usize, end: usize, flags: usize) {
    let mut va = start & !(PAGE_SIZE - 1);
    let end = (end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    while va < end {
        assert!(map(root_pa, va, super::virt_to_phys(va), flags), "нет фрейма под таблицу W^X");
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
        let pte = unsafe { *tbl_ptr(table).add(idx) };
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
        let pte = unsafe { *tbl_ptr(table).add(idx) };
        if pte & PTE_V == 0 {
            return None; // невалидно — отображения нет
        }
        if pte & (PTE_R | PTE_X) != 0 {
            // Листовой PTE: дальше не спускаемся. Лист может быть НЕ 4-КиБ (мегастраница на
            // уровне 1 — Веха 85, гигастраница на уровне 2 — Веха 87), поэтому смещение внутри
            // страницы берём по её настоящему размеру, а не по 4 КиБ.
            let size = 1usize << (12 + 9 * level as usize);
            let page = ((pte >> 10) & PPN_MASK) << 12;
            return Some(page | (va & (size - 1)));
        }
        // Нелистовой — спускаемся на уровень ниже.
        table = ((pte >> 10) & PPN_MASK) << 12;
        level -= 1;
    }
    None
}
