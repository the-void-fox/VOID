//! Холст тулкита: RGBA-кадр, смешивание и скруглённые прямоугольники со сглаживанием.
//!
//! ## Почему альфа настоящая, а не «цвет фона под панелью»
//!
//! Остров со скруглёнными углами обязан показывать то, что под ним, — иначе уголки придётся
//! закрашивать цветом обоев, а обои у человека свои. Поэтому холст пишет **прямую альфу** в
//! четвёртый байт пикселя, а накладывает поверхность композитор ([[layers]]): клиент объявляет
//! `Layer::alpha`, и `wm` смешивает её кадр по каждому пикселю вместо копирования строк.
//!
//! Прямая (не предумноженная) альфа выбрана потому, что кадр читает ещё и человек глазами через
//! `void-img`, и потому, что композитор и так умеет `blend(dst, src, cover)` — предумножение
//! потребовало бы второго пути смешивания ради одной панели.
//!
//! ## Сглаживание — то же, что у окон
//!
//! Скругление считается знаковым расстоянием до прямоугольника со скруглёнными углами, ровно как
//! в композиторе (`rrect_sd` в `bin/wm.rs`): целочисленно, в 1/256 пикселя, без единого `sqrt`
//! на пиксель середины. Две разные формулы дали бы два разных скругления — у окна и у панели, —
//! и система выглядела бы собранной из двух систем.
//!
//! ## Нижний ярус тулкита: годится всем, у кого есть буфер пикселей
//!
//! Здесь нет ни виджетов, ни ввода, ни damage — только [`Rgba`], [`Rect`] и [`Canvas`]. Ярус
//! выделен не ради красоты: у композитора есть буфер пикселей (плитка запуска) и нет ничего из
//! верхнего яруса — он не клиент самому себе. Пока эти вещи лежали вперемешку с [`super::Ui`],
//! он честно завёл СВОЙ холст, свой прямоугольник и свою палитру, и три цвета из пяти в них уже
//! разошлись. Граница проходит здесь: ниже — как рисовать, выше ([`super`]) — что рисовать.

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

    /// `self` МИНУС `hole` — до четырёх полос: сверху, снизу, слева и справа от дырки.
    ///
    /// Пустые прямоугольники в ответе значат «этой полосы нет»; отсеивать их — дело зовущего
    /// (`filter(|r| !r.is_empty())`), зато сам разбор не выделяет памяти и годится там, где кучи
    /// нет. Не пересекаются — `self` возвращается целиком первой полосой.
    ///
    /// Веха 149: композитор считал этим дополнение экрана к окнам (щели, поля, панель) — своей
    /// копией на кортежах. Тулкиту то же самое нужно всякий раз, когда что-то рисуется ВОКРУГ
    /// чего-то.
    pub fn subtract(self, hole: Rect) -> [Rect; 4] {
        let h = self.intersect(hole);
        if h.is_empty() {
            return [self, Rect::ZERO, Rect::ZERO, Rect::ZERO];
        }
        [
            Rect::new(self.x, self.y, self.w, h.y - self.y),
            Rect::new(self.x, h.bottom(), self.w, self.bottom() - h.bottom()),
            Rect::new(self.x, h.y, h.x - self.x, h.h),
            Rect::new(h.right(), h.y, self.right() - h.right(), h.h),
        ]
    }
}

/// Выравнивание текста внутри отведённого места.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// Цвет с прямой альфой. `a = 0` — пиксель не пишется вовсе.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const CLEAR: Rgba = Rgba { r: 0, g: 0, b: 0, a: 0 };

    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Rgba {
        Rgba { r, g, b, a }
    }

    /// Непрозрачный цвет из `0xRRGGBB` — так он пишется в конфиге и так же читается глазами.
    pub const fn hex(v: u32) -> Rgba {
        Rgba { r: (v >> 16) as u8, g: (v >> 8) as u8, b: v as u8, a: 0xff }
    }

    pub const fn with_a(self, a: u8) -> Rgba {
        Rgba { r: self.r, g: self.g, b: self.b, a }
    }

    /// Разобрать `#rrggbb` или `#rrggbbaa` из конфига. `None` — не цвет.
    pub fn parse(s: &str) -> Option<Rgba> {
        let h = s.strip_prefix('#').unwrap_or(s);
        if h.len() != 6 && h.len() != 8 {
            return None;
        }
        let mut v = [0u8; 4];
        v[3] = 0xff;
        for (i, b) in v.iter_mut().enumerate().take(h.len() / 2) {
            let byte = h.get(i * 2..i * 2 + 2)?;
            *b = u8::from_str_radix(byte, 16).ok()?;
        }
        Some(Rgba { r: v[0], g: v[1], b: v[2], a: v[3] })
    }

    /// Смешать с другим цветом: `t` в долях 256. Нужно состояниям «наведён» и «нажат» —
    /// отдельных цветов в теме на них нет намеренно, иначе тема разрослась бы втрое.
    pub fn mix(self, o: Rgba, t: u32) -> Rgba {
        let t = t.min(256);
        let m = |a: u8, b: u8| ((a as u32 * (256 - t) + b as u32 * t) / 256) as u8;
        Rgba { r: m(self.r, o.r), g: m(self.g, o.g), b: m(self.b, o.b), a: m(self.a, o.a) }
    }
}

/// Кадр клиента: RGBA8888 по строкам — те самые страницы, в которые смотрит композитор.
pub struct Canvas<'a> {
    px: &'a mut [u8],
    pub w: i32,
    pub h: i32,
    /// Веха 145 — за этот прямоугольник не пишется ни один пиксель.
    clip: Rect,
}

impl<'a> Canvas<'a> {
    pub fn new(px: &'a mut [u8], w: i32, h: i32) -> Canvas<'a> {
        Canvas { px, w, h, clip: Rect::new(0, 0, w, h) }
    }

    /// Веха 145 — ограничить рисование прямоугольником. Понадобилось ВЫЕЗЖАЮЩЕЙ карточке: она
    /// начинает движение выше своего места и обязана быть срезанной краем панели, а не наехать
    /// на её острова. Проверкой в [`Canvas::blend`], а не отдельным путём: клип, о котором можно
    /// забыть в одном виджете из десяти, хуже отсутствующего.
    pub fn set_clip(&mut self, r: Rect) {
        self.clip = r.intersect(Rect::new(0, 0, self.w, self.h));
    }

    pub fn clip(&self) -> Rect {
        self.clip
    }

    /// Весь холст — ПРОЗРАЧНЫМ. Не чёрным: чёрный это цвет, и на обоях он виден полосой.
    pub fn clear(&mut self) {
        self.px.fill(0);
    }

    /// Один пиксель: `cov` — покрытие в долях 256 (сглаживание), альфа берётся из цвета.
    ///
    /// Смешивание по прямой альфе честное, с делением: `out_a = sa + da(1-sa)`, цвет — среднее
    /// по вкладам. Без деления получилось бы «поверх непрозрачного» верно, а «поверх
    /// полупрозрачного» — темнее, чем надо, и щели у скруглений выдали бы это сразу.
    #[inline]
    pub fn blend(&mut self, x: i32, y: i32, c: Rgba, cov: u32) {
        if cov == 0 || c.a == 0 || !self.clip.contains(x, y) {
            return;
        }
        let i = ((y * self.w + x) * 4) as usize;
        if i + 3 >= self.px.len() {
            return;
        }
        let sa = c.a as u32 * cov.min(256) / 256;
        if sa == 0 {
            return;
        }
        if sa >= 255 {
            self.px[i] = c.r;
            self.px[i + 1] = c.g;
            self.px[i + 2] = c.b;
            self.px[i + 3] = 0xff;
            return;
        }
        let da = self.px[i + 3] as u32;
        // Веха 148.2 — НЕПРОЗРАЧНАЯ подложка отдельным путём, и это не микрооптимизация.
        //
        // Общая формула делит дважды на каждый канал (`/255` и `/oa`), а деление стоит десятков
        // тактов. В ОКНЕ же под каждым пикселем лежит непрозрачный фон (`Ui::background`), то есть
        // `da = 255` — и вся формула сворачивается: `oa` заведомо 255, остаётся одно деление на
        // константу, а его компилятор превращает в умножение. Это горячий путь СГЛАЖЕННОГО
        // ТЕКСТА: каждая буква — сотня краевых пикселей, и на кадре их сотни тысяч.
        if da >= 255 {
            let k = 255 - sa;
            let mix = |s: u8, d: u8| div255(s as u32 * sa + d as u32 * k) as u8;
            self.px[i] = mix(c.r, self.px[i]);
            self.px[i + 1] = mix(c.g, self.px[i + 1]);
            self.px[i + 2] = mix(c.b, self.px[i + 2]);
            self.px[i + 3] = 0xff;
            return;
        }
        let k = div255(da * (255 - sa)); // вклад того, что уже нарисовано
        let oa = sa + k;
        if oa == 0 {
            return;
        }
        let mix = |s: u8, d: u8| ((s as u32 * sa + d as u32 * k) / oa) as u8;
        self.px[i] = mix(c.r, self.px[i]);
        self.px[i + 1] = mix(c.g, self.px[i + 1]);
        self.px[i + 2] = mix(c.b, self.px[i + 2]);
        self.px[i + 3] = oa as u8;
    }

    /// Строка пикселей ОДНИМ цветом без смешивания. Только для непрозрачного цвета — на нём
    /// смешивание сводится к записи, и вся арифметика лишняя.
    #[inline]
    fn row_solid(&mut self, y: i32, x0: i32, x1: i32, c: Rgba) {
        if x1 <= x0 {
            return;
        }
        let a = ((y * self.w + x0) * 4) as usize;
        let b = ((y * self.w + x1) * 4) as usize;
        if b > self.px.len() {
            return;
        }
        let p = [c.r, c.g, c.b, 0xff];
        for px in self.px[a..b].chunks_exact_mut(4) {
            px.copy_from_slice(&p);
        }
    }

    /// Стереть кусок В ПРОЗРАЧНОСТЬ. Не «залить фоном»: под панелью обои, и любой цвет здесь
    /// был бы враньём о том, что там на самом деле.
    pub fn erase(&mut self, r: Rect) {
        let r = r.intersect(self.clip);
        for y in r.y..r.bottom() {
            let a = ((y * self.w + r.x) * 4) as usize;
            let b = ((y * self.w + r.right()) * 4) as usize;
            if b <= self.px.len() && a < b {
                self.px[a..b].fill(0);
            }
        }
    }

    /// Прямоугольник без скруглений.
    pub fn fill(&mut self, r: Rect, c: Rgba) {
        let vis = r.intersect(self.clip);
        if vis.is_empty() {
            return;
        }
        // Веха 148.2 — непрозрачное заливается СТРОКАМИ. Фон окна это полмиллиона пикселей, и
        // каждый из них шёл через `blend` с проверкой клипа и арифметикой альфы: 2.6 мс из
        // 5.2 мс кадра вьювера уходило ровно сюда (замер).
        if c.a == 0xff {
            for y in vis.y..vis.bottom() {
                self.row_solid(y, vis.x, vis.right(), c);
            }
            return;
        }
        for y in vis.y..vis.bottom() {
            for x in vis.x..vis.right() {
                self.blend(x, y, c, 256);
            }
        }
    }

    /// Скруглённый прямоугольник со сглаженным краем.
    pub fn rrect(&mut self, r: Rect, rad: i32, fill: Rgba) {
        self.rrect_bordered(r, rad, 0, fill, Rgba::CLEAR);
    }

    /// Скруглённый прямоугольник с рамкой толщиной `t` изнутри.
    ///
    /// Рамка рисуется КОЛЬЦОМ (покрытие внешней формы минус внутренней), а не «сначала вся
    /// форма цветом рамки, сверху заливка». Со сплошными цветами разницы нет, с
    /// полупрозрачными — есть: два наложения дали бы в середине альфу больше заказанной, и
    /// остров получился бы плотнее, чем просили.
    pub fn rrect_bordered(&mut self, r: Rect, rad: i32, t: i32, fill: Rgba, border: Rgba) {
        if r.w <= 0 || r.h <= 0 {
            return;
        }
        let rad = rad.clamp(0, r.w.min(r.h) / 2);
        // Обходим только видимую часть: карточка, срезанная клипом наполовину, не должна стоить
        // как целая — она рисуется каждый кадр движения.
        let vis = r.intersect(self.clip);
        // Веха 148.2 — СЕРЕДИНА без скруглений: строки дальше `rad` от верха и низа — это ровно
        // рамка слева, рамка справа и заливка между ними. Считать там знаковое расстояние и
        // смешивать по пикселю незачем: карточка подробностей во вьювере — это триста тысяч
        // пикселей, и почти все они здесь.
        let (my0, my1) = (r.y + rad, r.bottom() - rad);
        let straight = rad >= t && my1 > my0;
        if straight {
            let (y0, y1) = (my0.max(vis.y), my1.min(vis.bottom()));
            if y1 > y0 {
                if t > 0 {
                    self.fill(Rect::new(r.x, y0, t, y1 - y0), border);
                    self.fill(Rect::new(r.right() - t, y0, t, y1 - y0), border);
                }
                self.fill(Rect::new(r.x + t, y0, r.w - 2 * t, y1 - y0), fill);
            }
        }
        for y in vis.y..vis.bottom() {
            // Середину уже залили полосами — второй раз по ней не идём.
            if straight && y >= my0 && y < my1 {
                continue;
            }
            // Строка вдали от углов заливается без единого корня: покрытие там известно заранее.
            let near_y = (y - r.y) < rad || (r.y + r.h - 1 - y) < rad;
            for x in vis.x..vis.right() {
                let near_x = (x - r.x) < rad || (r.x + r.w - 1 - x) < rad;
                let (out, inn) = if near_y && near_x {
                    let d = rrect_sd(x, y, r, rad);
                    ((128 - d).clamp(0, 256) as u32, (128 - (d + t * 256)).clamp(0, 256) as u32)
                } else {
                    let edge = (x - r.x) < t
                        || (r.x + r.w - 1 - x) < t
                        || (y - r.y) < t
                        || (r.y + r.h - 1 - y) < t;
                    (256, if edge { 0 } else { 256 })
                };
                if out == 0 {
                    continue;
                }
                if out > inn {
                    self.blend(x, y, border, out - inn);
                }
                if inn > 0 {
                    self.blend(x, y, fill, inn);
                }
            }
        }
    }

    /// Веха 145.1 — картинка RGBA в прямоугольник `r`, обрезанная скруглением `rad`.
    ///
    /// Масштаба здесь нет: картинку под нужный размер приводит `void-img` (там честное
    /// усреднение, а не выборка каждого N-го пикселя — иначе лицо на аватаре превращается в
    /// муар). Наше дело — положить готовые пиксели под маску круга.
    pub fn image(&mut self, r: Rect, px: &[u8], iw: i32, ih: i32, rad: i32) {
        if r.w <= 0 || r.h <= 0 || iw <= 0 || ih <= 0 {
            return;
        }
        let rad = rad.clamp(0, r.w.min(r.h) / 2);
        let vis = r.intersect(self.clip);
        for y in vis.y..vis.bottom() {
            let sy = (y - r.y).clamp(0, ih - 1);
            for x in vis.x..vis.right() {
                let cov = if rad == 0 {
                    256
                } else {
                    (128 - rrect_sd(x, y, r, rad)).clamp(0, 256) as u32
                };
                if cov == 0 {
                    continue;
                }
                let sx = (x - r.x).clamp(0, iw - 1);
                let i = ((sy * iw + sx) * 4) as usize;
                let Some(s) = px.get(i..i + 4) else { continue };
                self.blend(x, y, Rgba::new(s[0], s[1], s[2], s[3]), cov);
            }
        }
    }

    /// Веха 158.2 — **ВОГНУТЫЙ угол**: квадрат `rad × rad`, из которого вырезана четверть круга.
    ///
    /// Нужен там, где панель примыкает к краю экрана. Полоса панели кончается прямой линией, а
    /// стол под ней в макете имеет скруглённые верхние углы — значит на стыке цвет полосы должен
    /// затекать в угол и сходить на нет по дуге. Скруглённым прямоугольником этого не сделать:
    /// он выпуклый, а тут нужна ровно обратная форма.
    ///
    /// `corner` — в каком углу `r` стоит ЦЕНТР дуги, `(dx, dy)` из `{-1, 1}`: положительное —
    /// слева/сверху, отрицательное — справа/снизу. Цвет ложится СНАРУЖИ дуги, то есть в углу,
    /// противоположном центру. Для стыка панели с левым краем экрана это `(-1, -1)`: центр в
    /// правом нижнем углу квадратика, цвет затекает в левый верхний — к самому краю.
    ///
    /// Сглаживание — тем же способом, что у скруглений: расстояние до центра дуги в 1/256 доли
    /// пикселя, покрытие из него. Две разные формулы дали бы выпуклый и вогнутый углы разной
    /// мягкости, и стык был бы виден именно там, где его и разглядывают.
    pub fn fillet(&mut self, r: Rect, corner: (i32, i32), fill: Rgba) {
        let rad = r.w.min(r.h);
        if rad <= 0 {
            return;
        }
        // Центр дуги — в том углу, куда смотрит вырез.
        let cx = if corner.0 > 0 { r.x } else { r.right() };
        let cy = if corner.1 > 0 { r.y } else { r.bottom() };
        let vis = r.intersect(self.clip);
        let r2 = (rad * 256) as i64;
        for y in vis.y..vis.bottom() {
            for x in vis.x..vis.right() {
                // Середина пикселя, в 1/256 — как и в `rrect_sd`.
                let dx = ((x - cx) * 256 + 128) as i64;
                let dy = ((y - cy) * 256 + 128) as i64;
                // Расстояние сравниваем БЕЗ корня: корня в no_std нет, а нужен он тут только
                // затем, чтобы получить мягкий край, — его даёт разность квадратов, делённая на
                // сумму, то есть та же дуга с точностью до полпикселя.
                let d2 = dx * dx + dy * dy;
                let sum = d2.isqrt().max(1);
                let sd = sum - r2; // >0 — снаружи дуги, там и есть цвет
                let cov = (sd + 128).clamp(0, 256) as u32;
                self.blend(x, y, fill, cov);
            }
        }
    }

    /// Веха 158 — ВЕКТОРНАЯ ИКОНКА `.vg`, вписанная в `r` и перекрашенная в `c`.
    ///
    /// Растеризатор чужой (`void-vec`), а смешивание наше: он отдаёт покрытие каждого пикселя,
    /// а кладёт его на холст [`Canvas::blend`] — то же, чем рисует весь тулкит. Иначе иконка не
    /// знала бы ни про отсечение (выезжающая карточка обязана срезаться краем панели), ни про
    /// честную альфу на полупрозрачной подложке.
    ///
    /// Цвет назначает ТЕМА, а не файл: иконки одноцветные, и состояний у них три (обычное,
    /// приглушённое, под курсором). Держать по файлу на состояние значило бы разъехаться с
    /// темой при первой же правке цвета.
    ///
    /// Битый или чужой файл просто не рисуется. Отказ здесь ничем не поможет: панель обязана
    /// подняться, а дырка на месте иконки видна и так.
    pub fn vg(&mut self, r: Rect, data: &[u8], c: Rgba) {
        let Some(v) = void_vec::Vg::parse(data) else { return };
        if r.w <= 0 || r.h <= 0 {
            return;
        }
        let cl = self.clip;
        let clip = void_vec::Clip::new(cl.x, cl.y, cl.right(), cl.bottom());
        let fit = void_vec::Fit::new(r.x, r.y, r.w as u32, r.h as u32);
        // Цвет один на всю иконку, поэтому альфу из файла спрашивать не у кого — её несёт `c`.
        let tint = [c.r, c.g, c.b, c.a];
        // `self` разделять нельзя (замыкание пишет в холст), поэтому растеризатор зовётся с
        // указателем на метод, а не с захватом полей.
        let mut sink = |x: i32, y: i32, col: [u8; 4], cov: u32| {
            self.blend(x, y, Rgba::new(col[0], col[1], col[2], col[3]), cov);
        };
        void_vec::rasterize(&v, fit, clip, Some(tint), &mut sink);
    }

    /// Веха 145.1 — знак ВЫКЛЮЧЕНИЯ: разомкнутое сверху кольцо и вертикальная черта.
    ///
    /// Нарисован формулой, а не взят из шрифта и не разобран из SVG. Довод простой: знаков
    /// системы единицы, и каждый — две-три геометрические фигуры, а SVG это парсер путей плюс
    /// растеризатор кривых плюс трансформации. Иконочный шрифт был бы дешевле SVG (растеризатор
    /// контуров у нас уже есть), но шрифт приходит ПАКЕТОМ — на свежей системе его нет, и кнопка
    /// выключения осталась бы пустым кружком. Формула не зависит ни от чего.
    pub fn power(&mut self, r: Rect, thick: i32, c: Rgba) {
        let (cx, cy) = (2 * r.x + r.w - 1, 2 * r.y + r.h - 1); // центр в ПОЛОВИНАХ пикселя
        let rad = (r.w.min(r.h) - thick) / 2;
        let (t2, rad16) = (thick * 8, rad * 16); // полутолщина и радиус в 1/16 пикселя
        let vis = r.intersect(self.clip);
        for y in vis.y..vis.bottom() {
            let dy = 16 * y - 8 * cy;
            for x in vis.x..vis.right() {
                let dx = 16 * x - 8 * cx;
                // Разрыв кольца сверху — клин примерно в 26°: `2|dx| < -dy`. Тригонометрии не
                // нужно, а знак читается ровно так же.
                if dy < 0 && 2 * dx.abs() < -dy {
                    continue;
                }
                let d = isqrt(dx * dx + dy * dy);
                // Полоса шириной в пиксель по обе стороны от окружности — это и есть сглаживание.
                let out = ((rad16 + t2 - d) * 16).clamp(0, 256) as u32;
                let inn = ((d - (rad16 - t2)) * 16).clamp(0, 256) as u32;
                let cov = out.min(inn);
                if cov > 0 {
                    self.blend(x, y, c, cov);
                }
            }
        }
        // Черта: от верхнего края кольца до его центра. Скруглённая — концы кольца тоже круглые.
        let top = r.y + (r.h - (2 * rad + thick)) / 2;
        let stem = Rect::new(r.x + (r.w - thick) / 2, top, thick, rad + thick / 2);
        self.rrect(stem, thick / 2, c);
    }

    /// Серая маска глифа (или иконки) цветом `c`. Нужна тексту: растеризатор отдаёт покрытие.
    pub fn mask(&mut self, x: i32, y: i32, w: i32, h: i32, mask: &[u8], c: Rgba) {
        // Веха 148.2 — клип проверяем ОДИН РАЗ на глиф, а не на каждый его пиксель. Так буква,
        // целиком лежащая вне клипа, не стоит ничего: на этом и держится перерисовка одной
        // колонки окна вместо всего кадра ([[store-viewer]]).
        let vis = Rect::new(x, y, w, h).intersect(self.clip);
        if vis.is_empty() {
            return;
        }
        for py in vis.y..vis.bottom() {
            let row = py - y;
            for px in vis.x..vis.right() {
                let v = mask[(row * w + (px - x)) as usize];
                if v != 0 {
                    self.blend(px, py, c, v as u32 + 1);
                }
            }
        }
    }
}

/// Знаковое расстояние до скруглённого прямоугольника в 1/256 пикселя (внутри — отрицательное).
///
/// Копия формулы композитора: одинаковое скругление у окна и у панели важнее, чем экономия
/// десяти строк на общем модуле, — общий модуль пришлось бы делать зависимостью `wm` от тулкита,
/// то есть класть в композитор чужой код (ровно то, чего [[layers]] избегает).
fn rrect_sd(px: i32, py: i32, r: Rect, rad: i32) -> i32 {
    let ax = (2 * px - (2 * r.x + r.w - 1)).abs();
    let ay = (2 * py - (2 * r.y + r.h - 1)).abs();
    let qx = ax - (r.w - 1 - 2 * rad);
    let qy = ay - (r.h - 1 - 2 * rad);
    let (mx, my) = (qx.max(0), qy.max(0));
    let len = isqrt(mx * mx + my * my);
    let inside = qx.max(qy).min(0);
    (len + inside - 2 * rad) * 128
}

/// Деление на 255 без деления (точное для `0..=255*255`). Стандартный приём: `v/255` это
/// `(v + v/256 + 1) / 256`, а деление на степень двойки — сдвиг.
#[inline]
fn div255(v: u32) -> u32 {
    let v = v + 128;
    (v + (v >> 8)) >> 8
}

/// Целочисленный квадратный корень (тот же алгоритм, что в композиторе): плавающей точки в
/// рисовании панели нет вовсе — она стоила бы дороже самого пикселя.
fn isqrt(v: i32) -> i32 {
    if v <= 0 {
        return 0;
    }
    let mut r = 0i32;
    let mut bit = 1i32 << 30;
    let mut rem = v;
    while bit > rem {
        bit >>= 2;
    }
    while bit != 0 {
        if rem >= r + bit {
            rem -= r + bit;
            r = (r >> 1) + bit;
        } else {
            r >>= 1;
        }
        bit >>= 2;
    }
    r
}
