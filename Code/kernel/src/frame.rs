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

/// Конец физической RAM — Веха 41: ОБНАРУЖИВАЕТСЯ (`arch::ram_limit()`) из карты памяти
/// загрузчика (multiboot/PVH), а не зашитая константа. `platform_init` вызывается ДО `init`.
fn ram_end() -> usize {
    crate::arch::ram_limit()
}

/// Адрес следующего свободного фрейма (двигается вверх). Пик розданного (high-water).
static NEXT: AtomicUsize = AtomicUsize::new(0);

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

/// Инициализировать аллокатор: начать за образом ядра ИЛИ за зарезервированным модулем (Веха 48).
pub fn init() {
    let start = align_up(&raw const _kernel_end as usize, PAGE_SIZE);
    NEXT.store(start.max(RESERVE_END.load(Ordering::Relaxed)), Ordering::Relaxed);
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
        let next = unsafe { core::ptr::read(head as *const usize) };
        if FREE_HEAD.compare_exchange_weak(head, next, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            FREE_COUNT.fetch_sub(1, Ordering::Relaxed);
            // Обнулить: вызывающий (новая таблица / bss ELF) ждёт чистый фрейм.
            unsafe { core::ptr::write_bytes(head as *mut u8, 0, PAGE_SIZE) };
            return Some(head);
        }
    }
    // 2) bump — новая RAM за уже роздённой.
    let pa = NEXT.fetch_add(PAGE_SIZE, Ordering::Relaxed);
    if pa + PAGE_SIZE > ram_end() {
        return None;
    }
    // Обнулить фрейм: нулевой PTE = невалидный, поэтому новая таблица сразу «пустая».
    unsafe { core::ptr::write_bytes(pa as *mut u8, 0, PAGE_SIZE) };
    Some(pa)
}

/// Веха 46 — вернуть фрейм в список свободных. `pa` обязан быть 4 КиБ-выровненным
/// физическим адресом фрейма, полученного из [`alloc`] и больше НЕ используемого.
/// Освобождать нельзя фреймы из [`reserve`] (арена кучи) и общие таблицы ядра.
pub fn free(pa: usize) {
    loop {
        let head = FREE_HEAD.load(Ordering::Acquire);
        // Записать текущую голову в первые 8 байт освобождаемого фрейма.
        unsafe { core::ptr::write(pa as *mut usize, head) };
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
    let bytes = align_up(bytes, PAGE_SIZE);
    let start = NEXT.fetch_add(bytes, Ordering::Relaxed);
    if start + bytes > ram_end() {
        return None;
    }
    Some(start)
}

/// Сколько байт RAM за образом ядра роздано bump'ом (high-water: пик, который NEXT
/// когда-либо достигал). Для отчёта потребления памяти (Веха 28).
pub fn used_bytes() -> usize {
    let start = align_up(&raw const _kernel_end as usize, PAGE_SIZE);
    NEXT.load(Ordering::Relaxed).saturating_sub(start)
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
    let start = align_up(&raw const _kernel_end as usize, PAGE_SIZE);
    pa >= start && pa < ram_end()
}

const fn align_up(x: usize, a: usize) -> usize {
    (x + a - 1) & !(a - 1)
}
