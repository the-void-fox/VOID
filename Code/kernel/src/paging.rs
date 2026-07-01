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

/// Число страниц под пользовательский стек (Веха 10).
const USER_STACK_PAGES: usize = 4;
/// Вершина пользовательского стека (задаётся в [`init`]).
static USER_STACK_TOP: AtomicUsize = AtomicUsize::new(0);

/// Вершина пользовательского стека (0, если ещё не настроен).
pub fn user_stack_top() -> usize {
    USER_STACK_TOP.load(Ordering::Relaxed)
}

/// Маска PPN внутри PTE — 44 бита.
const PPN_MASK: usize = (1 << 44) - 1;

// MMIO-регион QEMU virt: UART (0x1000_0000) + 8 слотов virtio-mmio (0x1000_1000..0x1000_9000).
const MMIO_START: usize = 0x1000_0000;
const MMIO_END: usize = 0x1000_9000;
// PLIC (контроллер прерываний устройств): до claim/complete контекста 1 включительно.
const PLIC_START: usize = 0x0c00_0000;
const PLIC_END: usize = 0x0c20_3000;
const RAM_START: usize = 0x8000_0000;
const RAM_END: usize = 0x8000_0000 + 128 * 1024 * 1024;

extern "C" {
    static _text_start: u8;
    static _text_end: u8;
    static _rodata_start: u8;
    static _rodata_end: u8;
    static _user_start: u8;
    static _user_end: u8;
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
        // 1) direct map всей RAM как RW — база, чтобы всё осталось доступно.
        map_range(root, RAM_START, RAM_END, PTE_R | PTE_W);
        // 2) MMIO как RW: UART (иначе пропадёт вывод) + слоты virtio-mmio (для диска) + PLIC.
        map_range(root, MMIO_START, MMIO_END, PTE_R | PTE_W);
        map_range(root, PLIC_START, PLIC_END, PTE_R | PTE_W);
        // 3) W^X: перетираем листовые PTE кода и констант более строгими правами.
        map_range(root, text_s, text_e, PTE_R | PTE_X);
        map_range(root, ro_s, ro_e, PTE_R);

        // 4) Веха 10: пользовательский код (.user) как U|R|X и стек U-mode как U|R|W.
        let user_s = &raw const _user_start as usize;
        let user_e = &raw const _user_end as usize;
        map_range(root, user_s, user_e, PTE_R | PTE_X | PTE_U);

        // Стек U-mode из свежих (обнулённых) фреймов; frame::alloc отдаёт их подряд.
        let mut stack_base = 0;
        for i in 0..USER_STACK_PAGES {
            let f = frame::alloc().expect("нет фрейма под стек U-mode");
            if i == 0 {
                stack_base = f;
            }
            map(root, f, f, PTE_R | PTE_W | PTE_U);
        }
        USER_STACK_TOP.store(stack_base + USER_STACK_PAGES * PAGE_SIZE, Ordering::Relaxed);
    }
    root
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
