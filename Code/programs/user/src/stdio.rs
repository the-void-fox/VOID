//! Соглашение о **чужом stdio** (Веха 98, шаг 4 фазы терминала — [[0014-terminal-ereb]]).
//!
//! До сих пор вывод программы уходил в одну общую консоль ядра, а ввод приходил из её же кольца.
//! Показать чужой вывод в панели было нечем — приватного потока байт у процесса не существовало.
//!
//! **Ядро для этого НЕ менялось.** «Стандартный ввод-вывод» — соглашение userspace, а не свойство
//! машины: ядру незачем знать, что такое строка на экране ([[process-stdio]], решение Б). Поэтому
//! stdio живёт здесь, ровно там же, где уже живут argv, env и преоткрытые права
//! ([[process-contract]]), и работает поверх обычного IPC.
//!
//! ## Как это устроено
//!
//! Хост (терминал, супервизор — кто угодно) при запуске ребёнка:
//! 1. даёт ему стартовое право на СВОЙ эндпоинт;
//! 2. объявляет в окружении `STDIO=<индекс права>`.
//!
//! Ребёнок читает `STDIO`, берёт право по индексу — и все его `write`/`read_stdin` идут туда
//! обычным `SYS_CALL`. Хост при этом просто сервер: у него уже есть реактор и отложенные ответы
//! (Веха 93), которые дают ребёнку блокирующее чтение, не блокируя сам хост.
//!
//! **Почему индекс в окружении, а не поиск перебором.** «Проба» эндпоинта — это отправка ему
//! сообщения, то есть побочный эффект; искать так своё право нельзя. С экраном (Веха 97) перебор
//! был допустим — там проба безобидна, `SYS_VIDEO_INFO` ничего не меняет.
//!
//! **Чего это не покрывает:** процессы мимо этой библиотеки (linux-abi) продолжают писать в общую
//! консоль. Принято сознательно — у них своя персоналия, и мост к ней это отдельная работа.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Байты от ребёнка хосту. Ответ — пустой (важен лишь факт приёма: он же и есть регулировка
/// потока — ребёнок не убежит вперёд, пока хост не разгребёт).
pub const OP_STDOUT: usize = 1;
/// Запрос ввода. Хост отвечает байтами; ответ может быть ОТЛОЖЕННЫМ — тогда ребёнок спит в
/// `SYS_CALL` ровно так же, как спал бы в `SYS_READ`.
pub const OP_STDIN: usize = 2;

/// Спросить размер своего окна в знакоместах. Ответ — 4 байта: `cols` и `rows` (u16 LE).
///
/// Аналог `TIOCSWINSZ` из Unix, но **опрашиваемый, а не проталкиваемый**: сигналов у нас нет
/// (`SIGWINCH` слать нечем), а ходить к хосту программа и так умеет. Программа спрашивает,
/// когда ей нужно, — на старте и при перерисовке.
pub const OP_WINSIZE: usize = 3;

/// Потолок одной посылки. Ограничение задаёт приёмный буфер СЕРВЕРА, а клиент его не знает —
/// поэтому размер куска согласован заранее, как в сетевом протоколе (Веха 93).
pub const CHUNK: usize = 1024;

/// Кэш найденного эндпоинта: искать его на каждый `write` — разбирать окружение на каждую строку.
static EP: AtomicUsize = AtomicUsize::new(0);
static PROBED: AtomicBool = AtomicBool::new(false);

/// Эндпоинт stdio этого процесса (`None` — его нет, работаем через консоль ядра).
///
/// Идемпотентна: гонка двух нитей в худшем случае разберёт окружение дважды и запишет то же
/// самое значение.
pub fn endpoint() -> Option<usize> {
    if !PROBED.load(Ordering::Relaxed) {
        let cap = probe().unwrap_or(super::NO_CAP);
        EP.store(cap, Ordering::Relaxed);
        PROBED.store(true, Ordering::Relaxed);
    }
    match EP.load(Ordering::Relaxed) {
        c if c == super::NO_CAP => None,
        c => Some(c),
    }
}

/// Разобрать окружение и достать право по индексу из `STDIO=`.
fn probe() -> Option<usize> {
    let mut buf = [0u8; 512];
    let n = super::env(&mut buf);
    let n = n.min(buf.len());
    let mut idx = None;
    for entry in buf[..n].split(|&b| b == 0) {
        if let Some(v) = entry.strip_prefix(b"STDIO=") {
            let mut val = 0usize;
            let mut any = false;
            for &d in v {
                if d.is_ascii_digit() {
                    val = val * 10 + (d - b'0') as usize;
                    any = true;
                } else {
                    any = false;
                    break;
                }
            }
            if any {
                idx = Some(val);
            }
            break;
        }
    }
    let cap = super::start_cap(idx?);
    (cap != super::NO_CAP).then_some(cap)
}

/// Отправить вывод хосту. `false` — не вышло (нет эндпоинта или хост не ответил); вызывающий
/// обязан откатиться на консоль ядра, а не молча потерять текст.
pub fn write(bytes: &[u8]) -> bool {
    let Some(ep) = endpoint() else {
        return false;
    };
    let mut rep = [0u8; 8];
    for part in bytes.chunks(CHUNK) {
        if super::call(ep, OP_STDOUT, part, &mut rep) == super::NO_CAP {
            return false; // хост умер или отказал — дальше слать бессмысленно
        }
    }
    true
}

/// Размер своего окна `(колонок, строк)`; `None` — хоста нет (пишем в консоль ядра, её
/// геометрия программе недоступна) либо хост не ответил.
pub fn win_size() -> Option<(u16, u16)> {
    let ep = endpoint()?;
    let mut rep = [0u8; 4];
    let n = super::call(ep, OP_WINSIZE, &[], &mut rep);
    if n != 4 {
        return None;
    }
    let cols = u16::from_le_bytes([rep[0], rep[1]]);
    let rows = u16::from_le_bytes([rep[2], rep[3]]);
    (cols > 0 && rows > 0).then_some((cols, rows))
}

/// Запросить ввод у хоста. `None` — эндпоинта нет; иначе число прочитанных байт (0 — конец ввода).
pub fn read(buf: &mut [u8]) -> Option<usize> {
    let ep = endpoint()?;
    match super::call(ep, OP_STDIN, &[], buf) {
        n if n == super::NO_CAP => None,
        n => Some(n.min(buf.len())),
    }
}
