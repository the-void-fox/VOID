//! Реализация контракта [`crate::arch`] для x86_64
//! (Веха 25 — ядро, Веха 26 — userspace, Веха 27 — устройства).
//!
//! Живое: PVH direct boot (QEMU `-kernel`, трамплин 32→64 в entry.s), консоль COM1
//! (вывод + приём: IRQ4 через IOAPIC и, в сессиях процессов, опрос LSR на тиках),
//! GDT с ring3 и TSS (gdt.rs), IDT + syscall `int 0x80` + классификация трапов из
//! ring3 (trap.rs), 4-уровневый пейджинг с W^X (paging.rs), LAPIC-таймер (lapic.rs),
//! контексты ядерных задач (switch.s), вход в процессы через iretq (enter_user.s),
//! virtio-blk-pci: поиск на шине + MSI-X (pci.rs) — диск и персистентность работают.

use core::fmt;
use core::sync::atomic::{AtomicUsize, Ordering};

pub(crate) mod fb;
mod acpi;
mod font;
mod gdt;
mod ioapic;
mod lapic;
mod paging;
mod pci;
mod ps2;
mod rtc;
mod trap;
mod vga;

// Точка входа: PVH-нота + трамплин 32→64 (см. entry.s).
core::arch::global_asm!(include_str!("entry.s"));
// Переключение контекстов ядерных задач.
core::arch::global_asm!(include_str!("switch.s"));
// Вход в процесс: iretq по подготовленному trap-кадру.
core::arch::global_asm!(include_str!("enter_user.s"));

pub use pci::{
    e1000_irq_setup, probe_ahci, probe_e1000, probe_virtio_blk, probe_virtio_net, probe_virtio_rng,
    probe_xhci,
};
/// Веха 130 — опись шины PCI в журнал ядра: что вообще стоит в этой машине.
pub use pci::dump as pci_dump;
/// Веха 132 — найти устройство на ЛЮБОЙ шине и отдать его BAR0 (карта ноутбука за мостом PCIe).
pub use pci::probe_bar0;
pub use trap::{init as trap_init, TrapFrame};

/// Имя архитектуры — арх-измерение корней программ `bin/<arch>/<имя>` (Веха 26).
pub const ARCH_NAME: &str = "x86_64";

/// Процессы/U-mode работают (Веха 26) — kmain гоняет процессные демо.
pub const USERSPACE_READY: bool = true;

/// Полная обнаруженная ёмкость RAM машины (для отчёта). Что из неё РАБОЧЕЕ — говорит карта
/// регионов ([`crate::frame::regions`], Веха 88): «граница RAM» одним числом умерла вместе с
/// предположением, что память сплошная. До discovery — дефолт QEMU q35 128 МиБ.
static RAM_TOTAL_CELL: AtomicUsize = AtomicUsize::new(128 * 1024 * 1024);

/// Полная обнаруженная RAM машины (байты) — для отчёта памяти.
pub fn ram_total() -> usize {
    RAM_TOTAL_CELL.load(Ordering::Relaxed)
}

/// Magic multiboot2 в eax при входе от GRUB (у PVH eax не определён → 0).
const MULTIBOOT2_MAGIC: usize = 0x36D76289;

/// Как загрузились: `0x36D76289` — реальная машина через GRUB (multiboot2), иначе QEMU (PVH).
static BOOT_MAGIC_CELL: AtomicUsize = AtomicUsize::new(0);

/// Веха 42 — на РЕАЛЬНОМ железе (загрузка GRUB'ом) ли мы? Отличаем эмулятор (QEMU/PVH) от
/// металла (GRUB/multiboot2): на металле kmain НЕ гоняет QEMU-демо (часть виснет — ждут таймер/
/// диск, которых как в QEMU нет), а сразу поднимает систему, как настоящая ОС без «демо на boot».
pub fn is_real_hardware() -> bool {
    BOOT_MAGIC_CELL.load(Ordering::Relaxed) == MULTIBOOT2_MAGIC
}

/// Веха 41/85/88 — разобрать инфо-структуру загрузчика и выставить карту RAM. `magic` — eax при
/// входе (`0x36D76289` = multiboot2/GRUB), `info` — ebx (указатель на инфо: multiboot2-теги ЛИБО
/// PVH `hvm_start_info` от QEMU).
///
/// Веха 87 сняла потолок адресации (RAM живёт в верхней половине и не спорит с процессами), а
/// Веха 88 снимает последнее ограничение: раньше брали только `low_end` — конец СПЛОШНОЙ нижней
/// RAM, — потому что аллокатор знал одну непрерывную арену, и всё, что выше PCI-дыры, пропадало.
/// Теперь каждая пригодная запись карты прошивки уходит в [`crate::frame::add_region`], а дыры
/// остаются дырами.
///
/// # Safety
/// `info` — валидный указатель инфо-структуры соответствующего типа (гарантирует загрузчик).
pub fn platform_init(magic: usize, info: usize) {
    BOOT_MAGIC_CELL.store(magic, Ordering::Relaxed);
    // Веха 87: ebx от загрузчика — ФИЗИЧЕСКИЙ адрес, ядро уже работает в верхней половине.
    let info = if info == 0 { 0 } else { phys_to_virt(info) };
    let total = discover_ram(magic, info);
    // Карта не разобралась (нет инфо/битая) — принять контракт QEMU по умолчанию: 128 МиБ снизу.
    if crate::frame::regions().is_empty() {
        crate::frame::add_region(0, 128 * 1024 * 1024);
    } else {
        RAM_TOTAL_CELL.store(total, Ordering::Relaxed);
    }
}

/// Magic PVH `hvm_start_info` (по смещению 0): так отличаем QEMU-PVH от multiboot2/мусора.
const PVH_MAGIC: u32 = 0x336e_c578;

/// Веха 85/88 — обнаружить RAM: зарегистрировать пригодные регионы в [`crate::frame`] и вернуть
/// полную ёмкость (байты, для отчёта). Два источника: multiboot2 (GRUB) и PVH-memmap (QEMU);
/// если ни один не разобрался, регионы не добавляются и `platform_init` берёт дефолт.
fn discover_ram(magic: usize, info: usize) -> usize {
    if magic == MULTIBOOT2_MAGIC && info != 0 {
        return discover_multiboot(info);
    }
    if info != 0 {
        if let Some(total) = discover_pvh(info) {
            return total;
        }
    }
    0
}

/// Веха 85/88 — разобрать PVH `hvm_start_info` (QEMU `-kernel` direct boot) и его memmap: каждая
/// запись типа 1 (обычная RAM) уходит регионом в [`crate::frame::add_region`]. Возвращает сумму
/// (для отчёта); `None` — не PVH/нет memmap. Раскладка структуры и записей — из PVH-ABI.
///
/// # Safety-примечание: `info` указывает на валидную структуру (гарантия PVH-загрузчика QEMU).
fn discover_pvh(info: usize) -> Option<usize> {
    unsafe {
        let rd32 = |off: usize| core::ptr::read_unaligned((info + off) as *const u32);
        let rd64 = |off: usize| core::ptr::read_unaligned((info + off) as *const u64);
        if rd32(0) != PVH_MAGIC || rd32(4) < 1 {
            return None; // не PVH или version < 1 (memmap появился с версии 1)
        }
        let memmap = rd64(40) as usize; // memmap_paddr
        let entries = rd32(48) as usize; // memmap_entries
        if memmap == 0 || entries == 0 {
            return None;
        }
        let memmap = phys_to_virt(memmap); // Веха 87: поле физическое, читаем через direct-map
        let mut total = 0usize;
        for i in 0..entries {
            let e = memmap + i * 24; // sizeof(hvm_memmap_table_entry) = 24
            let addr = rd64_at(e) as usize;
            let size = rd64_at(e + 8) as usize;
            let ty = core::ptr::read_unaligned((e + 16) as *const u32);
            if ty == 1 {
                // 1 = обычная RAM; всё остальное (reserved/ACPI/NVS) в аллокатор не идёт
                total = total.saturating_add(size);
                crate::frame::add_region(addr, addr.saturating_add(size));
            } else if addr < 0x1_0000_0000 {
                // Веха 105: ровно та же поправка, что Веха 101 сделала для multiboot, — здесь её
                // тогда не продублировали, и `poweroff` на PVH-загрузке (`cargo run`) падал в
                // #PF при чтении таблиц ACPI: они лежат отдельной записью над обычной RAM, а в
                // direct-map попадала только RAM. Раздавать эти страницы по-прежнему некому —
                // они идут исключительно в отображение (см. paging.rs).
                let top = addr.saturating_add(size).min(0x1_0000_0000);
                if top > PHYS_LOW_TOP.load(Ordering::Relaxed) {
                    PHYS_LOW_TOP.store(top, Ordering::Relaxed);
                }
            }
        }
        (total != 0).then_some(total)
    }
}

/// Прочитать невыровненный u64 по абсолютному адресу (для записей PVH-memmap).
///
/// # Safety
/// `addr` — читаемый адрес не менее 8 байт (гарантирует вызывающий по контракту PVH).
unsafe fn rd64_at(addr: usize) -> u64 {
    core::ptr::read_unaligned(addr as *const u64)
}

/// Веха 48 — загрузочный модуль multiboot2 (образ установки VOID, [`boot_module`]): база и
/// длина в RAM, куда GRUB положил его командой `module2`. 0 — модуля нет (обычная загрузка).
static MODULE_BASE: AtomicUsize = AtomicUsize::new(0);
static MODULE_LEN: AtomicUsize = AtomicUsize::new(0);

/// Разобрать карту памяти multiboot2 (от GRUB) И загрузочный модуль (Веха 48) за один проход.
/// Инфо — список тегов (`total_size@0`, теги с `@8`): type 6 «memory map» (E820) → регионы RAM,
/// type 4 «basic meminfo» (`mem_upper@+12` — КиБ выше 1 МиБ) → запасная ёмкость одним куском,
/// type 3 «module» (`mod_start@+8`, `mod_end@+12`) → образ установки.
/// `magic`/`info` уже проверены вызывающим ([`discover_ram`]). Возвращает ёмкость (0 — не нашли).
fn discover_multiboot(info: usize) -> usize {
    let rd = |off: usize| unsafe { core::ptr::read_volatile((info + off) as *const u32) };
    let rd64 = |off: usize| unsafe { core::ptr::read_unaligned((info + off) as *const u64) };
    let mut basic = 0usize; // ёмкость по тегу 4 — запасной ответ, если карты (тег 6) не дали
    let mut total = 0usize; // сумма пригодных записей карты
    let mut p = 8usize; // теги начинаются после total_size(u32)+reserved(u32)
    loop {
        let ty = rd(p);
        let size = rd(p + 4) as usize;
        if ty == 0 || size < 8 {
            break; // завершающий тег или мусор
        }
        match ty {
            4 => basic = 0x10_0000 + rd(p + 12) as usize * 1024, // basic meminfo: mem_upper (КиБ)
            // Веха 88 — КАРТА ПАМЯТИ (E820 в переводе GRUB). Заголовок тега: type, size,
            // entry_size@+8, entry_version@+12; дальше записи по entry_size:
            // base_addr(u64) length(u64) type(u32) reserved(u32). Тип 1 = пригодная RAM.
            // Раньше этот тег не читался вовсе, и на металле мы жили по одному `mem_upper`.
            6 => {
                let esize = rd(p + 8) as usize;
                if esize >= 24 {
                    let mut e = p + 16;
                    while e + esize <= p + size {
                        let base = rd64(e) as usize;
                        let len = rd64(e + 8) as usize;
                        if rd(e + 16) == 1 {
                            total = total.saturating_add(len);
                            crate::frame::add_region(base, base.saturating_add(len));
                        } else if base < 0x1_0000_0000 {
                            // Веха 101 — НЕпригодная память тоже нужна: таблицы ACPI лежат
                            // отдельными записями (тип 3 «reclaim») сразу над обычной RAM, и в
                            // direct-map они не попадали. Читать их — единственный способ узнать,
                            // как выключить машину. Раздавать эти страницы никто не будет: в
                            // аллокатор они не идут, только в отображение (см. paging.rs).
                            let top = base.saturating_add(len).min(0x1_0000_0000);
                            if top > PHYS_LOW_TOP.load(Ordering::Relaxed) {
                                PHYS_LOW_TOP.store(top, Ordering::Relaxed);
                            }
                        }
                        e += esize;
                    }
                }
            }
            3 => {
                let (start, end) = (rd(p + 8) as usize, rd(p + 12) as usize);
                MODULE_BASE.store(start, Ordering::Relaxed);
                MODULE_LEN.store(end.saturating_sub(start), Ordering::Relaxed);
                // GRUB кладёт модуль в свободную RAM (обычно сразу за образом ядра) — уберечь его
                // от bump-аллокатора фреймов: тот стартует с `_kernel_end`, а модуль может быть
                // выше. Резервируем [.._end), frame::init поднимет старт до этой границы.
                crate::frame::reserve_boot_module(end);
            }
            _ => {}
        }
        p += (size + 7) & !7; // следующий тег — с выравниванием на 8
    }
    // Карта разобралась — она и есть истина; иначе откатываемся на `basic` одним регионом.
    if total > 0 {
        return total;
    }
    if basic > 0 {
        crate::frame::add_region(0, basic);
    }
    basic
}

/// Веха 48 — загрузочный модуль (образ установки), переданный GRUB через `module2`:
/// `(база, длина)` в RAM или `None`. Читает установщик [`crate::install`].
pub fn boot_module() -> Option<(usize, usize)> {
    let len = MODULE_LEN.load(Ordering::Relaxed);
    (len != 0).then(|| (MODULE_BASE.load(Ordering::Relaxed), len))
}

// ─── консоль (COM1: вывод напрямую, приём — опросом LSR на тиках таймера) ────

const COM1: u16 = 0x3f8;

/// IRQ COM1 в классической маршрутизации — пригодится при подключении IOAPIC (Веха 27+).
pub const CONSOLE_IRQ: u32 = 4;

#[inline]
fn outb(port: u16, v: u8) {
    unsafe { core::arch::asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack)) }
}

#[inline]
fn inb(port: u16) -> u8 {
    let v: u8;
    unsafe { core::arch::asm!("in al, dx", in("dx") port, out("al") v, options(nomem, nostack)) }
    v
}

/// Zero-sized хэндл последовательной консоли (пишем в THR COM1 без инициализации линии —
/// QEMU этого достаточно; делитель/FIFO настроит приёмная часть при bring-up ввода).
pub struct Console;

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        // Идём по СИМВОЛАМ: в COM1 (терминал QEMU) — сырой UTF-8, в VGA — перевод в CP866
        // (там загружен наш шрифт), иначе кириллица на экране машины была бы «?».
        let mut buf = [0u8; 4];
        for c in s.chars() {
            if c == '\n' {
                outb(COM1, b'\r');
            }
            for &b in c.encode_utf8(&mut buf).as_bytes() {
                outb(COM1, b);
            }
            // Веха 41: VGA-текст — на реальной машине (без COM-порта) виден ОН.
            vga::put_char(c);
        }
        // Веха 43: подвинуть аппаратный курсор VGA туда, где мы остановились (иначе висит).
        vga::sync_cursor();
        Ok(())
    }
}

/// Веха 41/96 — ранняя инициализация консоли. Зовётся ПЕРВОЙ в `kmain`, до любого вывода:
/// на реальной машине COM-порта нет, и всё, что напечатано раньше, не увидит никто.
///
/// `magic`/`info` — то же, что придёт в [`platform_init`] (eax/ebx от загрузчика). Они нужны
/// здесь, а не позже, из-за Вехи 96: если GRUB поставил ГРАФИЧЕСКИЙ режим по нашему тегу
/// (`entry.s`), то текстового буфера `0xB8000` больше нет, и до разбора инфо-тега 8 экран
/// нем. Поэтому теги просматриваются дважды: тут — только ради фреймбуфера, в `platform_init` —
/// ради карты памяти и загрузочного модуля.
///
/// Дальше вилка:
/// - **пиксельный режим** — знакогенератор не трогаем вовсе ([`vga::load_font`] лезет в
///   регистры секвенсора и в графическом режиме только сломала бы картинку);
/// - **текстовый режим** — как было: свой CP866-шрифт в знакогенератор и очистка экрана
///   от мусора BIOS.
///
/// На riscv — no-op (там консоль — UART).
pub fn console_init(magic: usize, info: usize) {
    if magic == MULTIBOOT2_MAGIC && info != 0 {
        // Веха 87: ebx от загрузчика — физический адрес; ядро уже в верхней половине.
        discover_framebuffer(phys_to_virt(info));
    }
    if !fb::present() {
        vga::load_font();
    }
    vga::clear();
}

/// Веха 96 — режим пиксельной консоли `(ширина, высота, бит на пиксель)`; `None` — текстовый
/// VGA (GRUB режим не дал либо мы грузились через PVH). Только для отчёта на загрузке.
pub fn video_mode() -> Option<(usize, usize, usize)> {
    fb::present().then(fb::geometry)
}

/// Веха 97 — окно фреймбуфера `(физ. база, длина)` для выдачи процессу под capability
/// (`mmio:fb` в конфиге init). `None` — пиксельного режима нет.
pub fn video_window() -> Option<(usize, usize)> {
    fb::window()
}

/// Веха 97 — полное описание режима для `SYS_VIDEO_INFO`.
pub fn video_info() -> (usize, usize, usize, usize, [(u8, u8); 3]) {
    fb::info()
}

/// Веха 97 — экран отдан процессу / забрать обратно (паника).
pub fn video_give_to_user(pid: usize) {
    fb::give_to_user(pid);
}

/// Кто сейчас владеет экраном (`None` — ядро).
pub fn video_owner() -> Option<usize> {
    fb::owner()
}
pub fn video_take_back() {
    fb::take_back();
}

/// Веха 96 — найти в инфо-тегах multiboot2 тег 8 (framebuffer) и отдать его [`fb::init`].
/// Раскладка тега: `addr@+8` (u64), `pitch@+16`, `width@+20`, `height@+24`, `bpp@+28` (u8),
/// `type@+29` (u8), **`reserved@+30` — u16, а не байт** (общая часть тега ровно 32 байта),
/// дальше для типа 1 (прямой RGB) — позиции и ширины полей R/G/B с `+32`.
///
/// На этом легко ошибиться на единицу, и ошибка тихая: цвета уезжают, но система работает.
/// Так и вышло при отладке — красный канал вставал на место синего, и серый текст выходил
/// бирюзовым. Поймал скриншот, лог показать этого не мог.
///
/// Тип 2 («EGA-текст») означает, что GRUB режим не поставил и мы остались в текстовом VGA —
/// тег в этом случае игнорируется, консоль работает по-старому.
fn discover_framebuffer(info: usize) {
    let rd = |off: usize| unsafe { core::ptr::read_volatile((info + off) as *const u32) };
    let rd8 = |off: usize| unsafe { core::ptr::read_volatile((info + off) as *const u8) };
    let rd64 = |off: usize| unsafe { core::ptr::read_unaligned((info + off) as *const u64) };
    let mut p = 8usize;
    loop {
        let ty = rd(p);
        let size = rd(p + 4) as usize;
        if ty == 0 || size < 8 {
            return;
        }
        if ty == 8 && size >= 38 && rd8(p + 29) == 1 {
            fb::init(
                rd64(p + 8) as usize,
                rd(p + 16) as usize,
                rd(p + 20) as usize,
                rd(p + 24) as usize,
                rd8(p + 28),
                fb::RgbFields {
                    red: (rd8(p + 32), rd8(p + 33)),
                    green: (rd8(p + 34), rd8(p + 35)),
                    blue: (rd8(p + 36), rd8(p + 37)),
                },
            );
            return;
        }
        p += (size + 7) & !7;
    }
}

// Кольцевой буфер принятых байт — аналог riscv64/uart.rs. Наполняется двумя путями
// (оба идемпотентны и не пересекаются — прерывания в обработчиках выключены):
// IRQ4 через IOAPIC (Веха 27 — будит сон до ввода) и опрос на тиках таймера
// (политика Вехи 20.1 — подбирает байты в сессиях процессов между прерываниями).
/// Веха 101 — 1 КиБ, а не 256 Б: столько же, сколько строка команды в шелле. Прежние 256 были
/// ровно СТАРЫМ пределом строки, и длинный модуль `.vv`, вставленный в консоль, терял байты из
/// середины — кольцо переполнялось быстрее, чем шелл его вычерпывал.
const RX_CAP: usize = 1024;
static mut RX_BUF: [u8; RX_CAP] = [0; RX_CAP];
static RX_HEAD: AtomicUsize = AtomicUsize::new(0); // писатель (drain)
static RX_TAIL: AtomicUsize = AtomicUsize::new(0); // читатель (getc)

/// Положить принятый байт в кольцевой буфер (переполнение — байт теряется, как в riscv-кольце).
/// Единая точка для обоих источников ввода: COM1 (QEMU/serial) и PS/2-клавиатура (реальное
/// железо, Веха 42). Зовётся с выключенными прерываниями (из обработчика тика/IRQ) — гонок нет.
pub(super) fn rx_push(b: u8) {
    let head = RX_HEAD.load(Ordering::Relaxed);
    if head.wrapping_sub(RX_TAIL.load(Ordering::Relaxed)) < RX_CAP {
        unsafe { RX_BUF[head % RX_CAP] = b };
        RX_HEAD.store(head.wrapping_add(1), Ordering::Relaxed);
    } else {
        // Веха 101 — потеря ввода перестала быть молчаливой: счётчик заберёт и покажет чтение
        // (`SYS_READ`). Человек, увидевший испорченную строку, должен знать, что виноват не он.
        RX_LOST.fetch_add(1, Ordering::Relaxed);
    }
}

/// Код клавиши для байта, пришедшего без скан-кода (serial). Печатные символы нумеруются
/// самим ASCII — так `bind("wm", "Super+L", …)` и обычная буква говорят на одном языке.
fn keysym_of_ascii(b: u8) -> u16 {
    match b {
        b'\r' | b'\n' => 0x101,
        27 => 0x102,
        b'\t' => 0x103,
        8 | 0x7f => 0x104,
        c if c.is_ascii_uppercase() => c.to_ascii_lowercase() as u16,
        c => c as u16,
    }
}

/// Сколько байт ввода потеряно переполнением кольца (забирается и обнуляется [`console_take_lost`]).
static RX_LOST: AtomicUsize = AtomicUsize::new(0);

// ── события мыши (Веха 115) ──────────────────────────────────────────────────
//
// Кольцо СОБЫТИЙ, а не «текущей позиции». Позицию курсора ядро не ведёт намеренно: где курсор,
// решает владелец экрана (сегодня терминал, завтра композитор) — он знает границы, ускорение и
// то, во что курсор упирается. Ядро сообщает лишь то, что произошло на проводе.
//
// Порядок событий сохраняется потому, что нажатие ОСМЫСЛЕННО ЛИШЬ ВМЕСТЕ с положением: свернув
// поток в «сумму смещений плюс последние кнопки», мы бы получили клик не там, где он был.

/// Одно событие: смещения и состояние кнопок (бит0 левая, бит1 правая, бит2 средняя).
#[derive(Clone, Copy)]
pub struct MouseEvent {
    pub dx: i16,
    pub dy: i16,
    pub buttons: u8,
    /// Колесо: +1 от себя, -1 на себя, 0 — не крутили (Веха 123.1).
    pub wheel: i8,
}

const MOUSE_CAP: usize = 128;
static mut MOUSE_BUF: [MouseEvent; MOUSE_CAP] =
    [MouseEvent { dx: 0, dy: 0, buttons: 0, wheel: 0 }; MOUSE_CAP];
static MOUSE_HEAD: AtomicUsize = AtomicUsize::new(0);
static MOUSE_TAIL: AtomicUsize = AtomicUsize::new(0);
static MOUSE_LOST: AtomicUsize = AtomicUsize::new(0);

/// Положить событие (зовётся из обработчика/опроса с выключенными прерываниями).
pub(super) fn mouse_push(dx: i16, dy: i16, buttons: u8, wheel: i8) {
    let head = MOUSE_HEAD.load(Ordering::Relaxed);
    if head.wrapping_sub(MOUSE_TAIL.load(Ordering::Relaxed)) < MOUSE_CAP {
        unsafe { MOUSE_BUF[head % MOUSE_CAP] = MouseEvent { dx, dy, buttons, wheel } };
        MOUSE_HEAD.store(head.wrapping_add(1), Ordering::Relaxed);
    } else {
        // Переполнение значит, что владелец экрана не успевает читать. Молчать нельзя по той же
        // причине, что и с вводом консоли (Веха 101): рывок курсора должен иметь объяснение.
        MOUSE_LOST.fetch_add(1, Ordering::Relaxed);
    }
}

/// Забрать очередное событие.
pub fn mouse_pop() -> Option<MouseEvent> {
    let tail = MOUSE_TAIL.load(Ordering::Relaxed);
    if tail == MOUSE_HEAD.load(Ordering::Relaxed) {
        return None;
    }
    let e = unsafe { MOUSE_BUF[tail % MOUSE_CAP] };
    MOUSE_TAIL.store(tail.wrapping_add(1), Ordering::Relaxed);
    Some(e)
}

/// Есть ли непрочитанные события (для пробуждения спящего владельца экрана).
pub fn mouse_pending() -> bool {
    MOUSE_TAIL.load(Ordering::Relaxed) != MOUSE_HEAD.load(Ordering::Relaxed)
}

// ── события клавиатуры (Веха 119) ────────────────────────────────────────────
//
// Кольцо СОБЫТИЙ рядом с кольцом ASCII-байт, а не вместо него. Довод: байты нужны всем, кто
// читает «текст с клавиатуры» (vsh, панели терминала, ввод через serial), а события нужны тому,
// кто разбирает АККОРДЫ — оконному менеджеру. Из байта аккорд не восстановить: `Super+L` и `l`
// это один и тот же байт, а Super в ASCII не выражается вовсе.

/// Одно событие: код клавиши, маска модификаторов, нажатие/отпускание и готовый символ.
///
/// Символ кладётся СЮДА ЖЕ намеренно: раскладку знает ядро (таблица скан-кодов), и заставлять
/// оконный менеджер собирать букву заново значило бы завести вторую раскладку, которая разойдётся
/// с первой.
///
/// Веха 127.1 — это КОДОВАЯ ТОЧКА, а не байт: с раскладкой RU/EN печатается кириллица, которая в
/// байт не влезает. `sym` при этом остаётся US-кодом клавиши и от раскладки НЕ зависит.
#[derive(Clone, Copy)]
pub struct KeyEvent {
    pub sym: u16,
    pub mods: u8,
    pub down: bool,
    pub ch: u16,
}

const KEY_CAP: usize = 128;
static mut KEY_BUF: [KeyEvent; KEY_CAP] =
    [KeyEvent { sym: 0, mods: 0, down: false, ch: 0 }; KEY_CAP];
static KEY_HEAD: AtomicUsize = AtomicUsize::new(0);
static KEY_TAIL: AtomicUsize = AtomicUsize::new(0);

pub(super) fn key_push(sym: u16, mods: u8, down: bool, ch: u16) {
    let head = KEY_HEAD.load(Ordering::Relaxed);
    if head.wrapping_sub(KEY_TAIL.load(Ordering::Relaxed)) < KEY_CAP {
        unsafe { KEY_BUF[head % KEY_CAP] = KeyEvent { sym, mods, down, ch } };
        KEY_HEAD.store(head.wrapping_add(1), Ordering::Relaxed);
    }
}

pub fn key_pop() -> Option<KeyEvent> {
    let tail = KEY_TAIL.load(Ordering::Relaxed);
    if tail == KEY_HEAD.load(Ordering::Relaxed) {
        return None;
    }
    let e = unsafe { KEY_BUF[tail % KEY_CAP] };
    KEY_TAIL.store(tail.wrapping_add(1), Ordering::Relaxed);
    Some(e)
}

pub fn key_pending() -> bool {
    KEY_TAIL.load(Ordering::Relaxed) != KEY_HEAD.load(Ordering::Relaxed)
}

/// Есть ли рабочая PS/2-мышь (ответила на команды включения).
pub fn mouse_present() -> bool {
    ps2::mouse_present()
}

/// Отозвалась ли мышь колесом (Веха 123.1).
pub fn mouse_wheel() -> bool {
    ps2::mouse_wheel()
}

/// Забрать и обнулить счётчик потерянных событий.
pub fn mouse_take_lost() -> usize {
    MOUSE_LOST.swap(0, Ordering::Relaxed)
}

/// Веха 101 — верхняя граница НЕпригодной памяти ниже 4 ГиБ (ACPI reclaim/NVS/reserved из
/// карты firmware). Нужна одному потребителю — direct-map, чтобы ядро могло ПРОЧИТАТЬ таблицы
/// ACPI (иначе выключение машины упирается в page fault на первом же указателе).
static PHYS_LOW_TOP: AtomicUsize = AtomicUsize::new(0);

/// Докуда стоит дотянуть direct-map сверх обычной RAM (см. [`PHYS_LOW_TOP`]).
pub(super) fn phys_low_top() -> usize {
    PHYS_LOW_TOP.load(Ordering::Relaxed)
}

/// Размер консоли в знакоместах (Веха 120). Экран у неё бывает текстовый (80×25) и пиксельный
/// (сколько дал GRUB) — программа снаружи различить их не может, а редактору во весь экран это
/// первое, что нужно знать.
pub fn console_size() -> (usize, usize) {
    vga::size()
}

/// Забрать и обнулить счётчик потерянного ввода.
pub fn console_take_lost() -> usize {
    RX_LOST.swap(0, Ordering::Relaxed)
}

/// Вычерпать приёмные буферы в кольцо: COM1 FIFO (LSR.DR — «данные готовы») И скан-коды
/// PS/2-клавиатуры (Веха 42). На QEMU ввод идёт через COM1 (serial), на реальной машине —
/// через клавиатуру; оба пути наполняют одно кольцо, `console_getc` их не различает.
///
/// Веха 42: у ноутбука НЕТ COM-порта — чтение LSR (0x3F8+5) на открытой шине даёт `0xFF`, где
/// бит DR всегда «1» → наивный `while LSR&DR` крутился бы ВЕЧНО (это и вешало реальную машину
/// сразу после включения прерываний). `LSR == 0xFF` — надёжный признак отсутствия UART (у
/// живого 16550 бит 7 не бывает вместе со всеми): в этом случае COM1 пропускаем. Плюс страховка
/// от флуда — не больше кольца за проход.
pub fn console_drain() {
    let mut n = 0;
    loop {
        let lsr = inb(COM1 + 5);
        if lsr == 0xff || lsr & 1 == 0 || n >= RX_CAP {
            break;
        }
        // Веха 119 — байт из serial становится и СОБЫТИЕМ: у него нет модификаторов (взять их
        // неоткуда), поэтому аккорды по serial невыразимы — но печатать в окно можно, и это
        // важнее. Клавиатура PS/2 кладёт события сама, с настоящей маской.
        let b = inb(COM1);
        rx_push(b);
        key_push(keysym_of_ascii(b), 0, true, b as u16);
        n += 1;
    }
    ps2::drain();
    crate::xhci::poll(); // Веха 50: USB-клавиатура (если поднята) — тот же кольцевой буфер
}

/// Веха 50 — байт от USB-HID-клавиатуры в кольцо консоли (как PS/2 [`rx_push`]).
pub fn usb_key(b: u8) {
    rx_push(b);
}

pub fn console_has_input() -> bool {
    RX_HEAD.load(Ordering::Relaxed) != RX_TAIL.load(Ordering::Relaxed)
}

pub fn console_getc() -> Option<u8> {
    let tail = RX_TAIL.load(Ordering::Relaxed);
    if RX_HEAD.load(Ordering::Relaxed) == tail {
        return None;
    }
    let b = unsafe { RX_BUF[tail % RX_CAP] };
    RX_TAIL.store(tail + 1, Ordering::Relaxed);
    Some(b)
}

// ─── прерывания ─────────────────────────────────────────────────────────────

/// Выключить прерывания, вернув прежнее состояние rflags.IF.
pub fn irq_save_disable() -> bool {
    let rflags: usize;
    unsafe {
        core::arch::asm!("pushfq", "pop {0}", "cli", out(reg) rflags, options(nomem));
    }
    rflags & (1 << 9) != 0 // IF
}

/// Восстановить состояние IF из [`irq_save_disable`].
pub fn irq_restore(enabled: bool) {
    if enabled {
        unsafe { core::arch::asm!("sti", options(nomem, nostack)) }
    }
}

/// Глобально включить прерывания.
pub fn enable_interrupts() {
    unsafe { core::arch::asm!("sti", options(nomem, nostack)) }
}

/// Спать до прерывания. ОТЛИЧИЕ от `wfi`: hlt при IF=0 не просыпается от pending-
/// прерывания — поэтому классический идиом `sti; hlt` (sti вступает в силу ПОСЛЕ
/// следующей инструкции — прерывание не проскочит между ними, а ОБСЛУЖИТСЯ прямо
/// в окне hlt), затем cli возвращает состояние «в ядре прерывания выключены».
/// Семантика для вызывающих та же, что у wfi: вернулись — прерывание случилось
/// (просто обработчик уже отработал здесь, а не в «коротком окне» после).
pub fn wait_for_interrupt() {
    unsafe { core::arch::asm!("sti", "hlt", "cli", options(nomem, nostack)) }
}

// ─── маски прерываний сессий процессов (Веха 26/27) ─────────────────────────
// На riscv это биты sie (таймер/внешние). Здесь прерывания устройств (консоль по
// IOAPIC, диск по MSI-X) обрабатываются trap-диспетчером прозрачно ИЗ ЛЮБОГО кольца
// (короткая работа + EOI + iretq в прерванное) — маскировать их на сессию незачем;
// политики управляют только LVT-таймером LAPIC.

/// Снимок маски: бит 0 = LVT-таймер размаскирован.
pub fn irq_mask_read() -> usize {
    (!lapic::timer_masked()) as usize
}

/// Восстановить маску из снимка [`irq_mask_read`].
pub fn irq_mask_write(mask: usize) {
    lapic::set_timer_masked(mask & 1 == 0);
}

/// Сессия процессов: таймер вкл — вытеснение в ring3 + опрос консоли на тиках.
pub fn irq_mask_preempt(_saved: usize) {
    lapic::set_timer_masked(false);
}

/// Сон до ввода: таймер выкл — исполнять некого, разбудит IRQ4 консоли (Веха 27),
/// как SEIE-путь на riscv. Веха 50: но если поднята USB-клавиатура (у неё нет прерывания —
/// опрос), таймер ОСТАВЛЯЕМ вкл, чтобы тики опрашивали её (`console_drain` → `xhci::poll`);
/// иначе HLT спал бы до IRQ консоли и USB-нажатия терялись бы.
pub fn irq_mask_stdin(_saved: usize) {
    lapic::set_timer_masked(!crate::xhci::has_keyboard());
}

/// Веха 91 — сон СО СРОКОМ: таймер нужен, чтобы заметить срок; прерывания устройств (в т.ч.
/// MSI-X сети) на x86 идут через LAPIC и отдельной маски не требуют.
pub fn irq_mask_idle(_saved: usize) {
    lapic::set_timer_masked(false);
}

pub fn mark_in_kernel() {}

/// Веха 52 — «взвести» прерывание userspace-драйвера перед сном в `SYS_IRQ_WAIT`: размаскировать
/// его PCI INTx-линии в IOAPIC. Обработчик VEC_USERDRV снова замаскирует по факту прерывания
/// (oneshot: level-линию нельзя оставлять размаскированной, пока драйвер не снял причину в ICR).
pub fn userdrv_irq_arm() {
    ioapic::set_userdrv_masked(false);
}

/// Маршрутизация прерываний устройств (Веха 27): IOAPIC ведёт GSI4 (COM1) на
/// [`trap::VEC_CONSOLE`]; сам UART начинает слать прерывания приёма (IER.DR;
/// OUT2 в MCR — классический «разъём» линии до контроллера). Диск сюда не ходит:
/// его MSI-X взводит pci.rs при поиске устройства.
///
/// Веха 42: клавиатура — GSI1 на ТОТ ЖЕ вектор консоли (оба источника ввода наполняют одно
/// кольцо, `console_drain` черпает и COM1, и PS/2), плюс инициализация контроллера 8042. Так
/// на реальной машине нажатие клавиши будит систему из сна `wait_stdin` (как IRQ4 в QEMU).
pub fn init_device_interrupts() {
    // Веха 42: COM1 подключаем к прерыванию ТОЛЬКО если порт реально есть (в QEMU есть, на
    // ноутбуке нет: LSR читается как 0xFF). Иначе GSI4 слал бы спурьёзные прерывания в пустоту.
    if inb(COM1 + 5) != 0xff {
        ioapic::route(CONSOLE_IRQ, trap::VEC_CONSOLE);
        outb(COM1 + 1, 0x01); // IER: data ready
        outb(COM1 + 4, 0x0b); // MCR: DTR | RTS | OUT2
    }
    ioapic::route(1, trap::VEC_CONSOLE); // GSI1 — клавиатура PS/2 (общий вектор с COM1)
    // Веха 115 — GSI12: мышь на том же контроллере 8042 и том же векторе. Обработчик один,
    // `console_drain` разбирает, чей байт, по биту статуса.
    ioapic::route(12, trap::VEC_CONSOLE);
    ps2::init();
}

// ─── таймер (LAPIC) ─────────────────────────────────────────────────────────

/// Включить LAPIC (+ spurious), one-shot LVT-таймер и глобально прерывания.
pub fn timer_hw_init() {
    lapic::init();
    enable_interrupts();
}

/// Перевзвести квант вытеснения (one-shot: запись initial count = старт отсчёта).
pub fn timer_arm() {
    lapic::arm();
}

// ─── память (4-уровневый пейджинг) ──────────────────────────────────────────

/// Имя схемы трансляции — для баннера загрузки.
pub const MM_NAME: &str = "x86_64 4-level";

pub use paging::{clone_kernel_root, free_address_space, translate};

/// Флаги [`map`] — арх-нейтральные биты; в PTE их переводит сам `map` (x86 наоборот
/// ЗАПРЕЩАЕТ исполнение битом NX — см. paging.rs).
pub const MAP_R: usize = 1 << 0;
pub const MAP_W: usize = 1 << 1;
pub const MAP_X: usize = 1 << 2;
pub const MAP_U: usize = 1 << 3;
/// Веха 117 — write-combining: записи копятся и уходят пачками. Только для ФРЕЙМБУФЕРА;
/// регистрам устройства WC противопоказан (записи сливаются и переупорядочиваются, а регистр
/// ждёт ровно ту последовательность, которую ему написали).
pub const MAP_WC: usize = 1 << 4;

/// Веха 129 — страница из ОБЩЕЙ области: пометить её в записи, чтобы смерть процесса не
/// освободила чужие фреймы (см. `paging::PTE_SHARED`).
pub const MAP_SHARED: usize = 1 << 5;

/// Построить таблицы ядра (direct map + W^X + MMIO) и вернуть корень (PML4).
pub fn mm_init() -> usize {
    // PAT программируется здесь: до первой WC-страницы и до включения трансляции.
    paging::pat_init();
    paging::init()
}

/// Включить трансляцию по корню (CR3). До этого работали ВРЕМЕННЫЕ идентичные
/// таблицы трамплина (entry.s) — как «до paging::init» на RISC-V.
///
/// # Safety
/// См. paging::enable.
pub unsafe fn mm_enable(root: usize) {
    paging::enable(root)
}

/// Отобразить страницу `va → pa` с флагами `MAP_*` (перевод в биты PTE — внутри).
///
/// # Safety
/// См. paging::map.
#[must_use]
pub unsafe fn map(root: usize, va: usize, pa: usize, flags: usize) -> bool {
    let mut pte = 0u64;
    if flags & MAP_W != 0 {
        pte |= paging::PTE_W;
    }
    if flags & MAP_U != 0 {
        pte |= paging::PTE_U;
    }
    if flags & MAP_X == 0 {
        pte |= paging::PTE_NX; // x86: исполнение ЗАПРЕЩАЕТСЯ, а не разрешается
    }
    if flags & MAP_WC != 0 {
        pte |= paging::PTE_PAT; // строка PA4 = WC (см. paging::pat_init)
    }
    if flags & MAP_SHARED != 0 {
        pte |= paging::PTE_SHARED;
    }
    paging::map(root, va, pa, pte)
}

/// Веха 129 — снять отображение общей страницы `va`, лежащей на фрейме `pa`. `false` — там не
/// она (см. `paging::unmap_shared`: чужого этот вызов не трогает).
///
/// # Safety
/// См. `paging::unmap_shared`.
#[must_use]
pub unsafe fn unmap_shared(root: usize, va: usize, pa: usize) -> bool {
    paging::unmap_shared(root, va, pa)
}

/// Веха 37 — VA→(PA страницы, флаги MAP_*): обратный перевод листового PTE
/// (зеркало `map`): R — сам Present, W — PTE_W, X — ОТСУТСТВИЕ PTE_NX, U — PTE_U.
pub fn page_info(root: usize, va: usize) -> Option<(usize, usize)> {
    let (pa, pte) = paging::page_info(root, va)?;
    let mut flags = MAP_R;
    if pte & paging::PTE_W != 0 {
        flags |= MAP_W;
    }
    if pte & paging::PTE_NX == 0 {
        flags |= MAP_X;
    }
    if pte & paging::PTE_U != 0 {
        flags |= MAP_U;
    }
    Some((pa, flags))
}

/// Сбросить TLB после смены отображений активного пространства (перезагрузка CR3).
pub fn flush_tlb() {
    unsafe {
        core::arch::asm!(
            "mov {tmp}, cr3",
            "mov cr3, {tmp}",
            tmp = out(reg) _,
            options(nostack),
        );
    }
}

/// Токен адресного пространства — на x86 это значение CR3 (низ = флаги, нулевые).
pub fn space_token(root: usize) -> usize {
    root
}

/// Корень таблиц из токена.
pub fn space_root(token: usize) -> usize {
    token & !0xfff
}

/// Веха 35 — монотонный счётчик тиков для futex-дедлайнов (`rdtsc`; ~1 ГГц в QEMU TCG,
/// TICK_NS=1). Тот же счётчик, что читает U-mode для замеров и `Instant`.
pub fn now_ticks() -> u64 {
    let (lo, hi): (u32, u32);
    unsafe { core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack)) };
    (hi as u64) << 32 | lo as u64
}

/// Счётчик тактов для джиттер-источника энтропии. На x86 это тот же `rdtsc`, что и таймбаза:
/// он считает такты, а не микросекунды, поэтому разброс задержек памяти в нём виден.
/// `Some` всегда — `rdtsc` есть на любом x86_64.
pub fn now_cycles() -> Option<u64> {
    Some(now_ticks())
}

/// MSR базы сегмента `%fs` — Веха 35 несёт в нём TLS-указатель нити (Variant II).
const IA32_FS_BASE: u32 = 0xC000_0100;

/// Записать модельно-специфичный регистр (ring0). `edx:eax = value`, `ecx = msr`.
#[inline]
unsafe fn wrmsr(msr: u32, value: u64) {
    let lo = value as u32;
    let hi = (value >> 32) as u32;
    core::arch::asm!("wrmsr", in("ecx") msr, in("eax") lo, in("edx") hi, options(nostack, nomem));
}

// ─── вход в процесс (Веха 26) ───────────────────────────────────────────────

/// Войти в процесс: стек следующего трапа из ring3 — в TSS.rsp0 (аналог sscratch),
/// адресное пространство — в CR3 (заодно полный сброс TLB, как sfence.vma), кадр —
/// в регистры через iretq (enter_user.s). Селекторы ring3 и IF ставим здесь ВСЕГДА:
/// стартовые кадры их не заполняют, а кадрам из трапов не даём права понизить их.
///
/// # Safety
/// `space` — валидный токен пространства с отображённым ядром; `frame` — стартовый
/// или сохранённый трапом кадр этого процесса.
pub unsafe fn enter_user(frame: &TrapFrame, space: usize, trap_top: usize) -> ! {
    extern "C" {
        fn x86_enter_user(f: *const TrapFrame) -> !;
    }
    gdt::set_rsp0(trap_top);
    let mut f = *frame;
    f.cs = gdt::UCODE_SEL as usize;
    f.ss = gdt::UDATA_SEL as usize;
    f.rflags |= 1 << 9; // IF: в U-mode прерывания всегда включены (вытеснение)
    // Веха 35 — TLS нити: загрузить её базу %fs (0 у нитей без TLS — безвредно).
    // fsbase — глобальный регистр CPU, прошлая нить могла оставить свой → ставим всегда.
    wrmsr(IA32_FS_BASE, f.fsbase as u64);
    // Веха 36 — вернуть FP/SSE-состояние процесса (XMM — глобальные регистры CPU,
    // симметрично save_fp на трапе; ядро между ними их не трогает — soft-float).
    f.restore_fp();
    paging::enable(space_root(space));
    x86_enter_user(&f)
}

// ─── контексты ядерных задач ────────────────────────────────────────────────

/// Callee-saved x86_64. Раскладка строго совпадает со switch.s; адрес возврата не
/// хранится — он на стеке задачи (финальный `ret` switch.s возобновляет её).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Context {
    rsp: usize,
    rbx: usize,
    rbp: usize,
    r12: usize,
    r13: usize,
    r14: usize,
    r15: usize,
}

impl Context {
    /// Пустой контекст — для статиков и «заполнится при первом переключении».
    pub const EMPTY: Context =
        Context { rsp: 0, rbx: 0, rbp: 0, r12: 0, r13: 0, r14: 0, r15: 0 };

    /// Контекст новой ядерной ЗАДАЧИ: на дно стека кладётся адрес трамплина (его
    /// возьмёт `ret` в switch.s), функция задачи — в rbx (её вызовет трамплин; по
    /// возврату — `sched::task_exit`). Выравнивание: после `ret` rsp кратен 16 —
    /// SysV-состояние «как после call».
    pub fn new_task(entry: fn(), sp: usize) -> Context {
        extern "C" {
            fn x86_task_trampoline();
        }
        let top = (sp & !0xf) - 8;
        unsafe { *(top as *mut usize) = x86_task_trampoline as *const () as usize };
        let mut c = Context::EMPTY;
        c.rsp = top;
        c.rbx = entry as usize;
        c
    }

    /// Контекст прямого входа в функцию ядра на заданном стеке (лончер сессии
    /// процессов): на дно стека — адрес самой функции.
    pub fn new_kernel(entry: extern "C" fn() -> !, sp: usize) -> Context {
        let top = (sp & !0xf) - 8;
        unsafe { *(top as *mut usize) = entry as usize };
        let mut c = Context::EMPTY;
        c.rsp = top;
        c
    }
}

extern "C" {
    /// Сохранить текущий контекст в `*old`, загрузить `*new` и продолжить в нём
    /// (см. switch.s).
    pub fn context_switch(old: *mut Context, new: *const Context);
}

// ─── direct-map: физический адрес ↔ указатель ядра (Веха 87) ────────────────

/// Веха 87 — смещение direct-map: `VA = PA + KERNEL_OFFSET` ([[0010-address-space-layout]]).
/// Это начало верхней половины (в терминах Linux — `PAGE_OFFSET`), поэтому в direct-map
/// помещается вся физическая память, какую способен адресовать 4-уровневый пейджинг: 128 ТиБ.
pub const KERNEL_OFFSET: usize = 0xFFFF_8000_0000_0000;

/// Веха 87 — окно ОБРАЗА ядра (в терминах Linux — `__START_KERNEL_map`): последние 2 ГиБ.
///
/// Почему окон два, а не одно, как на riscv: `x86_64-unknown-none` собран с моделью кода
/// `kernel` — знаковая 32-битная адресация относительно −2 ГиБ. Слинковать образ куда-то ещё
/// (например, внутрь direct-map) нельзя, не пересобирая `core` с `code-model=large`; при
/// попытке линковщик валит сборку релокациями `R_X86_64_32S out of range`. Поэтому образ живёт
/// в своём окне (linker-x86_64.ld), а физическая память — в direct-map; ровно как в Linux.
pub const KIMAGE_BASE: usize = 0xFFFF_FFFF_8000_0000;

/// Физический адрес → указатель ядра на него (через direct-map).
#[inline(always)]
pub fn phys_to_virt(pa: usize) -> usize {
    pa.wrapping_add(KERNEL_OFFSET)
}

/// Указатель ядра → физический адрес: обратная к [`phys_to_virt`] для адресов из direct-map
/// И к раскладке линкера для символов самого образа (статики, стеки, буферы в `.bss`) — обе
/// разновидности встречаются как источник DMA-адресов, поэтому окно распознаётся по адресу.
///
/// Ветка однозначна: адрес ≥ [`KIMAGE_BASE`] означал бы физику ≥ 128 ТиБ − 2 ГиБ, а столько
/// RAM не бывает (и `ram_limit` такого не отдаст). MMIO-окна сюда не относятся — они остались
/// тождественными, и физический адрес устройства и так равен его виртуальному.
#[inline(always)]
pub fn virt_to_phys(va: usize) -> usize {
    if va >= KIMAGE_BASE {
        va - KIMAGE_BASE
    } else {
        va.wrapping_sub(KERNEL_OFFSET)
    }
}

// ─── часы и случайность (Веха 86) ───────────────────────────────────────────

/// Настенное время от прошивки — CMOS RTC, наносекунды Unix. `None` — часов нет.
/// Читается ОДИН раз на загрузке ([`crate::clock::init`]): дальше время идёт от монотонного
/// счётчика, а не от повторных походов в CMOS (они медленные — порты).
pub fn wall_clock_unix_ns() -> Option<u64> {
    rtc::unix_seconds().map(|s| s * 1_000_000_000)
}

/// Аппаратная случайность — `RDRAND` (Ivy Bridge и новее; QEMU TCG её предоставляет).
/// `None` — инструкции нет или чип не дал числа за отведённые попытки (тогда общий код
/// [`crate::random`] мешает энтропию сам).
pub fn hw_random_u64() -> Option<u64> {
    // CPUID.01H:ECX[30] — поддержка RDRAND. Лист 1 есть на любом x86_64.
    let ecx: u32;
    unsafe {
        core::arch::asm!(
            "push rbx", "cpuid", "pop rbx",
            inout("eax") 1u32 => _,
            out("ecx") ecx,
            out("edx") _,
            options(nostack),
        );
    }
    if ecx & (1 << 30) == 0 {
        return None;
    }
    // Спецификация Intel рекомендует до 10 попыток: CF=0 значит «энтропии сейчас нет».
    for _ in 0..10 {
        let v: u64;
        let ok: u8;
        unsafe {
            core::arch::asm!(
                "rdrand {v}",
                "setc {ok}",
                v = out(reg) v,
                ok = out(reg_byte) ok,
                options(nomem, nostack),
            );
        }
        if ok != 0 {
            return Some(v);
        }
    }
    None
}

// ─── разное ─────────────────────────────────────────────────────────────────

/// `e_machine` программ этого ядра (EM_X86_64) — с Вехи 26 программы собираются
/// под обе архитектуры и сеются под арх-корни `bin/<arch>/<имя>`.
pub const ELF_MACHINE: u16 = 62;

/// Веха 101 — выключение по-настоящему: ACPI (`_S5_` из DSDT → порт PM1), затем порты
/// гипервизоров, и только если ничего не сработало — честная остановка процессора. До этого
/// здесь был сразу `cli; hlt`, и «выключить» приходилось кнопкой.
pub fn power_off() -> ! {
    acpi::try_power_off();
    acpi::try_hypervisor_ports();
    loop {
        unsafe { core::arch::asm!("cli", "hlt", options(nomem, nostack)) }
    }
}

