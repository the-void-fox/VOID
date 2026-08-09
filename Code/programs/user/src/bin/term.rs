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
    Csi,
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
            _ => {
                // Печатный байт: keysym совпадает с кодом символа (латиница ASCII).
                let mut k = KeyEvent::new(Keysym(b as u32), ModMask::empty());
                k.text.push(b as char);
                Some(k)
            }
        },
        KeyScan::Esc => {
            if b == b'[' {
                *state = KeyScan::Csi;
                None
            } else {
                *state = KeyScan::Ground;
                Some(KeyEvent::new(Keysym::ESCAPE, ModMask::empty()))
            }
        }
        KeyScan::Csi => {
            *state = KeyScan::Ground;
            let sym = match b {
                b'A' => Keysym::UP,
                b'B' => Keysym::DOWN,
                b'C' => Keysym::RIGHT,
                b'D' => Keysym::LEFT,
                b'H' => Keysym::HOME,
                b'F' => Keysym::END,
                // `ESC [ 3 ~` (Delete) и прочие числовые — цифры глотаем, оставаясь в CSI.
                b'0'..=b'9' | b';' => {
                    *state = KeyScan::Csi;
                    return None;
                }
                b'~' => Keysym::DELETE,
                _ => return None,
            };
            Some(KeyEvent::new(sym, ModMask::empty()))
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
}

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    // Настройки читаются ПЕРВЫМИ: от них зависят и кегль шрифта (а значит вся геометрия), и то,
    // что запускать в панелях.
    let mut conf = Conf::load();

    let Some(fb_cap) = find_fb_cap() else {
        sys::write_console("[term] нет права на экран (mmio:fb в конфиге init)\n".as_bytes());
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

    loop {
        let mut worked = false;

        // ── 1. клавиатура (никогда не блокируемся) ─────────────────────────────────────────
        let n = sys::read_console_nonblock(&mut keys);
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
                    blit_rows(&view.surface, &info, y * ch, ((y + 1) * ch).min(info.height));
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
                    cells[y * cols + x] = p.grid.view_cell(col, row, 0);
                }
            }
        }
        if i == focus {
            mark_focus(&mut cells, r, cols, rows);
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

/// Перенести кадр из RAM в фреймбуфер, упаковав пиксели в формат прошивки.
fn blit_rows(surface: &Surface, info: &sys::VideoInfo, y_from: usize, y_to: usize) {
    let src = surface.data();
    let sw = surface.width() as usize;
    let bytes_pp = info.bpp / 8;
    let w = sw.min(info.width);
    let h = (surface.height() as usize).min(info.height).min(y_to);
    for y in y_from..h {
        let mut dst = FB_VA + y * info.pitch;
        let row = y * sw * 4;
        for x in 0..w {
            let p = row + x * 4;
            let px = pack(info, src[p], src[p + 1], src[p + 2]);
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
            dst += bytes_pp;
        }
    }
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
