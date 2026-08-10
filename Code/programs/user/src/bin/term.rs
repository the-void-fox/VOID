//! `term` — терминал-мультиплексор VOID на крейтах ereb (Вехи 97 и 99, ADR 0014).
//!
//! Веха 97 сняла потолок 256 глифов: экран стал пикселями, а глифы — настоящим TrueType.
//! Веха 99 добавила то, ради чего вся фаза и затевалась: **панели с живыми процессами**.
//!
//! ```text
//!   клавиатура ─→ префикс? ─да→ команда мультиплексора (разбить, перейти, закрыть)
//!                     └─нет→ отложенный ответ ребёнку ФОКУСНОЙ панели (его read_stdin)
//!
//!   ребёнок ──IPC(OP_STDOUT)──→ грид своей панели ──→ общий кадр ──→ фреймбуфер
//! ```
//!
//! ## Почему это устроено именно так
//!
//! - **Хост — обычный IPC-сервер.** Ничего нового: `net-srv` устроен так же, включая отложенные
//!   ответы. Ребёнок, спящий в `read_stdin`, спит в `SYS_CALL` к нам, и мы отвечаем ему, когда
//!   приходят клавиши. Никакого «драйвера терминала» в ядре не появилось.
//! - **Реактор не имеет права уснуть ни на одном источнике.** Клавиатура читается
//!   неблокирующе ([`sys::read_console_nonblock`], Веха 99), вывод детей — `try_recv`. Сон —
//!   только когда пусто и то и другое, и только со сроком.
//! - **Раскладка — `ereb-mux`**: дерево разбиений и навигация уже написаны и покрыты тестами в
//!   апстриме, своей геометрии не заводим.
//!
//! ## Управление
//!
//! **Ctrl-A** переводит в режим ПАНЕЛЕЙ (полоса внизу желтеет), где клавиши значат команды:
//! `|` разбить вертикально · `-` горизонтально · `h j k l` или стрелки — перейти в соседнюю
//! панель · `o` следующая по кругу · `x` закрыть · `q` выйти · Ctrl-A — отдать сам Ctrl-A
//! программе. Любая другая клавиша возвращает в обычный режим.
//!
//! Схема живёт не в `if`-ах, а в таблице биндингов `ereb-input` (Веха 99.4) — той же, что на
//! Linux.
//!
//! ## Настройка (Веха 100)
//!
//! Схема и вид **задаются конфигом системы**, а не кодом: `/etc/system/terminal.vv` — обычный
//! модуль конфигурации VOID, `rebuild` кладёт его результат в поколение, `term` читает активное
//! поколение из store и берёт свои строки:
//!
//! ```text
//! terminal font-size 18            ← настройка
//! bind normal C-a mode-pane        ← клавиша
//! bind pane | split-v
//! ```
//!
//! Конфиг у системы ОДИН (одна история поколений, один откат), читателей несколько: ядро берёт
//! `service`/`shell`, терминал — `terminal`/`bind`. На Linux ereb настраивается KDL-файлом; тут
//! незачем заводить второй язык конфигурации, когда у системы уже есть свой.
//!
//! Схема по умолчанию записана тем же текстом ([`DEFAULT_CONF`]) и разбирается тем же кодом:
//! два пути «из конфига» и «зашитый» разъехались бы при первой же правке.
//!
//! **Перечитать на ходу** — команда `reload` (по умолчанию `r` в режиме панелей, Веха 101):
//! `rebuild` в любой панели, затем `reload` — новые клавиши и кегль действуют сразу, без
//! перезагрузки. Живые панели и их процессы не трогаются: меняется вид и таблица клавиш. Это
//! первый шаг «switch без ребута», и он целиком в userspace — поколение уже лежит в store, а
//! терминал умеет его читать.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use ereb_core::{Cell, Color, Grid, NamedColor};
use ereb_input::{Action, Bindings, KeyEvent, Keysym, ModMask, Mode};
use ereb_mux::{neighbor, Area, Direction, PaneId, PaneRect, SplitDirection, SplitTree};
use ereb_render::{GlyphCache, GridRenderer, Palette, Surface, TtfFont};
use void_user as sys;
use void_user::{stdio, Wait};

/// Куча: кадр 1280×800 RGBA (4 МиБ), прочитанный шрифт, гриды панелей, кэш глифов.
#[global_allocator]
static ALLOC: sys::heap::Heap<{ 48 * 1024 * 1024 }> = sys::heap::Heap::new();

// Запасной шрифт 8×16 — на случай, когда настоящего нет (Веха 114).
#[path = "../bitfont.rs"]
mod bitfont;
use bitfont::BitmapFont;
// Профиль пакетов — чтобы найти шрифт по ИМЕНИ, а не по хэшу пути (тот же приём, что PATH).
#[allow(dead_code)] // писательская половина профиля нужна `pkg`, терминалу — чтение
#[path = "../profile.rs"]
mod profile;

/// Где в пакете лежат шрифты. Обходится вглубь: угадывать раскладку бесполезно — у
/// `nerd-fonts-fira-mono` это `share/fonts/opentype/NerdFonts/FiraMono/…otf`, у других пакетов
/// `share/fonts/truetype/…ttf`. Зато глубина ограничена: пакет чужой, и бродить по нему без
/// потолка — способ подвесить терминал ещё до первого кадра.
const FONT_ROOT: &str = "share/fonts";
const FONT_DEPTH: usize = 4;
/// Сколько файлов готовы перебрать в одном пакете.
const FONT_MAX: usize = 256;

/// Окно фреймбуфера в нашем адресном пространстве: между образом и кучей.
const FB_VA: usize = 0x5000_0000;

/// Размер окна терминала, когда он живёт под композитором (Веха 118).
const WIN_W: u16 = 900;
const WIN_H: u16 = 520;

/// Куда терминал кладёт кадр.
///
/// Один бинарь, два режима — и выбор делается САМ, по наличию композитора в окружении. Так
/// `term` остаётся и спасательным шеллом на голой машине (владеет экраном), и обычным окном,
/// когда окна есть. Заводить ради этого две программы значило бы поддерживать два терминала.
enum Out {
    /// Экран целиком: пишем прямо во фреймбуфер (`mmio:fb`).
    Screen,
    /// Окно композитора: собираем кадр в своей памяти и отдаём объектом (Веха 117).
    Window { win: sys::win::Window, buf: Vec<u8>, store: usize },
}

impl Out {
    /// Адрес, по которому лежит левый верхний пиксель кадра.
    fn base(&self) -> usize {
        match self {
            Out::Screen => FB_VA,
            Out::Window { buf, .. } => buf.as_ptr() as usize,
        }
    }
    /// Отдать композитору ПОЛОСУ строк `y0..y1` (в пикселях). Экрану предъявлять нечего —
    /// там кадр уже на месте.
    ///
    /// Полосу, а не кадр: полный кадр окна 900×520 — это 1,87 МБ, и по объекту на каждое
    /// нажатие клавиши кончало кучу ядра за восемь букв (Веха 118). Платим за изменившееся.
    fn present_rows(&self, info: &sys::VideoInfo, y0: usize, y1: usize) {
        let Out::Window { win, buf, store } = self else { return };
        let y0 = y0.min(info.height);
        let y1 = y1.min(info.height);
        if y1 <= y0 {
            return;
        }
        let row = info.pitch;
        win.present_rect(
            *store,
            &buf[y0 * row..y1 * row],
            0,
            y0 as u16,
            info.width as u16,
            (y1 - y0) as u16,
        );
    }
    fn windowed(&self) -> bool {
        matches!(self, Out::Window { .. })
    }
}

/// Настройки по умолчанию — тем же текстом, каким их задаёт конфиг поколения. Так «как оно
/// устроено из коробки» читается глазами, а разбор остаётся ОДИН: две ветки, «зашитая» и
/// «из конфига», разъехались бы при первой же правке схемы.
///
/// Без `repl` vvsh печатает справку и выходит — панель умирала мгновенно, и выглядело это как
/// «мультиплексор не работает».
const DEFAULT_CONF: &str = "\
terminal font-size 18
terminal shell bin/vvsh
terminal shell-args repl
bind normal C-a mode-pane
bind pane C-a literal-prefix
bind pane | split-v
bind pane - split-h
bind pane o next-pane
bind pane x close
bind pane q quit
bind pane r reload
bind pane h go-left
bind pane j go-down
bind pane k go-up
bind pane l go-right
bind pane Left go-left
bind pane Down go-down
bind pane Up go-up
bind pane Right go-right
bind pane PageUp scroll-up
bind pane PageDown scroll-down
bind pane Home scroll-top
bind pane End scroll-bottom
bind normal S-PageUp scroll-up
bind normal S-PageDown scroll-down
";


// ── клавиши: байты консоли → события ereb (Веха 99.4) ────────────────────────
//
// Управление раньше было заглушкой: один префикс и пять жёстко зашитых букв. Теперь работает
// то, что уже написано и покрыто тестами в апстриме ereb: таблица биндингов, режимы и навигация
// по направлениям (`ereb_input`, `ereb_mux::neighbor`).
//
// Промежуточное звено пришлось написать здесь: ereb на Linux получает события от xkbcommon, а
// нам консоль отдаёт СЫРЫЕ БАЙТЫ. Значит нужен разбор — управляющие коды, ASCII и ANSI-по-
// следовательности стрелок. Ровно тот же разбор делает любой терминал на своём входе.

/// Конечный автомат разбора: терминал шлёт стрелки как `ESC [ A`, и решить, что это, можно
/// только увидев следующие байты.
#[derive(Clone, Copy, PartialEq)]
enum KeyScan {
    Ground,
    Esc,
    /// Внутри `ESC [ …`: копим числовые параметры. До Вехи 116 они просто ГЛОТАЛИСЬ, и любая
    /// последовательность вида `ESC [ N ~` считалась Delete — то есть PageUp работал как
    /// Delete, а прокрутки не было в принципе.
    Csi {
        num: u16,
        /// Второй параметр (`ESC [ 5 ; 2 ~`) — модификатор xterm: 2 = Shift, 5 = Ctrl.
        modn: u16,
        /// Видели `;` — цифры идут во второй параметр.
        after_semi: bool,
    },
    /// Веха 120 — хвост многобайтного символа UTF-8. Без этого состояния каждый байт выше 0x7F
    /// становился ОТДЕЛЬНОЙ клавишей с текстом `b as char`, то есть кодовой точкой Latin-1, и
    /// [`encode`] честно перекодировал её обратно в UTF-8 — уже двумя байтами. Русская буква,
    /// пришедшая двумя байтами, доезжала до программы четырьмя: набрать кириллицу в панели
    /// терминала было нельзя вовсе (нашлось, когда в панели появился редактор).
    Utf8 {
        need: u8,
        len: u8,
        buf: [u8; 4],
    },
}

/// Разобрать байт. `Some(event)` — клавиша сложилась; `None` — ждём продолжения.
fn decode(state: &mut KeyScan, b: u8) -> Option<KeyEvent> {
    match *state {
        KeyScan::Ground => match b {
            0x1b => {
                *state = KeyScan::Esc;
                None
            }
            // Клавиши с ИМЕНЕМ — раньше Ctrl+буквы: их байты (0x08, 0x09, 0x0d) лежат внутри
            // диапазона управляющих, и на байтовой консоли Backspace неотличим от Ctrl-H,
            // Tab от Ctrl-I, Enter от Ctrl-M. Читаем их как названные клавиши: именно их
            // человек и нажал. Цена — Ctrl-H/I/M нельзя привязать; это честнее, чем «Enter не
            // работает, потому что он на самом деле Ctrl-M» (до Вехи 100 так и было — три
            // ветки ниже были недостижимы, а Enter спасал лишь обратный перевод в [`encode`]).
            0x7f | 0x08 => Some(KeyEvent::new(Keysym::BACKSPACE, ModMask::empty())),
            b'\r' | b'\n' => Some(KeyEvent::new(Keysym::RETURN, ModMask::empty())),
            b'\t' => Some(KeyEvent::new(Keysym::TAB, ModMask::empty())),
            // Ctrl+буква приходит управляющим байтом 1..26 — восстанавливаем букву и модификатор.
            0x01..=0x1a => {
                let ch = (b'a' + b - 1) as char;
                Some(KeyEvent::new(Keysym(ch as u32), ModMask::CTRL))
            }
            // Начало многобайтного символа: сам по себе он не клавиша, ждём продолжения.
            0xc0..=0xf7 => {
                let need = if b >= 0xf0 {
                    4
                } else if b >= 0xe0 {
                    3
                } else {
                    2
                };
                let mut buf = [0u8; 4];
                buf[0] = b;
                *state = KeyScan::Utf8 { need, len: 1, buf };
                None
            }
            // Одинокий байт продолжения (0x80..0xBF) или 0xF8+ — не начало символа: глотаем,
            // иначе он уедет в программу обломком чужой буквы.
            0x80..=0xbf | 0xf8..=0xff => None,
            _ => {
                // Печатный байт: keysym совпадает с кодом символа (латиница ASCII).
                let mut k = KeyEvent::new(Keysym(b as u32), ModMask::empty());
                k.text.push(b as char);
                Some(k)
            }
        },
        KeyScan::Utf8 { need, len, mut buf } => {
            if b & 0xc0 != 0x80 {
                // Последовательность оборвалась — начатое выбрасываем и разбираем этот байт
                // заново: он начало чего-то нового, а не продолжение испорченного.
                *state = KeyScan::Ground;
                return decode(state, b);
            }
            buf[len as usize] = b;
            let len = len + 1;
            if len < need {
                *state = KeyScan::Utf8 { need, len, buf };
                return None;
            }
            *state = KeyScan::Ground;
            let ch = core::str::from_utf8(&buf[..len as usize]).ok()?.chars().next()?;
            // Keysym — кодовая точка символа: привязать к ней клавишу можно, но схема этого не
            // делает, и событие уходит в программу через `text` в исходных байтах.
            let mut k = KeyEvent::new(Keysym(ch as u32), ModMask::empty());
            k.text.push(ch);
            Some(k)
        }
        KeyScan::Esc => {
            if b == b'[' {
                *state = KeyScan::Csi { num: 0, modn: 0, after_semi: false };
                None
            } else {
                *state = KeyScan::Ground;
                Some(KeyEvent::new(Keysym::ESCAPE, ModMask::empty()))
            }
        }
        KeyScan::Csi { num, modn, after_semi } => {
            // Цифры и `;` копим, не выходя из состояния: параметр — это и есть смысл клавиши.
            if b.is_ascii_digit() {
                let d = (b - b'0') as u16;
                *state = if after_semi {
                    KeyScan::Csi { num, modn: modn.saturating_mul(10) + d, after_semi }
                } else {
                    KeyScan::Csi { num: num.saturating_mul(10) + d, modn, after_semi: false }
                };
                return None;
            }
            if b == b';' {
                *state = KeyScan::Csi { num, modn, after_semi: true };
                return None;
            }
            *state = KeyScan::Ground;
            // Модификатор xterm приходит как «значение + 1»: 2 = Shift, 3 = Alt, 5 = Ctrl.
            let mods = match modn {
                2 => ModMask::SHIFT,
                3 => ModMask::ALT,
                5 => ModMask::CTRL,
                _ => ModMask::empty(),
            };
            let sym = match b {
                b'A' => Keysym::UP,
                b'B' => Keysym::DOWN,
                b'C' => Keysym::RIGHT,
                b'D' => Keysym::LEFT,
                b'H' => Keysym::HOME,
                b'F' => Keysym::END,
                b'~' => match num {
                    1 | 7 => Keysym::HOME,
                    3 => Keysym::DELETE,
                    4 | 8 => Keysym::END,
                    5 => Keysym::PAGE_UP,
                    6 => Keysym::PAGE_DOWN,
                    _ => return None,
                },
                _ => return None,
            };
            Some(KeyEvent::new(sym, mods))
        }
    }
}

/// Обратный перевод: событие → байты для программы в панели. Нужен потому, что перехваченными
/// оказываются НЕ все клавиши, а непойманные обязаны дойти до ребёнка в том же виде, в каком их
/// ждёт любая программа.
fn encode(k: &KeyEvent) -> Vec<u8> {
    if !k.text.is_empty() && !k.mods.contains(ModMask::CTRL) {
        return k.text.as_bytes().to_vec();
    }
    match k.keysym {
        Keysym::UP => b"\x1b[A".to_vec(),
        Keysym::DOWN => b"\x1b[B".to_vec(),
        Keysym::RIGHT => b"\x1b[C".to_vec(),
        Keysym::LEFT => b"\x1b[D".to_vec(),
        Keysym::HOME => b"\x1b[H".to_vec(),
        Keysym::END => b"\x1b[F".to_vec(),
        Keysym::DELETE => b"\x1b[3~".to_vec(),
        Keysym::RETURN => vec![b'\r'],
        Keysym::TAB => vec![b'\t'],
        Keysym::BACKSPACE => vec![0x7f],
        Keysym::ESCAPE => vec![0x1b],
        Keysym(c) if k.mods.contains(ModMask::CTRL) && (b'a' as u32..=b'z' as u32).contains(&c) => {
            vec![(c as u8 - b'a') + 1]
        }
        Keysym(c) if c < 0x80 => vec![c as u8],
        _ => Vec::new(),
    }
}

// ── настройка: строки конфига системы → схема управления (Веха 100) ──────────
//
// Префикс — Ctrl-A (как в screen): он переводит в режим `Pane`, где клавиши значат команды
// мультиплексора, а не текст. Один режим вместо «префикс + одна клавиша» — потому что так
// устроен `ereb_input`, и так можно жать несколько команд подряд, не повторяя префикс.
// Всё это — лишь ЗНАЧЕНИЕ ПО УМОЛЧАНИЮ: и клавиши, и режимы приходят строками конфига.

/// Одна клавиша схемы: разобранное сочетание, ИСХОДНЫЙ текст клавиши (для подсказки в
/// статус-баре — врущая подсказка хуже отсутствующей) и имя действия.
struct KeyBind {
    mode: Mode,
    mods: ModMask,
    sym: Keysym,
    key: String,
    action: String,
}

/// Настройки терминала: схема управления плюс то, что нельзя поменять на лету.
struct Conf {
    binds: Vec<KeyBind>,
    table: Bindings,
    font_px: u32,
    /// Шрифт: абсолютный путь либо имя файла, которое ищется в пакетах профиля (Веха 114).
    /// `None` — конфиг про шрифт молчит, рисуем встроенным битмапным.
    font: Option<String>,
    shell: Vec<u8>,
    shell_args: Vec<u8>,
}

impl Conf {
    /// Собрать настройки: конфиг активного поколения, а чего в нём нет — из [`DEFAULT_CONF`].
    fn load() -> Conf {
        let mut c = Conf {
            binds: Vec::new(),
            table: Bindings::new(),
            font_px: 18,
            font: None,
            shell: b"bin/vvsh".to_vec(),
            shell_args: b"repl".to_vec(),
        };
        if let Some(text) = read_generation() {
            c.apply(&text);
        }
        // Пустая схема — не «терминал без клавиш», а «конфиг про клавиши не говорил».
        // Схема задаётся ЦЕЛИКОМ: слияние с умолчаниями означало бы, что клавишу нельзя отвязать.
        if c.binds.is_empty() {
            c.apply(DEFAULT_CONF);
        }
        for b in &c.binds {
            c.table.bind(b.mode, b.mods, b.sym, action_of(&b.action));
        }
        c
    }

    /// Разобрать строки конфига. Чужие строки (`service`, `shell`, …) и непонятные пропускаются:
    /// текст поколения общий, и ругаться на строки соседа терминалу не на что.
    fn apply(&mut self, text: &str) {
        for line in text.lines() {
            let mut w = line.split_whitespace();
            match w.next() {
                Some("bind") => {
                    let (Some(m), Some(k), Some(a)) = (w.next(), w.next(), w.next()) else {
                        continue;
                    };
                    let (Some(mode), Some((mods, sym))) = (parse_mode(m), parse_key(k)) else {
                        continue;
                    };
                    self.binds.push(KeyBind {
                        mode,
                        mods,
                        sym,
                        key: k.to_string(),
                        action: a.to_string(),
                    });
                }
                Some("terminal") => {
                    let (Some(key), Some(val)) = (w.next(), w.next()) else { continue };
                    match key {
                        // Кегль ограничен с обеих сторон: слишком мелкий нечитаем, слишком
                        // крупный оставляет от экрана десяток знакомест.
                        "font-size" => {
                            if let Ok(n) = val.parse::<u32>() {
                                self.font_px = n.clamp(8, 48);
                            }
                        }
                        // Веха 114 — шрифт приходит ФАЙЛОМ: `/путь/шрифт.ttf` или просто имя
                        // файла, которое ищется в установленных пакетах. Имя без пути — чтобы в
                        // конфиге не появлялись хэши store (тот же довод, что у `packages`).
                        "font" => self.font = Some(val.to_string()),
                        "shell" => self.shell = val.as_bytes().to_vec(),
                        "shell-args" => {
                            let mut args = val.to_string();
                            for extra in w {
                                args.push(' ');
                                args.push_str(extra);
                            }
                            self.shell_args = args.into_bytes();
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }

    /// Клавиша, которой в режиме `mode` привязано действие `action` (как её написали в конфиге).
    fn key_for(&self, mode: Mode, action: &str) -> Option<&str> {
        self.binds
            .iter()
            .find(|b| b.mode == mode && b.action == action)
            .map(|b| b.key.as_str())
    }
}

/// Имя действия из конфига → действие ereb. Неизвестное имя остаётся `Custom` и до реактора
/// доходит как есть — там оно тихо ничего не делает (и режим сбрасывается).
fn action_of(name: &str) -> Action {
    match name {
        "mode-pane" => Action::EnterMode(Mode::Pane),
        "mode-normal" => Action::EnterMode(Mode::Normal),
        "mode-tab" => Action::EnterMode(Mode::Tab),
        "mode-scroll" => Action::EnterMode(Mode::Scroll),
        "next-pane" => Action::NextPane,
        other => Action::Custom(other.into()),
    }
}

fn parse_mode(s: &str) -> Option<Mode> {
    match s {
        "normal" => Some(Mode::Normal),
        "pane" => Some(Mode::Pane),
        "tab" => Some(Mode::Tab),
        "scroll" => Some(Mode::Scroll),
        _ => None,
    }
}

/// `C-a` → (Ctrl, «a»); `|` → сам символ; `Left` → keysym стрелки. Модификаторы префиксами,
/// как в tmux/emacs: `C-` Ctrl, `A-` Alt, `S-` Shift.
fn parse_key(tok: &str) -> Option<(ModMask, Keysym)> {
    let mut mods = ModMask::empty();
    let mut rest = tok;
    // Голый `-` — это клавиша «минус», а не хвост модификатора: проверяем длину.
    while rest.len() > 2 {
        let (m, tail) = rest.split_at(2);
        match m {
            "C-" => mods |= ModMask::CTRL,
            "A-" => mods |= ModMask::ALT,
            "S-" => mods |= ModMask::SHIFT,
            _ => break,
        }
        rest = tail;
    }
    let sym = match rest {
        "Left" => Keysym::LEFT,
        "Right" => Keysym::RIGHT,
        "Up" => Keysym::UP,
        "Down" => Keysym::DOWN,
        "Home" => Keysym::HOME,
        "PageUp" | "PgUp" => Keysym::PAGE_UP,
        "PageDown" | "PgDn" => Keysym::PAGE_DOWN,
        "End" => Keysym::END,
        "Delete" => Keysym::DELETE,
        "Enter" | "Return" => Keysym::RETURN,
        "Tab" => Keysym::TAB,
        "Esc" | "Escape" => Keysym::ESCAPE,
        "Space" => Keysym::SPACE,
        "Backspace" => Keysym::BACKSPACE,
        _ => {
            let mut ch = rest.chars();
            let c = ch.next()?;
            if ch.next().is_some() {
                return None; // не одиночный символ и не известное имя — не клавиша
            }
            Keysym(c as u32)
        }
    };
    Some((mods, sym))
}

/// Текст активного поколения из store: `system/current` → имя → `system/<имя>`. Это ровно тот
/// текст, по которому ядро подняло систему, — терминал берёт из него свои строки.
fn read_generation() -> Option<String> {
    let cap = store_cap()?;
    let name = read_root(cap, b"system/current")?;
    let name = core::str::from_utf8(&name).ok()?.trim().to_string();
    let mut root = b"system/".to_vec();
    root.extend_from_slice(name.as_bytes());
    String::from_utf8(read_root(cap, &root)?).ok()
}

/// Право на store: по имени (Веха 99.1), иначе перебором. Проба безобидна — чтение корня ничего
/// не меняет, а без права мы просто останемся при умолчаниях.
fn store_cap() -> Option<usize> {
    let readable = |c: usize| {
        let mut id = [0u8; 32];
        c != sys::NO_CAP && sys::obj_get_root(c, b"system/current", &mut id) == 32
    };
    sys::cap_named("STORE")
        .filter(|&c| readable(c))
        .or_else(|| (0..8).map(sys::start_cap).find(|&c| readable(c)))
}

/// Содержимое именованного корня store.
fn read_root(cap: usize, name: &[u8]) -> Option<Vec<u8>> {
    let mut id = [0u8; 32];
    if sys::obj_get_root(cap, name, &mut id) != 32 {
        return None;
    }
    let mut buf = vec![0u8; 64 * 1024];
    let n = sys::obj_get(cap, &id, &mut buf);
    if n == 0 || n > buf.len() {
        return None;
    }
    buf.truncate(n);
    Some(buf)
}

/// Всё, что зависит от КЕГЛЯ: шрифт, кэш глифов, рендер, поверхность кадра и геометрия экрана
/// в знакоместах. Отдельной структурой ровно потому, что настройки перечитываются на ходу
/// (Веха 101): смена `font-size` меняет и размер ячейки, и число колонок/строк, и размер кадра —
/// то есть всё это разом, а не по кусочку.
/// Откуда взяты глифы. Два источника вместо одного — не роскошь: настоящий шрифт лежит файлом
/// вне бинаря и может отсутствовать, а терминал обязан подняться в любом случае (Веха 114).
enum TermFont {
    Ttf(TtfFont),
    Bitmap(BitmapFont),
}

impl ereb_render::Rasterizer for TermFont {
    fn metrics(&self) -> ereb_render::CellMetrics {
        match self {
            TermFont::Ttf(f) => f.metrics(),
            TermFont::Bitmap(f) => f.metrics(),
        }
    }
    fn rasterize(
        &mut self,
        ch: char,
        style: ereb_render::RenderStyle,
    ) -> ereb_render::RasterizedGlyph {
        match self {
            TermFont::Ttf(f) => f.rasterize(ch, style),
            TermFont::Bitmap(f) => f.rasterize(ch, style),
        }
    }
}

struct View {
    cache: GlyphCache<TermFont>,
    renderer: GridRenderer,
    surface: Surface,
    cols: usize,
    rows: usize,
}

impl View {
    fn build(conf: &Conf, info: &sys::VideoInfo) -> Option<View> {
        let font = load_font(conf);
        let cache = GlyphCache::new(font);
        let metrics = cache.metrics();
        let palette = Palette::default();
        let renderer = GridRenderer::new(palette, metrics);
        // Геометрия в знакоместах; последняя строка — статус-бар мультиплексора.
        let cols = (info.width / metrics.width.max(1) as usize).max(8);
        let rows = (info.height / metrics.height.max(1) as usize).max(4);
        let (w, h) = renderer.pixel_size(cols, rows);
        let surface = Surface::new(w, h, palette.background);
        Some(View { cache, renderer, surface, cols, rows })
    }

    /// Высота знакоместа в пикселях (шаг переноса строк на экран).
    fn cell_h(&self) -> usize {
        self.renderer.metrics().height.max(1) as usize
    }
}

/// Сказать что-то человеку. В графическом режиме консоль ядра уезжает в serial, поэтому это
/// единственный способ терминала пожаловаться на себя — экран он в этот момент ещё не рисует.
fn log_line(s: &str) {
    sys::write_console(s.as_bytes());
    sys::write_console(b"\n");
}

/// Взять шрифт: сперва названный конфигом файл, иначе встроенный битмапный.
///
/// О неудаче говорится ВСЛУХ и с причиной. Молчаливый откат на запасной шрифт был бы худшим из
/// исходов: человек написал в конфиге путь, увидел не тот шрифт и не узнал, ошибся ли он в пути,
/// забыл ли поставить пакет или файл оказался не шрифтом.
fn load_font(conf: &Conf) -> TermFont {
    if let Some(name) = conf.font.as_deref() {
        match find_font(name) {
            Some(bytes) => match TtfFont::from_vec(bytes, conf.font_px) {
                Ok(f) => return TermFont::Ttf(f),
                Err(_) => log_line(&alloc::format!("term: {} — не разбирается как шрифт", name)),
            },
            None => {
                log_line(&alloc::format!(
                    "term: шрифт {} не найден (ни путь, ни пакет профиля)",
                    name
                ));
                // «Не найден» — половина ответа. Вторая половина: а что там ЕСТЬ. Без неё
                // человек остаётся гадать между опечаткой в имени, не тем пакетом и не той
                // раскладкой внутри пакета — и идёт выяснять это глазами по скриншоту.
                list_fonts();
            }
        }
    }
    // Кегль у растрового шрифта кратен 16, и просьбу «18» он выполнить не может. Говорим, что
    // получилось на самом деле: молча нарисовать не то — худший исход из возможных.
    let f = BitmapFont::new(conf.font_px);
    log_line(&alloc::format!(
        "term: рисую встроенным шрифтом 8×16 (кегль {} вместо {}); настоящий задаётся так: \
         packages(\"…\") + terminal(\"font\", \"имя.ttf\")",
        f.effective_px(),
        conf.font_px
    ));
    TermFont::Bitmap(f)
}

/// Найти файл шрифта: абсолютный путь читается как есть, имя — ищется в пакетах профиля.
fn find_font(name: &str) -> Option<Vec<u8>> {
    let ep = sys::cap_named("POSIXFS").unwrap_or_else(|| sys::start_cap(0));
    if name.starts_with('/') {
        return read_whole(ep, name.as_bytes());
    }
    let scap = store_cap()?;
    for path in font_files(ep, scap) {
        if path.rsplit('/').next() == Some(name) {
            return try_font(ep, &path);
        }
    }
    None
}

/// Все файлы шрифтов, какие видны в пакетах профиля.
fn font_files(ep: usize, scap: usize) -> Vec<String> {
    let mut out = Vec::new();
    for item in profile::path_items(scap) {
        if !item.top {
            continue;
        }
        let root = alloc::format!("/nix/store/{}/{}", item.base, FONT_ROOT);
        walk(ep, &root, FONT_DEPTH, &mut out);
    }
    out
}

/// Обойти каталог вглубь, складывая пути ФАЙЛОВ. Каталоги posixfs отдаёт с хвостовым '/'.
fn walk(ep: usize, dir: &str, depth: usize, out: &mut Vec<String>) {
    if depth == 0 || out.len() >= FONT_MAX {
        return;
    }
    let mut buf = alloc::vec![0u8; 8 * 1024];
    let n = void_user::posix::readdir(ep, dir.as_bytes(), &mut buf);
    if n == 0 || n > buf.len() {
        return;
    }
    let Ok(text) = core::str::from_utf8(&buf[..n]) else { return };
    for e in text.lines().filter(|e| !e.is_empty()) {
        match e.strip_suffix('/') {
            Some(sub) => walk(ep, &alloc::format!("{}/{}", dir, sub), depth - 1, out),
            None => {
                if out.len() < FONT_MAX {
                    out.push(alloc::format!("{}/{}", dir, e));
                }
            }
        }
    }
}

/// Показать, какие файлы шрифтов вообще есть в установленных пакетах.
fn list_fonts() {
    let ep = sys::cap_named("POSIXFS").unwrap_or_else(|| sys::start_cap(0));
    let Some(scap) = store_cap() else {
        log_line("  профиль не прочитать: нет права на store");
        return;
    };
    let files = font_files(ep, scap);
    if files.is_empty() {
        log_line("  в пакетах профиля нет ни одного файла шрифта — поставьте пакет со шрифтом");
        return;
    }
    for f in files.iter().take(24) {
        log_line(&alloc::format!("  есть: {}", f));
    }
    if files.len() > 24 {
        log_line(&alloc::format!("  …и ещё {}", files.len() - 24));
    }
}

/// Прочитать файл шрифта и сказать, откуда он взят.
fn try_font(ep: usize, path: &str) -> Option<Vec<u8>> {
    let bytes = read_whole(ep, path.as_bytes())?;
    log_line(&alloc::format!("term: шрифт {} ({} Б)", path, bytes.len()));
    Some(bytes)
}

/// Имена подкаталогов каталога (пусто, если его нет).
fn subdirs(ep: usize, path: &[u8]) -> Vec<String> {
    let mut buf = alloc::vec![0u8; 8 * 1024];
    let n = void_user::posix::readdir(ep, path, &mut buf);
    if n == 0 || n > buf.len() {
        return Vec::new();
    }
    core::str::from_utf8(&buf[..n])
        .unwrap_or("")
        .lines()
        .filter_map(|e| e.strip_suffix('/'))
        .map(|e| e.to_string())
        .collect()
}

/// Прочитать файл целиком через файловый сервер. `None` — файла нет или это каталог.
fn read_whole(ep: usize, path: &[u8]) -> Option<Vec<u8>> {
    use void_user::posix as px;
    match px::stat(ep, path) {
        Some((is_dir, _)) if !is_dir => {}
        _ => return None,
    }
    let fd = px::open(ep, path, 0);
    if fd == usize::MAX {
        return None;
    }
    let mut out = Vec::new();
    // Кусок побольше строчного: шрифт — мегабайты, а каждый вызов это IPC.
    let mut chunk = alloc::vec![0u8; 16 * 1024];
    loop {
        let n = px::read(ep, fd, &mut chunk);
        if n == 0 || n == usize::MAX {
            break;
        }
        out.extend_from_slice(&chunk[..n]);
    }
    px::close(ep, fd);
    (!out.is_empty()).then_some(out)
}

/// Панель: грид, разбор ANSI, ребёнок и его отложенный запрос ввода.
struct Pane {
    id: PaneId,
    grid: Grid,
    parser: vte::Parser,
    /// Номер процесса-ребёнка (`None` — запустить не удалось либо он уже завершился).
    child: Option<usize>,
    /// Ребёнок спит в `read_stdin` и ждёт ответа. Копим клавиши, пока он не спросит, и отвечаем
    /// сразу, как есть и запрос, и байты, — иначе ввод терялся бы между этими событиями.
    pending_read: Option<usize>,
    /// Сколько байт ребёнок готов принять в этом чтении (Веха 101). Отвечать больше нельзя:
    /// лишнее ядро отрежет по его буферу, а из нашей очереди мы бы его уже удалили — так
    /// терялись куски строки, набранной быстрее, чем её забирают.
    pending_want: usize,
    /// Не отданный ввод этой панели.
    inbox: Vec<u8>,
    /// На сколько строк вьюпорт поднят в историю (0 — «внизу», как обычно). Веха 116:
    /// у грида scrollback был с самого начала, но смотреть в него было нечем.
    scroll: usize,
}

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    // Настройки читаются ПЕРВЫМИ: от них зависят и кегль шрифта (а значит вся геометрия), и то,
    // что запускать в панелях.
    let mut conf = Conf::load();

    // Веха 118 — есть композитор? Тогда мы ОКНО, а не владелец экрана. Решение принимается
    // здесь и больше нигде: дальше по коду разница видна только в том, куда лёг кадр.
    let store = store_cap().unwrap_or(sys::NO_CAP);
    let (out, info) = match sys::win::Window::create(WIN_W, WIN_H, "терминал") {
        Some(win) => {
            log_line("term: работаю окном композитора");
            // Кадр окна — RGBA по строкам без выравнивания; описываем его теми же полями, что
            // ядро отдаёт для экрана, чтобы весь рисующий код остался общим.
            let info = sys::VideoInfo {
                width: WIN_W as usize,
                height: WIN_H as usize,
                pitch: WIN_W as usize * 4,
                bpp: 32,
                rgb: [(0, 8), (8, 8), (16, 8)],
            };
            let buf = vec![0u8; WIN_W as usize * WIN_H as usize * 4];
            (Out::Window { win, buf, store }, info)
        }
        None => {
            let Some(fb_cap) = find_fb_cap() else {
                sys::write_console("[term] нет ни композитора, ни права на экран\n".as_bytes());
                sys::exit(1);
            };
            let Some(info) = sys::video_info(fb_cap) else {
                sys::write_console("[term] ядро не отдало описание видеорежима\n".as_bytes());
                sys::exit(1);
            };
            if !sys::mmio_map(fb_cap, FB_VA) {
                sys::write_console("[term] не удалось замапить фреймбуфер\n".as_bytes());
                sys::exit(1);
            }
            // Веха 115 — сколько машина успевает писать в экран. Замер имеет смысл только у
            // владельца экрана: в окне мы пишем в обычную память.
            let (mbs32, mbs64) = measure_fb(&info);
            let frame = (info.width * info.height * (info.bpp / 8)) as u64;
            log_line(&alloc::format!(
                "term: экран {}×{}, {} бит — заливка {} МБ/с по 4 байта, {} МБ/с по 8 (полный кадр {} мс)",
                info.width, info.height, info.bpp, mbs32, mbs64,
                frame / mbs64.max(1) / 1000,
            ));
            (Out::Screen, info)
        }
    };

    let Some(mut view) = View::build(&conf, &info) else {
        sys::write_console("[term] шрифт не разобрался\n".as_bytes());
        sys::exit(1);
    };

    // Право на ЗАПУСК ищем перебором стартовых прав, а не по фиксированному индексу: порядок
    // токенов в конфиге init — дело конфига, и он уже менялся. Перебор здесь безопасен: неудачный
    // `spawn` ничего не делает, а удастся он ровно с тем правом, у которого есть EXEC на store.
    let me = sys::self_endpoint();

    let mut tree = SplitTree::leaf(PaneId(0));
    let mut next_id = 1usize;
    let mut rects = layout_of(&tree, view.cols, view.rows);
    let mut exec_cap = sys::NO_CAP;
    let mut panes: Vec<Pane> = vec![new_pane(PaneId(0), &rects, &mut exec_cap, me, &conf)];
    let mut focus = 0usize;

    // Прошлый кадр в ЯЧЕЙКАХ: по нему считаем, какие пиксельные строки реально изменились.
    // Без этого каждый чих перерисовывал весь экран — 4 МиБ записей в некэшируемую память на
    // КАЖДУЮ строку вывода. На железе это выглядело как «семидесятые».
    let mut prev_cells: Vec<Cell> = Vec::new();
    let mut mode = Mode::Normal;
    let mut scan = KeyScan::Ground;
    let mut keys = [0u8; 64];
    let mut msg = [0u8; stdio::CHUNK];
    let mut redraw = true;

    // Веха 115 — курсор. Позицию ведём МЫ, а не ядро: границы экрана знает тот, кто рисует.
    // Начинаем в середине — там его точно видно, а «где мой курсор» на старте не вопрос.
    let mut mx = info.width / 2;
    let mut my = info.height / 2;
    let mut mbtn = 0u8;
    let mut mouse_evs = [sys::MouseEvent { dx: 0, dy: 0, buttons: 0 }; 32];
    let mut keys_from_win: Vec<u8> = Vec::new();
    // Курсор виден СРАЗУ, а не после первого движения: это рабочий стол, а не телефон —
    // «где мой курсор» не должно быть первым вопросом к системе.
    let mut cursor_drawn = true;

    loop {
        let mut worked = false;

        // ── 0. события окна (Веха 118) ─────────────────────────────────────────────────────
        // Опрашиваем НЕ блокируясь: у нас свой реактор — панели ждут ответов, и уснуть в чужом
        // вызове мы не имеем права.
        if let Out::Window { win, .. } = &out {
            while let Some(ev) = win.poll_event() {
                worked = true;
                match ev {
                    sys::win::Event::Key(k) => keys_from_win.push(k),
                    sys::win::Event::Close => {
                        sys::write_console("[term] окно закрыто — выходим\n".as_bytes());
                        win.destroy();
                        sys::exit(0);
                    }
                    _ => {}
                }
            }
        }

        // ── 0б. мышь (только когда экран наш) ──────────────────────────────────────────────
        let mn = if out.windowed() { 0 } else { sys::mouse_read(&mut mouse_evs) };
        if mn > 0 {
            worked = true;
            let (ox, oy) = (mx, my);
            for e in &mouse_evs[..mn] {
                mx = (mx as i32 + e.dx as i32).clamp(0, info.width as i32 - 1) as usize;
                my = (my as i32 + e.dy as i32).clamp(0, info.height as i32 - 1) as usize;
                if e.buttons != mbtn {
                    log_line(&alloc::format!(
                        "term: кнопки {:#04b} в {}×{}",
                        e.buttons, mx, my
                    ));
                    mbtn = e.buttons;
                }
            }
            // Стереть старое место и нарисовать новое — два маленьких прямоугольника вместо
            // кадра. Стирать надо ДО отрисовки: иначе хвост курсора остаётся на экране.
            if cursor_drawn {
                restore_rect(&view.surface, &info, ox, oy, CUR_W, CUR_H);
            }
            draw_cursor(&info, mx, my, mbtn != 0);
            cursor_drawn = true;
        }

        // ── 1. клавиатура (никогда не блокируемся) ─────────────────────────────────────────
        // В окне байты приходят событиями от композитора, на голом экране — из консоли ядра.
        let n = if out.windowed() {
            let n = keys_from_win.len().min(keys.len());
            keys[..n].copy_from_slice(&keys_from_win[..n]);
            keys_from_win.clear();
            n
        } else {
            sys::read_console_nonblock(&mut keys)
        };
        if n > 0 {
            worked = true;
            for i in 0..n {
                let Some(key) = decode(&mut scan, keys[i]) else {
                    continue;
                };
                match ereb_input::translate(&key, &conf.table, mode) {
                    Action::EnterMode(m) => {
                        mode = m;
                        redraw = true;
                    }
                    Action::NextPane => {
                        focus = (focus + 1) % panes.len().max(1);
                        mode = Mode::Normal;
                        redraw = true;
                    }
                    Action::Custom(cmd) => {
                        // Команда мультиплексора. Режим сбрасываем всегда: «залипший» режим —
                        // худшее, что бывает с модальным управлением.
                        match cmd.as_str() {
                            // Веха 116 — прокрутка вывода. Шаг в полэкрана: так глаз не теряет
                            // место, а «страница целиком» на длинном выводе перескакивает мимо
                            // нужного. Потолок — длина истории самого грида.
                            "scroll-up" | "scroll-down" | "scroll-top" | "scroll-bottom" => {
                                if let Some(p) = panes.get_mut(focus) {
                                    let page = (view.rows / 2).max(1);
                                    let max = p.grid.scrollback_len();
                                    p.scroll = match cmd.as_str() {
                                        "scroll-up" => (p.scroll + page).min(max),
                                        "scroll-down" => p.scroll.saturating_sub(page),
                                        "scroll-top" => max,
                                        _ => 0,
                                    };
                                    redraw = true;
                                }
                            }
                            "split-v" | "split-h" => {
                                let dir = if cmd == "split-v" {
                                    SplitDirection::Vertical
                                } else {
                                    SplitDirection::Horizontal
                                };
                                split(&mut tree, &mut panes, &mut next_id, &mut focus, dir,
                                      view.cols, view.rows, &mut exec_cap, me, &conf);
                                rects = layout_of(&tree, view.cols, view.rows);
                            }
                            "close" => {
                                close_pane(&mut tree, &mut panes, &mut focus);
                                if panes.is_empty() {
                                    sys::write_console("[term] панелей не осталось — выход\n".as_bytes());
                                    sys::exit(0);
                                }
                                rects = layout_of(&tree, view.cols, view.rows);
                                resize_all(&mut panes, &rects);
                            }
                            "quit" => {
                                // Веха 101 — выход из ТЕРМИНАЛА выключает машину: он и есть
                                // shell поколения, после него интерактивного не остаётся, а
                                // машина раньше просто продолжала работать (замечание владельца
                                // по железу). Нет права `power` — как встарь, просто выходим.
                                sys::write_console("[term] выход по команде панелей\n".as_bytes());
                                if let Some(pc) = sys::cap_named("POWER") {
                                    sys::power_off(pc);
                                }
                                sys::exit(0);
                            }
                            // Веха 101 — перечитать конфиг НА ХОДУ. Первый шаг к `switch` без
                            // перезагрузки, и он оказался целиком в userspace: поколение уже
                            // лежит в store, а term умеет его читать. Клавиши и кегль меняются
                            // без ребута; живые панели и их процессы при этом не трогаются.
                            "reload" => {
                                conf = Conf::load();
                                match View::build(&conf, &info) {
                                    Some(v) => {
                                        view = v;
                                        // Кегль мог измениться — значит изменилось ВСЁ, что от
                                        // него зависит: раскладка, гриды панелей, размер кадра.
                                        rects = layout_of(&tree, view.cols, view.rows);
                                        resize_all(&mut panes, &rects);
                                        prev_cells.clear(); // кадр несравним со старым — весь заново
                                    }
                                    // Шрифт не собрался (кегль из конфига негоден) — остаёмся на
                                    // прежнем виде: терминал, погасший из-за опечатки в конфиге,
                                    // нечем было бы починить.
                                    None => sys::write_console(
                                        "[term] конфиг перечитан, но шрифт не построился — вид прежний\n"
                                            .as_bytes(),
                                    ),
                                }
                            }
                            // Сам префикс: отдать его программе (в конфиге — `literal-prefix`).
                            // Байт берём из клавиши, а не из константы: префикс перенастраиваемый.
                            "literal-prefix" => {
                                for b in encode(&key) {
                                    push_input(&mut panes, focus, b);
                                }
                            }
                            // Навигация по направлениям — из `ereb_mux`, а не своим перебором:
                            // «соседняя слева» это геометрия, и она там уже написана.
                            "go-left" | "go-right" | "go-up" | "go-down" => {
                                let dir = match cmd.as_str() {
                                    "go-left" => Direction::Left,
                                    "go-right" => Direction::Right,
                                    "go-up" => Direction::Up,
                                    _ => Direction::Down,
                                };
                                if let Some(id) = neighbor(&rects, panes[focus].id, dir) {
                                    if let Some(i) = panes.iter().position(|p| p.id == id) {
                                        focus = i;
                                    }
                                }
                            }
                            // Действие из конфига, которого мы не знаем: молча ничего. Ронять
                            // терминал из-за опечатки в конфиге нельзя, а сама опечатка ловится
                            // раньше — на сборке поколения (`rebuild` проверяет форму записи).
                            _ => {}
                        }
                        mode = Mode::Normal;
                        redraw = true;
                    }
                    // Не перехвачено — байты уходят программе в панели как есть.
                    _ => {
                        for b in encode(&key) {
                            push_input(&mut panes, focus, b);
                        }
                    }
                }
            }
        }

        // ── 2. вывод и запросы ввода от детей ──────────────────────────────────────────────
        while let Some(m) = sys::try_recv(&mut msg) {
            worked = true;
            redraw |= handle(&mut panes, &m, &msg);
        }

        // ── 3. отдать накопленный ввод тем, кто его ждёт ───────────────────────────────────
        for p in panes.iter_mut() {
            if p.pending_read.is_some() && !p.inbox.is_empty() {
                let take = p.inbox.len().min(p.pending_want.max(1));
                sys::reply(p.pending_read.take().unwrap(), &p.inbox[..take]);
                p.inbox.drain(..take);
                worked = true;
            }
        }

        // ── 4. умершие дети ────────────────────────────────────────────────────────────────
        for i in 0..panes.len() {
            if let Some(pid) = panes[i].child {
                if let Wait::Exited(_) = sys::wait(pid, true) {
                    let pane = &mut panes[i];
                    pane.child = None;
                    let note =
                        "\r\n\x1b[1;31m[процесс завершился — Ctrl-A x закрыть панель]\x1b[0m\r\n";
                    pane.parser.advance(&mut pane.grid, note.as_bytes());
                    redraw = true;
                    worked = true;
                }
            }
        }

        // ── 5. кадр ────────────────────────────────────────────────────────────────────────
        if redraw {
            let cells = compose(&panes, &rects, focus, mode, view.cols, view.rows, &conf);
            let dirty = dirty_rows(&prev_cells, &cells, view.cols, view.rows);
            if !dirty.is_empty() {
                // Рисуем И переносим ТОЛЬКО изменившиеся строки. Раньше отрисовка шла по всему
                // кадру «потому что RAM дешёвая» — оценка оказалась неверной: 116×36 знакомест
                // по ~2000 пиксельных операций и есть та медлительность, которую видно на
                // железе (в gen1 консоль ядра красит лишь изменившиеся ячейки — и она мгновенна).
                view.renderer.paint_cells_rows(
                    &cells, view.cols, view.rows, &mut view.cache, &mut view.surface, &dirty,
                );
                let ch = view.cell_h();
                for &y in &dirty {
                    blit_rows(&view.surface, &info, out.base(), y * ch,
                              ((y + 1) * ch).min(info.height));
                }
                // Кадр мог затереть курсор — вернуть его поверх (Веха 115). В ОКНЕ курсора
                // не рисуем вовсе: он принадлежит композитору и живёт поверх всех окон.
                if cursor_drawn && !out.windowed() {
                    draw_cursor(&info, mx, my, mbtn != 0);
                }
                // Композитору отдаём одну полосу — от первой изменившейся строки до последней.
                // Отдельными прямоугольниками на каждую строку было бы точнее, но каждый из них
                // это ещё один объект и ещё один вызов; полоса — верная середина.
                if out.windowed() {
                    if let (Some(&first), Some(&last)) = (dirty.first(), dirty.last()) {
                        out.present_rows(&info, first * ch, ((last + 1) * ch).min(info.height));
                    }
                }
            }
            prev_cells = cells;
            redraw = false;
        }

        // Спать только когда делать нечего — и коротко: клавиатура нас не разбудит, потому что
        // ждём мы на IPC. Осознанный компромисс: единого «ждать клавишу ИЛИ сообщение» в ядре
        // пока нет (записано долгом).
        if !worked {
            // Сообщение, пришедшее ВО СНЕ, обязано быть обработано здесь же. Выбросить его
            // нельзя: вместе с ним теряется одноразовое reply-право, и вызвавший ребёнок висит
            // навсегда — молча, без единой ошибки. Ровно на этом веха и споткнулась.
            // Веха 103 — спим до СОБЫТИЯ, а не по короткому таймеру: пробуждение по клавише
            // появилось в ядре (`recv_console`), и реактор наконец ждёт оба своих источника
            // сразу. Срок остался страховкой — на случай событий, о которых ядро нас не будит.
            if let Some(m) = sys::recv_console(&mut msg, 200) {
                redraw |= handle(&mut panes, &m, &msg);
            }
        }
    }
}

/// Обработать одно сообщение от ребёнка. Возвращает `true`, если кадр надо перерисовать.
///
/// Вынесено отдельно НЕ ради красоты: принимать сообщения приходится в двух местах — в опросе
/// и при пробуждении из сна, — и разошедшиеся копии этой обработки означали бы потерянные
/// reply-права и повисших детей.
fn handle(panes: &mut [Pane], m: &sys::Message, buf: &[u8]) -> bool {
    let who = owner_pane(panes, m.sender);
    match who {
        Some(i) if m.op == stdio::OP_STDOUT => {
            let len = m.len.min(buf.len());
            let pane = &mut panes[i];
            // Сколько истории было ДО вывода: если человек листает, вьюпорт обязан остаться на
            // месте, а не уезжать под новыми строками. Классическое поведение терминала —
            // и единственное, при котором чтение длинного лога вообще возможно.
            let before = pane.grid.scrollback_len();
            // Перевод строки: программы шлют голый `\n`, а грид (как и любой терминал) ждёт
            // CR+LF — иначе строка опускается, НЕ возвращая курсор, и вывод идёт лесенкой
            // вправо. В Unix это делает драйвер tty (ONLCR); у нас драйвера нет, поэтому
            // трансляция здесь — терминал и есть её законное место.
            let mut from = 0usize;
            for at in 0..len {
                if buf[at] == b'\n' && (at == 0 || buf[at - 1] != b'\r') {
                    pane.parser.advance(&mut pane.grid, &buf[from..at]);
                    pane.parser.advance(&mut pane.grid, b"\r\n");
                    from = at + 1;
                }
            }
            pane.parser.advance(&mut pane.grid, &buf[from..len]);
            if pane.scroll > 0 {
                let grown = pane.grid.scrollback_len().saturating_sub(before);
                pane.scroll = (pane.scroll + grown).min(pane.grid.scrollback_len());
            }
            sys::reply(m.reply_cap, &[]);
            true
        }
        Some(i) if m.op == stdio::OP_WINSIZE => {
            // Размер панели в знакоместах — программе нужно знать, куда она рисует.
            let (c, r) = (panes[i].grid.cols() as u16, panes[i].grid.rows() as u16);
            let mut rep = [0u8; 4];
            rep[..2].copy_from_slice(&c.to_le_bytes());
            rep[2..].copy_from_slice(&r.to_le_bytes());
            sys::reply(m.reply_cap, &rep);
            false
        }
        Some(i) if m.op == stdio::OP_STDIN => {
            // Отложенный ответ: держим право до появления клавиш (как в net-srv).
            panes[i].pending_read = Some(m.reply_cap);
            // Сколько ребёнок может принять — из запроса; пустой запрос значит старого клиента.
            let want = if m.len >= 4 {
                u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize
            } else {
                stdio::CHUNK
            };
            panes[i].pending_want = want.clamp(1, stdio::CHUNK);
            false
        }
        // Чужой или непонятный запрос — ответить пусто, а не молчать: молчание повесило бы
        // вызвавшего навсегда.
        _ => {
            sys::reply(m.reply_cap, &[]);
            false
        }
    }
}

/// Чья это панель. Отправитель может быть не самим шеллом панели, а тем, кого шелл запустил:
/// право на наш эндпоинт наследуется вглубь, и пишет нам ВНУК под своим номером процесса.
///
/// Это была настоящая пропажа (Веха 114): пока владелец искался сравнением «отправитель ==
/// ребёнок панели», вывод всего, что запущено из шелла, молча уходил в никуда — `pkg update`
/// отработал десять минут и не показал ни строки. Поднимаемся по родителям (`SYS_PARENT`) с
/// потолком: цепочка процессов конечна, а бесконечный цикл в реакторе терминала — это
/// зависшая система.
fn owner_pane(panes: &[Pane], sender: usize) -> Option<usize> {
    const MAX_DEPTH: usize = 16;
    let mut pid = sender;
    for _ in 0..MAX_DEPTH {
        if let Some(i) = panes.iter().position(|p| p.child == Some(pid)) {
            return Some(i);
        }
        pid = sys::parent_of(pid)?;
    }
    None
}

/// Завести панель: грид под её размер плюс запущенный в ней шелл с нашим stdio.
fn new_pane(id: PaneId, rects: &[PaneRect], exec_cap: &mut usize, me: usize, conf: &Conf) -> Pane {
    let (w, h) = rect_size(rects, id);
    let child = spawn_shell(exec_cap, me, conf);
    let mut pane = Pane {
        id,
        grid: Grid::new(w, h),
        parser: vte::Parser::new(),
        child,
        pending_read: None,
        pending_want: 0,
        inbox: Vec::new(),
        scroll: 0,
    };
    if child.is_none() {
        let msg = "\x1b[1;31m[не удалось запустить шелл]\x1b[0m\r\n";
        pane.parser.advance(&mut pane.grid, msg.as_bytes());
    }
    pane
}

/// Запустить шелл, подобрав право на запуск. Найденное запоминается — перебирать на каждую
/// панель незачем.
fn spawn_shell(exec_cap: &mut usize, me: usize, conf: &Conf) -> Option<usize> {
    let (prog, args) = (&conf.shell[..], &conf.shell_args[..]);
    if *exec_cap != sys::NO_CAP {
        return sys::spawn_with_stdio(*exec_cap, prog, args, me);
    }
    // Веха 99.1: сперва спрашиваем право ПО ИМЕНИ — так порядок токенов в конфиге перестал
    // что-либо значить. Перебор остался запасным путём для старых конфигов без имён.
    if let Some(c) = sys::cap_named("STORE") {
        if let Some(pid) = sys::spawn_with_stdio(c, prog, args, me) {
            *exec_cap = c;
            return Some(pid);
        }
    }
    for i in 0..8 {
        let c = sys::start_cap(i);
        if c == sys::NO_CAP {
            continue;
        }
        if let Some(pid) = sys::spawn_with_stdio(c, prog, args, me) {
            *exec_cap = c;
            return Some(pid);
        }
    }
    None
}

/// Разбить фокусную панель и завести в новой половине ещё один шелл.
#[allow(clippy::too_many_arguments)]
fn split(
    tree: &mut SplitTree, panes: &mut Vec<Pane>, next_id: &mut usize, focus: &mut usize,
    dir: SplitDirection, cols: usize, rows: usize, exec_cap: &mut usize, me: usize, conf: &Conf,
) {
    if panes.is_empty() {
        return;
    }
    let target = panes[*focus].id;
    let fresh = PaneId(*next_id);
    if !tree.split(target, dir, 0.5, fresh) {
        return;
    }
    *next_id += 1;
    let rects = layout_of(tree, cols, rows);
    resize_all(panes, &rects); // старые панели поменяли размер — грид обязан следовать
    panes.push(new_pane(fresh, &rects, exec_cap, me, conf));
    *focus = panes.len() - 1;
}

/// Закрыть фокусную панель — вместе с процессом, который в ней жил (Веха 103).
///
/// До этого ребёнок оставался сиротой: панели нет, а процесс живёт и ждёт ввода, которого больше
/// никто не пришлёт. Теперь его завершает `SYS_KILL` — право на это у нас есть по родительству,
/// мы его и запускали.
fn close_pane(tree: &mut SplitTree, panes: &mut Vec<Pane>, focus: &mut usize) {
    if panes.is_empty() {
        return;
    }
    let id = panes[*focus].id;
    if panes.len() > 1 && !tree.close(id) {
        return;
    }
    if let Some(pid) = panes[*focus].child {
        sys::kill(pid);
    }
    panes.remove(*focus);
    if *focus >= panes.len() {
        *focus = panes.len().saturating_sub(1);
    }
}

/// Подогнать гриды под текущую раскладку.
fn resize_all(panes: &mut [Pane], rects: &[PaneRect]) {
    for p in panes.iter_mut() {
        let (w, h) = rect_size(rects, p.id);
        if p.grid.cols() != w || p.grid.rows() != h {
            // `Grid::resize` переносит содержимое; раньше здесь создавался НОВЫЙ грид, и при
            // каждом разбиении соседние панели чернели — самая заметная ошибка первой версии.
            p.grid.resize(w, h);
        }
    }
}

/// Положить клавишу в ящик фокусной панели. Ответ уйдёт в шаге 3 реактора — там же, где
/// обслуживаются запросы, пришедшие РАНЬШЕ клавиш.
fn push_input(panes: &mut [Pane], focus: usize, byte: u8) {
    if let Some(p) = panes.get_mut(focus) {
        // Набрал что-то — вернулись вниз. Так ведут себя все терминалы, и по делу: человек,
        // который начал печатать, хочет видеть, что печатает, а не то место, куда листал.
        p.scroll = 0;
        p.inbox.push(byte);
    }
}

/// Раскладка дерева в знакоместах; снизу оставлена строка под статус-бар.
fn layout_of(tree: &SplitTree, cols: usize, rows: usize) -> Vec<PaneRect> {
    tree.layout(
        Area { col: 0, row: 0, cols: cols as u16, rows: (rows - 1) as u16 },
        1, // зазор в знакоместо: панели должны быть видимо разделены
    )
}

/// Размер панели в знакоместах (минимум 1×1 — вырожденную раскладку рендер переживать не обязан).
fn rect_size(rects: &[PaneRect], id: PaneId) -> (usize, usize) {
    rects
        .iter()
        .find(|r| r.id == id)
        .map(|r| (r.area.cols.max(1) as usize, r.area.rows.max(1) as usize))
        .unwrap_or((1, 1))
}

/// Собрать кадр: панели по своим прямоугольникам + подсветка фокуса + статус-бар.
#[allow(clippy::too_many_arguments)]
fn compose(
    panes: &[Pane], rects: &[PaneRect], focus: usize, mode: Mode, cols: usize, rows: usize,
    conf: &Conf,
) -> Vec<Cell> {
    // Общий кадр — мозаика из гридов панелей: у каждой свой, склеиваем по ячейкам.
    let mut cells = vec![Cell::default(); cols * rows];
    for (i, p) in panes.iter().enumerate() {
        let Some(r) = rects.iter().find(|r| r.id == p.id) else {
            continue;
        };
        for row in 0..r.area.rows as usize {
            for col in 0..r.area.cols as usize {
                let (x, y) = (r.area.col as usize + col, r.area.row as usize + row);
                if x < cols && y < rows {
                    cells[y * cols + x] = p.grid.view_cell(col, row, p.scroll);
                }
            }
        }
        if i == focus {
            mark_focus(&mut cells, r, cols, rows);
            mark_caret(&mut cells, p, r, cols, rows);
        }
    }
    status_bar(&mut cells, panes, focus, mode, cols, rows, conf);
    cells
}

/// СПИСОК изменившихся строк. Списком, а не диапазоном: при выводе меняются одна-две строки, а
/// диапазон «от первой до последней» захватил бы всё между ними — например строку вывода и
/// статус-бар внизу, то есть весь экран.
fn dirty_rows(prev: &[Cell], now: &[Cell], cols: usize, rows: usize) -> Vec<usize> {
    if prev.len() != now.len() {
        return (0..rows).collect(); // первый кадр или сменилась геометрия — весь экран
    }
    (0..rows)
        .filter(|&y| {
            let r = y * cols..(y + 1) * cols;
            prev[r.clone()] != now[r]
        })
        .collect()
}

/// Пометить фокусную панель по краям зазора: сплошную рамку рисовать негде — зазор между
/// панелями ровно одно знакоместо, а внутри панели каждая ячейка занята содержимым.
///
/// Веха 101 — помечаются **все четыре стороны, где зазор есть**, а не только левая и верхняя.
/// Прежняя версия оставляла панель в левом верхнем углу вовсе без пометки: зазора там нет, и
/// «где фокус» приходилось узнавать из статус-бара. У любой панели при двух и более панелях
/// зазор есть хотя бы с одной стороны — значит пометка будет всегда.
/// Текстовый курсор панели — инверсией знакоместа (Веха 120.1).
///
/// Его не было НИКОГДА: пока в панелях жили только шеллы, каждый рисовал себе подчёркивание сам
/// (`vsh`, `vvsh`), и отсутствия общего курсора никто не замечал. Первая же полноэкранная
/// программа (`ved`) показала цену: место ввода видно не было вовсе.
///
/// Инверсия, а не подчёркивание: она видна на любом фоне и не зависит от шрифта — символ под
/// курсором остаётся читаемым. Рисуется ТОЛЬКО у панели в фокусе (курсор — это «куда попадёт
/// клавиша», и два курсора означали бы два места ввода) и только когда панель не пролистана
/// назад: в истории курсора нет, он в живом кадре.
fn mark_caret(cells: &mut [Cell], p: &Pane, r: &PaneRect, cols: usize, rows: usize) {
    if p.scroll > 0 {
        return;
    }
    let c = p.grid.cursor();
    if !c.visible {
        return; // программа спрятала курсор (`ESC[?25l`) — это её право
    }
    let (x, y) = (r.area.col as usize + c.col, r.area.row as usize + c.row);
    if c.col < r.area.cols as usize && c.row < r.area.rows as usize && x < cols && y < rows {
        cells[y * cols + x].flags |= ereb_core::CellFlags::REVERSE;
    }
}

fn mark_focus(cells: &mut [Cell], r: &PaneRect, cols: usize, rows: usize) {
    let mut mark = |x: usize, y: usize, ch: char| {
        if x < cols && y < rows {
            let c = &mut cells[y * cols + x];
            c.ch = ch;
            c.fg = Color::Named(NamedColor::BrightCyan);
        }
    };
    let (x0, y0) = (r.area.col as usize, r.area.row as usize);
    let x1 = x0 + r.area.cols.saturating_sub(1) as usize;
    let y1 = y0 + r.area.rows.saturating_sub(1) as usize;
    if x0 > 0 {
        for y in y0..=y1 {
            mark(x0 - 1, y, '▏');
        }
    }
    if x1 + 1 < cols {
        for y in y0..=y1 {
            mark(x1 + 1, y, '▕');
        }
    }
    if y0 > 0 {
        for x in x0..=x1 {
            mark(x, y0 - 1, '▁');
        }
    }
    // Снизу — только если там ещё панельная область: последняя строка экрана занята статус-баром,
    // и писать в неё значит затирать его.
    if y1 + 1 < rows.saturating_sub(1) {
        for x in x0..=x1 {
            mark(x, y1 + 1, '▔');
        }
    }
}

/// Статус-бар: сколько панелей, какая в фокусе, жив ли её процесс, подсказка по префиксу.
#[allow(clippy::too_many_arguments)]
fn status_bar(
    cells: &mut [Cell], panes: &[Pane], focus: usize, mode: Mode, cols: usize, rows: usize,
    conf: &Conf,
) {
    let y = rows - 1;
    // Цветом же: в режиме команд полоса другая, и это видно боковым зрением.
    let bg = if mode == Mode::Pane { NamedColor::BrightYellow } else { NamedColor::BrightCyan };
    let mut text = String::new();
    use core::fmt::Write;
    let _ = write!(text, " VOID · панель {}/{} ", focus + 1, panes.len());
    if let Some(p) = panes.get(focus) {
        let _ = write!(text, "· {} ", if p.child.is_some() { "живая" } else { "мертва" });
        // Прокрутка показывается ТОЛЬКО когда она есть: строка состояния, в которой всегда
        // написано «0 строк», перестаёт читаться. Зато когда вьюпорт поднят — это видно сразу,
        // и «почему не появляется новый вывод» перестаёт быть загадкой.
        if p.scroll > 0 {
            let _ = write!(text, "· ↑{} из {} ", p.scroll, p.grid.scrollback_len());
        }
    }
    // Режим показывается всегда: модальное управление без индикатора — способ потеряться.
    // Клавиши в подсказке берутся ИЗ СХЕМЫ: после Вехи 100 их задаёт конфиг, и подсказка,
    // повторяющая зашитые буквы, врала бы ровно тому, кто схему поменял.
    let _ = write!(text, "· {}", hint(conf, mode));
    let mut i = 0usize;
    for ch in text.chars() {
        if i >= cols {
            break;
        }
        let c = &mut cells[y * cols + i];
        c.ch = ch;
        c.fg = Color::Named(NamedColor::Black);
        c.bg = Color::Named(bg);
        i += 1;
    }
    while i < cols {
        let c = &mut cells[y * cols + i];
        c.ch = ' ';
        c.bg = Color::Named(bg);
        i += 1;
    }
}

/// Подсказка по текущему режиму — из действующей схемы, а не из литерала.
fn hint(conf: &Conf, mode: Mode) -> String {
    use core::fmt::Write;
    let mut s = String::new();
    if mode != Mode::Pane {
        return match conf.key_for(Mode::Normal, "mode-pane") {
            Some(k) => {
                let _ = write!(s, "{} — команды панелей", k);
                s
            }
            None => s,
        };
    }
    // Без значка-«квадратика»: U+2B1B в шрифте нет, и вместо метки режима выходил тофу —
    // в статус-баре это читается как поломка. Режим и так виден цветом полосы и словом.
    let _ = write!(s, "ПАНЕЛИ:");
    for (action, label) in
        [("split-v", "разбить"), ("split-h", "поперёк"), ("close", "закрыть"), ("quit", "выход")]
    {
        if let Some(k) = conf.key_for(Mode::Pane, action) {
            let _ = write!(s, " {} {} ·", k, label);
        }
    }
    // Переходы — одной группой и не больше четырёх: в схеме по умолчанию их восемь (буквы и
    // стрелки), и полным списком подсказка вылезала за край полосы, обрываясь на полуслове.
    let nav: Vec<&str> = conf
        .binds
        .iter()
        .filter(|b| b.mode == Mode::Pane && b.action.starts_with("go-"))
        .map(|b| b.key.as_str())
        .collect();
    if !nav.is_empty() {
        let shown = nav.len().min(4);
        let _ = write!(s, " {}{} переход", nav[..shown].join(" "), if nav.len() > shown { " …" } else { "" });
    }
    s
}

/// Право на экран: опознаём по тому, что его ПРИНЯЛ `SYS_VIDEO_INFO` (проба безобидна).
fn find_fb_cap() -> Option<usize> {
    // По имени (Веха 99.1), иначе перебором: проба безобидна — `SYS_VIDEO_INFO` ничего не меняет.
    sys::cap_named("FB")
        .filter(|&c| sys::video_info(c).is_some())
        .or_else(|| {
            (0..8)
                .map(sys::start_cap)
                .find(|&c| c != sys::NO_CAP && sys::video_info(c).is_some())
        })
}

// ── курсор мыши (Веха 115) ───────────────────────────────────────────────────
//
// Курсор рисуется ПОВЕРХ кадра, прямо во фреймбуфер, и в теневую поверхность не попадает.
// Иначе его пришлось бы «стирать» перерисовкой знакомест, то есть гнать через растеризатор
// текст, который не менялся. А так движение стоит ровно два маленьких прямоугольника: вернуть
// пиксели под старым положением и нарисовать новое. Это тот же damage-подход, которым Веха 99.2
// вылечила отрисовку строк, — и он же понадобится композитору (ADR 0016: анимируем
// трансформации, а не содержимое).

const CUR_W: usize = 12;
const CUR_H: usize = 19;
/// Классическая стрелка: `#` — контур (чёрный), `.` — тело (белое), пробел — прозрачно.
/// Контур обязателен: белая стрелка на белом фоне иначе исчезает.
const CURSOR: [&str; CUR_H] = [
    "#           ",
    "##          ",
    "#.#         ",
    "#..#        ",
    "#...#       ",
    "#....#      ",
    "#.....#     ",
    "#......#    ",
    "#.......#   ",
    "#........#  ",
    "#.....##### ",
    "#..#..#     ",
    "#.# #..#    ",
    "##  #..#    ",
    "#    #..#   ",
    "     #..#   ",
    "      #.#   ",
    "      ###   ",
    "            ",
];

/// Нарисовать курсор во фреймбуфере. `pressed` — кнопка нажата: тело инвертируется, и это
/// единственная обратная связь, которая у нас пока есть на клик.
fn draw_cursor(info: &sys::VideoInfo, cx: usize, cy: usize, pressed: bool) {
    let bytes_pp = info.bpp / 8;
    let (fill_r, fill_g, fill_b) = if pressed { (90, 140, 255) } else { (255, 255, 255) };
    for (row, line) in CURSOR.iter().enumerate() {
        let y = cy + row;
        if y >= info.height {
            break;
        }
        for (col, ch) in line.bytes().enumerate() {
            let x = cx + col;
            if x >= info.width || ch == b' ' {
                continue;
            }
            let (r, g, b) = if ch == b'#' { (0, 0, 0) } else { (fill_r, fill_g, fill_b) };
            let px = pack(info, r, g, b);
            let dst = FB_VA + y * info.pitch + x * bytes_pp;
            unsafe {
                match bytes_pp {
                    4 => core::ptr::write_volatile(dst as *mut u32, px),
                    2 => core::ptr::write_volatile(dst as *mut u16, px as u16),
                    _ => {
                        core::ptr::write_volatile(dst as *mut u8, px as u8);
                        core::ptr::write_volatile((dst + 1) as *mut u8, (px >> 8) as u8);
                        core::ptr::write_volatile((dst + 2) as *mut u8, (px >> 16) as u8);
                    }
                }
            }
        }
    }
    fence();
}

/// Вернуть на место пиксели кадра в прямоугольнике (стирание курсора).
fn restore_rect(surface: &Surface, info: &sys::VideoInfo, x0: usize, y0: usize, w: usize, h: usize) {
    // (курсор и его стирание — мелкие прямоугольники; барьер ставится в конце каждой функции)
    let src = surface.data();
    let sw = surface.width() as usize;
    let sh = surface.height() as usize;
    let bytes_pp = info.bpp / 8;
    for y in y0..(y0 + h).min(info.height).min(sh) {
        for x in x0..(x0 + w).min(info.width).min(sw) {
            let p = (y * sw + x) * 4;
            let px = pack(info, src[p], src[p + 1], src[p + 2]);
            let dst = FB_VA + y * info.pitch + x * bytes_pp;
            unsafe {
                match bytes_pp {
                    4 => core::ptr::write_volatile(dst as *mut u32, px),
                    2 => core::ptr::write_volatile(dst as *mut u16, px as u16),
                    _ => {
                        core::ptr::write_volatile(dst as *mut u8, px as u8);
                        core::ptr::write_volatile((dst + 1) as *mut u8, (px >> 8) as u8);
                        core::ptr::write_volatile((dst + 2) as *mut u8, (px >> 16) as u8);
                    }
                }
            }
        }
    }
    fence();
}

/// Замерить, с какой скоростью машина пишет в фреймбуфер (Веха 115, требование ADR 0016).
///
/// В QEMU это обычная память хоста, и число ничего не значит. На настоящей машине фреймбуфер —
/// НЕКЭШИРУЕМЫЙ MMIO через PCIe, и вот там оно решает, возможны ли плавные анимации вообще:
/// полный кадр 1280×800×4 = 4 МиБ, и при 200 МБ/с это 20 мс — то есть 50 кадров в секунду ТОЛЬКО
/// на заливку экрана. Отсюда правило «анимируем трансформации, а не содержимое», и отсюда же
/// понятно, нужен ли WC-маппинг (PAT/MTRR).
fn measure_fb(info: &sys::VideoInfo) -> (u64, u64) {
    const PASSES: usize = 3;
    let bytes_pp = info.bpp / 8;
    let bytes = (PASSES * info.width * info.height * bytes_pp) as u64;

    // Проход A — по 4 байта, как писали раньше.
    let t0 = sys::monotonic_ns();
    for _ in 0..PASSES {
        for y in 0..info.height {
            let mut dst = FB_VA + y * info.pitch;
            for _ in 0..info.width {
                unsafe { core::ptr::write_volatile(dst as *mut u32, 0) };
                dst += bytes_pp;
            }
        }
    }
    fence();
    let a = bytes * 1000 / sys::monotonic_ns().saturating_sub(t0).max(1);

    // Проход B — по 8 байт (два пикселя разом). При UC это почти не помогает: цена в
    // транзакции, а не в байтах. При WC — помогает вдвое, потому что вдвое меньше записей
    // копится в буфер. Разница между A и B и есть ответ на вопрос «во что мы упёрлись».
    let t0 = sys::monotonic_ns();
    if bytes_pp == 4 {
        for _ in 0..PASSES {
            for y in 0..info.height {
                let mut dst = FB_VA + y * info.pitch;
                let mut x = 0;
                while x + 1 < info.width {
                    unsafe { core::ptr::write_volatile(dst as *mut u64, 0) };
                    dst += 8;
                    x += 2;
                }
                while x < info.width {
                    unsafe { core::ptr::write_volatile(dst as *mut u32, 0) };
                    dst += 4;
                    x += 1;
                }
            }
        }
    }
    fence();
    let b = bytes * 1000 / sys::monotonic_ns().saturating_sub(t0).max(1);
    (a, b)
}

/// Барьер записи. При write-combining записи копятся в буфере процессора и уходят пачкой —
/// без него последняя пачка может задержаться, и замер (как и кадр) окажется «быстрее» правды.
/// Инструкция непривилегированная, программе доступна.
///
/// На riscv барьера нет и не нужно: фреймбуфера там нет вовсе, а `term` собирается под обе
/// архитектуры лишь потому, что программы собираются все сразу (в store он сеется только на x86).
#[cfg(target_arch = "x86_64")]
fn fence() {
    unsafe { core::arch::asm!("sfence", options(nostack, preserves_flags)) };
}
#[cfg(not(target_arch = "x86_64"))]
fn fence() {}

/// Перенести кадр из RAM в фреймбуфер, упаковав пиксели в формат прошивки.
fn blit_rows(surface: &Surface, info: &sys::VideoInfo, base: usize, y_from: usize, y_to: usize) {
    let src = surface.data();
    let sw = surface.width() as usize;
    let bytes_pp = info.bpp / 8;
    let w = sw.min(info.width);
    let h = (surface.height() as usize).min(info.height).min(y_to);
    for y in y_from..h {
        let mut dst = base + y * info.pitch;
        let row = y * sw * 4;
        // Веха 117 — при write-combining выгодно писать ШИРЕ: пачку в 64 байта набирают 8
        // восьмибайтных записей вместо 16 четырёхбайтных. Пара соседних пикселей 32-бит
        // упаковывается в одно слово; хвост и невыровненное начало дописываются по 4 байта.
        if bytes_pp == 4 {
            let mut x = 0usize;
            if dst % 8 != 0 && x < w {
                let p = row + x * 4;
                let px = pack(info, src[p], src[p + 1], src[p + 2]);
                unsafe { core::ptr::write_volatile(dst as *mut u32, px) };
                dst += 4;
                x += 1;
            }
            while x + 1 < w {
                let p = row + x * 4;
                let a = pack(info, src[p], src[p + 1], src[p + 2]) as u64;
                let q = p + 4;
                let b = pack(info, src[q], src[q + 1], src[q + 2]) as u64;
                unsafe { core::ptr::write_volatile(dst as *mut u64, a | (b << 32)) };
                dst += 8;
                x += 2;
            }
            while x < w {
                let p = row + x * 4;
                let px = pack(info, src[p], src[p + 1], src[p + 2]);
                unsafe { core::ptr::write_volatile(dst as *mut u32, px) };
                dst += 4;
                x += 1;
            }
            continue;
        }
        for x in 0..w {
            let p = row + x * 4;
            let px = pack(info, src[p], src[p + 1], src[p + 2]);
            unsafe {
                match bytes_pp {
                    2 => core::ptr::write_volatile(dst as *mut u16, px as u16),
                    _ => {
                        core::ptr::write_volatile(dst as *mut u8, px as u8);
                        core::ptr::write_volatile((dst + 1) as *mut u8, (px >> 8) as u8);
                        core::ptr::write_volatile((dst + 2) as *mut u8, (px >> 16) as u8);
                    }
                }
            }
            dst += bytes_pp;
        }
    }
    // Пачки WC не должны залёживаться: кадр обязан оказаться на экране к моменту, когда мы
    // считаем его нарисованным.
    fence();
}

/// Упаковать RGB по раскладке прошивки (её сообщает `SYS_VIDEO_INFO`; зашивать нельзя — Веха 96).
#[inline]
fn pack(info: &sys::VideoInfo, r: u8, g: u8, b: u8) -> u32 {
    let mut out = 0u32;
    for (i, chan) in [r, g, b].iter().enumerate() {
        let (pos, size) = info.rgb[i];
        let size = size.clamp(1, 8);
        out |= ((*chan as u32) >> (8 - size)) << pos;
    }
    out
}
