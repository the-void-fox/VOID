//! Физический аллокатор фреймов — bump + список свободных (Веха 46).
//!
//! До появления кучи (Веха 4) ядру негде брать память под таблицы страниц.
//! Этот аллокатор раздаёт 4 КиБ-фреймы из свободной RAM, идущей сразу за образом
//! ядра (символ `_kernel_end` из linker.ld) и до конца физической RAM.
//!
//! Веха 46 — **освобождение фреймов**: раньше был чистый bump (`NEXT` только вверх),
//! и страницы завершившихся процессов не возвращались → долгая сессия текла к OOM.
//! Теперь [`free`] кладёт фрейм в интрузивный список свободных (указатель на
//! следующий хранится В САМОМ освобождённом фрейме — классический Treiber-стек), а
//! [`alloc`] сперва берёт оттуда, и лишь при пустом списке двигает bump. Так фреймы
//! адресных пространств переиспользуются (см. [`crate::arch::free_address_space`],
//! вызывается из [`crate::proc`] при полной гибели группы нитей).
//!
//! Гонок нет: аллокатор работает только в ядре, а ядро на этой машине однопроцессорно
//! и невытесняемо (S-mode `SIE=0` во время trap'ов), поэтому CAS всегда проходит с
//! первой попытки и ABA не возникает — вложенных alloc/free из прерываний не бывает.

use core::sync::atomic::{AtomicUsize, Ordering};

/// Размер страницы/фрейма в Sv39.
pub const PAGE_SIZE: usize = 4096;

extern "C" {
    /// Конец образа ядра (символ из linker.ld). Нас интересует его адрес.
    static _kernel_end: u8;
}

/// Веха 87 — ФИЗИЧЕСКИЙ конец образа ядра: с переездом в верхнюю половину символы линкера
/// стали высокими VA, а аллокатор раздаёт физику, поэтому адрес символа переводится обратно
/// ([`crate::arch::virt_to_phys`]). До переезда смещение было нулевым, и это был тот же адрес.
fn kernel_end_pa() -> usize {
    align_up(crate::arch::virt_to_phys(&raw const _kernel_end as usize), PAGE_SIZE)
}

// ─── карта RAM: СПИСОК регионов (Веха 88) ────────────────────────────────────
//
// До Вехи 88 аллокатор знал одно число — «конец RAM» (`arch::ram_limit()`), и раздавал
// ОДИН непрерывный кусок от конца образа ядра до него. На реальной машине это неверно:
// физическая память дырявая (BIOS/EBDA внизу, PCI-дыра под 4 ГиБ, ACPI-области), а всё,
// что лежит ВЫШЕ дыры, при таком взгляде просто не существует. Теперь карта — список
// регионов, который прошивка сообщает через `platform_init` ([`add_region`]).

/// Регион физической RAM: `[start, end)`. Адреса физические и кратны странице.
#[derive(Clone, Copy)]
pub struct Region {
    pub start: usize,
    pub end: usize,
}

/// Сколько регионов помещается в карту. Прошивки отдают единицы ПРИГОДНЫХ областей
/// (обычно 2–4: до дыры BIOS, до PCI-дыры и хвост выше 4 ГиБ) — 16 с большим запасом.
const MAX_REGIONS: usize = 16;

/// Карта RAM. Пишется ТОЛЬКО из `platform_init` — до прерываний, до планировщика, в один
/// поток, — а читается уже после; поэтому без замка, как `TRAP_STACK` в [`crate::proc`].
static mut REGIONS: [Region; MAX_REGIONS] = [Region { start: 0, end: 0 }; MAX_REGIONS];
static REGION_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Карта RAM (по возрастанию адреса, без пересечений). Пуста до `platform_init`.
pub fn regions() -> &'static [Region] {
    unsafe {
        core::slice::from_raw_parts(
            core::ptr::addr_of!(REGIONS) as *const Region,
            REGION_COUNT.load(Ordering::Relaxed),
        )
    }
}

/// Добавить в карту пригодный регион RAM. Зовётся из `platform_init` (ДО [`init`]) по одному
/// разу на запись карты прошивки: E820/multiboot-mmap и PVH-memmap на x86, `/memory` из DTB
/// на riscv. Границы подрезаются внутрь страницы (лишнего не присвоим), пустые игнорируются.
///
/// **Арх обязан подрезать регион по досягаемости direct-map**: аллокатор раздаёт то, что здесь
/// записано, а трогать фрейм ядро может только через `phys_to_virt`.
pub fn add_region(start: usize, end: usize) {
    let start = align_up(start, PAGE_SIZE);
    let end = end & !(PAGE_SIZE - 1);
    if end <= start {
        return;
    }
    unsafe {
        let regs = &mut *core::ptr::addr_of_mut!(REGIONS);
        let mut n = REGION_COUNT.load(Ordering::Relaxed);
        // Смежный или пересекающийся с уже известным — слить (карты прошивок иногда дробят
        // одну область на несколько записей). Каскадного слияния не делаем: для этого записи
        // должны прийти так, чтобы новая накрыла сразу две — прошивки такого не дают.
        for r in regs.iter_mut().take(n) {
            if start <= r.end && end >= r.start {
                r.start = r.start.min(start);
                r.end = r.end.max(end);
                return;
            }
        }
        if n == MAX_REGIONS {
            return; // карта переполнена — молча игнорируем хвост (лучше меньше RAM, чем каша)
        }
        // Вставка с сохранением порядка по возрастанию.
        let mut i = n;
        while i > 0 && regs[i - 1].start > start {
            regs[i] = regs[i - 1];
            i -= 1;
        }
        regs[i] = Region { start, end };
        n += 1;
        REGION_COUNT.store(n, Ordering::Relaxed);
    }
}

/// Нижняя граница раздачи: ниже неё лежит образ ядра (и, если был, загрузочный модуль).
/// Регионы целиком ниже неё пропускаются, регион вокруг неё начинается с неё.
fn alloc_floor() -> usize {
    kernel_end_pa().max(RESERVE_END.load(Ordering::Relaxed))
}

/// Курсор bump'а — физический адрес следующего невыданного фрейма (внутри региона [`REGION_IDX`]).
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// Индекс региона карты, в котором стоит [`NEXT`]. `>= regions().len()` — RAM исчерпана.
static REGION_IDX: AtomicUsize = AtomicUsize::new(0);

/// Сколько байт всего роздано bump'ом по всем регионам — для отчёта потребления (Веха 28).
/// Отдельный счётчик, а не «NEXT − начало»: с дырявой картой такая разность бессмысленна.
static BUMPED: AtomicUsize = AtomicUsize::new(0);

/// Голова списка свободных фреймов (Веха 46). `0` — список пуст (PA 0 у нас не бывает
/// валидным фреймом RAM: она начинается заметно выше). В первых 8 байтах свободного
/// фрейма лежит PA следующего свободного (или 0).
static FREE_HEAD: AtomicUsize = AtomicUsize::new(0);

/// Сколько фреймов сейчас в списке свободных — для отчёта потребления (Веха 46).
static FREE_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Веха 48 — верхняя граница «занятого» перед стартом аллокатора: GRUB кладёт загрузочный
/// модуль (образ установки) в RAM за образом ядра, и bump не должен раздать его фреймы.
/// Ставит [`reserve_boot_module`] (из `platform_init`, ДО [`init`]); `init` поднимет до неё старт.
static RESERVE_END: AtomicUsize = AtomicUsize::new(0);

/// Веха 48 — уберечь регион `[.., end)` от аллокатора (загрузочный модуль multiboot2). Зовётся
/// из `platform_init` до `init`; безвредно, если `end` ниже конца образа ядра (тогда `init` берёт
/// конец образа). Только x86 (у riscv загрузочных модулей нет).
#[cfg_attr(target_arch = "riscv64", allow(dead_code))]
pub fn reserve_boot_module(end: usize) {
    RESERVE_END.store(align_up(end, PAGE_SIZE), Ordering::Relaxed);
}

/// Инициализировать аллокатор: поставить курсор в первый регион карты, где есть что раздавать
/// выше образа ядра (и зарезервированного загрузочного модуля, Веха 48).
pub fn init() {
    let floor = alloc_floor();
    for (i, r) in regions().iter().enumerate() {
        if r.start.max(floor) < r.end {
            REGION_IDX.store(i, Ordering::Relaxed);
            NEXT.store(r.start.max(floor), Ordering::Relaxed);
            return;
        }
    }
    REGION_IDX.store(regions().len(), Ordering::Relaxed); // RAM нет — alloc честно вернёт None
}

/// Отрезать непрерывный кусок `bytes` (кратно странице) из карты RAM, двигая курсор.
/// `None` — во всех регионах не осталось непрерывного куска такого размера.
fn bump(bytes: usize) -> Option<usize> {
    let regs = regions();
    let floor = alloc_floor();
    loop {
        let i = REGION_IDX.load(Ordering::Relaxed);
        if i >= regs.len() {
            return None;
        }
        let pa = NEXT.load(Ordering::Relaxed);
        if pa.saturating_add(bytes) <= regs[i].end {
            NEXT.store(pa + bytes, Ordering::Relaxed);
            BUMPED.fetch_add(bytes, Ordering::Relaxed);
            return Some(pa);
        }
        // В этом регионе непрерывного куска не осталось — перейти в следующий. Хвост региона
        // при этом теряется: `reserve` просит крупные куски, и склеивать их через дыру нельзя.
        REGION_IDX.store(i + 1, Ordering::Relaxed);
        if i + 1 < regs.len() {
            NEXT.store(regs[i + 1].start.max(floor), Ordering::Relaxed);
        }
    }
}

/// Выделить один обнулённый фрейм. Возвращает физический адрес (он же
/// виртуальный, пока RAM отображена идентично). `None` при исчерпании.
///
/// Веха 46: сперва берём из списка свободных (переиспользование), иначе двигаем bump.
pub fn alloc() -> Option<usize> {
    // 1) список свободных — переиспользуем возвращённый ранее фрейм.
    loop {
        let head = FREE_HEAD.load(Ordering::Acquire);
        if head == 0 {
            break; // список пуст — на bump-путь
        }
        // Веха 87: `head` — ФИЗИЧЕСКИЙ адрес; читать по нему можно только через direct-map.
        let next = unsafe { core::ptr::read(ptr(head) as *const usize) };
        if FREE_HEAD.compare_exchange_weak(head, next, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            FREE_COUNT.fetch_sub(1, Ordering::Relaxed);
            // Обнулить: вызывающий (новая таблица / bss ELF) ждёт чистый фрейм.
            unsafe { core::ptr::write_bytes(ptr(head), 0, PAGE_SIZE) };
            return Some(head);
        }
    }
    // 2) bump — новая RAM за уже роздённой (Веха 88: по карте регионов, а не по одному куску).
    let pa = bump(PAGE_SIZE)?;
    // Обнулить фрейм: нулевой PTE = невалидный, поэтому новая таблица сразу «пустая».
    unsafe { core::ptr::write_bytes(ptr(pa), 0, PAGE_SIZE) };
    Some(pa)
}

/// Веха 87 — указатель ядра на физический фрейм (через direct-map, [`crate::arch::phys_to_virt`]).
/// Единственный законный способ ДОТРОНУТЬСЯ до памяти, адрес которой пришёл из [`alloc`]:
/// пока direct-map тождественный, это тот же адрес, после переезда ядра — уже другой.
#[inline(always)]
pub fn ptr(pa: usize) -> *mut u8 {
    crate::arch::phys_to_virt(pa) as *mut u8
}

/// Веха 46 — вернуть фрейм в список свободных. `pa` обязан быть 4 КиБ-выровненным
/// физическим адресом фрейма, полученного из [`alloc`] и больше НЕ используемого.
/// Освобождать нельзя фреймы из [`reserve`] (арена кучи) и общие таблицы ядра.
pub fn free(pa: usize) {
    loop {
        let head = FREE_HEAD.load(Ordering::Acquire);
        // Записать текущую голову в первые 8 байт освобождаемого фрейма (через direct-map).
        unsafe { core::ptr::write(ptr(pa) as *mut usize, head) };
        if FREE_HEAD.compare_exchange_weak(head, pa, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            FREE_COUNT.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
}

/// Зарезервировать непрерывный блок RAM (размер округляется вверх до страницы).
/// Возвращает физический адрес начала — используется под арену кучи ядра (Веха 4).
/// В отличие от [`alloc`], блок не обнуляется: кучей управляет её аллокатор.
pub fn reserve(bytes: usize) -> Option<usize> {
    bump(align_up(bytes, PAGE_SIZE))
}

/// Сколько байт RAM роздано bump'ом (high-water: пик, который аллокатор когда-либо занимал).
/// Для отчёта потребления памяти (Веха 28).
pub fn used_bytes() -> usize {
    BUMPED.load(Ordering::Relaxed)
}

/// Веха 46 — сколько байт сейчас в списке свободных (возвращено и ждёт переиспользования).
/// «Живое» потребление ≈ [`used_bytes`] − это. Для отчёта памяти.
pub fn available_bytes() -> usize {
    FREE_COUNT.load(Ordering::Relaxed) * PAGE_SIZE
}

/// Веха 51 — принадлежит ли физ-адрес нашей RAM (области аллокатора). Нужно сносу адресного
/// пространства ([`crate::arch::free_address_space`]): в таблицах userspace-драйвера есть листья,
/// указывающие на MMIO УСТРОЙСТВА (не RAM) — их нельзя класть в список свободных, иначе выдадим
/// адрес железа как страницу. RAM-фреймы (обычные, DMA, таблицы) — освобождаем; MMIO — минуем.
pub fn is_ram(pa: usize) -> bool {
    pa >= alloc_floor() && regions().iter().any(|r| pa >= r.start && pa < r.end)
}

const fn align_up(x: usize, a: usize) -> usize {
    (x + a - 1) & !(a - 1)
}
