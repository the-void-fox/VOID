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
pub const PTE_PS: u64 = 1 << 7; // Page Size — на PD-записи это ЛИСТ 2 МиБ (huge page, Веха 85)
pub const PTE_NX: u64 = 1 << 63; // No-eXecute (требует EFER.NXE)

const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000; // физ. адрес внутри PTE

/// Размер huge-страницы (2 МиБ) — direct-map стелем ею (Веха 85), чтобы гигабайты RAM отображались
/// дёшево (одна PD-запись на 2 МиБ вместо 512 листьев по 4 КиБ).
const HUGE: usize = 2 * 1024 * 1024;
/// Веха 87 — гигастраница: лист прямо в PDPT (1 ГиБ). В отличие от Sv39 это ОПЦИЯ процессора
/// (CPUID-бит PDPE1GB), поэтому перед использованием спрашиваем — см. [`giga_pages_supported`].
const GIGA: usize = 1024 * 1024 * 1024;

/// Веха 88 — граница «низа»: ниже неё дыры карты памяти закрываем отображением (там VGA/BIOS/ACPI,
/// к которым ядро обращается через direct-map), выше — стелем строго по регионам.
const FOUR_GIB: usize = 4 * GIGA;

use core::sync::atomic::{AtomicUsize, Ordering};

/// Веха 87 — указатель на таблицу страниц по её ФИЗИЧЕСКОМУ адресу (через direct-map).
/// Обходчики таблиц ходят по физическим адресам из PTE, а трогать их можно только так.
#[inline(always)]
fn tbl_ptr(pa: usize) -> *mut u64 {
    crate::arch::phys_to_virt(pa) as *mut u64
}

/// Корень таблиц ЯДРА (PML4) — основа адресных пространств процессов.
static KERNEL_ROOT: AtomicUsize = AtomicUsize::new(0);

extern "C" {
    static _kernel_start: u8;
    static _text_start: u8;
    static _text_end: u8;
    static _rodata_start: u8;
    static _rodata_end: u8;
    static _data_start: u8;
    static _kernel_end: u8;
}

/// Построить таблицы ядра: direct map RAM (RW+NX) → окно образа с W^X → окна MMIO.
/// Возвращает физический адрес PML4.
///
/// Веха 87 — три разных вида отображений, и их важно не путать:
/// - **direct-map** (`phys_to_virt`): вся физическая RAM, RW+NX. Через него ядро трогает
///   фреймы, таблицы, кучу, страницы процессов;
/// - **окно образа** (`KIMAGE_BASE`): сам бинарь ядра — единственное место, откуда ядро
///   ИСПОЛНЯЕТСЯ, и единственное, где есть страницы с правом X;
/// - **MMIO**: тождественно (VA == PA) — драйверы держат физические адреса регистров.
///
/// Образ попадает и в direct-map (он же часть RAM), но там его страницы кладутся ТОЛЬКО
/// на чтение: иначе рядом с исполняемым кодом жил бы его же пишущий алиас — дыра в W^X.
pub fn init() -> usize {
    frame::init();
    let root = frame::alloc().expect("нет фрейма под PML4");

    let kimg_s = &raw const _kernel_start as usize;
    let text_s = &raw const _text_start as usize;
    let text_e = &raw const _text_end as usize;
    let ro_s = &raw const _rodata_start as usize;
    let ro_e = &raw const _rodata_end as usize;
    let data_s = &raw const _data_start as usize;
    let kimg_e = &raw const _kernel_end as usize;

    unsafe {
        // 1) direct map RAM как RW+NX (данные не исполняются): huge/гигастраницами, а чанки
        //    образа ядра — постранично и только на чтение (см. преамбулу).
        //
        //    Веха 88 — по КАРТЕ РЕГИОНОВ, но не буквально по ней. Ниже 4 ГиБ дыры карты
        //    (VGA 0xB8000, BIOS, ACPI) ЗАКРЫВАЕМ сплошным отображением: ядро ходит к ним через
        //    direct-map, и вырезать их — тот самый triple fault, что ловили в Вехе 87. Раздачи
        //    это не касается: чего нет в карте, того аллокатор не выдаст. Выше 4 ГиБ — строго по
        //    регионам: закрывать там нечего, а PCI-дыра может быть огромной.
        //
        //    Порядок восхождения важен: пока не сделан `mm_enable`, ядро живёт на таблицах
        //    трамплина, а те покрывают только первые 4 ГиБ — фреймы под таблицы обязаны
        //    приходить снизу. Регионы отсортированы, аллокатор идёт снизу вверх — так и есть.
        let ks = super::virt_to_phys(kimg_s);
        let ke = super::virt_to_phys(kimg_e);
        let mut low_end = 0usize;
        for r in frame::regions() {
            if r.start < FOUR_GIB {
                low_end = low_end.max(r.end.min(FOUR_GIB));
            }
        }
        if low_end > 0 {
            map_direct(root, 0, low_end, ks, ke);
        }
        for r in frame::regions() {
            let start = r.start.max(FOUR_GIB);
            if start < r.end {
                map_direct(root, start, r.end, ks, ke);
            }
        }
        // 2) окно образа ядра — W^X: заголовки и константы R+NX, код R+X, данные RW+NX.
        map_kimage(root, kimg_s, text_s, PTE_NX);
        map_kimage(root, text_s, text_e, 0);
        map_kimage(root, ro_s, ro_e, PTE_NX);
        map_kimage(root, data_s, kimg_e, PTE_W | PTE_NX);
        // 3) MMIO контроллеров прерываний: LAPIC + IOAPIC (BAR'ы PCI отобразит
        //    map_mmio, когда их найдёт pci::probe_virtio_blk — они известны в рантайме).
        map_range(root, super::lapic::LAPIC_BASE, super::lapic::LAPIC_BASE + PAGE_SIZE, PTE_W | PTE_NX);
        map_range(root, super::ioapic::IOAPIC_BASE, super::ioapic::IOAPIC_BASE + PAGE_SIZE, PTE_W | PTE_NX);
        // 4) Веха 96 — окно ФРЕЙМБУФЕРА, если GRUB дал графический режим. Оно лежит ВЫШЕ карты
        //    RAM (у QEMU-stdvga 0xFD00_0000), значит direct-map его не покрывает: без этой
        //    строки первый же println после `mm_enable` ушёл бы в неотображённую память. До сих
        //    пор консоль жила на таблицах трамплина (первые 4 ГиБ тождественно) — потому и
        //    печаталась; здесь отображение становится постоянным.
        if let Some((base, len)) = super::fb::window() {
            map_range(root, base, base + len, PTE_W | PTE_NX);
        }
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
pub fn clone_kernel_root() -> Option<usize> {
    let kroot = KERNEL_ROOT.load(Ordering::Relaxed);
    // Веха 89: нет памяти — отказ, а не паника ядра. Первый фрейм вернём, если не дался второй.
    let new = frame::alloc()?;
    let Some(pdpt) = frame::alloc() else {
        frame::free(new);
        return None;
    };
    unsafe {
        let src = tbl_ptr(kroot) as *const u64;
        let dst = tbl_ptr(new);
        for i in 0..512 {
            *dst.add(i) = *src.add(i);
        }
        let kpdpt = tbl_ptr((*src & ADDR_MASK) as usize) as *const u64;
        let dpdpt = tbl_ptr(pdpt);
        for i in 0..512 {
            *dpdpt.add(i) = *kpdpt.add(i);
        }
        *dst = pdpt as u64 | (*src & !ADDR_MASK); // PML4[0] → приватный PDPT, флаги те же
    }
    Some(new)
}

/// Веха 46 — освободить ВСЕ приватные фреймы адресного пространства процесса (зеркало
/// riscv64::paging::free_address_space): листовые страницы + промежуточные таблицы + корень.
/// Общие с ядром узлы узнаём сравнением с корнем ядра. Тонкость x86: под PML4[0] у процесса
/// СВОЙ PDPT (клон ядерного, [`clone_kernel_root`]) — значит PML4[0] отличается и уходит в
/// рекурсию, а уже ВНУТРИ приватного PDPT общие с ядром записи ([0],[3] — PD ядра) совпадут
/// с ядерным PDPT и будут пропущены; приватна лишь запись [1] (user 1..2 ГиБ).
///
/// # Safety
/// `root_pa` — PML4 процесса, который БОЛЬШЕ НЕ АКТИВЕН (CR3 уже переключён на живое
/// пространство). Вызывать один раз на пространство.
pub unsafe fn free_address_space(root_pa: usize) {
    let kroot = KERNEL_ROOT.load(Ordering::Relaxed);
    free_private(root_pa, kroot, 3); // 4 уровня: PML4 — уровень 3
}

/// Рекурсивно освободить таблицу `tbl` (уровня `level`) и её приватных потомков; `ktbl` —
/// параллельная таблица ЯДРА (`0` — её нет). Записи, совпадающие с ядром, — общие, минуем.
/// Суперстраниц (PS) мы не используем, поэтому лист — только на уровне 0.
unsafe fn free_private(tbl: usize, ktbl: usize, level: usize) {
    let t = tbl_ptr(tbl) as *const u64;
    let k = tbl_ptr(ktbl) as *const u64;
    for i in 0..512 {
        let pte = *t.add(i);
        if pte & PTE_P == 0 {
            continue;
        }
        if ktbl != 0 && pte == *k.add(i) {
            continue; // общая с ядром запись
        }
        // Веха 85: huge-лист (PS) — не спускаться и не освобождать как один 4-КиБ фрейм. В
        // пространствах процессов huge-страниц не бывает (они только в общем direct-map ядра,
        // отсекаемом проверкой выше) — это лишь щит от ошибочного спуска.
        if level > 0 && pte & PTE_PS != 0 {
            continue;
        }
        let child = (pte & ADDR_MASK) as usize;
        if level == 0 {
            // Веха 51: лист userspace-драйвера может указывать на MMIO устройства (не RAM) —
            // такой НЕ освобождаем (иначе адрес железа попал бы в список свободных фреймов).
            if frame::is_ram(child) {
                frame::free(child); // листовая страница RAM
            }
        } else {
            let kchild = if ktbl != 0 && *k.add(i) & PTE_P != 0 {
                (*k.add(i) & ADDR_MASK) as usize
            } else {
                0
            };
            free_private(child, kchild, level - 1);
        }
    }
    frame::free(tbl); // сама таблица — после детей
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
/// Веха 89 — `false`, если не хватило фрейма под промежуточную таблицу (см. riscv-двойник):
/// это зовут по запросу ПРОЦЕССА, и паника здесь роняла ядро вместо программы.
#[must_use]
pub unsafe fn map(root_pa: usize, va: usize, pa: usize, flags: u64) -> bool {
    let mut table = root_pa;
    let mut level = 3usize; // PML4 → PDPT → PD → PT
    while level >= 1 {
        let idx = (va >> (12 + 9 * level)) & 0x1ff;
        let pte = tbl_ptr(table).add(idx);
        if *pte & PTE_P == 0 {
            let Some(next) = frame::alloc() else { return false };
            *pte = next as u64 | PTE_P | PTE_W | PTE_U;
            table = next;
        } else {
            table = (*pte & ADDR_MASK) as usize;
        }
        level -= 1;
    }
    let idx = (va >> 12) & 0x1ff;
    let pte = tbl_ptr(table).add(idx);
    *pte = (pa as u64 & ADDR_MASK) | PTE_P | flags;
    true
}

/// Отобразить диапазон [start, end) ТОЖДЕСТВЕННО (VA == PA), постранично — окна MMIO.
unsafe fn map_range(root_pa: usize, start: usize, end: usize, flags: u64) {
    let mut va = start & !(PAGE_SIZE - 1);
    let end = (end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    while va < end {
        assert!(map(root_pa, va, va, flags), "нет фрейма под таблицу MMIO");
        va += PAGE_SIZE;
    }
}

/// Веха 87 — отобразить диапазон ОКНА ОБРАЗА [start, end) на его физику
/// (`VA → virt_to_phys(VA)`), постранично: так образ получает свои права (W^X).
unsafe fn map_kimage(root_pa: usize, start: usize, end: usize, flags: u64) {
    let mut va = start & !(PAGE_SIZE - 1);
    let end = (end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    while va < end {
        assert!(map(root_pa, va, super::virt_to_phys(va), flags), "нет фрейма под таблицу образа");
        va += PAGE_SIZE;
    }
}

/// Веха 85 — отобразить одну HUGE-страницу (2 МиБ) `va → pa`: ЛИСТ на уровне PD (бит PS). `va`/`pa`
/// выровнены на 2 МиБ. Спускается PML4 → PDPT (создавая их при нужде), затем ставит PD-лист.
///
/// # Safety
/// Как [`map`]: `root_pa` валиден, таблицы доступны по VA == PA.
unsafe fn map_huge(root_pa: usize, va: usize, pa: usize, flags: u64) {
    let mut table = root_pa;
    for level in [3usize, 2] {
        // PML4 → PDPT, дойти до PD
        let idx = (va >> (12 + 9 * level)) & 0x1ff;
        let pte = tbl_ptr(table).add(idx);
        if *pte & PTE_P == 0 {
            let next = frame::alloc().expect("нет фрейма под таблицу");
            *pte = next as u64 | PTE_P | PTE_W | PTE_U;
            table = next;
        } else {
            table = (*pte & ADDR_MASK) as usize;
        }
    }
    let idx = (va >> 21) & 0x1ff; // индекс в PD
    let pte = tbl_ptr(table).add(idx);
    *pte = (pa as u64 & ADDR_MASK) | PTE_P | PTE_PS | flags; // PS → лист 2 МиБ
}

/// Веха 87 — отобразить ГИГАСТРАНИЦУ (1 ГиБ) `va → pa`: ЛИСТ прямо в PDPT (бит PS). `va`/`pa`
/// выровнены на 1 ГиБ. Десятки гигабайт RAM так стелются десятками записей вместо десятков тысяч.
///
/// # Safety
/// Как [`map`]: `root_pa` валиден. CPU обязан поддерживать 1-ГиБ страницы ([`giga_pages_supported`]).
unsafe fn map_giga(root_pa: usize, va: usize, pa: usize, flags: u64) {
    let idx4 = (va >> 39) & 0x1ff; // PML4 → PDPT
    let pte4 = tbl_ptr(root_pa).add(idx4);
    let pdpt = if *pte4 & PTE_P == 0 {
        let next = frame::alloc().expect("нет фрейма под PDPT");
        *pte4 = next as u64 | PTE_P | PTE_W | PTE_U;
        next
    } else {
        (*pte4 & ADDR_MASK) as usize
    };
    let idx3 = (va >> 30) & 0x1ff; // индекс в PDPT
    *tbl_ptr(pdpt).add(idx3) = (pa as u64 & ADDR_MASK) | PTE_P | PTE_PS | flags;
}

/// Поддерживает ли процессор 1-ГиБ страницы (CPUID.80000001H:EDX[26], «PDPE1GB»)?
/// Без этого бита лист в PDPT — зарезервированная комбинация, то есть #PF на первом же доступе.
fn giga_pages_supported() -> bool {
    let edx: u32;
    unsafe {
        core::arch::asm!(
            "push rbx", "cpuid", "pop rbx",
            inout("eax") 0x8000_0001u32 => _,
            out("ecx") _,
            out("edx") edx,
            options(nostack),
        );
    }
    edx & (1 << 26) != 0
}

/// Веха 85/87 — застелить direct-map физической памяти [start, end) гигастраницами там, где
/// целый гигабайт свободен от образа ядра, иначе huge-страницами RW+NX по
/// `VA = phys_to_virt(PA)`. Чанки, перекрывающие образ ядра [prot_s, prot_e) (границы
/// ФИЗИЧЕСКИЕ), кладём постранично и БЕЗ права записи: исполняется образ из своего окна, а
/// пишущий алиас рядом с кодом сделал бы W^X фикцией. Хвост < 2 МиБ — тоже постранично.
unsafe fn map_direct(root_pa: usize, start: usize, end: usize, prot_s: usize, prot_e: usize) {
    let end = (end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let prot_s = prot_s & !(PAGE_SIZE - 1);
    let mut pa = start & !(HUGE - 1);
    let giga_ok = giga_pages_supported();
    while pa < end {
        // Целый свободный гигабайт — ОДНОЙ записью PDPT. Образ ядра лежит в первом гигабайте,
        // поэтому проверяем КАЖДЫЙ кусок отдельно, а не прекращаем цикл на первом занятом.
        if giga_ok
            && pa & (GIGA - 1) == 0
            && pa + GIGA <= end
            && !(pa < prot_e && pa + GIGA > prot_s)
        {
            map_giga(root_pa, super::phys_to_virt(pa), pa, PTE_W | PTE_NX);
            pa += GIGA;
            continue;
        }
        let chunk_end = pa + HUGE;
        let overlaps_kernel = pa < prot_e && chunk_end > prot_s;
        if chunk_end <= end && !overlaps_kernel {
            map_huge(root_pa, super::phys_to_virt(pa), pa, PTE_W | PTE_NX);
        } else {
            // Вырез образа ядра или хвост меньше huge-страницы — постранично.
            let mut p = pa;
            let stop = chunk_end.min(end);
            while p < stop {
                let ro = p >= prot_s && p < prot_e;
                let flags = if ro { PTE_NX } else { PTE_W | PTE_NX };
                assert!(map(root_pa, super::phys_to_virt(p), p, flags), "нет фрейма под таблицу direct-map");
                p += PAGE_SIZE;
            }
        }
        pa += HUGE;
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
        pte = unsafe { *tbl_ptr(table).add(idx) };
        if pte & PTE_P == 0 {
            return None;
        }
        // Веха 85: huge-лист (PS на PDPT/PD) — не спускаться в него как в таблицу. В процессах
        // huge-страниц нет (они лишь в общем direct-map ядра), так что для чекпойнта это лишь щит.
        if level > 0 && pte & PTE_PS != 0 {
            let size = 1usize << (12 + 9 * level as usize);
            return Some(((pte & ADDR_MASK) as usize + (va & (size - 1)), pte));
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
        let pte = unsafe { *tbl_ptr(table).add(idx) };
        if pte & PTE_P == 0 {
            return None;
        }
        // Веха 85: huge-лист (PS) — вернуть его страницу, а не спускаться в 2-МиБ область как в PT.
        if level > 0 && pte & PTE_PS != 0 {
            let size = 1usize << (12 + 9 * level as usize);
            return Some((pte & ADDR_MASK) as usize + (va & (size - 1)));
        }
        table = (pte & ADDR_MASK) as usize;
        level -= 1;
    }
    Some(table | (va & (PAGE_SIZE - 1)))
}
