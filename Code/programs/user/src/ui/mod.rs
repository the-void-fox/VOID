//! `void-ui` — тулкит оболочки VOID (Веха 144, [[0016-void-ui-toolkit]]).
//!
//! ## Он родился из панели, а не до неё
//!
//! Правило фазы — «виджеты не писать впрок»: библиотека, у которой нет потребителя, описывает не
//! задачу, а фантазию о ней. Поэтому тулкит появился **переписыванием бара** и содержит ровно те
//! виджеты, которые бару понадобились: остров, текст, пилюля, кнопка, разделитель. Следующий
//! потребитель (меню, Веха 145) добавит свои — и это будет вторая проверка того, что здесь
//! библиотека, а не «вынесенные функции панели».
//!
//! ## Immediate mode
//!
//! Дерева виджетов нет. Программа каждый кадр говорит, что и где нарисовать, а виджет тут же
//! отвечает, попал ли в него клик. Состояния у тулкита нет вовсе — значит нечему разойтись с
//! состоянием программы; на этих граблях композитор уже стоял (обзор считал попадание клика не
//! тем кодом, которым рисовал, — Веха 123).
//!
//! ## Рисуем по damage
//!
//! [`Ui`] копит объединение того, что нарисовано, и отдаёт его [`Ui::dirty`] — клиент объявляет
//! композитору ровно этот прямоугольник. Наивная перерисовка всей поверхности была бы по карману
//! панели, но не окну приложения, а второй способ рисовать заводить потом дороже.
//!
//! ## Почему модуль по пути, а не крейт
//!
//! Тому же правилу подчиняются [`profile`](../profile.rs) и [`archive`](../archive.rs): здесь
//! нужен `alloc`, а объявить его в библиотеке `void_user` значило бы потребовать глобальный
//! аллокатор от ВСЕХ программ, включая живущие без кучи (`hello`, драйверы). Подключается так:
//!
//! ```ignore
//! #[path = "../ui/mod.rs"]
//! mod ui;
//! ```

// Запасной шрифт 8×16 и профиль пакетов — общие с терминалом, поэтому подключены по пути, а не
// скопированы: «панель нашла шрифт, а терминал нет» человек не смог бы объяснить себе ничем.
#[path = "../bitfont.rs"]
mod bitfont;
#[allow(dead_code)] // писательская половина профиля нужна `pkg`, тулкиту — чтение
#[path = "../profile.rs"]
mod profile;

pub mod conf;
pub mod font;
pub mod paint;
pub mod text;
pub mod theme;

pub use paint::{Canvas, Rgba};
pub use text::Font;
pub use theme::Theme;

/// Прямоугольник в координатах поверхности.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }

    pub const ZERO: Rect = Rect::new(0, 0, 0, 0);

    pub fn right(self) -> i32 {
        self.x + self.w
    }
    pub fn bottom(self) -> i32 {
        self.y + self.h
    }
    pub fn is_empty(self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    /// Сжать со всех сторон.
    pub fn inset(self, n: i32) -> Rect {
        self.inset_xy(n, n)
    }
    pub fn inset_xy(self, dx: i32, dy: i32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.w - 2 * dx, self.h - 2 * dy)
    }

    /// Отрезать полосу слева/справа: раскладка панели — это ряд, а ряд удобно резать.
    pub fn cut_left(&mut self, w: i32) -> Rect {
        let w = w.min(self.w.max(0));
        let r = Rect::new(self.x, self.y, w, self.h);
        self.x += w;
        self.w -= w;
        r
    }
    pub fn cut_right(&mut self, w: i32) -> Rect {
        let w = w.min(self.w.max(0));
        self.w -= w;
        Rect::new(self.x + self.w, self.y, w, self.h)
    }

    pub fn contains(self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.right() && y < self.bottom()
    }

    /// Наименьший прямоугольник, покрывающий оба. Пустой считается «ничего».
    pub fn union(self, o: Rect) -> Rect {
        if self.is_empty() {
            return o;
        }
        if o.is_empty() {
            return self;
        }
        let x = self.x.min(o.x);
        let y = self.y.min(o.y);
        Rect::new(x, y, self.right().max(o.right()) - x, self.bottom().max(o.bottom()) - y)
    }
}

/// Выравнивание текста внутри отведённого места.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// Кадр интерфейса: холст, тема, шрифт и ввод этого кадра.
pub struct Ui<'a> {
    pub c: Canvas<'a>,
    pub th: &'a Theme,
    pub font: &'a mut Font,
    /// Где курсор (`None` — не над нами). Нужен подсветке наведения.
    ptr: Option<(i32, i32)>,
    /// Где НАЖАЛИ в этом кадре. Ровно один кадр: иначе один щелчок сработал бы дважды.
    click: Option<(i32, i32)>,
    dirty: Rect,
}

impl<'a> Ui<'a> {
    pub fn new(px: &'a mut [u8], w: i32, h: i32, th: &'a Theme, font: &'a mut Font) -> Ui<'a> {
        Ui { c: Canvas::new(px, w, h), th, font, ptr: None, click: None, dirty: Rect::ZERO }
    }

    /// Начать кадр: где курсор и был ли клик. Клик действует ОДИН кадр — так у immediate-mode
    /// не заводится состояния, которое надо гасить.
    pub fn input(&mut self, ptr: Option<(i32, i32)>, click: Option<(i32, i32)>) {
        self.ptr = ptr;
        self.click = click;
    }

    /// Прямоугольник, который изменился с начала кадра. Пустой — не рисовали ничего.
    pub fn dirty(&self) -> Rect {
        self.dirty
    }

    /// Забыть накопленное: следующий кадр начнёт damage заново.
    pub fn reset(&mut self) {
        self.dirty = Rect::ZERO;
    }

    /// Стереть кусок поверхности В ПРОЗРАЧНОСТЬ (не в чёрный: чёрный — это цвет, и на обоях он
    /// виден полосой).
    pub fn clear(&mut self, r: Rect) {
        self.c.erase(r);
        self.mark(r);
    }

    /// Вся поверхность прозрачна.
    pub fn clear_all(&mut self) {
        self.c.clear();
        self.mark(Rect::new(0, 0, self.c.w, self.c.h));
    }

    fn mark(&mut self, r: Rect) {
        self.dirty = self.dirty.union(r);
    }

    /// Курсор внутри?
    pub fn hot(&self, r: Rect) -> bool {
        self.ptr.is_some_and(|(x, y)| r.contains(x, y))
    }

    /// Клик этого кадра внутри?
    pub fn clicked(&self, r: Rect) -> bool {
        self.click.is_some_and(|(x, y)| r.contains(x, y))
    }

    // ── виджеты ──────────────────────────────────────────────────────────────

    /// **Остров**: скруглённая подложка с рамкой. Возвращает место ВНУТРИ, где можно рисовать.
    ///
    /// Основа вида системы: панель — не полоса во всю ширину, а несколько островов на обоях
    /// (так же выглядит оболочка, которой владелец пользуется сегодня). Полоса потребовала бы
    /// непрозрачного фона, а он спорит со скруглением окон под ней.
    pub fn island(&mut self, r: Rect) -> Rect {
        self.c.rrect_bordered(r, self.th.radius, self.th.line, self.th.bg, self.th.border);
        self.mark(r);
        r.inset_xy(self.th.pad, 0)
    }

    /// Строка текста, выровненная по вертикали посередине `r`.
    pub fn label(&mut self, r: Rect, s: &str, col: Rgba, align: Align) -> i32 {
        if r.is_empty() || s.is_empty() {
            return 0;
        }
        let tw = self.font.width(s).min(r.w);
        let x = match align {
            Align::Left => r.x,
            Align::Center => r.x + (r.w - tw) / 2,
            Align::Right => r.right() - tw,
        };
        let base = r.y + (r.h - self.font.line_h()) / 2 + self.font.ascent();
        self.font.draw_clip(&mut self.c, x, base, s, col, r.w);
        self.mark(r);
        tw
    }

    /// **Пилюля**: рабочий стол в панели. Активная залита акцентом — цвет виден боковым зрением,
    /// а цифра нет.
    pub fn pill(&mut self, r: Rect, s: &str, active: bool) -> bool {
        let hot = self.hot(r);
        let (bg, fg) = if active {
            (self.th.accent, self.th.on_accent)
        } else if hot {
            (self.th.text.with_a(0x1f), self.th.text)
        } else {
            (Rgba::CLEAR, self.th.muted)
        };
        if bg.a != 0 {
            self.c.rrect(r, r.h / 2, bg);
        }
        self.label(r, s, fg, Align::Center);
        self.mark(r);
        self.clicked(r)
    }

    /// Кнопка: текст в скруглённой подложке, которая проявляется под курсором.
    pub fn button(&mut self, r: Rect, s: &str) -> bool {
        let bg = if self.hot(r) { self.th.text.with_a(0x22) } else { Rgba::CLEAR };
        if bg.a != 0 {
            self.c.rrect(r, self.th.radius.min(r.h / 2), bg);
        }
        self.label(r, s, self.th.text, Align::Center);
        self.mark(r);
        self.clicked(r)
    }

    /// Вертикальный волосок между частями острова.
    pub fn sep(&mut self, r: Rect) {
        let w = self.th.line.max(1);
        let line = Rect::new(r.x + (r.w - w) / 2, r.y, w, r.h);
        self.c.fill(line, self.th.border);
        self.mark(line);
    }

    /// Ширина строки — раскладке ряда её надо знать заранее.
    pub fn text_w(&mut self, s: &str) -> i32 {
        self.font.width(s)
    }
}
