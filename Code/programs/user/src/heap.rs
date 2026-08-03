//! Куча программы: bump + свободный список поверх ленивой кучи процесса.
//!
//! Вынесено из `bin/vvsh.rs` Вехой 95, когда аллокатор понадобился второй программе (TLS-клиенту).
//! Копировать сотню строк ради этого нельзя: аллокатор — то место, где расхождение двух копий
//! замечают последним и по самым странным симптомам.
//!
//! Веха 89 (история). Сначала это был чистый bump с пустым `dealloc`: арена на весь сеанс, память
//! не возвращалась НИКОГДА. Для шелла, который обязан жить долго, это значит «через N команд
//! аллокация вернёт null». Теперь освобождённые блоки идут в адресно-упорядоченный список со
//! СЛИЯНИЕМ соседей (та же схема, что у кучи ядра): память переиспользуется, фрагментация
//! ограничена.
//!
//! Размер арены — параметр типа: у шелла и у TLS-клиента запросы разной природы. Арена берётся
//! ленивой кучей процесса (`SYS_MAP`), поэтому «попросить с запасом» ничего не стоит: страницы
//! появляются по факту обращения.

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering};

/// Узел свободного списка живёт ВНУТРИ свободного блока: `[size][next]`. Отсюда минимальный
/// размер блока и минимальное выравнивание.
const NODE: usize = 2 * core::mem::size_of::<usize>();

/// Аллокатор с ареной в `ARENA` байт. Программа объявляет его так:
/// ```ignore
/// #[global_allocator]
/// static ALLOC: sys::heap::Heap<{ 4 * 1024 * 1024 }> = sys::heap::Heap::new();
/// ```
pub struct Heap<const ARENA: usize> {
    base: AtomicUsize,
    next: AtomicUsize,
    end: AtomicUsize,
    /// Голова списка свободных блоков (адрес; 0 — пуст). Список отсортирован по адресу —
    /// это и даёт дешёвое слияние соседей.
    free: AtomicUsize,
}

impl<const ARENA: usize> Default for Heap<ARENA> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const ARENA: usize> Heap<ARENA> {
    pub const fn new() -> Self {
        Self {
            base: AtomicUsize::new(0),
            next: AtomicUsize::new(0),
            end: AtomicUsize::new(0),
            free: AtomicUsize::new(0),
        }
    }

    /// Сколько байт арены ещё не тронуто bump-указателем (для диагностики).
    pub fn untouched(&self) -> usize {
        self.end.load(Ordering::Relaxed).saturating_sub(self.next.load(Ordering::Relaxed))
    }

    #[inline]
    unsafe fn nsize(p: usize) -> usize {
        *(p as *const usize)
    }

    #[inline]
    unsafe fn nnext(p: usize) -> usize {
        *((p + core::mem::size_of::<usize>()) as *const usize)
    }

    #[inline]
    unsafe fn nset(p: usize, size: usize, next: usize) {
        *(p as *mut usize) = size;
        *((p + core::mem::size_of::<usize>()) as *mut usize) = next;
    }

    /// Запрос → (размер, выравнивание), нормализованные под узел списка.
    fn norm(layout: Layout) -> (usize, usize) {
        let align = layout.align().max(core::mem::align_of::<usize>());
        let size = ((layout.size() + align - 1) & !(align - 1)).max(NODE);
        (size, align)
    }

    /// Вернуть блок `[addr, addr+size)` в список: вставка по адресу + слияние с соседями слева и
    /// справа. Без слияния долгий сеанс раскрошил бы арену в пыль.
    unsafe fn dealloc_raw(&self, addr: usize, size: usize) {
        let (mut prev, mut cur) = (0usize, self.free.load(Ordering::Relaxed));
        while cur != 0 && cur < addr {
            prev = cur;
            cur = Self::nnext(cur);
        }
        let mut size = size;
        let mut next = cur;
        // слияние с правым соседом
        if cur != 0 && addr + size == cur {
            size += Self::nsize(cur);
            next = Self::nnext(cur);
        }
        // слияние с левым соседом
        if prev != 0 && prev + Self::nsize(prev) == addr {
            Self::nset(prev, Self::nsize(prev) + size, next);
            return;
        }
        Self::nset(addr, size, next);
        if prev == 0 {
            self.free.store(addr, Ordering::Relaxed);
        } else {
            Self::nset(prev, Self::nsize(prev), addr);
        }
    }
}

unsafe impl<const ARENA: usize> GlobalAlloc for Heap<ARENA> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if self.base.load(Ordering::Relaxed) == 0 {
            let base = crate::heap_map(ARENA);
            if base == 0 || base == usize::MAX {
                return core::ptr::null_mut();
            }
            self.base.store(base, Ordering::Relaxed);
            self.next.store(base, Ordering::Relaxed);
            self.end.store(base + ARENA, Ordering::Relaxed);
        }
        let (size, align) = Self::norm(layout);

        // 1) First-fit по свободному списку. Хвост блока возвращаем в список, если в нём
        //    помещается узел; иначе отдаём блок целиком (остаток был бы потерян навсегда).
        let (mut prev, mut cur) = (0usize, self.free.load(Ordering::Relaxed));
        while cur != 0 {
            let bsize = Self::nsize(cur);
            let start = (cur + align - 1) & !(align - 1);
            let head_gap = start - cur; // «хвостик» перед выравненным началом
            if (head_gap == 0 || head_gap >= NODE) && start + size <= cur + bsize {
                let next = Self::nnext(cur);
                let rest = (cur + bsize) - (start + size);
                // вынуть блок из списка
                if prev == 0 {
                    self.free.store(next, Ordering::Relaxed);
                } else {
                    Self::nset(prev, Self::nsize(prev), next);
                }
                if head_gap >= NODE {
                    self.dealloc_raw(cur, head_gap); // «хвостик» слева — обратно в список
                }
                if rest >= NODE {
                    self.dealloc_raw(start + size, rest); // остаток справа — тоже
                }
                return start as *mut u8;
            }
            prev = cur;
            cur = Self::nnext(cur);
        }

        // 2) Свободного блока нет — отрезать от нетронутой части арены.
        let aligned = (self.next.load(Ordering::Relaxed) + align - 1) & !(align - 1);
        let new_next = aligned + size;
        if new_next > self.end.load(Ordering::Relaxed) {
            return core::ptr::null_mut();
        }
        self.next.store(new_next, Ordering::Relaxed);
        aligned as *mut u8
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let (size, _) = Self::norm(layout);
        self.dealloc_raw(ptr as usize, size);
    }
}
