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

use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};

use void_user as sys;
use void_user::snd_cli as snd;

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

/// Формат ВЫВОДА: 48 кГц, 16 бит, два канала. Разложение по битам регистра — база 48 кГц
/// (бит 14 = 0), без умножения и деления, `001` = 16 бит, `0001` = два канала.
///
/// Веха 204 — он теперь ПОСТОЯННЫЙ. Кодек умеет перестраиваться под частоту дорожки, и до
/// миксера мы этим пользовались: один голос — одна частота, пересчитывать нечего. Голосов стало
/// несколько, у каждого своя частота, и перестроить кодек можно только под одну из них —
/// остальные играли бы не с той скоростью. Поэтому частоту пересчитываем мы сами
/// (`Ring::next_frame`), а кодек всегда стоит на 48 кГц.
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
/// Какие частоты и разрядности умеет конвертер (биты частот — 0..11, разрядностей — 16..20).
const PARAM_PCM: u32 = 0x0a;

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
#[derive(Clone, Copy)]
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

/// Веха 204 — ВСЕ ВЫХОДЫ кодека, годные для звука, в порядке предпочтения.
///
/// До этой вехи драйвер искал ровно один — лучший — и о существовании остальных не сообщал
/// никому. На ноутбуке это значит «звук всегда в динамике»: воткнутые наушники системе просто
/// негде было выбрать. Теперь список отдаётся наружу (`OP_OUTPUTS`), а выбор делает человек в
/// меню звука.
///
/// Предпочтение то же, что и было, и оно определяет ПЕРВЫЙ выход: встроенный динамик, потом
/// линейный выход, потом наушники. Разъём, о котором кодек говорит «никуда не подключён»
/// (связность 0x1), не годится вовсе — звук в нём просто некуда деть.
fn find_outputs() -> Vec<Path> {
    let mut out: Vec<(u32, Path)> = Vec::new();
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
                let mut path = Path { cad, dac: 0, pin: nid, hops: [0; 4], nhops: 0 };
                // Путь до конвертера обязан найтись: пин, от которого некуда вести звук, — это
                // не выход, как бы он себя ни называл в конфигурации.
                if trace(cad, nid, &mut path) {
                    out.push((rank, path));
                }
            }
        }
    }
    // Лучший — первым: им драйвер и пользуется, пока человек не выбрал другой.
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.into_iter().map(|(_, p)| p).collect()
}

/// Чем этот выход называется для человека: тип устройства из конфигурации пина.
fn out_name(p: &Path) -> &'static str {
    let cfg = cmd(p.cad, p.pin, VERB_GET_CONFIG_DEFAULT, 0).unwrap_or(0);
    match (cfg >> 20) & 0xf {
        0 => "линейный выход",
        1 => "динамики",
        2 => "наушники",
        4 => "SPDIF",
        _ => "выход",
    }
}

/// Веха 204 — ОТКРЫТЬ ТРАКТ: разбудить узлы, назначить поток и выпустить звук в пин.
///
/// Вынесено из подъёма отдельной функцией, потому что делается теперь дважды: при старте и при
/// смене выхода в меню. Порядок важен и он тот же, что был: питание → формат → поток →
/// громкость → и только в конце пин, чтобы наружу не вышел щелчок недонастроенного тракта.
fn path_open(p: &Path) {
    for &nid in [p.dac, p.pin].iter().chain(p.hops[..p.nhops].iter()) {
        let _ = cmd(p.cad, nid, VERB_SET_POWER, 0);
    }
    let _ = cmd(p.cad, p.dac, VERB_SET_STREAM_FORMAT, FORMAT as u32);
    let _ = cmd(p.cad, p.dac, VERB_SET_CONV_STREAM, STREAM_TAG << 4);
    unmute(p.cad, p.dac);
    for &nid in &p.hops[..p.nhops] {
        unmute(p.cad, nid);
    }
    unmute(p.cad, p.pin);
    let _ = cmd(p.cad, p.pin, VERB_SET_PIN_CTL, PIN_CTL_OUT_EN | PIN_CTL_HP_EN);
    // Внешний усилитель динамика: на ноутбуках без него звук просто не выходит наружу, а
    // регистры при этом выглядят совершенно здоровыми. Бит 16 возможностей пина — есть ли он.
    if param(p.cad, p.pin, PARAM_PIN_CAP) & (1 << 16) != 0 {
        let _ = cmd(p.cad, p.pin, VERB_SET_EAPD, 0x2);
    }
}

/// Закрыть тракт: погасить пин, чтобы из него ничего не шло.
///
/// Нужен при смене выхода: без этого звук пошёл бы сразу в оба (динамик И наушники), а смысл
/// выбора ровно обратный — человек втыкает наушники, чтобы динамик замолчал.
fn path_close(p: &Path) {
    let _ = cmd(p.cad, p.pin, VERB_SET_PIN_CTL, 0);
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

/// Источник звука: что играть прямо сейчас.
///
/// Тон, а не готовая дорожка, потому что это и есть системный звук: короткий сигнал. Держать
/// его дорожкой значило бы хранить в памяти то, что считается тремя действиями на отсчёт.
struct Tone {
    /// Шаг фазы в неподвижной точке (256 делений на период, 16 дробных бит).
    step: u32,
    phase: u32,
    /// Сколько кадров тона ещё осталось выдать.
    left: usize,
    /// Сколько всего было — нужно краям: тон, начатый и оборванный отвесно, даёт щелчок,
    /// который слышно лучше самого тона.
    total: usize,
    /// Сколько кадров тишины выдать ПЕРЕД тоном.
    ///
    /// Нужно только что запущенному потоку. Контроллер, пущенный на кольцо, первые десятки
    /// миллисекунд читает его быстрее реального времени — догоняет то, что «должно было
    /// прозвучать» с момента запуска, — и начало сигнала съедается. Измерено: у первого сигнала
    /// после подъёма драйвера нарастание громкости на месте, у каждого следующего (то есть
    /// после перезапуска потока) звук начинается отвесно, и не хватает ровно сорока с лишним
    /// миллисекунд. Пусть эти миллисекунды будут тишиной.
    lead: usize,
}

/// Сколько кадров занимают края, где громкость нарастает и спадает (по 5 мс).
const EDGE: usize = RATE / 200;

impl Tone {
    fn new(hz: u32, ms: u32, lead: usize) -> Self {
        let frames = (RATE * ms as usize / 1000).max(EDGE * 2);
        Tone {
            step: (hz as usize * 256 * 65536 / RATE) as u32,
            phase: 0,
            left: frames,
            total: frames,
            lead,
        }
    }


    /// Следующий отсчёт. Ноль, когда тон кончился, — дальше кольцо доигрывает тишину.
    fn next(&mut self) -> i16 {
        if self.lead > 0 {
            self.lead -= 1;
            return 0;
        }
        if self.left == 0 {
            return 0;
        }
        let done = self.total - self.left;
        let v = sine((self.phase >> 16) as u8) as i32;
        // Края: линейный подъём и спад. Дешевле, чем кажется, — деление на константу.
        let v = if done < EDGE {
            v * done as i32 / EDGE as i32
        } else if self.left < EDGE {
            v * self.left as i32 / EDGE as i32
        } else {
            v
        };
        self.phase = self.phase.wrapping_add(self.step);
        self.left -= 1;
        v as i16
    }
}

/// Поток из общей памяти: кольцо, которое пишет клиент, а читаем мы.
///
/// Позиции — счётчики в байтах от начала потока, а не индексы: так «пусто» и «полно» не
/// путаются между собой, и не нужно держать отдельный признак.
struct Ring {
    va: usize,
    len: usize,
    /// Докуда дописал клиент и докуда дочитали мы.
    write: u32,
    read: u32,
    cap: usize,
    /// Когда клиент в последний раз о себе напоминал: умерший клиент иначе держал бы общую
    /// память вечно, а места в ней столько же, сколько голосов.
    seen_ns: u64,
    /// Веха 204 — ПЕРЕСЧЁТ ЧАСТОТЫ. Шаг чтения в неподвижной точке 16.16: сколько исходных
    /// кадров приходится на один выходной. У дорожки в 48 кГц он ровно 1.0, и пересчёт
    /// вырождается в точную копию — платит только тот, у кого частота другая.
    step: u32,
    /// Дробная часть позиции чтения (те же 16 бит), для линейной интерполяции.
    frac: u32,
    /// Прошлый кадр — второй конец отрезка, по которому интерполируем.
    prev: (i16, i16),
}

impl Ring {
    /// Следующий отсчёт или `None`, если клиент не успел дописать.
    fn next_sample(&mut self) -> Option<i16> {
        if self.read.wrapping_add(2) > self.write {
            return None;
        }
        let at = (self.read as usize) % self.len;
        // Кадр может лежать на стыке конца кольца и начала — читаем побайтно, это дешевле
        // ветвления на каждый отсчёт.
        let lo = unsafe { read_volatile((self.va + at) as *const u8) };
        let hi = unsafe { read_volatile((self.va + (at + 1) % self.len) as *const u8) };
        self.read = self.read.wrapping_add(2);
        Some(i16::from_le_bytes([lo, hi]))
    }

    /// Следующий КАДР (левый и правый) в частоте ВЫВОДА.
    ///
    /// Пересчёт линейный: между соседними исходными кадрами проводится отрезок. Для 44,1 → 48
    /// это слышно как чуть приглушённые верхи — и это честная цена за то, что несколько
    /// дорожек с разными частотами звучат одновременно. До Вехи 204 частоту под дорожку
    /// перестраивал сам кодек, но так умеет ровно ОДИН голос: второй пришлось бы играть
    /// не с той скоростью.
    fn next_frame(&mut self) -> Option<(i16, i16)> {
        while self.frac >= 0x1_0000 {
            let l = self.next_sample()?;
            let r = self.next_sample()?;
            self.prev = (l, r);
            self.frac -= 0x1_0000;
        }
        // Заглянуть вперёд нельзя (кольцо читается только вперёд), поэтому отрезок строится
        // между ПРОШЛЫМ и следующим кадром, а не между текущим и будущим.
        let out = self.prev;
        self.frac += self.step;
        Some(out)
    }

    /// Есть ли ещё что читать.
    fn ready(&self) -> bool {
        self.read.wrapping_add(4) <= self.write
    }
}

/// Откуда берёт звук один голос миксера.
enum Src {
    Tone(Tone),
    Stream(Ring),
}

/// Веха 204 — ГОЛОС МИКСЕРА: один источник со своей громкостью и своим именем.
///
/// До этой вехи источник был один на всю систему, и второй звук получал отказ «занято». Теперь
/// голосов несколько, они складываются, и у каждого своя громкость — то, что человек видит в
/// меню строкой с ползунком.
struct Voice {
    /// Номер, по которому на голос ссылается меню. Нумерация с единицы: ноль — это мастер.
    id: u16,
    src: Src,
    /// Громкость голоса в процентах.
    vol: u8,
    name: [u8; snd::NAME_MAX],
    name_len: u8,
    /// Кто попросил — по нему находятся `OP_ADVANCE` и `OP_CLOSE` того же клиента.
    owner: usize,
}

impl Voice {
    /// Есть ли ещё что играть (иначе голос пора убирать).
    fn alive(&self) -> bool {
        match &self.src {
            Src::Tone(t) => t.left > 0 || t.lead > 0,
            Src::Stream(r) => r.ready(),
        }
    }
}

/// Громкость, с которой сервер поднялся. Половина, а не максимум: первый звук в системе не
/// должен быть самым громким, который она умеет.
const VOL_DEFAULT: u8 = 70;

/// Заполнить ПОЛОВИНУ кольца СУММОЙ голосов. Возвращает `true`, если хоть одному голосу ещё
/// есть что играть, — а не «были ли ненулевые отсчёты»: тишина перед тоном тоже работа, и
/// гасить поток на ней значило бы гасить его ровно перед сигналом.
///
/// Складываем в 32 битах и зажимаем в 16 (насыщение). Складывать в 16 нельзя: два громких
/// голоса дают переполнение, а переполнение знакового числа — это переход от максимума к
/// минимуму, то есть громкий треск ровно в тех местах, где музыка и так громкая.
fn fill_half(half: usize, voices: &mut [Voice], master: u8) -> bool {
    let frames = PCM_BYTES / 2 / 4;
    let base = unsafe { (PCM_VA as *mut i16).add(half * frames * 2) };
    let mut alive = false;
    // Тишина сперва: половина заполняется всегда целиком, а голоса могут кончиться на середине.
    for i in 0..frames * 2 {
        unsafe { write_volatile(base.add(i), 0) };
    }
    for v in voices.iter_mut() {
        // Громкость голоса и общая — одним множителем на отсчёт: делить дважды дороже, а
        // разница округления ниже слышимого.
        let gain = v.vol as i32 * master as i32;
        if gain == 0 {
            // Тихий голос всё равно ДОЛЖЕН читать своё кольцо: иначе клиент упрётся в
            // переполнение и остановится, а выкрученная обратно громкость оживит звук с того
            // места, где его выключили, — то есть с опозданием на всю паузу.
            alive |= drain(v);
            continue;
        }
        match &mut v.src {
            Src::Tone(t) => {
                for i in 0..frames {
                    let s = t.next() as i32 * gain / 10_000;
                    // Тон моно по своей природе: один отсчёт в оба канала.
                    unsafe {
                        add_sample(base.add(i * 2), s);
                        add_sample(base.add(i * 2 + 1), s);
                    }
                }
            }
            Src::Stream(r) => {
                for i in 0..frames {
                    let Some((l, rr)) = r.next_frame() else { break };
                    unsafe {
                        add_sample(base.add(i * 2), l as i32 * gain / 10_000);
                        add_sample(base.add(i * 2 + 1), rr as i32 * gain / 10_000);
                    }
                }
            }
        }
        alive |= v.alive();
    }
    alive
}

/// Прочитать голос вхолостую — ровно столько, сколько ушло бы в звук на полной громкости.
fn drain(v: &mut Voice) -> bool {
    if let Src::Stream(r) = &mut v.src {
        for _ in 0..PCM_BYTES / 2 / 4 {
            if r.next_frame().is_none() {
                break;
            }
        }
    } else if let Src::Tone(t) = &mut v.src {
        for _ in 0..PCM_BYTES / 2 / 4 {
            t.next();
        }
    }
    v.alive()
}

/// Прибавить отсчёт к тому, что уже лежит, с насыщением.
unsafe fn add_sample(at: *mut i16, add: i32) {
    let v = read_volatile(at) as i32 + add;
    write_volatile(at, v.clamp(i16::MIN as i32, i16::MAX as i32) as i16);
}

// ── чипсет: то, чего нет в спецификации HDA, но без чего она не работает ─────────────────────

/// Смещения в конфигурации PCI, которые трогает Linux (`azx_init_pci`) — и не от хорошей жизни.
const PCI_VENDOR: usize = 0x00;
const PCI_TCSEL: usize = 0x44;
const INTEL_DEVC: usize = 0x78;
const INTEL_NOSNOOP: u32 = 1 << 11;
const ATI_MISC_CNTR2: usize = 0x42;
const ATI_SNOOP_ON: u32 = 0x02;

/// Подготовить контроллер СРЕДСТВАМИ ЧИПСЕТА: класс трафика и слежение за кэшем.
///
/// Обе настройки живут в конфигурации PCI, обеих нет в спецификации HDA, и обе решают, будет
/// звук или будет треск.
///
/// **Класс трафика (`TCSEL`).** Linux чистит младшие три бита с комментарием «clear TCSEL to
/// clear playback on some HD Audio codecs» — то есть на части чипсетов воспроизведение без
/// этого попросту сломано. Прошивка оставляет там что угодно.
///
/// **Слежение за кэшем (snoop).** Вот это главное. Если у контроллера выставлен `NOSNOOP`, его
/// обращения к памяти идут МИМО кэш-когерентности: он читает то, что лежит в оперативной
/// памяти, а наши только что записанные отсчёты в этот момент могут ещё сидеть в кэше
/// процессора. Контроллер играет содержимое памяти «как получилось» — обрывки прошлого звука,
/// нули, мусор. На слух это ровно треск, и он не воспроизводится в QEMU вовсе: у эмулятора нет
/// ни кэша, ни разницы между «записал» и «дошло до памяти».
///
/// Регистр у каждого вендора свой, общего нет — поэтому смотрим, чей это чипсет.
fn chipset_prepare(mmio_cap: usize) -> (u16, u16) {
    let id = sys::pci_cfg_read(mmio_cap, PCI_VENDOR);
    let (vendor, device) = (id as u16, (id >> 16) as u16);
    let tcsel = sys::pci_cfg_read(mmio_cap, PCI_TCSEL);
    if tcsel != usize::MAX {
        let _ = sys::pci_cfg_write(mmio_cap, PCI_TCSEL, tcsel as u32 & !0x07);
    }
    match vendor {
        // Intel: слежение включено, когда бит `NOSNOOP` СНЯТ.
        0x8086 => {
            let devc = sys::pci_cfg_read(mmio_cap, INTEL_DEVC);
            if devc != usize::MAX {
                let _ = sys::pci_cfg_write(mmio_cap, INTEL_DEVC, devc as u32 & !INTEL_NOSNOOP);
            }
        }
        // AMD/ATI: наоборот, слежение включается установкой бита.
        0x1002 | 0x1022 => {
            let misc = sys::pci_cfg_read(mmio_cap, ATI_MISC_CNTR2);
            if misc != usize::MAX {
                let _ = sys::pci_cfg_write(mmio_cap, ATI_MISC_CNTR2, misc as u32 | ATI_SNOOP_ON);
            }
        }
        _ => {}
    }
    (vendor, device)
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

/// Завести поток вывода на готовый буфер в заданном формате.
fn stream_start(sd: usize, bdl_pa: usize, bytes: usize, fmt: u16) -> bool {
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
        wr16(sd + SD_FMT, fmt);
        wr32(sd + SD_BDPL, bdl_pa as u32);
        wr32(sd + SD_BDPU, (bdl_pa as u64 >> 32) as u32);
        // Номер потока — в старшие биты управления; им контроллер и кодек узнают друг друга.
        wr32(sd + SD_CTL, STREAM_TAG << 20);
        wr32(sd + SD_CTL, STREAM_TAG << 20 | SD_CTL_RUN);
    }
    true
}

/// Жить дальше БЕЗ звука: отвечать на просьбы «звука нет» и не умирать.
///
/// Выйти было бы короче, но дороже для всей системы. Право играть — это канал к этому серверу,
/// и он роздан всем по конфигу; умерший сервер превращает его в мёртвый дескриптор, а мёртвый
/// дескриптор отличается от живого только тем, что ядро отказывает по нему молча. Клиент видит
/// «отказ», не понимая, отказали ему по существу просьбы или в системе просто нет карты, и
/// сообщает человеку чушь. Поэтому сервер живёт всегда и на всякую просьбу отвечает честно:
/// звука в этой машине нет. Стоит это одного спящего процесса.
fn deaf(reason: &str) -> ! {
    sys::write(alloc::format!("[hda] {} — звука в этой машине не будет\n", reason).as_bytes());
    let mut req = [0u8; 64];
    loop {
        let m = sys::recv(&mut req);
        sys::reply(m.reply_cap, &[snd::ST_NO_SOUND]);
    }
}

#[no_mangle]
pub extern "C" fn _start(mmio_cap: usize, dma_cap: usize) -> ! {
    if !sys::mmio_map(mmio_cap, MMIO_VA) {
        deaf("окна регистров нет (звуковой карты в машине не нашлось)");
    }
    let (Some(ring_pa), Some(pcm_pa)) = (
        sys::dma_alloc(dma_cap, RING_VA),
        sys::dma_alloc_pages(dma_cap, PCM_VA, PCM_PAGES),
    ) else {
        deaf("DMA-памяти не дали");
    };

    // ДО подъёма: класс трафика и слежение за кэшем (см. `chipset_prepare`).
    let (vendor, device) = chipset_prepare(mmio_cap);

    if !controller_up(ring_pa) {
        deaf("контроллер не ожил");
    }
    let version = unsafe { (rd8(0x03), rd8(0x02)) };
    let present = unsafe { rd16(STATESTS) };
    let gcap = unsafe { rd16(GCAP) };
    // Числа, которых не хватало бы при первом же разборе на живой машине: чей чипсет, сколько
    // потоков, умеет ли контроллер 64-битные адреса и КУДА мы положили буфер. Последнее важно
    // вместе с предпоследним: 32-битный контроллер с буфером выше четырёх гигабайт читает не
    // то, что мы написали, и это опять же треск.
    sys::write(
        alloc::format!(
            "[hda] контроллер {:04x}:{:04x} версия {}.{}, потоков вход/выход {}/{}, адреса {}-бит\n\
             [hda] кодеки {:#06x}, кольца физ {:#x}, буфер физ {:#x} ({} КиБ)\n",
            vendor, device, version.0, version.1,
            (gcap >> 8) & 0xf, (gcap >> 12) & 0xf,
            if gcap & 1 != 0 { 64 } else { 32 },
            present, ring_pa, pcm_pa, PCM_BYTES / 1024,
        )
        .as_bytes(),
    );
    if gcap & 1 == 0 && pcm_pa >> 32 != 0 {
        deaf("буфер лёг выше четырёх гигабайт, а контроллер столько не адресует");
    }

    let outs = find_outputs();
    if outs.is_empty() {
        w("[hda] выхода у кодека не нашлось; вот что он о себе говорит:\n");
        say_rings();
        for cad in 0..15u8 {
            if present & (1 << cad) != 0 {
                say_codec(cad);
            }
        }
        deaf("выхода у кодека нет");
    }
    // Какие выходы нашлись и куда играем — словом, а не номером: «играет не туда» и «не играет
    // вовсе» лечатся по-разному, а на живой машине пинов у кодека бывает десяток.
    let mut line = alloc::format!("[hda] выходы ({}):", outs.len());
    for (i, p) in outs.iter().enumerate() {
        line += &alloc::format!(" {}{} (пин {})", if i == 0 { "→" } else { "" }, out_name(p), p.pin);
    }
    sys::write((line + "\n").as_bytes());
    let mut cur_out = 0usize;
    let path = outs[cur_out];
    sys::write(
        alloc::format!(
            "[hda] кодек {}: {} (пин {}) ← конвертер {}; формат 48000/16/2, кодек умеет {:#010x}\n",
            path.cad, out_name(&path), path.pin, path.dac,
            param(path.cad, path.dac, PARAM_PCM),
        )
        .as_bytes(),
    );
    path_open(&path);

    // Список кусков: две записи по половине буфера. Из одной записи список спецификация не
    // допускает вовсе — минимум два куска, даже если память под ними одна.
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
    w("[hda] звук готов, жду просьб\n");

    // ── сервер ───────────────────────────────────────────────────────────────────────────────
    //
    // Поток железа запускается ТОЛЬКО когда есть что играть, и гасится, как только кончилось.
    // Иначе контроллер вечно ходил бы по кольцу тишины, а сервер — просыпался бы подсыпать ему
    // нули: звук в простое стоил бы системе больше, чем звук во время игры (см. «простой жжёт
    // ядро», Веха 168).
    //
    // Пока играем, просыпаемся раз в половину куска: к этому времени контроллер успевает уйти
    // из той половины, которую мы заполняли, и её можно заполнять снова.
    let half_ns = (PCM_BYTES / 2 / 4 * 1_000_000_000 / RATE) as u64;
    /// Тишина перед тоном на только что запущенном потоке — чуть больше измеренной потери.
    const LEAD_FRAMES: usize = RATE / 16;
    /// Сколько тихих половин держать поток живым (≈3 секунды): столько стоит не перезапускать.
    const QUIET_HALVES_OFF: usize = 18;
    /// Куда отображаются кольца клиентов: по два мегабайта на голос, с запасом на кольцо в 192 КиБ.
    const RING_CLIENT_VA: usize = 0x5200_0000;
    const RING_CLIENT_STEP: usize = 0x0020_0000;
    /// Сколько ждать напоминаний от клиента, прежде чем считать его ушедшим.
    const CLIENT_SILENCE_NS: u64 = 10_000_000_000;

    // ── миксер (Веха 204) ────────────────────────────────────────────────────────────────────
    //
    // Голосов несколько, и они СКЛАДЫВАЮТСЯ. До этой вехи источник был один: второй звук
    // получал «занято», то есть уведомление молчало, пока играет музыка. Теперь у каждого
    // голоса своя громкость, а над ними общая, — это и есть миксер, который человек видит в
    // меню панели.
    //
    // Частота вывода ВСЕГДА 48 кГц: кодек умеет перестраиваться под дорожку, но ровно под одну,
    // а голосов теперь много. Чужие частоты пересчитывает `Ring::next_frame`.
    let mut voices: Vec<Voice> = Vec::new();
    let mut master: u8 = VOL_DEFAULT;
    // Номер следующего голоса. Растёт всегда: номер, выданный повторно, означал бы, что
    // ползунок в меню однажды подвинет громкость не тому, кто под ним написан.
    let mut next_id: u16 = 1;
    let mut playing = false;
    // Половина, в которой контроллер был в прошлый раз. НОЛЬ, а не единица: пущенный поток
    // начинает с нулевой, и «прошлой» для него сразу является она же. С единицы первое
    // пробуждение решало, что контроллер только что покинул половину 1, и заполняло её — то
    // есть затирало ещё не сыгранное. Слышно это как звук, обрывающийся на середине.
    let mut last_half = 0usize;
    let mut quiet_halves = 0usize;
    let mut req = [0u8; 64];
    loop {
        let msg = if playing {
            // Треть половины, а не половина: на живой машине между «проснулся» и «записал»
            // лежит планировщик, и просыпаться ровно на границе значит однажды опоздать.
            sys::recv_timeout(&mut req, half_ns / 3)
        } else {
            Some(sys::recv(&mut req))
        };
        if let Some(m) = msg {
            let mut extra = [0u8; snd::STATE_BYTES];
            let mut extra_len = 0usize;
            let status = match m.op {
                snd::OP_BEEP if m.len >= 8 => {
                    let hz = u32::from_le_bytes([req[0], req[1], req[2], req[3]]);
                    let ms = u32::from_le_bytes([req[4], req[5], req[6], req[7]]);
                    if !(20..=20_000).contains(&hz) || ms == 0 {
                        snd::ST_BAD
                    } else if voices.len() >= snd::MAX_VOICES {
                        snd::ST_BUSY
                    } else {
                        let lead = if playing { 0 } else { LEAD_FRAMES };
                        voices.push(Voice {
                            id: take_id(&mut next_id),
                            src: Src::Tone(Tone::new(hz, ms.min(snd::MAX_MS), lead)),
                            vol: 100,
                            name: name_bytes("Система"),
                            name_len: "Система".len() as u8,
                            owner: m.sender,
                        });
                        quiet_halves = 0;
                        if !playing {
                            // Обе половины заполняются ДО запуска: контроллер, пущенный на
                            // кольцо, в котором ещё нет звука, честно сыграет его пустоту.
                            fill_half(0, &mut voices, master);
                            fill_half(1, &mut voices, master);
                            last_half = 0;
                            playing = stream_start(sd, ring_pa + BDL_OFF, PCM_BYTES, FORMAT);
                        }
                        if playing { snd::ST_OK } else { snd::ST_NO_SOUND }
                    }
                }
                snd::OP_OPEN if m.len >= 4 && m.cap != sys::NO_CAP => {
                    let rate = u32::from_le_bytes([req[0], req[1], req[2], req[3]]);
                    let name = core::str::from_utf8(&req[4..m.len.min(req.len())]).unwrap_or("");
                    if !(8_000..=192_000).contains(&rate) {
                        snd::ST_BAD
                    } else if voices.len() >= snd::MAX_VOICES {
                        snd::ST_BUSY
                    } else {
                        // Место отображения ищется по СВОБОДНОМУ слоту, а не по числу голосов:
                        // голоса кончаются вразнобой, и «следующий по счёту» однажды указал бы
                        // на адрес, где ещё живёт чужое кольцо.
                        let slot = (0..snd::MAX_VOICES)
                            .find(|i| {
                                let va = RING_CLIENT_VA + i * RING_CLIENT_STEP;
                                !voices.iter().any(|v| matches!(&v.src, Src::Stream(r) if r.va == va))
                            })
                            .unwrap_or(0);
                        let va = RING_CLIENT_VA + slot * RING_CLIENT_STEP;
                        match sys::shm_map(m.cap, va) {
                            None => snd::ST_BAD,
                            Some(len) => {
                                let name = if name.is_empty() { "Звук" } else { name };
                                voices.push(Voice {
                                    id: take_id(&mut next_id),
                                    src: Src::Stream(Ring {
                                        va,
                                        len,
                                        write: 0,
                                        read: 0,
                                        cap: m.cap,
                                        seen_ns: sys::monotonic_ns(),
                                        // Шаг пересчёта: во столько раз частота дорожки быстрее
                                        // нашей. Ровно 1.0 у дорожки в 48 кГц.
                                        step: (rate as u64 * 0x1_0000 / RATE as u64) as u32,
                                        // Первый кадр читается сразу: дробная часть начинается
                                        // за границей отрезка, и `next_frame` возьмёт кадр.
                                        frac: 0x1_0000,
                                        prev: (0, 0),
                                    }),
                                    vol: 100,
                                    name: name_bytes(name),
                                    name_len: name.len().min(snd::NAME_MAX) as u8,
                                    owner: m.sender,
                                });
                                quiet_halves = 0;
                                // Поток пойдёт с первым `OP_ADVANCE`: пускать его сейчас
                                // значило бы сыграть пустое кольцо.
                                snd::ST_OK
                            }
                        }
                    }
                }
                snd::OP_ADVANCE if m.len >= 4 => {
                    let wpos = u32::from_le_bytes([req[0], req[1], req[2], req[3]]);
                    // Свой голос клиент находит по ОТПРАВИТЕЛЮ, а не по номеру в сообщении:
                    // чужой номер иначе двигал бы чужую позицию чтения.
                    match voices.iter_mut().find(|v| v.owner == m.sender) {
                        Some(Voice { src: Src::Stream(r), .. }) => {
                            r.write = wpos;
                            r.seen_ns = sys::monotonic_ns();
                            extra[..4].copy_from_slice(&r.read.to_le_bytes());
                            extra_len = 4;
                            let enough = r.write >= PCM_BYTES as u32;
                            if !playing && enough {
                                // Ждём, пока клиент накопит ЦЕЛЫЙ буфер, а не половину: перед
                                // запуском мы заполняем обе половины, и если звука хватило
                                // только на первую, вторая уедет тишиной — в записи это дыра
                                // ровно в половину кольца, и слышно её как проглоченную ноту.
                                fill_half(0, &mut voices, master);
                                fill_half(1, &mut voices, master);
                                last_half = 0;
                                quiet_halves = 0;
                                playing = stream_start(sd, ring_pa + BDL_OFF, PCM_BYTES, FORMAT);
                            }
                            snd::ST_OK
                        }
                        _ => snd::ST_BAD,
                    }
                }
                snd::OP_CLOSE => {
                    drop_voices(&mut voices, |v| v.owner == m.sender);
                    snd::ST_OK
                }
                snd::OP_HUSH => {
                    drop_voices(&mut voices, |_| true);
                    if playing {
                        unsafe { wr32(sd + SD_CTL, 0) };
                        playing = false;
                    }
                    snd::ST_OK
                }
                // Веха 204 — СОСТОЯНИЕ для меню: мастер, куда играем и кто играет.
                snd::OP_STATE => {
                    extra_len =
                        write_state(&mut extra, master, playing, out_name(&outs[cur_out]), &voices);
                    snd::ST_OK
                }
                // Веха 204 — ГРОМКОСТЬ: нулевой номер значит общую, иначе голос.
                snd::OP_VOLUME if m.len >= 3 => {
                    let id = u16::from_le_bytes([req[0], req[1]]);
                    let vol = req[2].min(100);
                    if id == 0 {
                        master = vol;
                        snd::ST_OK
                    } else if let Some(v) = voices.iter_mut().find(|v| v.id == id) {
                        v.vol = vol;
                        snd::ST_OK
                    } else {
                        // Голос кончился, пока человек вёл ползунок, — это не ошибка вызова, а
                        // обычная гонка: меню просто перерисуется без него.
                        snd::ST_BAD
                    }
                }
                // Веха 204 — КУДА МОЖНО ИГРАТЬ: список выходов кодека и тот, что выбран.
                snd::OP_OUTPUTS => {
                    extra[0] = outs.len().min(snd::MAX_VOICES) as u8;
                    extra[1] = cur_out as u8;
                    let mut at = 2;
                    for p in outs.iter().take(snd::MAX_VOICES) {
                        let name = out_name(p);
                        extra[at] = name.len().min(snd::NAME_MAX) as u8;
                        extra[at + 1..at + 1 + snd::NAME_MAX].copy_from_slice(&name_bytes(name));
                        at += 1 + snd::NAME_MAX;
                    }
                    extra_len = at;
                    snd::ST_OK
                }
                // Веха 204 — ВЫБРАТЬ ВЫХОД: погасить прежний пин и открыть новый тракт.
                //
                // Гасим обязательно: иначе звук пошёл бы сразу в оба, а смысл выбора ровно
                // обратный — наушники втыкают, чтобы динамик замолчал.
                snd::OP_PICK if m.len >= 1 => {
                    let want = req[0] as usize;
                    if want >= outs.len() {
                        snd::ST_BAD
                    } else if want == cur_out {
                        snd::ST_OK
                    } else {
                        path_close(&outs[cur_out]);
                        cur_out = want;
                        path_open(&outs[cur_out]);
                        // Поток при этом не трогаем: он привязан к НОМЕРУ (`STREAM_TAG`), а не
                        // к пину, и новый конвертер забирает те же данные с того же места.
                        snd::ST_OK
                    }
                }
                _ => snd::ST_BAD,
            };
            let mut body = [0u8; 1 + snd::STATE_BYTES];
            body[0] = status;
            body[1..1 + extra_len].copy_from_slice(&extra[..extra_len]);
            sys::reply(m.reply_cap, &body[..1 + extra_len]);
        }

        // Клиент, который замолчал надолго, считается ушедшим: его кольцо занимает и общую
        // память, и место в миксере, а процесса за ним может уже не быть.
        let now = sys::monotonic_ns();
        drop_voices(&mut voices, |v| {
            matches!(&v.src, Src::Stream(r) if now.saturating_sub(r.seen_ns) > CLIENT_SILENCE_NS)
        });
        // Догоревшие голоса — тоны, которые доиграли. Поток при этом не гасим: он ждёт тишины
        // несколько секунд (см. ниже).
        drop_voices(&mut voices, |v| matches!(&v.src, Src::Tone(t) if t.left == 0 && t.lead == 0));

        if !playing {
            continue;
        }
        // Где контроллер — там трогать нельзя; заполняем ту половину, которую он прошёл.
        let pos = unsafe { rd32(sd + SD_LPIB) } as usize;
        let cur = (pos / (PCM_BYTES / 2)).min(1);
        if cur != last_half {
            if fill_half(last_half, &mut voices, master) {
                quiet_halves = 0;
            } else {
                quiet_halves += 1;
            }
            last_half = cur;
        }
        // Поток гасится НЕ сразу после звука, а через несколько секунд тишины. Причина в цене
        // перезапуска: он съедает начало следующего сигнала и даёт щелчок, а серия сигналов
        // подряд — обычное дело (уведомление за уведомлением). Зато молчащая система не платит
        // за звук ничем: погашенный поток не читает память, а сервер спит в `recv` без срока.
        if quiet_halves >= QUIET_HALVES_OFF {
            unsafe { wr32(sd + SD_CTL, 0) };
            playing = false;
        }
    }
}

/// Выдать номер следующему голосу, перешагнув ноль: нулём обозначен МАСТЕР.
fn take_id(next: &mut u16) -> u16 {
    let id = *next;
    *next = next.wrapping_add(1).max(1);
    id
}

/// Имя голоса в поле фиксированной длины, с обрезкой по границе символа.
fn name_bytes(s: &str) -> [u8; snd::NAME_MAX] {
    let mut out = [0u8; snd::NAME_MAX];
    let mut end = s.len().min(snd::NAME_MAX);
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    out[..end].copy_from_slice(&s.as_bytes()[..end]);
    out
}

/// Убрать голоса по условию, отпустив их общую память.
///
/// Отдельной функцией, потому что забыть `shm_unmap` можно ровно один раз: отображения копятся
/// молча, а кончаются адреса — через сутки работы и в чужом месте.
fn drop_voices(voices: &mut Vec<Voice>, pred: impl Fn(&Voice) -> bool) {
    let mut i = 0;
    while i < voices.len() {
        if pred(&voices[i]) {
            if let Src::Stream(r) = &voices[i].src {
                sys::shm_unmap(r.cap, r.va);
            }
            voices.remove(i);
        } else {
            i += 1;
        }
    }
}

/// Собрать ответ `OP_STATE`: заголовок и голоса подряд (разбирает его `snd::parse_state`).
fn write_state(out: &mut [u8], master: u8, playing: bool, outname: &str, voices: &[Voice]) -> usize {
    out[0] = master;
    out[1] = playing as u8;
    let name = name_bytes(outname);
    let mut len = outname.len().min(snd::NAME_MAX) as u8;
    while len > 0 && !outname.is_char_boundary(len as usize) {
        len -= 1;
    }
    out[2] = len;
    out[3..3 + snd::NAME_MAX].copy_from_slice(&name);
    let mut at = 3 + snd::NAME_MAX;
    for v in voices.iter().take(snd::MAX_VOICES) {
        out[at..at + 2].copy_from_slice(&v.id.to_le_bytes());
        out[at + 2] = v.vol;
        out[at + 3] = v.name_len;
        out[at + 4..at + 4 + snd::NAME_MAX].copy_from_slice(&v.name);
        at += 4 + snd::NAME_MAX;
    }
    at
}
