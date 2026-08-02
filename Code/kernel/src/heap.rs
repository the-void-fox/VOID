//! Куча ядра: аллокатор со свободным списком (free list) + `#[global_allocator]`.
//!
//! Куча даёт динамическую память с произвольным временем жизни — то, чего не умеют
//! ни стек (жёсткий LIFO), ни статика (фиксированный размер). После её подключения
//! работают `Box`, `Vec`, `String` из крейта `alloc`. Подробнее про области памяти —
//! в справке `20-reference/memory-regions`.
//!
//! Устройство: арена — непрерывный кусок RAM, взятый у [`crate::frame::reserve`]
//! (RAM уже отображена идентично в Вехе 3, поэтому физический адрес = виртуальный).
//! По арене ведётся односвязный список свободных блоков, **отсортированный по адресу**.
//! Узел списка хранится ПРЯМО в свободной памяти: первые байты свободного блока — это его
//! размер и ссылка на следующий свободный блок. Выделение — first-fit (первый подходящий).
//!
//! Освобождение **сливает** соседние свободные блоки (coalescing): список держится в
//! порядке адресов, и при возврате блока мы примыкаем его к соседу слева/справа, если они
//! непрерывны. Это не даёт куче деградировать в мелкие несливаемые дырки при частых
//! alloc/free (а их стало много: `Box`/`Vec`/`Arc`/future).
//!
//! Осознанные упрощения (чиним позже): «дырка» перед выровненным началом при экзотических
//! выравниваниях теряется; арена фиксированного размера (2 МиБ), не растёт.

use core::alloc::{GlobalAlloc, Layout};
use core::mem;
use core::ptr;

use crate::frame::{self, PAGE_SIZE};
use crate::sync::SpinLock;

/// Размер кучи: 4096 страниц = 16 МиБ. Веха 31: std-ELF весит сотни КиБ и при
/// exec живёт в куче дважды (кэш store + копия для загрузчика), а GC-обход
/// подгружает в кэш все достижимые объекты — 2 МиБ стало тесно. Веха 32:
/// multicall-бинарь uutils — 2.2 МиБ, дважды в куче плюс кэш store — 8 МиБ
/// перестало хватать.
const HEAP_PAGES: usize = 4096;

/// Узел свободного списка, живущий внутри свободного блока.
struct FreeRegion {
    size: usize,
    next: Option<&'static mut FreeRegion>,
}

impl FreeRegion {
    const fn new(size: usize) -> Self {
        Self { size, next: None }
    }
    fn start(&self) -> usize {
        self as *const Self as usize
    }
    fn end(&self) -> usize {
        self.start() + self.size
    }
}

/// Аллокатор: фиктивная голова, `head.next` — первый реальный свободный блок.
pub struct FreeListAllocator {
    head: FreeRegion,
}

impl FreeListAllocator {
    pub const fn new() -> Self {
        Self {
            head: FreeRegion::new(0),
        }
    }

    /// Инициализировать кучу одним большим свободным блоком [start, start+size).
    ///
    /// # Safety
    /// Диапазон должен быть валиден, не использоваться больше никем и жить вечно.
    pub unsafe fn init(&mut self, start: usize, size: usize) {
        self.push_free(start, size);
    }

    /// Вернуть регион [addr, addr+size) в свободный список, сохраняя порядок по адресу
    /// и **сливая** с примыкающими соседями (слева/справа).
    unsafe fn push_free(&mut self, addr: usize, size: usize) {
        debug_assert!(size >= mem::size_of::<FreeRegion>());
        debug_assert_eq!(align_up(addr, mem::align_of::<FreeRegion>()), addr);

        // Адрес фиктивной головы — чтобы отличать её от реальных блоков (не сливать в неё).
        let head_addr = &self.head as *const FreeRegion as usize;

        // 1) Дойти до места вставки: `current` — последний узел с адресом < addr
        //    (или сама голова). После цикла current.next — первый узел с адресом >= addr.
        let mut current = &mut self.head;
        while let Some(ref next) = current.next {
            if next.start() >= addr {
                break;
            }
            current = current.next.as_mut().unwrap();
        }

        // 2) Слить с последующим блоком, если он примыкает справа: addr+size == next.start.
        let mut size = size;
        let merge_next = matches!(current.next, Some(ref n) if addr + size == n.start());
        if merge_next {
            let next_node = current.next.take().unwrap();
            size += next_node.size;
            current.next = next_node.next.take(); // перецепить хвост списка выше
        }

        // 3) Слить с предыдущим (current), если это реальный блок и он примыкает слева.
        if current.start() != head_addr && current.end() == addr {
            current.size += size;
            return;
        }

        // 4) Иначе вставить новый узел [addr, addr+size) между current и current.next.
        let mut node = FreeRegion::new(size);
        node.next = current.next.take();
        let node_ptr = addr as *mut FreeRegion;
        node_ptr.write(node);
        current.next = Some(&mut *node_ptr);
    }

    /// First-fit: найти подходящий блок и вынуть его из списка.
    /// Возвращает (узел-блок, выровненный адрес начала выделения).
    fn find(&mut self, size: usize, align: usize) -> Option<(&'static mut FreeRegion, usize)> {
        let mut current = &mut self.head;
        while let Some(ref mut region) = current.next {
            if let Ok(start) = Self::fit(&**region, size, align) {
                let next = region.next.take();
                let ret = Some((current.next.take().unwrap(), start));
                current.next = next;
                return ret;
            }
            current = current.next.as_mut().unwrap();
        }
        None
    }

    /// Помещается ли (size, align) в блок? Возвращает выровненный старт.
    fn fit(region: &FreeRegion, size: usize, align: usize) -> Result<usize, ()> {
        let start = align_up(region.start(), align);
        let end = start.checked_add(size).ok_or(())?;
        if end > region.end() {
            return Err(());
        }
        // Остаток должен быть либо нулевым, либо достаточным для узла — иначе потеряется.
        let rest = region.end() - end;
        if rest > 0 && rest < mem::size_of::<FreeRegion>() {
            return Err(());
        }
        Ok(start)
    }

    /// Нормализуем запрос: выравнивание не меньше, чем у узла, размер — не меньше узла.
    fn size_align(layout: Layout) -> (usize, usize) {
        let layout = layout
            .align_to(mem::align_of::<FreeRegion>())
            .expect("align_to")
            .pad_to_align();
        let size = layout.size().max(mem::size_of::<FreeRegion>());
        (size, layout.align())
    }
}

// Глобальный аллокатор — это замок вокруг списка (нужен для Sync и взаимного исключения).
unsafe impl GlobalAlloc for SpinLock<FreeListAllocator> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let (size, align) = FreeListAllocator::size_align(layout);
        let mut heap = self.lock_irq();
        match heap.find(size, align) {
            Some((region, start)) => {
                let end = start + size;
                let rest = region.end() - end;
                if rest > 0 {
                    heap.push_free(end, rest); // хвост блока — обратно в список
                }
                start as *mut u8
            }
            None => ptr::null_mut(), // OOM
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let (size, _) = FreeListAllocator::size_align(layout);
        self.lock_irq().push_free(ptr as usize, size);
    }
}

#[global_allocator]
static ALLOCATOR: SpinLock<FreeListAllocator> = SpinLock::new(FreeListAllocator::new());

/// Зарезервировать RAM под кучу и инициализировать аллокатор. Вызывать один раз
/// после включения paging (арена лежит в отображённой direct-map RAM).
pub fn init() {
    let size = HEAP_PAGES * PAGE_SIZE;
    // Веха 87: `reserve` отдаёт ФИЗИЧЕСКИЙ адрес, а аллокатор раздаёт указатели —
    // переводим через direct-map ([`crate::arch::phys_to_virt`]).
    let start_pa = frame::reserve(size).expect("нет RAM под кучу ядра");
    unsafe { ALLOCATOR.lock().init(frame::ptr(start_pa) as usize, size) };
}

const fn align_up(x: usize, a: usize) -> usize {
    (x + a - 1) & !(a - 1)
}
