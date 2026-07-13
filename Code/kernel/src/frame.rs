//! Физический аллокатор фреймов — простой bump-аллокатор.
//!
//! До появления кучи (Веха 4) ядру негде брать память под таблицы страниц.
//! Этот аллокатор раздаёт 4 КиБ-фреймы из свободной RAM, идущей сразу за образом
//! ядра (символ `_kernel_end` из linker.ld) и до конца физической RAM.
//! Освобождения нет — для ранней загрузки этого достаточно; позже заменим
//! настоящим аллокатором физических страниц.

use core::sync::atomic::{AtomicUsize, Ordering};

/// Размер страницы/фрейма в Sv39.
pub const PAGE_SIZE: usize = 4096;

extern "C" {
    /// Конец образа ядра (символ из linker.ld). Нас интересует его адрес.
    static _kernel_end: u8;
}

/// Конец физической RAM — платформенная константа арха (`-m 128M` у runner'ов обеих
/// архитектур в .cargo/config.toml; позже возьмём из DTB / PVH start_info).
const RAM_END: usize = crate::arch::RAM_LIMIT;

/// Адрес следующего свободного фрейма (двигается вверх).
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// Инициализировать аллокатор: начать сразу за образом ядра.
pub fn init() {
    let start = &raw const _kernel_end as usize;
    NEXT.store(align_up(start, PAGE_SIZE), Ordering::Relaxed);
}

/// Выделить один обнулённый фрейм. Возвращает физический адрес (он же
/// виртуальный, пока RAM отображена идентично). `None` при исчерпании.
pub fn alloc() -> Option<usize> {
    let pa = NEXT.fetch_add(PAGE_SIZE, Ordering::Relaxed);
    if pa + PAGE_SIZE > RAM_END {
        return None;
    }
    // Обнулить фрейм: нулевой PTE = невалидный, поэтому новая таблица сразу «пустая».
    unsafe { core::ptr::write_bytes(pa as *mut u8, 0, PAGE_SIZE) };
    Some(pa)
}

/// Зарезервировать непрерывный блок RAM (размер округляется вверх до страницы).
/// Возвращает физический адрес начала — используется под арену кучи ядра (Веха 4).
/// В отличие от [`alloc`], блок не обнуляется: кучей управляет её аллокатор.
pub fn reserve(bytes: usize) -> Option<usize> {
    let bytes = align_up(bytes, PAGE_SIZE);
    let start = NEXT.fetch_add(bytes, Ordering::Relaxed);
    if start + bytes > RAM_END {
        return None;
    }
    Some(start)
}

/// Сколько байт RAM за образом ядра уже роздано (пик: освобождения у bump-аллокатора
/// нет). Для отчёта потребления памяти (Веха 28).
pub fn used_bytes() -> usize {
    let start = align_up(&raw const _kernel_end as usize, PAGE_SIZE);
    NEXT.load(Ordering::Relaxed).saturating_sub(start)
}

const fn align_up(x: usize, a: usize) -> usize {
    (x + a - 1) & !(a - 1)
}
