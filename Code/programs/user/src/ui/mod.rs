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

pub mod anim;
pub mod conf;
pub mod font;
pub mod paint;
pub mod text;
pub mod theme;

// Реэкспорт для клиентов; сам тулкит движением не пользуется — оно принадлежит им (см. `anim`).
#[allow(unused_imports)]
pub use anim::Motion;
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

    /// Веха 145 — то же по вертикали: карточка меню это столбец, а столбец удобно резать сверху.
    pub fn cut_top(&mut self, h: i32) -> Rect {
        let h = h.min(self.h.max(0));
        let r = Rect::new(self.x, self.y, self.w, h);
        self.y += h;
        self.h -= h;
        r
    }
    pub fn cut_bottom(&mut self, h: i32) -> Rect {
        let h = h.min(self.h.max(0));
        self.h -= h;
        Rect::new(self.x, self.y + self.h, self.w, h)
    }

    /// Сдвинуть целиком — выезжающая карточка едет ровно этим.
    pub fn offset(self, dx: i32, dy: i32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.w, self.h)
    }

    pub fn contains(self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.right() && y < self.bottom()
    }

    /// Общая часть. Пустая — не пересекаются.
    pub fn intersect(self, o: Rect) -> Rect {
        let x = self.x.max(o.x);
        let y = self.y.max(o.y);
        Rect::new(x, y, self.right().min(o.right()) - x, self.bottom().min(o.bottom()) - y)
    }

    /// Насколько `o` накрывает нас по ширине — в долях 256. Этим считается цвет цифры стола под
    /// едущей капсулой: буква перекрашивается по мере того, как капсула её накрывает, а не
    /// скачком в момент прибытия.
    pub fn cover_x(self, o: Rect) -> u32 {
        if self.w <= 0 {
            return 0;
        }
        let n = (self.right().min(o.right()) - self.x.max(o.x)).max(0);
        (n * 256 / self.w).clamp(0, 256) as u32
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
    /// Веха 145 — ОБЩАЯ прозрачность всего, что рисуется дальше (1/256). Ею проявляется меню:
    /// иначе выезд пришлось бы городить из полутора десятков полупрозрачных цветов, согласованных
    /// между собой руками.
    fade: u32,
}

impl<'a> Ui<'a> {
    pub fn new(px: &'a mut [u8], w: i32, h: i32, th: &'a Theme, font: &'a mut Font) -> Ui<'a> {
        Ui {
            c: Canvas::new(px, w, h),
            th,
            font,
            ptr: None,
            click: None,
            dirty: Rect::ZERO,
            fade: 256,
        }
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
        // Помечаем ВИДИМУЮ часть: объявить композитору кусок, отрезанный клипом, значит попросить
        // его перерисовать то, чего мы не трогали.
        self.dirty = self.dirty.union(r.intersect(self.c.clip()));
    }

    /// Веха 145 — рисовать только внутри `r` (см. [`Canvas::set_clip`]).
    pub fn clip(&mut self, r: Rect) {
        self.c.set_clip(r);
    }

    /// Веха 145 — общая прозрачность всего последующего: 256 — как задумано, 0 — невидимо.
    pub fn fade(&mut self, a: u32) {
        self.fade = a.min(256);
    }

    /// Цвет темы, приглушённый общей прозрачностью кадра.
    fn tint(&self, c: Rgba) -> Rgba {
        if self.fade >= 256 {
            return c;
        }
        c.with_a((c.a as u32 * self.fade / 256) as u8)
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
        let (bg, br) = (self.tint(self.th.bg), self.tint(self.th.border));
        self.c.rrect_bordered(r, self.th.radius, self.th.line, bg, br);
        self.mark(r);
        r.inset_xy(self.th.pad, 0)
    }

    /// Строка текста, выровненная по вертикали посередине `r`.
    pub fn label(&mut self, r: Rect, s: &str, col: Rgba, align: Align) -> i32 {
        if r.is_empty() || s.is_empty() {
            return 0;
        }
        let col = self.tint(col);
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

    /// Веха 145 — **карточка**: тот же остров, но ПЛОТНЕЕ.
    ///
    /// Прозрачность панели — украшение: под островом обои, и сквозь тонкую полоску они читаются
    /// как глубина. Под карточкой лежит ТЕКСТ чужого окна, а РАЗМЫТИЯ У НАС НЕТ ВОВСЕ — и те же
    /// 10 % превращают её в грязь: на первом же снимке сквозь заголовок меню читалось «…ислить,
    /// иначе команда» из терминала под ней. Пробовал и промежуточное (97 %): при яркости текста
    /// в 200 уровней даже три процента дают видимую рябь.
    ///
    /// Поэтому карточка НЕПРОЗРАЧНА, и `ui("opacity", …)` на неё не действует. Это не игнор
    /// настройки, а её граница: человек просил прозрачную ПАНЕЛЬ, а не нечитаемое меню. Появится
    /// размытие ([[0018-gpu-ladder]]) — прозрачность вернётся сюда сама собой.
    pub fn card(&mut self, r: Rect) -> Rect {
        let bg = self.tint(self.th.bg.with_a(0xff));
        let br = self.tint(self.th.border);
        self.c.rrect_bordered(r, self.th.radius, self.th.line, bg, br);
        self.mark(r);
        r.inset_xy(self.th.pad, 0)
    }

    /// Веха 145 — **капсула активного**: одна на ряд, и она ЕДЕТ.
    ///
    /// Отдельный виджет, а не заливка внутри [`Ui::pill`], потому что в ряду столов активен ровно
    /// один, и капсула у него общая: рисуй её каждая пилюля сама — переезд превратился бы в
    /// «одна погасла, другая зажглась», то есть в мигание вместо движения. Прямоугольник сюда
    /// приезжает уже посчитанным ([`anim::Motion`]) — виджет не знает, что он движется.
    pub fn indicator(&mut self, r: Rect, rad: i32) {
        if r.is_empty() {
            return;
        }
        let c = self.tint(self.th.accent);
        self.c.rrect(r, rad, c);
        self.mark(r);
    }

    /// **Пилюля**: рабочий стол в панели.
    ///
    /// `hot` — насколько проявлена подсветка под курсором, `on` — насколько пилюлю накрыла
    /// капсула ([`Ui::indicator`]); оба в 1/256. Числами, а не `bool`, ровно затем, чтобы
    /// перекраска шла вместе с движением, а не рывком в его конце.
    pub fn pill(&mut self, r: Rect, s: &str, hot: u32, on: u32) -> bool {
        let bg = self.th.text.with_a((0x1f * hot.min(256) / 256) as u8);
        if bg.a != 0 {
            let c = self.tint(bg);
            self.c.rrect(r, r.h / 2, c);
        }
        let fg = self.th.muted.mix(self.th.text, hot).mix(self.th.on_accent, on);
        self.label(r, s, fg, Align::Center);
        self.mark(r);
        self.clicked(r)
    }

    /// Кнопка: текст в скруглённой подложке, которая проявляется под курсором.
    pub fn button(&mut self, r: Rect, s: &str, hot: u32) -> bool {
        let bg = self.th.text.with_a((0x22 * hot.min(256) / 256) as u8);
        if bg.a != 0 {
            let c = self.tint(bg);
            self.c.rrect(r, self.th.radius.min(r.h / 2), c);
        }
        self.label(r, s, self.th.text, Align::Center);
        self.mark(r);
        self.clicked(r)
    }

    /// Веха 145 — **плитка-переключатель** меню: крупнее пилюли, с рамкой, включённая залита
    /// акцентом. Пилюлей их не сделать: у пилюли нет ни рамки, ни своего состояния «включено» —
    /// у неё есть общая на ряд капсула, а плиток может гореть сколько угодно сразу.
    pub fn tile(&mut self, r: Rect, s: &str, hot: u32, on: u32) -> bool {
        let rad = self.th.radius.min(r.h / 2);
        let bg = self.th.text.with_a((0x14 * hot.min(256) / 256) as u8).mix(self.th.accent, on);
        let br = self.th.border.mix(self.th.accent, on);
        let (bg, br) = (self.tint(bg), self.tint(br));
        self.c.rrect_bordered(r, rad, self.th.line, bg, br);
        let fg = self.th.text.mix(self.th.on_accent, on);
        self.label(r, s, fg, Align::Center);
        self.mark(r);
        self.clicked(r)
    }

    /// Веха 145 — **строка сведений**: название слева приглушённо, значение справа. Единственный
    /// способ, которым меню что-то РАССКАЗЫВАЕТ; всё остальное в нём — кнопки.
    pub fn row(&mut self, r: Rect, key: &str, val: &str) {
        let kw = self.font.width(key) + self.th.gap;
        let mut r2 = r;
        let left = r2.cut_left(kw.min(r.w / 2));
        self.label(left, key, self.th.muted, Align::Left);
        self.label(r2, val, self.th.text, Align::Right);
    }

    /// Вертикальный волосок между частями острова.
    pub fn sep(&mut self, r: Rect) {
        let w = self.th.line.max(1);
        let line = Rect::new(r.x + (r.w - w) / 2, r.y, w, r.h);
        let c = self.tint(self.th.border);
        self.c.fill(line, c);
        self.mark(line);
    }

    /// Веха 145 — горизонтальный волосок: карточка меню делится на разделы.
    pub fn hsep(&mut self, r: Rect) {
        let h = self.th.line.max(1);
        let line = Rect::new(r.x, r.y + (r.h - h) / 2, r.w, h);
        let c = self.tint(self.th.border);
        self.c.fill(line, c);
        self.mark(line);
    }

    /// Веха 145 — кнопка ОПАСНОГО действия (выключение). Отличается цветом рамки и текста, а не
    /// формой: форма говорит «сюда можно нажать», цвет — «подумай».
    pub fn danger(&mut self, r: Rect, s: &str, hot: u32) -> bool {
        let rad = self.th.radius.min(r.h / 2);
        let bg = self.th.danger.with_a((0x28 * hot.min(256) / 256) as u8);
        let (bg, br) = (self.tint(bg), self.tint(self.th.danger.with_a(0x88)));
        self.c.rrect_bordered(r, rad, self.th.line, bg, br);
        self.label(r, s, self.th.danger, Align::Center);
        self.mark(r);
        self.clicked(r)
    }

    /// Ширина строки — раскладке ряда её надо знать заранее.
    pub fn text_w(&mut self, s: &str) -> i32 {
        self.font.width(s)
    }
}
