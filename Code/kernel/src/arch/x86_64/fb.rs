//! Пиксельный фреймбуфер и консоль по нему (Веха 96) — выход из потолка 256 глифов.
//!
//! Текстовый режим VGA ([`super::vga`]) держит ровно столько символов, сколько влезает в
//! знакогенератор: 256 глифов по 16 байт ([`super::font`]). Это потолок РЕЖИМА, а не нехватка
//! кода — поэтому под настоящие шрифты нужен пиксель ([[0014-terminal-ereb]]).
//!
//! Буфер даёт **GRUB**: в заголовок multiboot2 ([`super::entry`] — `entry.s`) добавлен тег-запрос
//! режима (type 5), а загрузчик ставит его через VBE и возвращает инфо-тег 8 с адресом, шагом
//! строки и раскладкой цвета. Ни GOP, ни VESA-вызовов из long mode, ни DRM.
//!
//! Три решения, продиктованные железом:
//!
//! - **Адрес фреймбуфера — MMIO, а не RAM.** Он лежит ВЫШЕ карты памяти (у QEMU-stdvga
//!   `0xFD00_0000`), а direct-map стелется только по регионам RAM, поэтому окно отображается
//!   тождественно (`paging::map_mmio`, как BAR'ы PCI) и адрес используется как VA. До
//!   `mm_enable` работают таблицы трамплина (первые 4 ГиБ тождественно) — поэтому печатать
//!   можно с самого начала, но буфер выше 4 ГиБ мы отвергаем ([`init`]).
//! - **Фреймбуфер НИКОГДА не читается.** Прошивка обычно помечает его UC/WC, и чтение оттуда
//!   на порядки медленнее записи: скролл «сдвинуть кадр вверх» стоил бы сотни миллисекунд на
//!   строку. Вместо этого ядро держит **теневой текст** ([`SH_CH`]/[`SH_AT`]) и при скролле
//!   перерисовывает экран из него — только записи.
//! - **Курсор — подчёркивание, стираемое перерисовкой ячейки** из той же тени. Инверсия была бы
//!   короче, но она требует чтения.
//!
//! Кириллица берётся тем же CP866-шрифтом, что и в текстовом режиме: перевод UTF-8 → CP866
//! остался в [`super::vga`], сюда приходит уже байт. ANSI-разбор тоже общий — этот модуль
//! знает только про ячейки и пиксели.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::font::CP866_8X16;

/// Размер знакоместа — размер глифа в [`super::font`].
pub const CELL_W: usize = 8;
pub const CELL_H: usize = 16;

/// Потолок теневого буфера: 256×96 знакомест хватает на 2048×1536 (48 КиБ `.bss`). Режим больше
/// не отвергается — консоль просто занимает левый верхний угол.
const MAX_COLS: usize = 256;
const MAX_ROWS: usize = 96;
const MAX_CELLS: usize = MAX_COLS * MAX_ROWS;

/// Фреймбуфер выше этой границы не поддерживаем: до `mm_enable` мы живём на таблицах трамплина,
/// а те покрывают ровно первые 4 ГиБ (см. `entry.s`).
const FOUR_GIB: usize = 4 * 1024 * 1024 * 1024;

static PRESENT: AtomicBool = AtomicBool::new(false);
static BASE: AtomicUsize = AtomicUsize::new(0);
static PITCH: AtomicUsize = AtomicUsize::new(0);
static PIX_W: AtomicUsize = AtomicUsize::new(0);
static PIX_H: AtomicUsize = AtomicUsize::new(0);
/// Байт на пиксель (4 / 3 / 2) — из `bpp` инфо-тега.
static BYTES_PP: AtomicUsize = AtomicUsize::new(0);
static COLS: AtomicUsize = AtomicUsize::new(0);
static ROWS: AtomicUsize = AtomicUsize::new(0);

/// Раскладка цвета из инфо-тега: позиция младшего бита поля и ширина маски, для R/G/B.
/// Не зашиваем `0x00RRGGBB` — VBE-режимы бывают и 16-битными (5-6-5).
static RGB_POS: [AtomicUsize; 3] = [AtomicUsize::new(16), AtomicUsize::new(8), AtomicUsize::new(0)];
static RGB_SIZE: [AtomicUsize; 3] = [AtomicUsize::new(8), AtomicUsize::new(8), AtomicUsize::new(8)];

/// Теневой текст экрана: символ (CP866) и атрибут VGA на знакоместо. Единственный источник
/// правды для перерисовки — фреймбуфер не читаем никогда (см. преамбулу).
static mut SH_CH: [u8; MAX_CELLS] = [b' '; MAX_CELLS];
static mut SH_AT: [u8; MAX_CELLS] = [0x07; MAX_CELLS];

/// Где нарисован курсор сейчас (`None` — не нарисован). Нужен, чтобы стереть старый.
static mut CURSOR: Option<(usize, usize)> = None;

/// Палитра VGA (16 цветов) → RGB. Индекс — младшие 4 бита атрибута (бит 3 = яркость).
const PALETTE: [(u8, u8, u8); 16] = [
    (0x00, 0x00, 0x00), // 0 чёрный
    (0x00, 0x00, 0xAA), // 1 синий
    (0x00, 0xAA, 0x00), // 2 зелёный
    (0x00, 0xAA, 0xAA), // 3 голубой
    (0xAA, 0x00, 0x00), // 4 красный
    (0xAA, 0x00, 0xAA), // 5 пурпурный
    (0xAA, 0x55, 0x00), // 6 коричневый
    (0xAA, 0xAA, 0xAA), // 7 светло-серый
    (0x55, 0x55, 0x55), // 8 тёмно-серый
    (0x55, 0x55, 0xFF), // 9 яркий синий
    (0x55, 0xFF, 0x55), // 10 яркий зелёный
    (0x55, 0xFF, 0xFF), // 11 яркий голубой
    (0xFF, 0x55, 0x55), // 12 яркий красный
    (0xFF, 0x55, 0xFF), // 13 яркий пурпурный
    (0xFF, 0xFF, 0x55), // 14 жёлтый
    (0xFF, 0xFF, 0xFF), // 15 белый
];

/// Раскладка цвета одного канала: (позиция младшего бита, ширина маски).
pub struct RgbFields {
    pub red: (u8, u8),
    pub green: (u8, u8),
    pub blue: (u8, u8),
}

/// Принять фреймбуфер от загрузчика. `base` — ФИЗИЧЕСКИЙ адрес (он же VA: окно тождественное).
/// Возвращает `false`, если режим нам не по зубам, — тогда консоль остаётся текстовой.
///
/// Зовётся ДО первого вывода (из [`super::console_init`]) и до `mm_init`, поэтому опирается
/// только на таблицы трамплина.
pub fn init(base: usize, pitch: usize, width: usize, height: usize, bpp: u8, rgb: RgbFields) -> bool {
    let bytes_pp = match bpp {
        32 => 4,
        24 => 3,
        16 => 2,
        _ => return false, // 8-битные палитровые режимы не поддерживаем — палитру не грузим
    };
    if base == 0 || width < CELL_W || height < CELL_H || pitch < width * bytes_pp {
        return false;
    }
    // Окно целиком обязано лежать ниже 4 ГиБ: см. FOUR_GIB.
    let bytes = pitch.saturating_mul(height);
    if base.saturating_add(bytes) > FOUR_GIB {
        return false;
    }
    BASE.store(base, Ordering::Relaxed);
    PITCH.store(pitch, Ordering::Relaxed);
    PIX_W.store(width, Ordering::Relaxed);
    PIX_H.store(height, Ordering::Relaxed);
    BYTES_PP.store(bytes_pp, Ordering::Relaxed);
    COLS.store((width / CELL_W).min(MAX_COLS), Ordering::Relaxed);
    ROWS.store((height / CELL_H).min(MAX_ROWS), Ordering::Relaxed);
    for (i, (pos, size)) in [rgb.red, rgb.green, rgb.blue].iter().enumerate() {
        RGB_POS[i].store(*pos as usize, Ordering::Relaxed);
        RGB_SIZE[i].store((*size).clamp(1, 8) as usize, Ordering::Relaxed);
    }
    PRESENT.store(true, Ordering::Relaxed);
    true
}

/// Есть ли пиксельный режим (иначе работает текстовый VGA). Отвечает за ГЕОМЕТРИЮ, а не за
/// право рисовать — см. [`owned_by_user`].
#[inline]
pub fn present() -> bool {
    PRESENT.load(Ordering::Relaxed)
}

/// Веха 97 — экран отдан процессу (тот замапил окно через `SYS_MMIO_MAP`).
///
/// **Правило владения экраном.** Пиксели один, а рисовать хотят двое: ядро (`println!`) и
/// терминал. Делить их нечем — оверлея у нас нет, — поэтому владелец ровно один: как только
/// процесс получил окно, ядро перестаёт рисовать и уходит в serial. Исключение одно —
/// **паника забирает экран обратно** ([`take_back`]): замерший терминал без объяснения хуже,
/// чем испорченная картинка.
/// Хранится `pid + 1` (0 = экран у ядра): владельца надо знать поимённо, иначе его смерть
/// оставила бы экран навсегда занятым — ядро молчало бы в мёртвый терминал.
static USER_OWNED: AtomicUsize = AtomicUsize::new(0);

/// Экран у процесса?
#[inline]
pub fn owned_by_user() -> bool {
    USER_OWNED.load(Ordering::Relaxed) != 0
}

/// Кто владеет экраном (`None` — ядро).
pub fn owner() -> Option<usize> {
    match USER_OWNED.load(Ordering::Relaxed) {
        0 => None,
        n => Some(n - 1),
    }
}

/// Отдать экран процессу (зовётся из `SYS_MMIO_MAP`, когда замаплено окно фреймбуфера).
pub fn give_to_user(pid: usize) {
    USER_OWNED.store(pid + 1, Ordering::Relaxed);
}

/// Забрать экран ядру и очистить его: путь паники и путь смерти владельца.
pub fn take_back() {
    if present() {
        USER_OWNED.store(0, Ordering::Relaxed);
        unsafe { clear(0x07) };
    }
}

/// Полное описание режима для программы: `(ширина, высота, шаг строки, бит/пиксель,
/// [(позиция, ширина маски); R, G, B])`. Отдаётся через `SYS_VIDEO_INFO`.
pub fn info() -> (usize, usize, usize, usize, [(u8, u8); 3]) {
    let mut rgb = [(0u8, 0u8); 3];
    for (i, slot) in rgb.iter_mut().enumerate() {
        *slot = (
            RGB_POS[i].load(Ordering::Relaxed) as u8,
            RGB_SIZE[i].load(Ordering::Relaxed) as u8,
        );
    }
    (
        PIX_W.load(Ordering::Relaxed),
        PIX_H.load(Ordering::Relaxed),
        PITCH.load(Ordering::Relaxed),
        BYTES_PP.load(Ordering::Relaxed) * 8,
        rgb,
    )
}

/// Геометрия консоли в знакоместах.
#[inline]
pub fn cols() -> usize {
    COLS.load(Ordering::Relaxed)
}
#[inline]
pub fn rows() -> usize {
    ROWS.load(Ordering::Relaxed)
}

/// Окно фреймбуфера `(физ. база, длина в байтах)` — для отображения в таблицы ядра
/// (`paging::mm_init`) и для отчёта на загрузке. `None` — пиксельного режима нет.
pub fn window() -> Option<(usize, usize)> {
    present().then(|| {
        (
            BASE.load(Ordering::Relaxed),
            PITCH.load(Ordering::Relaxed) * PIX_H.load(Ordering::Relaxed),
        )
    })
}

/// Размеры в пикселях и глубина — для баннера загрузки.
pub fn geometry() -> (usize, usize, usize) {
    (
        PIX_W.load(Ordering::Relaxed),
        PIX_H.load(Ordering::Relaxed),
        BYTES_PP.load(Ordering::Relaxed) * 8,
    )
}

/// Цвет палитры → машинное слово пикселя по раскладке инфо-тега.
fn encode(idx: usize) -> u32 {
    let (r, g, b) = PALETTE[idx & 0x0f];
    let mut out = 0u32;
    for (i, chan) in [r, g, b].iter().enumerate() {
        let size = RGB_SIZE[i].load(Ordering::Relaxed);
        let pos = RGB_POS[i].load(Ordering::Relaxed);
        // Канал у нас 8-битный: сузить до ширины поля, затем поставить на место.
        out |= ((*chan as u32) >> (8 - size)) << pos;
    }
    out
}

/// Записать пиксель по смещению `off` от базы. Только запись — фреймбуфер не читаем.
///
/// # Safety
/// `off` — внутри окна фреймбуфера.
#[inline]
unsafe fn put_pixel(off: usize, color: u32) {
    let p = BASE.load(Ordering::Relaxed) + off;
    match BYTES_PP.load(Ordering::Relaxed) {
        4 => core::ptr::write_volatile(p as *mut u32, color),
        2 => core::ptr::write_volatile(p as *mut u16, color as u16),
        _ => {
            // 24 бита: три байта, порядок — младший первым (little-endian машина).
            core::ptr::write_volatile(p as *mut u8, color as u8);
            core::ptr::write_volatile((p + 1) as *mut u8, (color >> 8) as u8);
            core::ptr::write_volatile((p + 2) as *mut u8, (color >> 16) as u8);
        }
    }
}

/// Нарисовать знакоместо: глиф `ch` (CP866) атрибутом `attr`. Тень НЕ трогает — её ведёт
/// [`put_cell`], а перерисовка после скролла зовёт эту функцию напрямую.
///
/// # Safety
/// Координаты внутри геометрии; фреймбуфер инициализирован.
unsafe fn blit(row: usize, col: usize, ch: u8, attr: u8) {
    let fg = encode((attr & 0x0f) as usize);
    let bg = encode(((attr >> 4) & 0x07) as usize);
    let bytes_pp = BYTES_PP.load(Ordering::Relaxed);
    let pitch = PITCH.load(Ordering::Relaxed);
    let glyph = &CP866_8X16[ch as usize * CELL_H..][..CELL_H];
    let mut line = (row * CELL_H) * pitch + (col * CELL_W) * bytes_pp;
    for &bits in glyph {
        let mut off = line;
        for gx in 0..CELL_W {
            put_pixel(off, if bits & (0x80 >> gx) != 0 { fg } else { bg });
            off += bytes_pp;
        }
        line += pitch;
    }
}

/// Положить символ в знакоместо: обновить тень и нарисовать.
///
/// # Safety
/// Зовётся из консоли с выключенными прерываниями (машина однопроцессорная) — гонок за тень нет.
pub unsafe fn put_cell(row: usize, col: usize, ch: u8, attr: u8) {
    if row >= rows() || col >= cols() {
        return;
    }
    let i = row * cols() + col;
    SH_CH[i] = ch;
    SH_AT[i] = attr;
    blit(row, col, ch, attr);
}

/// Очистить экран атрибутом `attr` (фон берётся из него же).
///
/// # Safety
/// Как [`put_cell`].
pub unsafe fn clear(attr: u8) {
    for i in 0..cols() * rows() {
        SH_CH[i] = b' ';
        SH_AT[i] = attr;
    }
    CURSOR = None;
    repaint();
}

/// Сдвинуть экран на строку вверх, очистив последнюю атрибутом `attr`.
///
/// Дороже, чем сдвиг кадра памятью, но НЕ читает фреймбуфер (см. преамбулу): двигаем тень и
/// перерисовываем.
///
/// # Safety
/// Как [`put_cell`].
pub unsafe fn scroll(attr: u8) {
    let (c, r) = (cols(), rows());
    // Через сырые указатели, а не срезом: `&mut` на mutable static — UB-ловушка (и предупреждение
    // rust_2024_compatibility), хотя гонок тут нет по построению.
    core::ptr::copy((&raw mut SH_CH as *mut u8).add(c), &raw mut SH_CH as *mut u8, c * (r - 1));
    core::ptr::copy((&raw mut SH_AT as *mut u8).add(c), &raw mut SH_AT as *mut u8, c * (r - 1));
    for i in c * (r - 1)..c * r {
        SH_CH[i] = b' ';
        SH_AT[i] = attr;
    }
    CURSOR = None; // курсор уехал вместе с кадром — нарисуется заново
    repaint();
}

/// Перерисовать весь экран из тени.
///
/// # Safety
/// Как [`put_cell`].
unsafe fn repaint() {
    for row in 0..rows() {
        for col in 0..cols() {
            let i = row * cols() + col;
            blit(row, col, SH_CH[i], SH_AT[i]);
        }
    }
}

/// Поставить курсор в `(row, col)`: подчёркивание в две пиксельные строки цветом символа.
/// Прошлый курсор стирается перерисовкой его знакоместа из тени.
///
/// # Safety
/// Как [`put_cell`].
pub unsafe fn cursor(row: usize, col: usize) {
    if let Some((pr, pc)) = CURSOR {
        if (pr, pc) != (row, col) && pr < rows() && pc < cols() {
            let i = pr * cols() + pc;
            blit(pr, pc, SH_CH[i], SH_AT[i]);
        }
    }
    if row >= rows() || col >= cols() {
        CURSOR = None;
        return;
    }
    let i = row * cols() + col;
    let fg = encode((SH_AT[i] & 0x0f) as usize);
    let bytes_pp = BYTES_PP.load(Ordering::Relaxed);
    let pitch = PITCH.load(Ordering::Relaxed);
    for gy in CELL_H - 2..CELL_H {
        let mut off = (row * CELL_H + gy) * pitch + (col * CELL_W) * bytes_pp;
        for _ in 0..CELL_W {
            put_pixel(off, fg);
            off += bytes_pp;
        }
    }
    CURSOR = Some((row, col));
}
