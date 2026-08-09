//! PS/2-клавиатура (контроллер 8042) — ввод на РЕАЛЬНОМ железе (Веха 42).
//!
//! У ноутбука/мини-ПК нет COM-порта, зато внутренняя клавиатура висит на контроллере 8042 как
//! PS/2 (это делает встроенный контроллер платы). Читаем скан-коды **набора 1** из порта 0x60,
//! переводим в ASCII с учётом Shift/CapsLock и кладём в то же кольцо консоли, что и COM1
//! ([`super::rx_push`]) — `vsh` не различает, откуда пришёл байт. Наполняется двумя путями:
//! опрос на тиках таймера ([`super::console_drain`]) и IRQ1 через IOAPIC (будит сон до ввода,
//! когда таймер замаскирован в `wait_stdin`) — оба маршрутизированы на общий вектор консоли.
//!
//! На QEMU (`-nographic`) ввод идёт через serial, PS/2 молчит — драйвер безвреден. На реальной
//! машине именно он делает `vsh>` интерактивным.

const DATA: u16 = 0x60; // чтение скан-кодов / запись данных устройству
const STATUS: u16 = 0x64; // чтение статуса / запись команд контроллеру
const STAT_OUT_FULL: u8 = 1 << 0; // есть байт для чтения из 0x60
const STAT_IN_FULL: u8 = 1 << 1; // входной буфер занят (нельзя писать)
/// Веха 115: байт пришёл из ВТОРОГО порта (мышь), а не от клавиатуры. Тот же 0x60 на двоих —
/// различить их можно только этим битом статуса, и прочитать его надо ДО чтения данных.
const STAT_AUX: u8 = 1 << 5;

#[inline]
unsafe fn outb(port: u16, v: u8) {
    core::arch::asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack));
}
#[inline]
unsafe fn inb(port: u16) -> u8 {
    let v: u8;
    core::arch::asm!("in al, dx", in("dx") port, out("al") v, options(nomem, nostack));
    v
}

/// Дождаться, пока входной буфер контроллера освободится (можно писать). Спин ограничен —
/// на отсутствующем/зависшем 8042 не виснем навечно (реального железа без него у нас нет,
/// но осторожность дешёвая).
unsafe fn wait_write() {
    for _ in 0..100_000 {
        if inb(STATUS) & STAT_IN_FULL == 0 {
            return;
        }
    }
}

// ── мышь (Веха 115) ──────────────────────────────────────────────────────────
//
// Стандартный PS/2-пакет — три байта: флаги (кнопки, знаки, переполнения), затем смещения по X
// и по Y. Знак хранится ОТДЕЛЬНЫМ битом в первом байте, а не в самом смещении, поэтому байт
// доводится до i16 вручную. Ось Y у мыши направлена ВВЕРХ, у экрана — вниз; переворачиваем один
// раз здесь, чтобы каждый потребитель не помнил об этом сам.

/// Есть ли мышь (успела ли ответить на команды включения).
static mut MOUSE: bool = false;
/// Накопитель пакета и позиция в нём.
static mut PKT: [u8; 3] = [0; 3];
static mut PKT_AT: usize = 0;

/// Отправить команду мыши через контроллер (0xD4 = «следующий байт — во второй порт»).
/// `true` — устройство ответило `0xFA` (ACK).
unsafe fn mouse_cmd(byte: u8) -> bool {
    wait_write();
    outb(STATUS, 0xD4);
    wait_write();
    outb(DATA, byte);
    for _ in 0..100_000 {
        if inb(STATUS) & STAT_OUT_FULL != 0 {
            return inb(DATA) == 0xFA;
        }
    }
    false
}

/// Есть ли рабочая мышь — для отчёта на загрузке.
pub fn mouse_present() -> bool {
    unsafe { MOUSE }
}

/// Разобрать очередной байт от мыши; полный пакет уходит в кольцо событий.
unsafe fn mouse_byte(b: u8) {
    // Синхронизация: у первого байта бит 3 всегда 1. Если мы «посреди» пакета и это не так —
    // значит поток сбился (потеряли байт), и начинать надо заново, а не копить мусор.
    if PKT_AT == 0 && b & 0x08 == 0 {
        return;
    }
    PKT[PKT_AT] = b;
    PKT_AT += 1;
    if PKT_AT < 3 {
        return;
    }
    PKT_AT = 0;
    let f = PKT[0];
    // Переполнение счётчика — данные бессмысленны, пакет выбрасываем целиком.
    if f & 0xC0 != 0 {
        return;
    }
    let dx = PKT[1] as i16 - if f & 0x10 != 0 { 256 } else { 0 };
    let dy = PKT[2] as i16 - if f & 0x20 != 0 { 256 } else { 0 };
    super::mouse_push(dx, -dy, f & 0x07);
}

// ── состояние модификаторов ──────────────────────────────────────────────────
static mut SHIFT: bool = false;
static mut CAPS: bool = false;
/// Веха 97.1 — **Ctrl**. Не отслеживался вовсе, из-за чего на реальном железе не работал НИ ОДИН
/// управляющий аккорд: Ctrl-C, Ctrl-D, Ctrl-U просто печатали букву. В QEMU это не всплывало —
/// там ввод идёт через serial, и терминал хоста шлёт готовый управляющий байт. Найдено
/// владельцем на X54C.
static mut CTRL: bool = false;
/// Веха 45 — видели префикс 0xE0 (расширенная клавиша: стрелки/Home/End/Del).
static mut EXT: bool = false;
/// Веха 119 — **Alt и Super**. До неё их не существовало вовсе: драйвер знал Shift и Ctrl,
/// потому что от него требовались только печатные байты. Оконному менеджеру нужны аккорды, а
/// `Super+L` без Super — это просто `l`.
static mut ALT: bool = false;
static mut SUPER: bool = false;

/// Биты маски модификаторов в событии (userspace знает их по тем же номерам).
const M_SHIFT: u8 = 1;
const M_CTRL: u8 = 2;
const M_ALT: u8 = 4;
const M_SUPER: u8 = 8;

/// Скан-коды модификаторов, которых раньше не разбирали.
const SC_LALT: u8 = 0x38;
const SC_LSUPER: u8 = 0x5B; // приходит с префиксом 0xE0
const SC_RSUPER: u8 = 0x5C;

/// Текущая маска модификаторов.
unsafe fn mods() -> u8 {
    let mut m = 0;
    if SHIFT { m |= M_SHIFT }
    if CTRL { m |= M_CTRL }
    if ALT { m |= M_ALT }
    if SUPER { m |= M_SUPER }
    m
}

/// Скан-код набора 1 → код клавиши VOID. Печатные нумеруются своим ASCII (в НЕсдвинутом виде),
/// прочие — числами выше 0x100. Так строка конфига `"Super+L"` и буква `l` — про одно и то же.
fn keysym(sc: u8, ext: bool) -> u16 {
    if ext {
        return match sc {
            0x48 => 0x112, // Up
            0x50 => 0x113, // Down
            0x4D => 0x111, // Right
            0x4B => 0x110, // Left
            0x47 => 0x114, // Home
            0x4F => 0x115, // End
            0x49 => 0x116, // PageUp
            0x51 => 0x117, // PageDown
            0x53 => 0x105, // Delete
            0x52 => 0x106, // Insert
            0x1C => 0x101, // Enter на цифровой части
            SC_LSUPER | SC_RSUPER => 0x133,
            SC_LALT => 0x132,
            SC_LCTRL => 0x131,
            _ => 0,
        };
    }
    match sc {
        0x01 => 0x102, // Esc
        0x0E => 0x104, // Backspace
        0x0F => 0x103, // Tab
        0x1C => 0x101, // Enter
        0x2A | 0x36 => 0x130,
        SC_LCTRL => 0x131,
        SC_LALT => 0x132,
        0x3B..=0x44 => 0x120 + (sc - 0x3B) as u16, // F1..F10
        0x57 => 0x12A,                             // F11
        0x58 => 0x12B,                             // F12
        _ => {
            let i = sc as usize;
            if i < MAP.len() && MAP[i].0 != 0 {
                MAP[i].0 as u16 // печатная клавиша — её НЕсдвинутый символ
            } else {
                0
            }
        }
    }
}

// ── скан-код набора 1 → ASCII (US-раскладка) ─────────────────────────────────
// Индекс — скан-код нажатия (0x00..0x3A). Пара (обычный, с Shift). 0 — нет символа.
// Хватает того, что набирают в vsh: буквы, цифры, пробел, Enter, Backspace, / - . : и пр.
#[rustfmt::skip]
static MAP: [(u8, u8); 0x40] = [
    (0,0),(27,27),(b'1',b'!'),(b'2',b'@'),(b'3',b'#'),(b'4',b'$'),(b'5',b'%'),(b'6',b'^'),
    (b'7',b'&'),(b'8',b'*'),(b'9',b'('),(b'0',b')'),(b'-',b'_'),(b'=',b'+'),(8,8),(b'\t',b'\t'),
    (b'q',b'Q'),(b'w',b'W'),(b'e',b'E'),(b'r',b'R'),(b't',b'T'),(b'y',b'Y'),(b'u',b'U'),(b'i',b'I'),
    (b'o',b'O'),(b'p',b'P'),(b'[',b'{'),(b']',b'}'),(b'\r',b'\r'),(0,0),(b'a',b'A'),(b's',b'S'),
    (b'd',b'D'),(b'f',b'F'),(b'g',b'G'),(b'h',b'H'),(b'j',b'J'),(b'k',b'K'),(b'l',b'L'),(b';',b':'),
    (b'\'',b'"'),(b'`',b'~'),(0,0),(b'\\',b'|'),(b'z',b'Z'),(b'x',b'X'),(b'c',b'C'),(b'v',b'V'),
    (b'b',b'B'),(b'n',b'N'),(b'm',b'M'),(b',',b'<'),(b'.',b'>'),(b'/',b'?'),(0,0),(b'*',b'*'),
    (0,0),(b' ',b' '),(0,0),(0,0),(0,0),(0,0),(0,0),(0,0),
];

const SC_LSHIFT: u8 = 0x2A;
const SC_RSHIFT: u8 = 0x36;
const SC_CAPS: u8 = 0x3A;
/// Левый Ctrl. Правый приходит как `0xE0 0x1D` — тот же код за префиксом расширенных клавиш.
const SC_LCTRL: u8 = 0x1D;
const RELEASE: u8 = 0x80; // старший бит скан-кода набора 1 = отпускание

/// Веха 42 — настроить контроллер 8042: включить порт клавиатуры, разрешить генерацию IRQ1 и
/// трансляцию в набор 1, вычистить остатки. BIOS обычно уже это сделал, но переинициализация
/// безопасна и не зависит от его настроек.
pub fn init() {
    unsafe {
        // Слить всё, что накопилось во входном буфере.
        for _ in 0..16 {
            if inb(STATUS) & STAT_OUT_FULL == 0 {
                break;
            }
            let _ = inb(DATA);
        }
        // Разрешить первый PS/2-порт (клавиатуру).
        wait_write();
        outb(STATUS, 0xAE);
        // Прочитать управляющий байт (команда 0x20 → ответ в 0x60).
        wait_write();
        outb(STATUS, 0x20);
        for _ in 0..100_000 {
            if inb(STATUS) & STAT_OUT_FULL != 0 {
                break;
            }
        }
        let mut cmd = inb(DATA);
        cmd |= 1 << 0; // бит0: прерывание клавиатуры (IRQ1) вкл
        cmd |= 1 << 1; // бит1: прерывание мыши (IRQ12) вкл — Веха 115
        cmd |= 1 << 6; // бит6: трансляция скан-кодов в набор 1
        cmd &= !(1 << 4); // бит4=0: тактирование клавиатуры НЕ отключено
        cmd &= !(1 << 5); // бит5=0: тактирование мыши НЕ отключено
        // Записать управляющий байт (команда 0x60, затем значение в 0x60).
        wait_write();
        outb(STATUS, 0x60);
        wait_write();
        outb(DATA, cmd);
        // Веха 115 — включить ВТОРОЙ порт (мышь) и саму мышь. Порядок важен: сперва порт
        // разрешён контроллером (0xA8), только потом устройство слышит команды.
        wait_write();
        outb(STATUS, 0xA8);
        // 0xF6 — настройки по умолчанию (частота 100/с, разрешение, масштаб 1:1),
        // 0xF4 — начать слать пакеты. Без второй мышь молчит, сколько её ни двигай.
        MOUSE = mouse_cmd(0xF6) && mouse_cmd(0xF4);
        // Ещё раз слить возможный «ack».
        for _ in 0..16 {
            if inb(STATUS) & STAT_OUT_FULL == 0 {
                break;
            }
            let _ = inb(DATA);
        }
    }
}

/// Веха 42 — вычерпать все готовые скан-коды и положить ASCII-байты в кольцо консоли
/// ([`super::rx_push`]). Зовётся из [`super::console_drain`] (тик/IRQ1) с выключенными
/// прерываниями — гонок за состоянием модификаторов и кольцом нет.
pub fn drain() {
    unsafe {
        let mut guard = 0;
        // `STATUS == 0xFF` — контроллера нет (открытая шина); иначе читаем, пока есть коды,
        // но не больше страховочного лимита (защита от флуда/залипшего бита).
        while inb(STATUS) != 0xff && inb(STATUS) & STAT_OUT_FULL != 0 && guard < 256 {
            guard += 1;
            // Клавиатура и мышь делят порт данных; чей это байт, говорит бит статуса — и
            // прочитать его надо ДО чтения 0x60, иначе признак уже потерян.
            let aux = inb(STATUS) & STAT_AUX != 0;
            let sc = inb(DATA);
            if aux {
                mouse_byte(sc);
                continue;
            }
            // 0xE0 — префикс расширенных клавиш; следующий байт — код стрелки/Home/End/Del.
            if sc == 0xE0 {
                EXT = true;
                continue;
            }
            if EXT {
                // Веха 45: расширенная клавиша → стандартная ANSI-последовательность (её понимает
                // и терминал QEMU, и разбор в vga.rs/vsh). Только на НАЖАТИЕ (старший бит = 0).
                EXT = false;
                // Правый Ctrl (`0xE0 0x1D`) — такой же модификатор, как левый, и приходит здесь.
                if sc & !RELEASE == SC_LCTRL {
                    CTRL = sc & RELEASE == 0;
                    super::key_push(0x131, mods(), CTRL, 0);
                    continue;
                }
                // Веха 119 — Super (клавиша с логотипом) и правый Alt приходят тем же префиксом.
                if sc & !RELEASE == SC_LSUPER || sc & !RELEASE == SC_RSUPER {
                    SUPER = sc & RELEASE == 0;
                    super::key_push(0x133, mods(), SUPER, 0);
                    continue;
                }
                if sc & !RELEASE == SC_LALT {
                    ALT = sc & RELEASE == 0;
                    super::key_push(0x132, mods(), ALT, 0);
                    continue;
                }
                if sc & RELEASE == 0 {
                    // Веха 116 — PageUp/PageDown появились здесь же: без них листать вывод
                    // на реальной машине было нечем (на QEMU они приходят из serial готовой
                    // последовательностью, поэтому пробел не замечался).
                    let seq: &[u8] = match (sc, SHIFT) {
                        (0x48, _) => b"\x1b[A",  // Up
                        (0x50, _) => b"\x1b[B",  // Down
                        (0x4D, _) => b"\x1b[C",  // Right
                        (0x4B, _) => b"\x1b[D",  // Left
                        (0x47, _) => b"\x1b[H",  // Home
                        (0x4F, _) => b"\x1b[F",  // End
                        (0x53, _) => b"\x1b[3~", // Delete
                        // Shift несём отдельным параметром (`;2`) — так же, как это делают
                        // xterm-совместимые терминалы: Shift+PageUp принято отдавать
                        // ТЕРМИНАЛУ (прокрутка), а голый PageUp — программе.
                        (0x49, false) => b"\x1b[5~",
                        (0x49, true) => b"\x1b[5;2~",
                        (0x51, false) => b"\x1b[6~",
                        (0x51, true) => b"\x1b[6;2~",
                        _ => b"",
                    };
                    for &b in seq {
                        super::rx_push(b);
                    }
                }
                super::key_push(keysym(sc & !RELEASE, true), mods(), sc & RELEASE == 0, 0);
                continue;
            }
            let released = sc & RELEASE != 0;
            let code = sc & !RELEASE;
            match code {
                SC_LSHIFT | SC_RSHIFT => SHIFT = !released,
                SC_LCTRL => CTRL = !released,
                SC_LALT => ALT = !released,
                SC_CAPS => {
                    if !released {
                        CAPS = !CAPS;
                    }
                }
                _ => {}
            }
            // Веха 119 — СОБЫТИЕ выдаётся на любую клавишу, включая модификаторы и отпускания:
            // оконному менеджеру нужно знать состояние, а не только напечатанное. ASCII считается
            // тут же и едет в событии, чтобы раскладка осталась в одном месте.
            let mut ascii = 0u8;
            if (code as usize) < MAP.len() {
                let (lo, up) = MAP[code as usize];
                if lo != 0 {
                    // Shift даёт верхний вариант; CapsLock влияет ТОЛЬКО на буквы.
                    let is_letter = lo.is_ascii_lowercase();
                    let upper = SHIFT ^ (CAPS && is_letter);
                    let ch = if upper { up } else { lo };
                    // Ctrl+буква → управляющий байт (Ctrl-A = 1 … Ctrl-Z = 26), как это делает
                    // любой терминал. Прочие сочетания с Ctrl отдаём как есть.
                    ascii = if CTRL && ch.is_ascii_alphabetic() {
                        ch.to_ascii_uppercase() - b'@'
                    } else {
                        ch
                    };
                }
            }
            super::key_push(keysym(code, false), mods(), !released, ascii);
            if !released && ascii != 0 {
                super::rx_push(ascii);
            }
        }
    }
}
