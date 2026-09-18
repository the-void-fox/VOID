//! `play` — проиграть WAV-файл (Веха 202.4).
//!
//! ## Почему WAV и только он
//!
//! Потому что это единственный формат, который **не надо декодировать**: в файле лежат ровно те
//! отсчёты, которые уходят в звуковую карту. Всё остальное (FLAC, Opus, MP3) — это декодер,
//! то есть отдельная работа, сравнимая с самим драйвером, и делать её ради того, чтобы система
//! умела звучать, незачем. Формат для проверки звука и системных звуков — этот.
//!
//! ## Как отдаётся звук
//!
//! Кольцом в общей памяти: полсекунды звука это сто килобайт, а сообщение у нас — сотни байт.
//! Мы пишем в кольцо, говорим серверу «дописал досюда» и узнаём, докуда он дочитал; когда места
//! нет — спим. Сервер читает наши отсчёты прямо из этой памяти, без копирования через ядро.
//!
//! ## Чего здесь нет
//!
//! Пересчёта частоты. Кодек умеет и 44.1, и 48 кГц сам, и просить его играть на своей частоте
//! честнее, чем пересчитывать дорожку у себя, теряя на этом и качество, и время. Файл с
//! частотой, которой звуковая карта не умеет, отвергается словами — а не играется мусором.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use void_user as sys;
use void_user::posix as px;
use void_user::snd_cli as snd;

#[global_allocator]
static ALLOC: sys::heap::Heap<{ 512 * 1024 }> = sys::heap::Heap::new();

/// Кольцо живёт здесь — между кучей программы и её стеком.
const RING_VA: usize = 0x7800_0000;

fn w(s: &str) {
    sys::write(s.as_bytes());
}

/// Разобранный заголовок: где начинается звук и что он из себя представляет.
struct Wav {
    rate: u32,
    channels: u16,
    bits: u16,
    /// Смещение и длина куска `data`.
    at: usize,
    len: usize,
}

/// Разобрать заголовок WAV. `Err` — чем именно файл не годится (человеку это и нужно знать).
///
/// Куски ищутся обходом, а не по фиксированным смещениям: между `fmt ` и `data` редакторы
/// кладут что угодно (`LIST`, `fact`, метаданные), и файл с ними — совершенно обычный WAV.
fn parse(b: &[u8]) -> Result<Wav, &'static str> {
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err("это не WAV (нет заголовка RIFF/WAVE)");
    }
    let u16at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let u32at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let (mut fmt, mut data) = (None, None);
    let mut at = 12;
    while at + 8 <= b.len() {
        let id = &b[at..at + 4];
        let size = u32at(at + 4) as usize;
        let body = at + 8;
        match id {
            b"fmt " if body + 16 <= b.len() => fmt = Some(body),
            b"data" => data = Some((body, size.min(b.len().saturating_sub(body)))),
            _ => {}
        }
        // Куски выравниваются по чётной границе — нечётный размер дополняется нулём.
        at = body + size + (size & 1);
    }
    let (Some(f), Some((dat, len))) = (fmt, data) else {
        return Err("в файле нет куска fmt или data");
    };
    if u16at(f) != 1 {
        return Err("сжатый WAV: играть умеем только несжатые отсчёты (формат 1)");
    }
    Ok(Wav { rate: u32at(f + 4), channels: u16at(f + 2), bits: u16at(f + 14), at: dat, len })
}

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let argv = sys::argv::Argv::take();
    // Аргументы нумеруются С НУЛЯ и имени программы не включают (см. `Argv::get`).
    let Some(path) = argv.get(0) else {
        w("play: чем играть? play <файл.wav>\n");
        sys::exit(2);
    };
    let Some(ep) = snd::find_cap() else {
        w("play: звука в этой машине нет (нет канала к серверу `hda`)\n");
        sys::exit(1);
    };
    let Some(fs) = sys::cap_named("POSIXFS").or_else(|| Some(sys::start_cap(0))) else {
        w("play: файлового сервера нет\n");
        sys::exit(1);
    };

    // Файл читается целиком: самая длинная дорожка, которую здесь заводят, — это системный звук
    // на несколько секунд, а потоковое чтение с диска ради них означало бы держать открытым файл
    // всё время игры и обрабатывать его пропажу посреди дорожки.
    // Размер НЕ спрашиваем: `stat` отвечает по индексу каталога, а открыть файл можно и по
    // одному его имени-корню в store — так, например, лежит всё, что приехало мостом с хоста.
    // Спросив размер заранее, мы отказались бы играть файл, который прекрасно открывается.
    let fd = px::open(fs, path, 0);
    if fd == usize::MAX {
        sys::write(alloc::format!("play: не открывается {}\n", str(path)).as_bytes());
        sys::exit(1);
    }
    let mut file = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = px::read(fs, fd, &mut chunk);
        if n == 0 || n == usize::MAX {
            break;
        }
        file.extend_from_slice(&chunk[..n]);
    }
    px::close(fs, fd);

    // «Пусто» и «не тот формат» — разные беды с разным лечением, и путать их в одном
    // сообщении значит отправить человека искать не там.
    if file.is_empty() {
        sys::write(alloc::format!("play: файл {} пуст (ни байта не прочиталось)\n", str(path)).as_bytes());
        sys::exit(1);
    }
    let wav = match parse(&file) {
        Ok(w) => w,
        Err(why) => {
            sys::write(alloc::format!("play: {}\n", why).as_bytes());
            sys::exit(1);
        }
    };
    if wav.bits != 16 || wav.channels == 0 || wav.channels > 2 {
        sys::write(
            alloc::format!(
                "play: умею 16 бит, один или два канала; здесь {} бит, каналов {}\n",
                wav.bits, wav.channels
            )
            .as_bytes(),
        );
        sys::exit(1);
    }

    // Кольцо заводим ДО открытия потока: право на него уезжает вместе с запросом.
    let Some(ring_cap) = sys::shm_new(snd::RING_BYTES, RING_VA) else {
        w("play: не завелась общая память под кольцо\n");
        sys::exit(1);
    };
    // Серверу отдаём право на ЧТЕНИЕ (писать в наше кольцо ему незачем) — плюс `GRANT`, без
    // которого право не уезжает по IPC вовсе: ядро проверяет его при передаче. Чистое `READ`
    // здесь даёт не отказ, а молчание — вызов не состоится, и выглядит это как «сервер звука не
    // ответил». Та же ловушка описана у окон (`win::RIGHT_SHARE`), и наступить на неё второй раз
    // стоило одного захода.
    let ro = sys::cap_derive(ring_cap, 0b101);
    match snd::open(ep, wav.rate, ro) {
        snd::ST_OK => {}
        snd::ST_BUSY => {
            w("play: звук занят другой программой\n");
            sys::exit(1);
        }
        snd::ST_BAD => {
            sys::write(
                alloc::format!(
                    "play: частоту {} Гц звуковая карта не умеет (умеет 44100, 48000 и их родню)\n",
                    wav.rate
                )
                .as_bytes(),
            );
            sys::exit(1);
        }
        _ => {
            w("play: сервер звука не ответил\n");
            sys::exit(1);
        }
    }

    sys::write(
        alloc::format!(
            "play: {} — {} Гц, {} кан., {} КиБ звука\n",
            str(path), wav.rate, wav.channels, wav.len / 1024
        )
        .as_bytes(),
    );

    // ── подача ───────────────────────────────────────────────────────────────────────────────
    //
    // Счётчики в байтах от начала потока, как и у сервера: так «пусто» и «полно» не путаются.
    // Пишем стерео всегда — моно раздваивается здесь, потому что здесь это стоит одного
    // присваивания, а серверу пришлось бы держать два пути заполнения кольца.
    let ring = RING_VA as *mut u8;
    let mut write: u32 = 0;
    let mut read: u32 = 0;
    let mut src = wav.at;
    let end = wav.at + wav.len;
    let mono = wav.channels == 1;
    loop {
        let free = snd::RING_BYTES as u32 - write.wrapping_sub(read);
        if free >= 4 && src + 2 <= end {
            let lo = file[src];
            let hi = file[src + 1];
            let (l, r) = if mono {
                src += 2;
                ([lo, hi], [lo, hi])
            } else {
                let (a, b) = ([lo, hi], [file[src + 2], file[src + 3]]);
                src += 4;
                (a, b)
            };
            for (k, byte) in [l[0], l[1], r[0], r[1]].into_iter().enumerate() {
                unsafe { ring.add((write as usize + k) % snd::RING_BYTES).write_volatile(byte) };
            }
            write = write.wrapping_add(4);
            // Сервера дёргаем не на каждый кадр, а раз в кусок: иначе на секунду звука уходило
            // бы сорок восемь тысяч вызовов ядра — дороже самого звука в сотни раз.
            if write % 8192 != 0 {
                continue;
            }
        }
        let Some((st, r)) = snd::advance(ep, write) else {
            w("play: сервер звука замолчал\n");
            break;
        };
        if st != snd::ST_OK {
            w("play: сервер звука отказал\n");
            break;
        }
        read = r;
        if src + 2 > end && read >= write {
            break; // всё отдано и всё сыграно
        }
        if snd::RING_BYTES as u32 - write.wrapping_sub(read) < 4 {
            // Кольцо полно — спим примерно на четверть его, а не крутимся: нам тут делать
            // нечего, а процессор нужен всем остальным.
            sys::sleep_ns(120_000_000);
        }
    }
    snd::close(ep);
    sys::exit(0);
}

/// Путь как строка — для сообщений человеку.
fn str(b: &[u8]) -> &str {
    core::str::from_utf8(b).unwrap_or("<не UTF-8>")
}
