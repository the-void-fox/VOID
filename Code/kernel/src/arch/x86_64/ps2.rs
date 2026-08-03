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
        cmd |= 1 << 6; // бит6: трансляция скан-кодов в набор 1
        cmd &= !(1 << 4); // бит4=0: тактирование клавиатуры НЕ отключено
        // Записать управляющий байт (команда 0x60, затем значение в 0x60).
        wait_write();
        outb(STATUS, 0x60);
        wait_write();
        outb(DATA, cmd);
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
            let sc = inb(DATA);
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
                    continue;
                }
                if sc & RELEASE == 0 {
                    let seq: &[u8] = match sc {
                        0x48 => b"\x1b[A", // Up
                        0x50 => b"\x1b[B", // Down
                        0x4D => b"\x1b[C", // Right
                        0x4B => b"\x1b[D", // Left
                        0x47 => b"\x1b[H", // Home
                        0x4F => b"\x1b[F", // End
                        0x53 => b"\x1b[3~", // Delete
                        _ => b"",
                    };
                    for &b in seq {
                        super::rx_push(b);
                    }
                }
                continue;
            }
            let released = sc & RELEASE != 0;
            let code = sc & !RELEASE;
            match code {
                SC_LSHIFT | SC_RSHIFT => SHIFT = !released,
                SC_LCTRL => CTRL = !released,
                SC_CAPS => {
                    if !released {
                        CAPS = !CAPS;
                    }
                }
                _ if !released && (code as usize) < MAP.len() => {
                    let (lo, up) = MAP[code as usize];
                    if lo != 0 {
                        // Shift даёт верхний вариант; CapsLock влияет ТОЛЬКО на буквы.
                        let is_letter = lo.is_ascii_lowercase();
                        let upper = SHIFT ^ (CAPS && is_letter);
                        let ch = if upper { up } else { lo };
                        // Ctrl+буква → управляющий байт (Ctrl-A = 1 … Ctrl-Z = 26), как это
                        // делает любой терминал. Прочие сочетания с Ctrl отдаём как есть.
                        super::rx_push(if CTRL && ch.is_ascii_alphabetic() {
                            ch.to_ascii_uppercase() - b'@'
                        } else {
                            ch
                        });
                    }
                }
                _ => {}
            }
        }
    }
}
