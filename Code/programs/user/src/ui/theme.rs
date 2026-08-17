//! Тема: цвета, скругления, отступы и кегль — из КОНФИГА ПОКОЛЕНИЯ (Веха 144).
//!
//! ## Почему настройка вида живёт в конфиге, а не в файле темы
//!
//! Потому что тогда вид получает поколения и откат бесплатно ([[0017-settings-from-config]]):
//! `ui("accent", "#4c7dfd")` → `rebuild` → новое поколение; не понравилось — откат системы
//! возвращает и цвет. Отдельный файл темы пришлось бы версионировать своими руками, а «система
//! откатилась, а панель осталась чужого цвета» — ровно тот рассинхрон, ради отсутствия которого
//! в VOID вообще есть поколения.
//!
//! ## Масштаб закладывается СРАЗУ
//!
//! `ui("scale", 150)` умножает все размеры темы один раз, здесь, а не в каждом виджете. Задним
//! числом это переписывание всех отступов системы — поэтому и сразу. Виджет, которому нужна
//! собственная величина, обязан пропустить её через [`Theme::px`]; голое число в пикселях внутри
//! виджета — ошибка, которую видно только на чужом экране.

use alloc::string::{String, ToString};

use super::paint::Rgba;

/// Всё, чем тулкит рисует. Числовые поля УЖЕ в пикселях экрана (масштаб применён).
#[derive(Clone)]
pub struct Theme {
    /// Фон острова/карточки. С альфой: скруглённый угол обязан показывать обои под собой.
    pub bg: Rgba,
    /// Рамка острова — на тёмных обоях без неё край растворяется.
    pub border: Rgba,
    /// Основной текст.
    pub text: Rgba,
    /// Второстепенный текст и неактивное.
    pub muted: Rgba,
    /// Акцент: активный стол, фокус, выделение.
    pub accent: Rgba,
    /// Текст поверх акцента.
    pub on_accent: Rgba,
    /// Опасное действие (выключение, удаление).
    pub danger: Rgba,
    /// Радиус скругления острова.
    pub radius: i32,
    /// Зазор между соседями.
    pub gap: i32,
    /// Внутреннее поле острова.
    pub pad: i32,
    /// Толщина рамки.
    pub line: i32,
    /// Кегль в пикселях.
    pub font_px: u32,
    /// Имя файла шрифта в пакетах профиля. `None` — встроенный 8×16 ([[known-gaps]]: шрифт
    /// приходит пакетом, и его может не быть).
    pub font: Option<String>,
    /// Масштаб в процентах — хранится, чтобы виджет мог пересчитать свою величину.
    pub scale: u32,
}

impl Default for Theme {
    fn default() -> Theme {
        Theme::VOID
    }
}

impl Theme {
    /// Вид системы по умолчанию. Цвета те же, что у композитора (`C_DESKTOP`, `C_BORDER`,
    /// `C_ACCENT` в `bin/wm.rs`) — панель и окна обязаны выглядеть одной системой, а не двумя.
    pub const VOID: Theme = Theme {
        bg: Rgba::hex(0x151b23).with_a(0xe6),
        border: Rgba::hex(0x30363d).with_a(0xcc),
        text: Rgba::hex(0xc9d1d9),
        muted: Rgba::hex(0x7d8590),
        accent: Rgba::hex(0x4c7dfd),
        on_accent: Rgba::hex(0x080c12),
        danger: Rgba::hex(0xf85149),
        radius: 10,
        gap: 8,
        pad: 10,
        line: 1,
        font_px: 15,
        font: None,
        scale: 100,
    };

    /// Перевести величину, задуманную в «единицах темы», в пиксели этого экрана.
    pub fn px(&self, n: i32) -> i32 {
        n * self.scale as i32 / 100
    }

    /// Собрать тему из текста поколения: строки `ui <ключ> <значение>`.
    ///
    /// Неизвестный ключ молча пропускается ЗДЕСЬ, но не в системе: опечатку ловит `rebuild`
    /// (`vvsh-core`, `normalize_config`), потому что этап сборки конфига для того и есть. Здесь
    /// же строка уже проверена, и спорить с ней поздно — панель обязана подняться.
    pub fn from_config(text: &str) -> Theme {
        let mut t = Theme::VOID;
        // Масштаб читается ПЕРВЫМ проходом: им умножаются все размеры, включая те, что придут
        // следующими строками. Иначе порядок строк в конфиге менял бы результат.
        if let Some(v) = value(text, "scale").and_then(|v| v.parse::<u32>().ok()) {
            t.scale = v.clamp(50, 400);
        }
        let mut opacity = None;
        for line in text.lines() {
            let Some(rest) = line.trim().strip_prefix("ui ") else { continue };
            let rest = rest.trim();
            let (key, val) = match rest.split_once(char::is_whitespace) {
                Some((k, v)) => (k, v.trim()),
                None => continue,
            };
            let color = |slot: &mut Rgba| {
                if let Some(c) = Rgba::parse(val) {
                    // Восьмизначная запись задаёт альфу сама, шестизначная её не трогает: иначе
                    // `ui("panel", "#202020")` сделал бы панель непрозрачной, хотя человек
                    // менял только цвет.
                    let explicit = val.trim_start_matches('#').len() == 8;
                    *slot = if explicit { c } else { c.with_a(slot.a) };
                }
            };
            match key {
                "accent" => color(&mut t.accent),
                "text" => color(&mut t.text),
                "muted" => color(&mut t.muted),
                "panel" => color(&mut t.bg),
                "border" => color(&mut t.border),
                "danger" => color(&mut t.danger),
                "on-accent" => color(&mut t.on_accent),
                "radius" => num(val, &mut t.radius, 0, 64),
                "gap" => num(val, &mut t.gap, 0, 64),
                "pad" => num(val, &mut t.pad, 0, 64),
                "font-size" => {
                    let mut n = t.font_px as i32;
                    num(val, &mut n, 8, 64);
                    t.font_px = n as u32;
                }
                "font" => t.font = Some(val.to_string()),
                "opacity" => {
                    let mut n = 100;
                    num(val, &mut n, 0, 100);
                    opacity = Some(n as u32);
                }
                _ => {}
            }
        }
        if let Some(p) = opacity {
            // Непрозрачность задаётся ОДНИМ числом на всю панель, а не альфой в каждом цвете:
            // человек хочет «панель на 80 %», а не подбирать восемь согласованных `#rrggbbaa`.
            t.bg.a = (0xff * p / 100) as u8;
            t.border.a = (t.border.a as u32 * p / 100) as u8;
        }
        t.radius = t.px(t.radius);
        t.gap = t.px(t.gap);
        t.pad = t.px(t.pad);
        t.line = t.px(t.line).max(1);
        t.font_px = (t.font_px * t.scale / 100).max(8);
        t
    }
}

/// Значение ключа `ui <key> <value>` — первое вхождение.
fn value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("ui ") else { continue };
        let Some(v) = rest.trim().strip_prefix(key) else { continue };
        let v = v.trim();
        if !v.is_empty() {
            return Some(v);
        }
    }
    None
}

fn num(val: &str, slot: &mut i32, lo: i32, hi: i32) {
    if let Ok(n) = val.parse::<i32>() {
        *slot = n.clamp(lo, hi);
    }
}
