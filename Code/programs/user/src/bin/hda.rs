//! `hda` — ЗВУК: свой драйвер Intel High Definition Audio, вывод (Веха 202).
//!
//! ## Почему свой, а не хостируемый Linux
//!
//! Весь сетевой класс мы получили хостингом: шим `lx-linux` эмулирует API ядра Linux, и в него
//! въезжают настоящие `atl1c`, `e1000`, `8139too` без единой правки. Для звука тот же приём
//! обошёлся бы ДОРОЖЕ своей реализации: драйвер HDA в Linux (`snd-hda-intel`) — это не
//! самостоятельная программа, а житель подсистемы ALSA, и без `snd_core`, `pcm`, `control`,
//! `timer`, `hwdep` он не существует вовсе. Пришлось бы втащить подсистему ради того, что
//! помещается в эту тысячу строк.
//!
//! Спецификация HDA открытая, и вывод звука в ней устроен просто: кодек отвечает на команды,
//! буфер в памяти описан списком кусков (BDL), контроллер обходит его сам и отдаёт кодеку.
//!
//! ## Почему опросом, а не по прерыванию
//!
//! Прерывание нам сейчас не выдают, и это осознанно: линию INTx ядро умеет завести ровно
//! ОДНОМУ устройству (`intx_irq_setup` глушит её всем прочим — иначе разделяемая линия
//! устраивает шторм, Веха 195), и занята она сетевой картой. Вывод звука прекрасно живёт
//! опросом: буфер кольцевой, контроллер идёт по нему сам, а нам достаточно изредка смотреть,
//! где он, и дописывать впереди. Так же у нас работают AHCI, NVMe и xHCI.
//!
//! ## Как проверять
//!
//! `VOID_QEMU_SND=wav:/tmp/out.wav` — всё сыгранное пишется в файл, который видно глазами и
//! можно измерить. Без этого «звук пошёл» проверялось бы ушами на живой машине, то есть ещё
//! одним кругом «пересобрал → флешка → загрузился».

#![no_std]
#![no_main]

extern crate alloc;

use core::ptr::{read_volatile, write_volatile};

use void_user as sys;

/// Куча нужна только печати (`format!`): числа этого драйвера человек читает часто.
#[global_allocator]
static ALLOC: sys::heap::Heap<{ 32 * 1024 }> = sys::heap::Heap::new();

// ── окна в нашем адресном пространстве (USER-регион, ниже кучи) ───────────────────────────────
const MMIO_VA: usize = 0x5000_0000; // регистры контроллера (BAR0, 16 КиБ)
const RING_VA: usize = 0x5100_0000; // CORB + RIRB + BDL — одна страница на всё
const PCM_VA: usize = 0x5110_0000; // сам звук

/// Кольца команд и ответов кладутся в одну страницу: CORB 1 КиБ, RIRB 2 КиБ, BDL следом.
/// Все три требуют выравнивания на 128 байт, а страница выровнена на 4096 — сходится само.
const CORB_OFF: usize = 0;
const RIRB_OFF: usize = 1024;
const BDL_OFF: usize = 3072;
const RING_ENTRIES: usize = 256;

/// Буфер звука: 64 КиБ = треть секунды при 48 кГц / 16 бит / 2 канала. Кольцо, две половины.
const PCM_PAGES: usize = 16;
const PCM_BYTES: usize = PCM_PAGES * 4096;

// ── регистры контроллера ─────────────────────────────────────────────────────────────────────
const GCAP: usize = 0x00;
const GCTL: usize = 0x08;
const STATESTS: usize = 0x0e;
const CORBLBASE: usize = 0x40;
const CORBUBASE: usize = 0x44;
const CORBWP: usize = 0x48;
const CORBRP: usize = 0x4a;
const CORBCTL: usize = 0x4c;
const CORBSIZE: usize = 0x4e;
const RIRBLBASE: usize = 0x50;
const RIRBUBASE: usize = 0x54;
const RIRBWP: usize = 0x58;
const RINTCNT: usize = 0x5a;
const RIRBCTL: usize = 0x5c;
const RIRBSTS: usize = 0x5d;
const RIRBSIZE: usize = 0x5e;

const GCTL_CRST: u32 = 1 << 0;
const CORBRPRST: u16 = 1 << 15;
const DMA_RUN: u8 = 1 << 1;

/// Дескриптор потока: первый ВЫХОДНОЙ лежит после всех входных, а сколько их — говорит GCAP.
/// Смещения внутри дескриптора.
const SD_CTL: usize = 0x00;
const SD_STS: usize = 0x03;
const SD_LPIB: usize = 0x04;
const SD_CBL: usize = 0x08;
const SD_LVI: usize = 0x0c;
const SD_FMT: usize = 0x12;
const SD_BDPL: usize = 0x18;
const SD_BDPU: usize = 0x1c;

const SD_CTL_SRST: u32 = 1 << 0;
const SD_CTL_RUN: u32 = 1 << 1;

/// Номер потока. Им связаны две стороны: дескриптор контроллера и конвертер кодека — контроллер
/// кладёт данные «в поток N», кодек берёт их «из потока N». Ноль означает «не назначен».
const STREAM_TAG: u32 = 1;

/// Формат: 48 кГц, 16 бит, два канала. Разложение по битам регистра — база 48 кГц (бит 14 = 0),
/// без умножения и деления, `001` = 16 бит, `0001` = два канала.
const FORMAT: u16 = 0x0011;
const RATE: usize = 48_000;

// ── команды кодеку (verb'ы) ──────────────────────────────────────────────────────────────────
// Команда кодеку — двадцать бит, и делятся они ДВУМЯ способами: 12 бит кода плюс 8 бит
// нагрузки, либо 4 бита кода плюс 16 бит нагрузки. Записываются оба одинаково — код сдвинут на
// восемь, нагрузка ложится в младшие биты, — и потому здесь все коды в одном, спецификационном
// виде, а сдвигает их `cmd`. Забыть этот сдвиг легко, а выглядит это как молчащий кодек:
// контроллер честно забирает команду, честно кладёт ответ, и ответ — ноль.
const VERB_GET_PARAM: u32 = 0xf00;
const VERB_GET_CONN_ENTRY: u32 = 0xf02;
const VERB_GET_CONFIG_DEFAULT: u32 = 0xf1c;
const VERB_SET_STREAM_FORMAT: u32 = 0x200; // 4-битный: нагрузка 16 бит
const VERB_SET_AMP: u32 = 0x300; // тоже 4-битный
const VERB_SET_CONV_STREAM: u32 = 0x706;
const VERB_SET_PIN_CTL: u32 = 0x707;
const VERB_SET_EAPD: u32 = 0x70c;
const VERB_SET_POWER: u32 = 0x705;

const PARAM_NODE_COUNT: u32 = 0x04;
const PARAM_FUNC_TYPE: u32 = 0x05;
const PARAM_WIDGET_CAP: u32 = 0x09;
const PARAM_PIN_CAP: u32 = 0x0c;
const PARAM_AMP_OUT_CAP: u32 = 0x12;
const PARAM_CONN_LEN: u32 = 0x0e;

const WIDGET_DAC: u32 = 0x0;
const WIDGET_MIXER: u32 = 0x2;
const WIDGET_SELECTOR: u32 = 0x3;
const WIDGET_PIN: u32 = 0x4;

const PIN_CTL_OUT_EN: u32 = 1 << 6;
const PIN_CTL_HP_EN: u32 = 1 << 7;

// ── доступ к регистрам ───────────────────────────────────────────────────────────────────────
unsafe fn rd8(off: usize) -> u8 {
    read_volatile((MMIO_VA + off) as *const u8)
}
unsafe fn rd16(off: usize) -> u16 {
    read_volatile((MMIO_VA + off) as *const u16)
}
unsafe fn rd32(off: usize) -> u32 {
    read_volatile((MMIO_VA + off) as *const u32)
}
unsafe fn wr8(off: usize, v: u8) {
    write_volatile((MMIO_VA + off) as *mut u8, v);
}
unsafe fn wr16(off: usize, v: u16) {
    write_volatile((MMIO_VA + off) as *mut u16, v);
}
unsafe fn wr32(off: usize, v: u32) {
    write_volatile((MMIO_VA + off) as *mut u32, v);
}

fn w(s: &str) {
    sys::write(s.as_bytes());
}

/// Дождаться, пока биты `mask` в регистре примут значение `want`. `false` — не дождались.
///
/// Ждём СНОМ, а не пустым циклом: драйвер живёт в обычном процессе, и крутить процессор, пока
/// железо думает свои полмиллисекунды, значит отнимать его у всей системы (см. «простой жжёт
/// ядро», Веха 168).
fn wait32(off: usize, mask: u32, want: u32, ms: usize) -> bool {
    for _ in 0..ms * 10 {
        if unsafe { rd32(off) } & mask == want {
            return true;
        }
        sys::sleep_ns(100_000);
    }
    unsafe { rd32(off) & mask == want }
}

// ── кольца CORB/RIRB: разговор с кодеком ─────────────────────────────────────────────────────
//
// Команда кладётся в кольцо CORB, ответ приезжает в кольцо RIRB — оба в обычной памяти, оба
// обходит сам контроллер. Регистры «немедленных команд» (ICW/IRR) были бы короче, но они
// необязательны по спецификации и на части чипсетов не работают вовсе; кольца есть у всех.

static mut CORB_WP: u16 = 0;
static mut RIRB_RP: u16 = 0;

/// Отправить команду и дождаться ответа. `None` — кодек промолчал.
fn cmd(cad: u8, nid: u8, verb: u32, payload: u32) -> Option<u32> {
    let word = (cad as u32) << 28 | (nid as u32) << 20 | (verb << 8 | payload) & 0xf_ffff;
    unsafe {
        let wp = (CORB_WP + 1) % RING_ENTRIES as u16;
        write_volatile((RING_VA + CORB_OFF + wp as usize * 4) as *mut u32, word);
        CORB_WP = wp;
        wr16(CORBWP, wp);
        // Ответ ждём по указателю записи RIRB: контроллер двигает его, положив ответ.
        for _ in 0..2000 {
            let rirb_wp = rd16(RIRBWP) & 0xff;
            if rirb_wp != RIRB_RP {
                RIRB_RP = (RIRB_RP + 1) % RING_ENTRIES as u16;
                let resp = read_volatile((RING_VA + RIRB_OFF + RIRB_RP as usize * 8) as *const u32);
                // Бит «ответ пришёл» снимается записью единицы; не снять — кольцо встанет.
                wr8(RIRBSTS, 0x05);
                return Some(resp);
            }
            sys::sleep_ns(50_000);
        }
    }
    None
}

/// Параметр узла (`GET_PARAMETER`) — им кодек описывает сам себя.
fn param(cad: u8, nid: u8, p: u32) -> u32 {
    cmd(cad, nid, VERB_GET_PARAM, p).unwrap_or(0)
}

/// Диапазон дочерних узлов: (первый, сколько).
fn subnodes(cad: u8, nid: u8) -> (u8, u8) {
    let v = param(cad, nid, PARAM_NODE_COUNT);
    (((v >> 16) & 0xff) as u8, (v & 0xff) as u8)
}

// ── поиск пути «пин → конвертер» ─────────────────────────────────────────────────────────────

/// Что мы ищем у кодека: куда воткнут динамик (пин) и что превращает числа в звук (DAC).
struct Path {
    cad: u8,
    dac: u8,
    pin: u8,
    /// Узлы между ними (микшер или селектор) — им тоже надо снять заглушку.
    hops: [u8; 4],
    nhops: usize,
}

/// Тип виджета из его возможностей.
fn widget_type(cad: u8, nid: u8) -> u32 {
    (param(cad, nid, PARAM_WIDGET_CAP) >> 20) & 0xf
}

/// Список того, к чему узел подключён. Возвращаем до восьми записей — больше нам не нужно.
fn connections(cad: u8, nid: u8, out: &mut [u8; 8]) -> usize {
    let len = (param(cad, nid, PARAM_CONN_LEN) & 0x7f) as usize;
    let mut n = 0;
    // Записи приезжают по четыре в слове (короткая форма) — длинную форму (по две) у кодеков
    // вывода не встретить, и разбирать её здесь значило бы писать непроверяемый код.
    while n < len && n < out.len() {
        let word = cmd(cad, nid, VERB_GET_CONN_ENTRY, n as u32 & !3).unwrap_or(0);
        for k in 0..4 {
            if n < len && n < out.len() {
                out[n] = ((word >> (8 * k)) & 0xff) as u8;
                n += 1;
            }
        }
    }
    n
}

/// Пройти от пина вглубь, пока не найдётся конвертер. Глубже трёх шагов не ходим: у кодеков
/// вывода путь короткий (пин → микшер → DAC), а бесконечный обход по кольцу соединений —
/// известная ловушка этой шины.
fn trace(cad: u8, pin: u8, path: &mut Path) -> bool {
    let mut node = pin;
    for _ in 0..3 {
        let mut conn = [0u8; 8];
        let n = connections(cad, node, &mut conn);
        for &c in &conn[..n] {
            if c == 0 {
                continue;
            }
            match widget_type(cad, c) {
                WIDGET_DAC => {
                    path.dac = c;
                    return true;
                }
                WIDGET_MIXER | WIDGET_SELECTOR => {
                    if path.nhops < path.hops.len() {
                        path.hops[path.nhops] = c;
                        path.nhops += 1;
                    }
                    node = c;
                }
                _ => continue,
            }
            break;
        }
    }
    false
}

/// Состояние обоих колец и первой команды в них — что именно не доехало.
///
/// Здесь ровно четыре вопроса, и каждый отсекает половину: положили ли мы команду в память
/// (слово CORB), забрал ли её контроллер (двинулся ли его указатель чтения), ответил ли кодек
/// (двинулся ли указатель записи RIRB), и что лежит в ответе.
fn say_rings() {
    unsafe {
        let corb0 = read_volatile((RING_VA + CORB_OFF + 4) as *const u32);
        let rirb0 = read_volatile((RING_VA + RIRB_OFF + 8) as *const u32);
        sys::write(
            alloc::format!(
                "[hda] кольца: CORB ctl {:#04x} wp {} rp {} размер {:#04x} · RIRB ctl {:#04x} wp {} размер {:#04x} sts {:#04x}\n[hda]   в CORB[1] {:#010x}, в RIRB[1] {:#010x}\n",
                rd8(CORBCTL), rd16(CORBWP), rd16(CORBRP), rd8(CORBSIZE),
                rd8(RIRBCTL), rd16(RIRBWP), rd8(RIRBSIZE), rd8(RIRBSTS),
                corb0, rirb0,
            )
            .as_bytes(),
        );
    }
}

/// Рассказать про кодек всё, что он о себе говорит: узлы, их типы, конфигурацию пинов.
///
/// Печатается ТОЛЬКО когда выход не нашёлся. Здоровый кодек описывать незачем (сообщение о
/// нормальном состоянии — шум), а вот молчащий приходится разбирать по узлам, и на живой машине
/// владельца это единственный способ понять, чем её кодек отличается от стендового.
fn say_codec(cad: u8) {
    let (start, count) = subnodes(cad, 0);
    sys::write(alloc::format!("[hda] кодек {}: узлы {}..{}\n", cad, start, start as u16 + count as u16).as_bytes());
    for fg in start..start.saturating_add(count) {
        let ftype = param(cad, fg, PARAM_FUNC_TYPE);
        let (ws, wc) = subnodes(cad, fg);
        sys::write(
            alloc::format!("[hda]  группа {}: тип {:#x}, узлы {}..{}\n", fg, ftype & 0x7f, ws, ws as u16 + wc as u16)
                .as_bytes(),
        );
        for nid in ws..ws.saturating_add(wc) {
            let caps = param(cad, nid, PARAM_WIDGET_CAP);
            let kind = (caps >> 20) & 0xf;
            let name = match kind {
                WIDGET_DAC => "конвертер",
                0x1 => "вход",
                WIDGET_MIXER => "микшер",
                WIDGET_SELECTOR => "селектор",
                WIDGET_PIN => "пин",
                0x5 => "питание",
                0x6 => "громкость",
                0x7 => "маяк",
                _ => "прочее",
            };
            let mut line = alloc::format!("[hda]   узел {}: {} (возможности {:#010x})", nid, name, caps);
            if kind == WIDGET_PIN {
                let pc = param(cad, nid, PARAM_PIN_CAP);
                let cfg = cmd(cad, nid, VERB_GET_CONFIG_DEFAULT, 0).unwrap_or(0);
                line += &alloc::format!(" пин-возм {:#010x} конфиг {:#010x}", pc, cfg);
            }
            let mut conn = [0u8; 8];
            let n = connections(cad, nid, &mut conn);
            if n > 0 {
                line += " ←";
                for &c in &conn[..n] {
                    line += &alloc::format!(" {}", c);
                }
            }
            line += "\n";
            sys::write(line.as_bytes());
        }
    }
}

/// Найти кодек, а у него — выход. Предпочтение отдаём встроенному динамику: на ноутбуке звук
/// должен пойти именно в него, а не в разъём, куда ничего не воткнуто.
fn find_path() -> Option<Path> {
    let present = unsafe { rd16(STATESTS) };
    for cad in 0..15u8 {
        if present & (1 << cad) == 0 {
            continue;
        }
        let (start, count) = subnodes(cad, 0);
        for fg in start..start.saturating_add(count) {
            // 0x01 — звуковая группа функций; у кодека может быть и модем (0x02).
            if param(cad, fg, PARAM_FUNC_TYPE) & 0x7f != 0x01 {
                continue;
            }
            // Питание группе — иначе она не ответит про свои узлы ничего осмысленного.
            let _ = cmd(cad, fg, VERB_SET_POWER, 0);
            let (wstart, wcount) = subnodes(cad, fg);
            let mut best: Option<(u32, u8)> = None; // (предпочтение, пин)
            for nid in wstart..wstart.saturating_add(wcount) {
                if widget_type(cad, nid) != WIDGET_PIN {
                    continue;
                }
                // Бит 4 возможностей пина — «умеет наружу». Вход нам не нужен.
                if param(cad, nid, PARAM_PIN_CAP) & (1 << 4) == 0 {
                    continue;
                }
                // Конфигурация по умолчанию говорит, ЧТО за этим пином: биты 23:20 — тип
                // устройства (0 — линейный выход, 1 — динамик, 2 — наушники), биты 31:30 —
                // связность (0x1 = «никуда не подключён», такой пин бесполезен).
                let cfg = cmd(cad, nid, VERB_GET_CONFIG_DEFAULT, 0).unwrap_or(0);
                if (cfg >> 30) & 0x3 == 0x1 {
                    continue;
                }
                let rank = match (cfg >> 20) & 0xf {
                    1 => 3, // динамик
                    0 => 2, // линейный выход
                    2 => 1, // наушники
                    _ => 0,
                };
                if rank == 0 {
                    continue;
                }
                if best.map_or(true, |(r, _)| rank > r) {
                    best = Some((rank, nid));
                }
            }
            if let Some((_, pin)) = best {
                let mut path = Path { cad, dac: 0, pin, hops: [0; 4], nhops: 0 };
                if trace(cad, pin, &mut path) {
                    return Some(path);
                }
            }
        }
    }
    None
}

/// Снять заглушку с выходного усилителя узла и вывести громкость на «нулевые децибелы» —
/// то есть на то деление, которое сам кодек называет отсутствием усиления.
fn unmute(cad: u8, nid: u8) {
    let caps = param(cad, nid, PARAM_AMP_OUT_CAP);
    if caps == 0 {
        return; // усилителя нет — и снимать нечего
    }
    let offset = caps & 0x7f;
    // Биты полезной нагрузки: 15 — «ставим выходной», 13/12 — левый и правый канал,
    // 7 — заглушка (ноль = звук идёт), 6:0 — деление громкости.
    let payload = 0xb000 | (offset & 0x7f);
    let _ = cmd(cad, nid, VERB_SET_AMP, payload);
}

// ── звук ─────────────────────────────────────────────────────────────────────────────────────

/// Четверть периода синуса, 64 отсчёта, амплитуда 8192 (четверть шкалы). Остальные три четверти
/// получаются отражением — таблица целиком стоила бы вчетверо дороже ни за что.
const SINE_Q: [i16; 64] = [
    0, 201, 402, 603, 803, 1003, 1202, 1401, 1598, 1795, 1990, 2185, 2378, 2570, 2760, 2948,
    3135, 3320, 3503, 3683, 3862, 4038, 4212, 4383, 4551, 4717, 4880, 5040, 5197, 5351, 5501,
    5649, 5793, 5933, 6070, 6203, 6333, 6458, 6580, 6698, 6811, 6921, 7027, 7128, 7225, 7317,
    7405, 7489, 7568, 7643, 7713, 7779, 7839, 7895, 7946, 7993, 8035, 8071, 8103, 8130, 8153,
    8170, 8182, 8190,
];

/// Синус по фазе 0..255.
fn sine(phase: u8) -> i16 {
    match phase / 64 {
        0 => SINE_Q[phase as usize],
        1 => SINE_Q[63 - (phase as usize - 64)],
        2 => -SINE_Q[phase as usize - 128],
        _ => -SINE_Q[63 - (phase as usize - 192)],
    }
}

/// Залить буфер тоном: `millis` миллисекунд звука, дальше до конца — тишина.
fn fill_tone(hz: usize, millis: usize) {
    let frames = (RATE * millis / 1000).min(PCM_BYTES / 4);
    // Шаг фазы в неподвижной точке: 256 делений на период, 16 дробных бит.
    let step = (hz * 256 * 65536 / RATE) as u32;
    let mut phase: u32 = 0;
    let buf = PCM_VA as *mut i16;
    for i in 0..frames {
        let v = sine((phase >> 16) as u8);
        unsafe {
            write_volatile(buf.add(i * 2), v);
            write_volatile(buf.add(i * 2 + 1), v);
        }
        phase = phase.wrapping_add(step);
    }
    // Хвост буфера — тишина: контроллер идёт по кольцу и без неё повторил бы обрывок тона.
    for i in frames..PCM_BYTES / 4 {
        unsafe {
            write_volatile(buf.add(i * 2), 0);
            write_volatile(buf.add(i * 2 + 1), 0);
        }
    }
}

// ── подъём контроллера ───────────────────────────────────────────────────────────────────────

/// Сброс и кольца. `false` — контроллер не ожил.
fn controller_up(ring_pa: usize) -> bool {
    unsafe {
        // Сброс: бит уводится в ноль, и контроллер обязан подтвердить это чтением.
        wr32(GCTL, 0);
        if !wait32(GCTL, GCTL_CRST, 0, 100) {
            w("[hda] контроллер не ушёл в сброс\n");
            return false;
        }
        wr32(GCTL, GCTL_CRST);
        if !wait32(GCTL, GCTL_CRST, GCTL_CRST, 100) {
            w("[hda] контроллер не вышел из сброса\n");
            return false;
        }
        // Кодекам нужно время объявиться — спецификация называет 521 микросекунду. Дадим
        // с запасом: опросить STATESTS раньше значит не найти кодек, который есть.
        sys::sleep_ns(2_000_000);

        // CORB: 256 записей (значение 2 в младших битах размера), адрес, сброс указателя чтения.
        wr8(CORBCTL, 0);
        wr8(CORBSIZE, (rd8(CORBSIZE) & !0x3) | 0x2);
        wr32(CORBLBASE, (ring_pa + CORB_OFF) as u32);
        wr32(CORBUBASE, ((ring_pa + CORB_OFF) as u64 >> 32) as u32);
        wr16(CORBRP, CORBRPRST);
        // Подтверждение сброса — сам бит: контроллер поднимает его, приняв команду, и опускает,
        // когда указатель обнулён. Пропустить это рукопожатие значит писать в кольцо, которое
        // ещё не готово.
        for _ in 0..1000 {
            if rd16(CORBRP) & CORBRPRST != 0 {
                break;
            }
            sys::sleep_ns(100_000);
        }
        wr16(CORBRP, 0);
        for _ in 0..1000 {
            if rd16(CORBRP) & CORBRPRST == 0 {
                break;
            }
            sys::sleep_ns(100_000);
        }
        wr16(CORBWP, 0);
        CORB_WP = 0;

        // RIRB: то же кольцо в обратную сторону.
        wr8(RIRBCTL, 0);
        wr8(RIRBSIZE, (rd8(RIRBSIZE) & !0x3) | 0x2);
        wr32(RIRBLBASE, (ring_pa + RIRB_OFF) as u32);
        wr32(RIRBUBASE, ((ring_pa + RIRB_OFF) as u64 >> 32) as u32);
        wr16(RIRBWP, 1 << 15); // сброс указателя записи
        // Сколько ответов копить до прерывания. Единица (как у Linux) здесь ЛОВУШКА: пока
        // счётчик упёрт в этот предел, контроллер перестаёт разбирать очередь команд — он ждёт,
        // когда обслужат прерывание, а мы прерываний не берём вовсе. Выглядит это как кодек,
        // ответивший ровно один раз и замолчавший навсегда. Нам нужен предел, до которого не
        // дойти: мы читаем ответы опросом, сразу.
        wr16(RINTCNT, 0xff);
        RIRB_RP = 0;

        wr8(CORBCTL, DMA_RUN);
        wr8(RIRBCTL, DMA_RUN);
    }
    true
}

/// Смещение первого ВЫХОДНОГО дескриптора потока: они идут после всех входных, а сколько тех —
/// говорит GCAP. Считать иначе (взять нулевой) значило бы играть в микрофон.
fn out_stream_off() -> usize {
    let cap = unsafe { rd16(GCAP) };
    let iss = ((cap >> 8) & 0xf) as usize;
    0x80 + iss * 0x20
}

/// Завести поток вывода на готовый буфер.
fn stream_start(sd: usize, bdl_pa: usize, bytes: usize) -> bool {
    unsafe {
        // Сброс дескриптора — с тем же рукопожатием, что у CORB.
        wr32(sd + SD_CTL, SD_CTL_SRST);
        if !wait32(sd + SD_CTL, SD_CTL_SRST, SD_CTL_SRST, 100) {
            w("[hda] поток не ушёл в сброс\n");
            return false;
        }
        wr32(sd + SD_CTL, 0);
        if !wait32(sd + SD_CTL, SD_CTL_SRST, 0, 100) {
            w("[hda] поток не вышел из сброса\n");
            return false;
        }
        wr8(sd + SD_STS, 0x1c); // снять залипшие признаки (пишутся единицей)
        wr32(sd + SD_CBL, bytes as u32);
        wr16(sd + SD_LVI, 1); // две записи в списке кусков
        wr16(sd + SD_FMT, FORMAT);
        wr32(sd + SD_BDPL, bdl_pa as u32);
        wr32(sd + SD_BDPU, (bdl_pa as u64 >> 32) as u32);
        // Номер потока — в старшие биты управления; им контроллер и кодек узнают друг друга.
        wr32(sd + SD_CTL, STREAM_TAG << 20);
        wr32(sd + SD_CTL, STREAM_TAG << 20 | SD_CTL_RUN);
    }
    true
}

#[no_mangle]
pub extern "C" fn _start(mmio_cap: usize, dma_cap: usize) -> ! {
    if !sys::mmio_map(mmio_cap, MMIO_VA) {
        w("[hda] окно регистров не замаплено (нет права?)\n");
        sys::exit(1);
    }
    let (Some(ring_pa), Some(pcm_pa)) = (
        sys::dma_alloc(dma_cap, RING_VA),
        sys::dma_alloc_pages(dma_cap, PCM_VA, PCM_PAGES),
    ) else {
        w("[hda] DMA-память не выделена (нет права?)\n");
        sys::exit(1);
    };

    if !controller_up(ring_pa) {
        sys::exit(1);
    }
    let version = unsafe { (rd8(0x03), rd8(0x02)) };
    let present = unsafe { rd16(STATESTS) };
    sys::write(
        alloc::format!(
            "[hda] контроллер {}.{} поднят, кодеки {:#06x}\n",
            version.0, version.1, present
        )
        .as_bytes(),
    );

    let Some(path) = find_path() else {
        w("[hda] выхода у кодека не нашлось — звука не будет; вот что он о себе говорит:\n");
        say_rings();
        for cad in 0..15u8 {
            if present & (1 << cad) != 0 {
                say_codec(cad);
            }
        }
        sys::exit(1);
    };
    sys::write(
        alloc::format!(
            "[hda] кодек {}: выход — пин {}, конвертер {}\n",
            path.cad, path.pin, path.dac
        )
        .as_bytes(),
    );

    // Разбудить и открыть весь путь. Порядок важен: питание → формат → поток → громкость →
    // и только в конце пин, чтобы наружу не вышел щелчок недонастроенного тракта.
    for &nid in [path.dac, path.pin].iter().chain(path.hops[..path.nhops].iter()) {
        let _ = cmd(path.cad, nid, VERB_SET_POWER, 0);
    }
    let _ = cmd(path.cad, path.dac, VERB_SET_STREAM_FORMAT, FORMAT as u32);
    let _ = cmd(path.cad, path.dac, VERB_SET_CONV_STREAM, STREAM_TAG << 4);
    unmute(path.cad, path.dac);
    for &nid in &path.hops[..path.nhops] {
        unmute(path.cad, nid);
    }
    unmute(path.cad, path.pin);
    let _ = cmd(path.cad, path.pin, VERB_SET_PIN_CTL, PIN_CTL_OUT_EN | PIN_CTL_HP_EN);
    // Внешний усилитель динамика: на ноутбуках без него звук просто не выходит наружу, а
    // регистры при этом выглядят совершенно здоровыми. Бит 16 возможностей пина — есть ли он.
    if param(path.cad, path.pin, PARAM_PIN_CAP) & (1 << 16) != 0 {
        let _ = cmd(path.cad, path.pin, VERB_SET_EAPD, 0x2);
    }

    // Тон и список кусков: две записи по половине буфера. Список из одной записи спецификация
    // не допускает вовсе — минимум два куска, даже если буфер один.
    fill_tone(440, 300);
    let bdl = (RING_VA + BDL_OFF) as *mut u32;
    unsafe {
        for i in 0..2 {
            let half = PCM_BYTES / 2;
            let pa = pcm_pa + i * half;
            write_volatile(bdl.add(i * 4), pa as u32);
            write_volatile(bdl.add(i * 4 + 1), (pa as u64 >> 32) as u32);
            write_volatile(bdl.add(i * 4 + 2), half as u32);
            write_volatile(bdl.add(i * 4 + 3), 0); // прерывание по куску нам не нужно
        }
    }

    let sd = out_stream_off();
    if !stream_start(sd, ring_pa + BDL_OFF, PCM_BYTES) {
        sys::exit(1);
    }

    // Дать буферу проиграться и посмотреть, ШЁЛ ЛИ контроллер по нему: позиция в буфере —
    // единственное честное свидетельство того, что звук ушёл в железо, а не остался в памяти.
    sys::sleep_ns(400_000_000);
    let pos = unsafe { rd32(sd + SD_LPIB) };
    unsafe { wr32(sd + SD_CTL, 0) }; // остановить поток: DMA-память сейчас уйдёт вместе с нами
    sys::write(
        alloc::format!(
            "[hda] тон 440 Гц сыгран, контроллер прошёл {} байт буфера\n",
            pos
        )
        .as_bytes(),
    );
    sys::exit(0);
}
