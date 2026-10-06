//! virtio-gpu — экран от гипервизора, первая ступень лестницы [[0018-gpu-ladder]] (Веха 224).
//!
//! ## Зачем он, если экран уже есть
//!
//! Экран сейчас даёт GRUB: тег multiboot2 отдаёт адрес линейного буфера, и композитор пишет
//! прямо в него ([`crate::arch::x86_64::fb`]). Этого хватает, чтобы РИСОВАТЬ, и не хватает,
//! чтобы ПОКАЗЫВАТЬ: у линейного буфера нет ни «кадр готов», ни переключения страницы, ни
//! вертикального гашения. Пока кадр блитится, развёртка идёт своим ходом — отсюда рвань,
//! записанная в последствиях ADR 0018 как принятая сознательно.
//!
//! virtio-gpu даёт ту самую недостающую семантику: драйвер объявляет устройству область памяти
//! (`ATTACH_BACKING`), пишет в неё как в обычный буфер, а показ — отдельная команда
//! (`TRANSFER_TO_HOST_2D` + `RESOURCE_FLUSH`). Кадр попадает на экран целиком или не попадает
//! вовсе. Это и есть «кадр готов → покажи», ради которого ADR ставит virtio-gpu первой ступенью:
//! на ней проверяется композитор, и на неё же потом ложится свой дисплейный контроллер.
//!
//! **И второе, не менее важное: на riscv экрана не было вовсе.** QEMU virt не даёт ни VBE, ни
//! тега от загрузчика — графика там жила только в планах. Один и тот же драйвер закрывает обе
//! дыры, и это ровно тот довод, по которому ADR ставит virtio-gpu раньше своего драйвера:
//! она окупается дважды.
//!
//! ## Чего здесь НЕТ
//!
//! 3D (`VIRGL`), курсорной очереди, нескольких экранов и смены режима на ходу. Устройство всё
//! это умеет, но нужны они не сейчас: первая ступень — это «есть экран и есть показ кадра».
//! Курсор рисует композитор (он и так свой слой), а второй экран без раскладки столов по
//! мониторам смысла не имеет.
//!
//! ## Как устроен разговор
//!
//! Одна очередь (`controlq`), и каждая команда — цепочка из двух дескрипторов: читаемый с
//! запросом и пишущий под ответ. Ответ всегда начинается заголовком, и по его типу видно,
//! приняло устройство команду или нет. Ждём завершения в used-кольце, как [`crate::virtio_rng`]:
//! команд за кадр единицы, прерывание тут дороже опроса.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

extern crate alloc;

use crate::arch;
use crate::frame;
use crate::sync::SpinLock;
use crate::virtio::{Cfg, Queue, DESC_F_NEXT, DESC_F_WRITE};

/// Длина очереди. Команд в полёте у нас всегда одна — ждём завершения каждой, — но кольцо
/// короче четырёх неудобно по выравниванию.
const QSIZE: usize = 8;

// ── команды протокола (2D-подмножество) ──────────────────────────────────────

const CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const CMD_SET_SCANOUT: u32 = 0x0103;
const CMD_RESOURCE_FLUSH: u32 = 0x0104;
const CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;

const RESP_OK_NODATA: u32 = 0x1100;
const RESP_OK_DISPLAY_INFO: u32 = 0x1101;

/// Раскладка пикселя. `B8G8R8X8` значит байты B,G,R,X — то есть little-endian слово
/// `0x00RRGGBB`, ровно то, что собирает композитор (`pack` с позициями 16/8/0). Берём её, чтобы
/// между форматом устройства и форматом тулкита не было преобразования на каждый пиксель.
const FORMAT_B8G8R8X8: u32 = 2;

/// Наш единственный ресурс и единственный экран.
const RES_ID: u32 = 1;
const SCANOUT_ID: u32 = 0;

/// Размер по умолчанию, если устройство не назвало своего. QEMU отдаёт 1280×800 сам, но
/// отсутствие ответа не повод остаться без экрана.
const DEF_W: u32 = 1280;
const DEF_H: u32 = 800;

/// Потолок режима. Не каприз: кадр живёт в НЕПРЕРЫВНОЙ памяти ядра (`alloc_contig`), и просить
/// у аллокатора десятки мегабайт подряд на машине с 1 ГиБ — верный способ не получить ничего.
const MAX_W: u32 = 1920;
const MAX_H: u32 = 1200;

/// Смещения буферов в служебной странице: запрос и ответ врозь, чтобы устройство не писало
/// поверх того, что читает.
const REQ_OFF: usize = 0;
const RESP_OFF: usize = 1024;
/// Длина заголовка команды. ДВАДЦАТЬ ЧЕТЫРЕ байта: `type`, `flags`, `fence_id` (восемь),
/// `ctx_id`, `padding`. Считать его тридцатидвухбайтным — ошибка, которая стоила первого
/// прогона: все поля уезжают на восемь байт, и устройство честно отвечает
/// «неверный идентификатор ресурса» (`0x1203`), потому что читает нули.
const HDR: usize = 24;
/// Ответ `GET_DISPLAY_INFO` — заголовок плюс шестнадцать режимов по 24 байта.
const RESP_MAX: usize = HDR + 16 * 24;

struct Gpu {
    q: Queue,
    /// Служебная страница: физический адрес и адрес, по которому в неё пишет ядро.
    page_pa: usize,
    page_va: usize,
    w: u32,
    h: u32,
}

/// Заголовок команды — [`HDR`] байт, одинаковых у всех запросов и ответов.
fn hdr(buf: &mut [u8], cmd: u32) {
    buf[..HDR].fill(0);
    buf[0..4].copy_from_slice(&cmd.to_le_bytes());
}

fn put32(buf: &mut [u8], at: usize, v: u32) {
    buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn put64(buf: &mut [u8], at: usize, v: u64) {
    buf[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

impl Gpu {
    /// Срез служебной страницы под запрос.
    fn req(&self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut((self.page_va + REQ_OFF) as *mut u8, 1024) }
    }

    /// Выполнить команду: `len` байт запроса уже лежат в [`Gpu::req`]. Возвращает тип ответа.
    ///
    /// Цепочка из двух дескрипторов — читаемый и пишущий, — и ожидание в used-кольце. Потолок
    /// оборотов тот же, что у [`crate::virtio_rng`]: молчащее устройство не должно подвешивать
    /// загрузку, экран важен, но не ценой мёртвой машины.
    fn cmd(&mut self, len: usize) -> u32 {
        unsafe {
            // Дескрипторы ВСЕГДА нулевой и первый — как у блока ([`crate::virtio_blk`]). Это не
            // упрощение, а следствие модели: команда в полёте одна, её завершения мы тут же и
            // ждём, поэтому крутить слоты по кругу незачем. Счётчик кольца доступных при этом
            // идёт своим ходом — его двигает `offer`.
            self.q.set_desc(0, (self.page_pa + REQ_OFF) as u64, len as u32, DESC_F_NEXT, 1);
            self.q.set_desc(1, (self.page_pa + RESP_OFF) as u64, RESP_MAX as u32, DESC_F_WRITE, 0);
            // Ответ затираем заранее: иначе «устройство промолчало» неотличимо от «ответило то
            // же, что в прошлый раз», и ошибка выглядит как успех.
            core::ptr::write_bytes((self.page_va + RESP_OFF) as *mut u8, 0, RESP_MAX);
            self.q.offer(0);
            self.q.kick();

            let mut spins = 0u32;
            while !self.q.has_used() {
                spins += 1;
                if spins > 50_000_000 {
                    return 0;
                }
                core::hint::spin_loop();
            }
            self.q.take_used();
            core::ptr::read_volatile((self.page_va + RESP_OFF) as *const u32)
        }
    }

    /// Спросить устройство о режиме первого экрана. `None` — не ответило или экран выключен.
    fn display_info(&mut self) -> Option<(u32, u32)> {
        hdr(self.req(), CMD_GET_DISPLAY_INFO);
        if self.cmd(HDR) != RESP_OK_DISPLAY_INFO {
            return None;
        }
        // pmodes[0] = { rect{x,y,w,h}, enabled, flags } сразу за заголовком.
        let at = self.page_va + RESP_OFF + HDR;
        let rd = |i: usize| unsafe { core::ptr::read_volatile((at + i * 4) as *const u32) };
        let (w, h, enabled) = (rd(2), rd(3), rd(4));
        (enabled != 0 && w > 0 && h > 0).then_some((w, h))
    }
}

static GPU: SpinLock<Option<Gpu>> = SpinLock::new(None);
static PRESENT: AtomicBool = AtomicBool::new(false);
/// Физический адрес кадра и его размеры — чтобы отдавать окно наружу без взятия замка.
static FB_PA: AtomicUsize = AtomicUsize::new(0);
static FB_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Поднять первое найденное virtio-gpu и сделать его экраном системы.
///
/// `false` — устройства нет либо оно не договорилось. Это не ошибка: на машине с фреймбуфером
/// от загрузчика virtio-gpu просто не нужен, а на riscv без него графики и не было.
pub fn init() -> bool {
    // Отказы здесь ГОВОРЯЩИЕ, и это не отладочный мусор. Экран, который «просто не появился», —
    // худший вид поломки: система выглядит живой, а посмотреть на неё нечем, и причина молчит.
    fn no(why: &str) -> bool {
        crate::println!("  [gpu]  virtio-gpu не поднялась: {}", why);
        false
    }
    let Some(transport) = arch::probe_virtio_gpu() else {
        // Это НЕ ошибка и не печатается: устройства на шине просто нет.
        return false;
    };
    let cfg = Cfg::new(transport);
    cfg.begin();
    // Своих feature-битов 2D-подмножеству не нужно: `VIRGL` это 3D, `EDID` — чтение монитора.
    if !cfg.accept_features(0) {
        return no("устройство не приняло набор возможностей");
    }
    cfg.no_config_msix();
    // Очередь 0 — `controlq`. Очередь 1 (`cursorq`) не заводим: курсор рисует композитор.
    let Some(q) = cfg.queue("virtio-gpu", 0, QSIZE, None) else {
        return no("не завелась очередь команд");
    };
    cfg.ready();

    let Some(page_pa) = frame::alloc() else {
        return no("нет страницы под служебный буфер");
    };
    let mut gpu = Gpu { q, page_pa, page_va: arch::phys_to_virt(page_pa), w: DEF_W, h: DEF_H };

    if let Some((w, h)) = gpu.display_info() {
        gpu.w = w.min(MAX_W);
        gpu.h = h.min(MAX_H);
    }
    let (w, h) = (gpu.w, gpu.h);
    let pitch = w as usize * 4;
    let bytes = pitch * h as usize;

    // 1. Ресурс на стороне устройства.
    {
        let r = gpu.req();
        hdr(r, CMD_RESOURCE_CREATE_2D);
        put32(r, HDR, RES_ID);
        put32(r, HDR + 4, FORMAT_B8G8R8X8);
        put32(r, HDR + 8, w);
        put32(r, HDR + 12, h);
    }
    let r = gpu.cmd(HDR + 16);
    if r != RESP_OK_NODATA {
        return no(&alloc::format!("CREATE_2D отвечено {:#x}", r));
    }

    // 2. Память под кадр — НЕПРЕРЫВНАЯ, одним куском. Протокол разрешает список кусков, но
    //    линейный буфер нужен не только устройству: по нему рисует консоль ядра и в него же
    //    смотрит композитор, замапив окно. Разрывный кадр потребовал бы и там, и там считать
    //    адрес каждой строки отдельно — ради чего, кроме удобства аллокатора, неясно.
    let pages = bytes.div_ceil(4096);
    let Some(fb_pa) = frame::alloc_contig(pages) else {
        return no(&alloc::format!("не нашлось {} КиБ подряд под кадр", bytes / 1024));
    };
    {
        let r = gpu.req();
        hdr(r, CMD_RESOURCE_ATTACH_BACKING);
        put32(r, HDR, RES_ID);
        put32(r, HDR + 4, 1); // ровно один кусок — см. выше
        put64(r, HDR + 8, fb_pa as u64);
        put32(r, HDR + 16, bytes as u32);
        put32(r, HDR + 20, 0);
    }
    let r = gpu.cmd(HDR + 24);
    if r != RESP_OK_NODATA {
        return no(&alloc::format!("ATTACH_BACKING отвечено {:#x}", r));
    }

    // 3. Показать этот ресурс на экране.
    {
        let r = gpu.req();
        hdr(r, CMD_SET_SCANOUT);
        put32(r, HDR, 0); // x
        put32(r, HDR + 4, 0); // y
        put32(r, HDR + 8, w);
        put32(r, HDR + 12, h);
        put32(r, HDR + 16, SCANOUT_ID);
        put32(r, HDR + 20, RES_ID);
    }
    let r = gpu.cmd(HDR + 24);
    if r != RESP_OK_NODATA {
        return no(&alloc::format!("SET_SCANOUT отвечено {:#x}", r));
    }

    FB_PA.store(fb_pa, Ordering::Relaxed);
    FB_BYTES.store(bytes, Ordering::Relaxed);
    *GPU.lock() = Some(gpu);
    PRESENT.store(true, Ordering::Relaxed);
    crate::println!("  [gpu] virtio-gpu: экран {}×{}, кадр {} КиБ", w, h, bytes / 1024);

    // Кадр отдаётся общей части видео тем же способом, что буфер от загрузчика: адрес, шаг,
    // раскладка. Дальше консоль и композитор не знают, откуда он взялся, — и это главное
    // следствие этой вехи.
    arch::video_adopt(arch::phys_to_virt(fb_pa), fb_pa, pitch, w as usize, h as usize)
}

/// Есть ли рабочий virtio-gpu.
pub fn present() -> bool {
    PRESENT.load(Ordering::Relaxed)
}

/// Окно кадра `(физический адрес, байт)` — для выдачи процессу под capability.
pub fn window() -> Option<(usize, usize)> {
    present().then(|| (FB_PA.load(Ordering::Relaxed), FB_BYTES.load(Ordering::Relaxed)))
}

/// ПОКАЗАТЬ прямоугольник: перенести его в ресурс устройства и обновить им экран.
///
/// Две команды, а не одна, и это не лишняя работа: `TRANSFER` говорит «возьми из моей памяти»,
/// `FLUSH` — «покажи взятое». Разделение и даёт ту самую целость кадра, ради которой устройство
/// здесь: пока идёт перенос, на экране остаётся прошлый кадр, а не половина нового.
///
/// Пустой прямоугольник пропускается молча: звать устройство ради нуля точек — чистая задержка.
pub fn flush(x: u32, y: u32, w: u32, h: u32) {
    if w == 0 || h == 0 {
        return;
    }
    let mut g = GPU.lock_irq();
    let Some(gpu) = g.as_mut() else { return };
    let (sw, sh) = (gpu.w, gpu.h);
    if x >= sw || y >= sh {
        return;
    }
    let (w, h) = (w.min(sw - x), h.min(sh - y));
    let offset = (y as u64 * sw as u64 + x as u64) * 4;
    {
        let r = gpu.req();
        hdr(r, CMD_TRANSFER_TO_HOST_2D);
        put32(r, HDR, x);
        put32(r, HDR + 4, y);
        put32(r, HDR + 8, w);
        put32(r, HDR + 12, h);
        put64(r, HDR + 16, offset);
        put32(r, HDR + 24, RES_ID);
        put32(r, HDR + 28, 0);
    }
    if gpu.cmd(HDR + 32) != RESP_OK_NODATA {
        return;
    }
    {
        let r = gpu.req();
        hdr(r, CMD_RESOURCE_FLUSH);
        put32(r, HDR, x);
        put32(r, HDR + 4, y);
        put32(r, HDR + 8, w);
        put32(r, HDR + 12, h);
        put32(r, HDR + 16, RES_ID);
        put32(r, HDR + 20, 0);
    }
    gpu.cmd(HDR + 24);
}

/// Показать экран целиком — для консоли ядра, которой считать области незачем.
pub fn flush_all() {
    let (w, h) = {
        let g = GPU.lock_irq();
        match g.as_ref() {
            Some(gpu) => (gpu.w, gpu.h),
            None => return,
        }
    };
    flush(0, 0, w, h);
}
