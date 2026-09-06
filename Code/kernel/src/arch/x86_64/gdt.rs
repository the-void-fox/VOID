//! GDT с сегментами ring3 + TSS (Веха 26 — вход в U-mode).
//!
//! Трамплин (entry.s) грузит минимальную GDT «код/данные ring0» — её хватает ядру.
//! Для ring3 нужны ещё дескрипторы user-кода/данных (DPL=3) и TSS: при прерывании из
//! CPL=3 процессор сам переключает стек на `TSS.rsp0` — это наш ядерный trap-стек
//! (аналог `sscratch` на RISC-V). Здесь — полная GDT, загружаемая в [`init`];
//! селекторы ядра совпадают с трамплинными (0x08/0x10), перегружать CS не нужно.

use core::mem::size_of;
use core::ptr::addr_of;

/// Селекторы: ядро — как в трамплине; user — с RPL=3 (иначе #GP на iretq).
pub const UDATA_SEL: u16 = 0x18 | 3;
pub const UCODE_SEL: u16 = 0x20 | 3;
const TSS_SEL: u16 = 0x28;

/// 64-битный TSS: интересен только `rsp0` (стек ring0 при трапе из ring3) и пустая
/// карта портов (`iomap` = за пределами лимита ⇒ IN/OUT из ring3 запрещены).
#[repr(C, packed)]
struct Tss {
    _res0: u32,
    rsp0: u64,
    _rsp12: [u64; 2],
    _res1: u64,
    _ist: [u64; 7],
    _res2: u64,
    _res3: u16,
    iomap: u16,
}

static mut TSS: Tss = Tss {
    _res0: 0,
    rsp0: 0,
    _rsp12: [0; 2],
    _res1: 0,
    _ist: [0; 7],
    _res2: 0,
    _res3: 0,
    iomap: size_of::<Tss>() as u16,
};

/// GDT: null, код/данные ring0 (= трамплин), данные/код ring3, TSS (2 слота — 16 байт).
/// Дескриптор TSS зависит от адреса [`TSS`] — дозаполняется в [`init`].
static mut GDT: [u64; 7] = [
    0,
    0x00AF9A000000FFFF, // 0x08: код ring0, L=1
    0x00CF92000000FFFF, // 0x10: данные ring0
    0x00CFF2000000FFFF, // 0x18: данные ring3 (DPL=3)
    0x00AFFA000000FFFF, // 0x20: код ring3, L=1 (DPL=3)
    0,                  // 0x28: TSS (низ)
    0,                  //       TSS (верх)
];

#[repr(C, packed)]
struct GdtPtr {
    limit: u16,
    base: u64,
}

/// Собрать дескриптор TSS, загрузить GDT и TR. Позвать один раз при старте (до
/// первого входа в U-mode); ядровые селекторы не меняются — далёкий прыжок не нужен.
pub fn init() {
    unsafe {
        let base = addr_of!(TSS) as u64;
        let limit = (size_of::<Tss>() - 1) as u64;
        // Системный дескриптор: limit | base[23:0] | type=0x9 (avail 64-bit TSS) | P | base[31:24].
        GDT[5] = limit
            | (base & 0xff_ffff) << 16
            | 0x89u64 << 40
            | ((base >> 24) & 0xff) << 56;
        GDT[6] = base >> 32;

        let ptr = GdtPtr {
            limit: (size_of::<[u64; 7]>() - 1) as u16,
            base: addr_of!(GDT) as u64,
        };
        core::arch::asm!(
            "lgdt [{p}]",
            "ltr {sel:x}",
            p = in(reg) &ptr,
            sel = in(reg) TSS_SEL,
            options(nostack),
        );
    }
}

/// Веха 170 — GDT ПРИКЛАДНОГО ядра: та же таблица, но БЕЗ `ltr`.
///
/// Таблица одна на машину, и это правильно — она только читается процессором. А вот TSS у
/// каждого ядра обязана быть своя: в неё пишется вершина ядерного стека для трапа из ring3, и
/// две регистровые копии одного `TSS.rsp0` означали бы, что два ядра войдут в ядро на одном
/// стеке. Запаркованное ядро в ring3 не входит вовсе, поэтому здесь TSS просто не загружается;
/// своя на ядро появится вместе с планировщиком.
pub fn init_ap() {
    unsafe {
        let ptr = GdtPtr {
            limit: (size_of::<[u64; 7]>() - 1) as u16,
            base: addr_of!(GDT) as u64,
        };
        core::arch::asm!("lgdt [{p}]", p = in(reg) &ptr, options(nostack));
    }
}

/// Задать стек ring0 для следующего трапа из U-mode — зовётся на каждом входе в
/// процесс ([`super::enter_user`]), как `csrw sscratch` на RISC-V.
pub fn set_rsp0(top: usize) {
    unsafe { (*core::ptr::addr_of_mut!(TSS)).rsp0 = top as u64 }
}
