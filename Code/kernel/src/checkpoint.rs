//! Веха 37 — checkpoint процессов: ортогональная персистентность ВЫЧИСЛЕНИЙ.
//!
//! Наследие KeyKOS ([[0002-persistent-content-addressed-capability-core]]): у VOID
//! перезагрузку переживают не только данные, но и сами процессы. Образ процесса —
//! обычные объекты store: каждая отображённая страница пространства — отдельный
//! объект по content-id (дедуп нулевых/одинаковых страниц бесплатен), манифест —
//! node-объект ([`object::put_node`]), ДЕТИ которого — страницы: GC видит образ как
//! дерево достижимости от корня `proc/<arch>/<имя>` и не съест ни одной страницы.
//!
//! Морозится процесс СЕБЯ сам (`SYS_CHECKPOINT`, семантика setjmp: живому — 0,
//! размороженному — 1), размораживает кто угодно с правом `EXEC` (`SYS_RESTORE`,
//! семантика `SYS_EXEC`: вызывающий ждёт завершения). Кадр в образе — с уже
//! продвинутым pc и результатом 1: «возврат из syscall'а в прошлой жизни».
//!
//! Образ арх-специфичен (кадр, код страниц): и корень арх-именован, и манифест
//! несёт `ARCH_NAME` с размером кадра — thaw чужого образа честно отказывает.
//!
//! Формат манифеста (LE):
//! `"VOIDPRC1" | arch_len u8 | arch | frame_len u32 | frame | heap_brk u64 |
//!  args_len u32 | args | env_len u32 | env | npages u32 |
//!  npages × (va u64 | flags u32 | child u32)`
//! `flags` — арх-нейтральные R=1|W=2|X=4 (перевод MAP_* ↔ нейтральные — здесь);
//! `child` — индекс content-id страницы в списке детей node-объекта.

use alloc::vec::Vec;

use crate::arch::{self, TrapFrame};
use core::mem::size_of;

use void_abi::ContentId;

use crate::{frame, object};

const MAGIC: &[u8; 8] = b"VOIDPRC1";
const PAGE: usize = 4096;
const R: u32 = 1;
const W: u32 = 2;
const X: u32 = 4;

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Снять образ пространства и состояния в store, привязав корень `root_name`.
/// `frame` — кадр, каким его увидит РАЗМОРОЖЕННЫЙ (результат/pc уже выставлены
/// вызывающим). Сканируются страницы [lo, hi) пространства `space`; ленивые
/// (ещё не отображённые) страницы кучи в образ не попадают — после thaw придут
/// нулями по фолту, как и жили. Возвращает число страниц образа.
pub fn freeze(
    root_name: &str,
    space: usize,
    frame: &TrapFrame,
    heap_brk: usize,
    args: &[u8],
    env: &[u8],
    lo: usize,
    hi: usize,
) -> usize {
    let root_pa = arch::space_root(space);

    // Страницы: содержимое → объекты store (дедуп по хэшу), в манифест — va+флаги+индекс.
    let mut children: Vec<ContentId> = Vec::new();
    let mut pages: Vec<(u64, u32, u32)> = Vec::new();
    let mut va = lo;
    while va < hi {
        if let Some((pa, fl)) = arch::page_info(root_pa, va) {
            // Веха 87: `page_info` отдаёт ФИЗИЧЕСКИЙ адрес страницы — читаем через direct-map.
        let bytes = unsafe { core::slice::from_raw_parts(crate::frame::ptr(pa) as *const u8, PAGE) };
            let id = object::put(bytes);
            let idx = match children.iter().position(|c| *c == id) {
                Some(i) => i,
                None => {
                    children.push(id);
                    children.len() - 1
                }
            };
            let mut nf = 0u32;
            if fl & arch::MAP_R != 0 {
                nf |= R;
            }
            if fl & arch::MAP_W != 0 {
                nf |= W;
            }
            if fl & arch::MAP_X != 0 {
                nf |= X;
            }
            pages.push((va as u64, nf, idx as u32));
        }
        va += PAGE;
    }

    let fbytes = unsafe {
        core::slice::from_raw_parts(frame as *const TrapFrame as *const u8, size_of::<TrapFrame>())
    };
    let mut m = Vec::with_capacity(64 + fbytes.len() + args.len() + env.len() + pages.len() * 16);
    m.extend_from_slice(MAGIC);
    m.push(arch::ARCH_NAME.len() as u8);
    m.extend_from_slice(arch::ARCH_NAME.as_bytes());
    put_u32(&mut m, fbytes.len() as u32);
    m.extend_from_slice(fbytes);
    put_u64(&mut m, heap_brk as u64);
    put_u32(&mut m, args.len() as u32);
    m.extend_from_slice(args);
    put_u32(&mut m, env.len() as u32);
    m.extend_from_slice(env);
    put_u32(&mut m, pages.len() as u32);
    for (va, fl, idx) in &pages {
        put_u64(&mut m, *va);
        put_u32(&mut m, *fl);
        put_u32(&mut m, *idx);
    }

    let id = object::put_node(&m, &children);
    object::set_root(root_name, id);
    pages.len()
}

/// Восстановленное из образа состояние процесса — всё, что нужно `proc`, чтобы
/// завести запись таблицы и продолжить исполнение с точки заморозки.
pub struct Thawed {
    /// Корень НОВОГО адресного пространства со страницами образа (стек/код/куча).
    pub root: usize,
    pub frame: TrapFrame,
    pub heap_brk: usize,
    pub args: Vec<u8>,
    pub env: Vec<u8>,
    pub pages: usize,
}

/// Курсор чтения манифеста с проверками границ.
struct Rd<'a>(&'a [u8], usize);

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.0.get(self.1..self.1 + n)?;
        self.1 += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
}

/// Поднять образ по корню: разобрать манифест, построить свежее пространство
/// (клон корня ядра БЕЗ стандартного стека — стек приедет страницами образа) и
/// замапить страницы с флагами образа. `None` — корня нет / манифест чужой или
/// битый / нарушен W^X (образ мог прийти извне мостом — не доверяем).
pub fn thaw(root_name: &str) -> Option<Thawed> {
    let id = object::root(root_name)?;
    let manifest = object::with(&id, |b| b.map(Vec::from))?;
    let children = object::children(&id);

    let mut r = Rd(&manifest, 0);
    if r.take(MAGIC.len())? != MAGIC {
        return None;
    }
    let alen = r.u8()? as usize;
    if r.take(alen)? != arch::ARCH_NAME.as_bytes() {
        return None; // образ другой архитектуры
    }
    let flen = r.u32()? as usize;
    if flen != size_of::<TrapFrame>() {
        return None; // кадр другого ядра/ABI
    }
    let frame = unsafe {
        core::ptr::read_unaligned(r.take(flen)?.as_ptr() as *const TrapFrame)
    };
    let heap_brk = r.u64()? as usize;
    let alen = r.u32()? as usize;
    let args = Vec::from(r.take(alen)?);
    let elen = r.u32()? as usize;
    let env = Vec::from(r.take(elen)?);
    let npages = r.u32()? as usize;

    let root = arch::clone_kernel_root()?; // Веха 89: нет памяти — разморозка не состоялась
    for _ in 0..npages {
        let va = r.u64()? as usize;
        let nf = r.u32()?;
        let idx = r.u32()? as usize;
        if nf & W != 0 && nf & X != 0 {
            return None; // W^X: подделанный образ не пройдёт (как elf.rs)
        }
        if va % PAGE != 0
            || va < crate::proc::USER_REGION_START
            || va >= crate::proc::USER_STACK_TOP_VA
        {
            return None; // страница вне региона процесса — не дать перетереть ядро
        }
        let child = children.get(idx)?;
        let pa = frame::alloc()?;
        let ok = object::with(child, |b| match b {
            Some(bytes) if bytes.len() == PAGE => {
                unsafe {
                    core::ptr::copy_nonoverlapping(bytes.as_ptr(), crate::frame::ptr(pa), PAGE);
                }
                true
            }
            _ => false,
        });
        if !ok {
            return None;
        }
        let mut fl = arch::MAP_U;
        if nf & R != 0 {
            fl |= arch::MAP_R;
        }
        if nf & W != 0 {
            fl |= arch::MAP_W;
        }
        if nf & X != 0 {
            fl |= arch::MAP_X;
        }
        // Веха 89: не хватило памяти под таблицы — разморозка не состоялась (частично
        // построенное пространство снесёт вызывающий по `None`).
        if !unsafe { arch::map(root, va, pa, fl) } {
            return None;
        }
    }

    Some(Thawed { root, frame, heap_brk, args, env, pages: npages })
}
