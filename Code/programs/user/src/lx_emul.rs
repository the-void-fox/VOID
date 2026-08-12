//! lx_emul — минимальный Linux-API шим (Веха 53) поверх фундамента userspace-драйверов.
//!
//! Верхний слой пути к хостингу Linux-драйверов (X54C: Atheros, EHCI, wifi, GPU). Фундамент
//! готов — три примитива ядра дают драйверу-процессу прямой доступ к железу по capability
//! (Вехи 51–52): MMIO-окно регистров, DMA-память с физ-адресом, доставка прерывания. Здесь —
//! ПЕРВЫЙ каркас Linux-подобного API поверх них (в стиле Genode `dde_linux` / `lx_kit`): драйвер
//! пишется так, будто он в ядре Linux (`ioremap`/`kmalloc`/`dma_alloc_coherent`/`request_irq`/
//! `readl`/`writel`/`probe`), а шим переводит эти вызовы в syscall'ы VOID. Каркас на Rust
//! доказывает driver-model и threaded-IRQ; C-путь (реальные `.c` из Linux) — следующая веха.
//!
//! Соответствие «Linux → VOID»:
//! - `ioremap(cap)`            → [`crate::mmio_map`] (регистры устройства в адресный простор);
//! - `dma_alloc_coherent(cap)` → [`crate::dma_alloc`] (DMA-страница + её физический адрес);
//! - `request_irq(cap, fn)`    → нить (`SYS_THREAD_SPAWN`), крутящая [`crate::irq_wait`] — это
//!   в точности Linux threaded-oneshot IRQ (наш IOAPIC-oneshot Вехи 52 == его модель);
//! - `kmalloc`/`kfree`         → ленивая куча процесса ([`crate::heap_map`]) + свой аллокатор;
//! - `wait_for_completion`     → futex ([`crate::futex_wait`]/[`crate::futex_wake`]);
//! - `printk`                  → [`crate::write`] (консоль ядра).

use core::mem::size_of;
use core::ptr::{null_mut, read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

// ─── раскладка адресного простора драйвера (USER-регион, ниже кучи 0x6000_0000) ───
const IOREMAP_BASE: usize = 0x5000_0000; // окна ioremap, шаг 16 МиБ на устройство
const IOREMAP_STEP: usize = 0x0100_0000;
const DMA_BASE: usize = 0x5800_0000; //   DMA-страницы dma_alloc_coherent, шаг 4 КиБ
const DMA_STEP: usize = 0x1000;
const ARENA: usize = 4 * 1024 * 1024; //  куча под kmalloc (лениво замапится по факту)

// ─── driver-model: устройство и его входная точка ─────────────────────────────

/// Устройство, отдаваемое драйверу в `probe` — как `struct device`/`pci_dev` в Linux, но
/// вместо шинных ресурсов несёт capability'и на MMIO/DMA/IRQ (их сминтил init, см. init.rs).
pub struct Device {
    /// Право замапить окно регистров устройства (`ioremap`).
    pub mmio_cap: usize,
    /// Право выделять DMA-память (`dma_alloc_coherent`).
    pub dma_cap: usize,
    /// Право ждать прерывание устройства (`request_irq`); [`crate::NO_CAP`] — нет.
    pub irq_cap: usize,
}

/// Точка входа хостируемого драйвера (роль `module_init` + матчинг шины `probe`): «kit»
/// поднимает окружение (куча), собирает [`Device`] из стартовых прав процесса
/// (slot 0=mmio, 1=dma, 2=irq — так их кладёт init) и зовёт `probe`. Код возврата `probe`
/// становится кодом выхода процесса (0 — успех). Не возвращается.
pub fn module_init(probe: fn(&Device) -> i32) -> ! {
    kit_init();
    let dev = Device {
        mmio_cap: crate::start_cap(0),
        dma_cap: crate::start_cap(1),
        irq_cap: crate::start_cap(2),
    };
    let rc = probe(&dev);
    crate::exit(if rc == 0 { 0 } else { 1 })
}

// ─── printk ───────────────────────────────────────────────────────────────────

/// `printk`/`pr_info` — строка в консоль ядра.
pub fn printk(msg: &[u8]) {
    crate::write(msg);
}

/// Напечатать 32-битное значение как `0x........` (для дампа регистров в демо).
pub fn printk_hex(v: u32) {
    let hex = b"0123456789abcdef";
    let mut out = [0u8; 10];
    out[0] = b'0';
    out[1] = b'x';
    for i in 0..8 {
        out[2 + i] = hex[((v >> ((7 - i) * 4)) & 0xf) as usize];
    }
    crate::write(&out);
}

// ─── kmalloc/kfree: аллокатор поверх ленивой кучи процесса ─────────────────────
//
// Одна ленивая резервация `heap_map(ARENA)` на старте; дальше — bump с интрузивным списком
// свободных блоков (first-fit). У каждого блока 8-байтный заголовок (размер payload'а); у
// свободного в payload'е лежит адрес следующего свободного заголовка. Физические страницы
// приходят по page fault под реально тронутое. Под замком — на случай доступа из нити-IRQ.

static mut HEAP_CUR: usize = 0;
static mut HEAP_END: usize = 0;
static mut FREE_HEAD: usize = 0; // адрес заголовка первого свободного блока (0 — пусто)
static ALLOC_LOCK: AtomicBool = AtomicBool::new(false);

fn kit_init() {
    let base = crate::heap_map(ARENA);
    if base == crate::NO_CAP {
        crate::write("[lx_emul] heap_map отказал — нет кучи под kmalloc\n".as_bytes());
        crate::exit(2);
    }
    unsafe {
        HEAP_CUR = base;
        HEAP_END = base + ARENA;
    }
}

fn lock() {
    while ALLOC_LOCK.swap(true, Ordering::Acquire) {
        core::hint::spin_loop();
    }
}
fn unlock() {
    ALLOC_LOCK.store(false, Ordering::Release);
}

/// `kmalloc(size)` — блок ≥ `size` байт (8-выровнен) или NULL. Флаги GFP не моделируем.
pub fn kmalloc(size: usize) -> *mut u8 {
    let n = (size.max(8) + 7) & !7;
    lock();
    unsafe {
        // first-fit по списку свободных
        let mut prev = 0usize; // 0 — «предыдущего нет, это голова»
        let mut cur = FREE_HEAD;
        while cur != 0 {
            let blk_size = *(cur as *const usize);
            let next = *((cur + 8) as *const usize);
            if blk_size >= n {
                if prev == 0 {
                    FREE_HEAD = next;
                } else {
                    *((prev + 8) as *mut usize) = next;
                }
                unlock();
                return (cur + 8) as *mut u8;
            }
            prev = cur;
            cur = next;
        }
        // не нашли — откусить от арены
        if HEAP_CUR + 8 + n > HEAP_END {
            unlock();
            return null_mut();
        }
        let hdr = HEAP_CUR;
        HEAP_CUR += 8 + n;
        *(hdr as *mut usize) = n;
        unlock();
        (hdr + 8) as *mut u8
    }
}

/// `kzalloc(size)` — как [`kmalloc`], но обнулённый.
pub fn kzalloc(size: usize) -> *mut u8 {
    let p = kmalloc(size);
    if !p.is_null() {
        let n = (size.max(8) + 7) & !7;
        unsafe {
            for i in 0..n {
                write_volatile(p.add(i), 0);
            }
        }
    }
    p
}

/// `kfree(ptr)` — вернуть блок в список свободных (NULL игнорируется).
pub fn kfree(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    lock();
    unsafe {
        let hdr = ptr as usize - 8;
        *(ptr as *mut usize) = FREE_HEAD; // payload[0] = старая голова
        FREE_HEAD = hdr;
    }
    unlock();
}

// ─── MMIO: ioremap + readl/writel ─────────────────────────────────────────────

static IOREMAP_NEXT: AtomicUsize = AtomicUsize::new(IOREMAP_BASE);

/// `ioremap(cap, len)` — замапить окно регистров устройства (по MMIO-cap) и вернуть базу для
/// `readl`/`writel`. NULL — нет права. `len` пока справочный (окно даёт ядро по cap целиком).
pub fn ioremap(mmio_cap: usize, _len: usize) -> *mut u8 {
    let va = IOREMAP_NEXT.fetch_add(IOREMAP_STEP, Ordering::Relaxed);
    if crate::mmio_map(mmio_cap, va) {
        va as *mut u8
    } else {
        null_mut()
    }
}

/// `iounmap` — заглушка (окно живёт до выхода процесса; распаковки простора отдельного нет).
pub fn iounmap(_addr: *mut u8) {}

/// # Safety
/// `addr` — валидный, выровненный указатель в окне [`ioremap`]. Порядок аргументов — как в Linux
/// (`writel(value, addr)`).
#[inline]
pub unsafe fn writel(val: u32, addr: *mut u8) {
    write_volatile(addr as *mut u32, val);
}
/// # Safety
/// см. [`writel`].
#[inline]
pub unsafe fn readl(addr: *const u8) -> u32 {
    read_volatile(addr as *const u32)
}
/// # Safety
/// см. [`writel`].
#[inline]
pub unsafe fn readw(addr: *const u8) -> u16 {
    read_volatile(addr as *const u16)
}
/// # Safety
/// см. [`writel`].
#[inline]
pub unsafe fn readb(addr: *const u8) -> u8 {
    read_volatile(addr)
}
/// # Safety
/// см. [`writel`].
#[inline]
pub unsafe fn writew(val: u16, addr: *mut u8) {
    write_volatile(addr as *mut u16, val);
}
/// # Safety
/// см. [`writel`].
#[inline]
pub unsafe fn writeb(val: u8, addr: *mut u8) {
    write_volatile(addr, val);
}
/// # Safety
/// см. [`writel`]. `writeq` — 64-битная запись (адрес DMA-буфера в дескриптор).
#[inline]
pub unsafe fn writeq(val: u64, addr: *mut u8) {
    write_volatile(addr as *mut u64, val);
}

// ─── DMA ──────────────────────────────────────────────────────────────────────

static DMA_NEXT: AtomicUsize = AtomicUsize::new(DMA_BASE);

/// DMA-буфер: `cpu` — как видит его драйвер (VA), `dma` — физ-адрес для железа (`dma_handle`).
pub struct DmaBuf {
    pub cpu: *mut u8,
    pub dma: usize,
}

/// `dma_alloc_coherent(dev, size)` — когерентная DMA-память ≤ 4 КиБ (одна страница: кольца и
/// буферы демо в неё влезают). `cpu==NULL` — нет права/памяти.
pub fn dma_alloc_coherent(dma_cap: usize, _size: usize) -> DmaBuf {
    let va = DMA_NEXT.fetch_add(DMA_STEP, Ordering::Relaxed);
    match crate::dma_alloc(dma_cap, va) {
        Some(pa) => DmaBuf { cpu: va as *mut u8, dma: pa },
        None => DmaBuf { cpu: null_mut(), dma: 0 },
    }
}

// ─── request_irq: threaded-oneshot IRQ через нить ──────────────────────────────

const IRQ_STACK: usize = 32 * 1024;

struct IrqCtx {
    irq_cap: usize,
    handler: fn(),
}

/// Тело нити-обработчика: крутит `irq_wait` (усыпляет до прерывания, взводя oneshot-линию) и на
/// каждое прерывание зовёт `handler`. Это threaded-oneshot IRQ Linux: обработчик — в контексте
/// нити (может спать/брать замки). Отказ `irq_wait` (нет права) — нить завершается.
extern "C" fn irq_thread(arg: usize) -> ! {
    let ctx = unsafe { &*(arg as *const IrqCtx) };
    loop {
        if !crate::irq_wait(ctx.irq_cap) {
            crate::thread_exit(1);
        }
        (ctx.handler)();
    }
}

/// `request_irq(irq, handler)` — зарегистрировать threaded-обработчик прерывания устройства.
/// Заводит нить, которая спит в `irq_wait` и зовёт `handler` на каждое прерывание. `false` —
/// не удалось (нет кучи под стек/контекст или нить не завелась).
pub fn request_irq(irq_cap: usize, handler: fn()) -> bool {
    let stack = kmalloc(IRQ_STACK);
    let ctx = kmalloc(size_of::<IrqCtx>()) as *mut IrqCtx;
    if stack.is_null() || ctx.is_null() {
        return false;
    }
    unsafe {
        (*ctx).irq_cap = irq_cap;
        (*ctx).handler = handler;
    }
    let stack_top = stack as usize + IRQ_STACK;
    crate::thread_spawn(irq_thread as *const () as usize, ctx as usize, stack_top) != crate::NO_CAP
}

// ─── синхронизация: completion (futex) ─────────────────────────────────────────

/// `struct completion` — одноразовый сигнал «событие случилось» между нитью-IRQ и драйвером.
/// Уровневый (не фронтовой): `complete` до `wait_for_completion` не теряется.
pub struct Completion {
    flag: AtomicU32,
}

impl Completion {
    pub const fn new() -> Self {
        Completion { flag: AtomicU32::new(0) }
    }

    /// Отметить событие и разбудить ждущего (`complete`).
    pub fn complete(&self) {
        self.flag.store(1, Ordering::SeqCst);
        let ptr = &self.flag as *const AtomicU32 as *const u32;
        crate::futex_wake(ptr, 1);
    }

    /// Уснуть до `complete` (`wait_for_completion`). Если уже случилось — вернуться сразу.
    pub fn wait_for_completion(&self) {
        let ptr = &self.flag as *const AtomicU32 as *const u32;
        while self.flag.load(Ordering::SeqCst) == 0 {
            crate::futex_wait(ptr, 0, 0);
        }
    }
}

// ─── задержки ───────────────────────────────────────────────────────────────

/// `udelay(us)` — активная задержка на `us` микросекунд (по монотонному счётчику [`crate::now`]).
pub fn udelay(us: usize) {
    // Веха 136: тики считаем по таймбазе, которую измерило ядро. С прежней константой «1 нс на
    // тик» задержки драйверов на живом процессоре выходили втрое короче заказанных — а это ровно
    // те задержки, которыми чип обязан успеть выполнить команду.
    let ticks = crate::ns_to_ticks(us as u64 * 1000) as usize;
    let start = crate::now();
    while crate::now().wrapping_sub(start) < ticks {
        core::hint::spin_loop();
    }
}

/// `mdelay(ms)` — активная задержка на `ms` миллисекунд.
pub fn mdelay(ms: usize) {
    udelay(ms * 1000);
}
