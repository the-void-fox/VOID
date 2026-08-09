//! `wm` — композитор окон VOID (Веха 117, [[0007-graphics-native-compositor]]).
//!
//! ```text
//!   клиент ─рисует в свою память─→ obj_put ─content-id─→ ATTACH+COMMIT ─→ композитор
//!   мышь/клавиши ─→ композитор ─→ событие ОКНУ под курсором (отложенный ответ)
//!   композитор ─→ damage-прямоугольники ─→ фреймбуфер
//! ```
//!
//! ## Что здесь принципиального
//!
//! - **Окно — это буфер-ОБЪЕКТ плюс право звать нас**, а не участок чужой памяти
//!   ([[0018-gpu-ladder]]). Клиент называет content-id; мы читаем объект ОДИН РАЗ и держим у
//!   себя копию. Пока содержимое не менялось, повторный `commit` не стоит ничего: content-id
//!   тот же — перечитывать нечего.
//! - **Двигает окна композитор.** Перемещение не трогает клиента вообще: его пиксели уже у нас.
//!   Это и есть правило ADR 0016 «анимируем трансформации, а не содержимое» — здесь оно не
//!   пожелание, а следствие устройства.
//! - **Рисуем по damage.** Кадр никогда не перерисовывается целиком: закрашиваются только
//!   прямоугольники, которые изменились (старое и новое место окна, содержимое, курсор).
//! - **Рамки рисуем МЫ** (server-side decorations). Так у всех окон один вид без единой строчки
//!   в приложениях — ровно то, ради чего затевался общий тулкит (ADR 0016).
//!
//! ## Чего здесь пока нет
//!
//! Анимаций, буфера обмена, drag&drop и плитки запуска: место под них в протоколе есть
//! (ADR 0016), но веха про другое — про то, чтобы окна вообще появились и жили.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use void_user as sys;
use void_user::win;

// Запасной шрифт 8×16 — заголовки окон. Настоящий (из пакета) придёт вместе с тулкитом.
#[path = "../bitfont.rs"]
mod bitfont;
use bitfont::BitmapFont;
use ereb_render::{RasterizedGlyph, Rasterizer, RenderStyle};

/// Кадр 1280×800 RGBA (4 МиБ) + копии содержимого окон.
#[global_allocator]
static ALLOC: sys::heap::Heap<{ 32 * 1024 * 1024 }> = sys::heap::Heap::new();

/// Окно фреймбуфера в нашем адресном пространстве (как у `term`).
const FB_VA: usize = 0x5000_0000;

// ── вид (ADR 0016: один палитра-источник, приложения цветов не знают) ────────
const C_DESKTOP: (u8, u8, u8) = (0x0d, 0x11, 0x17);
const C_FRAME: (u8, u8, u8) = (0x16, 0x1b, 0x22);
const C_FRAME_ACTIVE: (u8, u8, u8) = (0x24, 0x2c, 0x38);
const C_BORDER: (u8, u8, u8) = (0x30, 0x36, 0x3d);
const C_ACCENT: (u8, u8, u8) = (0x4c, 0x7d, 0xfd);
const C_TEXT: (u8, u8, u8) = (0xc9, 0xd1, 0xd9);

/// Высота титульной полосы и толщина рамки.
const TITLE_H: i32 = 22;
const BORDER: i32 = 1;

fn main_loop() -> ! {
    let Some(fb_cap) = find_fb_cap() else {
        sys::write_console("[wm] нет права на экран (mmio:fb в конфиге)\n".as_bytes());
        sys::exit(1);
    };
    let Some(info) = sys::video_info(fb_cap) else {
        sys::write_console("[wm] ядро не отдало описание видеорежима\n".as_bytes());
        sys::exit(1);
    };
    if !sys::mmio_map(fb_cap, FB_VA) {
        sys::write_console("[wm] не удалось замапить фреймбуфер\n".as_bytes());
        sys::exit(1);
    }
    let me = sys::self_endpoint();
    let store = store_cap();
    let mut font = BitmapFont::new(16);

    let mut wm = Wm {
        info,
        wins: Vec::new(),
        next_id: 1,
        cursor: (info.width as i32 / 2, info.height as i32 / 2),
        buttons: 0,
        drag: None,
        focus: None,
        transient: 0,
    };

    // Рабочий стол целиком — единственная полная заливка за всю сессию.
    wm.fill_rect(0, 0, info.width as i32, info.height as i32, C_DESKTOP);
    wm.draw_cursor();

    // Клиенты — из argv. Право на себя отдаём под именем `WM`: терминал даёт детям `STDIO`,
    // мы даём окна, и путать эти два хоста нельзя.
    let mut abuf = [0u8; 256];
    let n = sys::args(&mut abuf);
    let mut spawned = 0usize;
    for prog in abuf[..n].split(|&b| b == 0).filter(|s| !s.is_empty()).skip(1) {
        match sys::spawn_with_endpoint(store, prog, &[], me, b"WM\0") {
            Some(pid) => {
                sys::write_console("[wm] запущен клиент ".as_bytes());
                sys::write_console(prog);
                sys::write_console(b"\n");
                let _ = pid;
                spawned += 1;
            }
            None => {
                sys::write_console("[wm] не запустился клиент ".as_bytes());
                sys::write_console(prog);
                sys::write_console(b"\n");
            }
        }
    }
    if spawned == 0 {
        sys::write_console("[wm] клиентов нет — пустой рабочий стол\n".as_bytes());
    }

    let mut msg = [0u8; 1024];
    let mut mouse = [sys::MouseEvent { dx: 0, dy: 0, buttons: 0 }; 32];
    loop {
        let mut worked = false;

        // ── мышь ───────────────────────────────────────────────────────────────────────
        let mn = sys::mouse_read(&mut mouse);
        if mn > 0 {
            worked = true;
            for e in &mouse[..mn] {
                wm.on_mouse(e, &mut font);
            }
        }

        // ── клавиши: целиком уходят окну в фокусе ──────────────────────────────────────
        let mut keys = [0u8; 32];
        let kn = sys::read_console_nonblock(&mut keys);
        if kn > 0 {
            worked = true;
            for &k in &keys[..kn] {
                wm.key(k);
            }
        }

        // ── запросы клиентов ───────────────────────────────────────────────────────────
        // Спим только когда делать нечего — и просыпаемся по клавише ИЛИ движению мыши
        // (Веха 115 научила ядро будить на мышь тех же, кого будит клавиша).
        let got = if worked {
            sys::try_recv(&mut msg)
        } else {
            sys::recv_console(&mut msg, 200)
        };
        if let Some(m) = got {
            wm.request(&m, &msg, store, &mut font);
        }
    }
}

/// Одно окно.
struct Win {
    id: u32,
    owner: usize,
    /// Левый верхний угол РАМКИ на экране.
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    title: String,
    /// Копия пикселей клиента (RGBA), прочитанная по content-id.
    pixels: Vec<u8>,
    /// Что уже прочитано — чтобы не читать то же самое дважды.
    cid: [u8; 32],
    /// Отложенный ответ на `OP_EVENT` (клиент спит в `SYS_CALL`).
    waiting: Option<usize>,
    /// События, накопленные до того, как клиент спросил.
    inbox: Vec<[u8; 8]>,
}

impl Win {
    /// Прямоугольник рамки (вместе с титульной полосой).
    fn frame(&self) -> (i32, i32, i32, i32) {
        (self.x, self.y, self.w + 2 * BORDER, self.h + TITLE_H + 2 * BORDER)
    }
    /// Левый верхний угол СОДЕРЖИМОГО.
    fn content_at(&self) -> (i32, i32) {
        (self.x + BORDER, self.y + BORDER + TITLE_H)
    }
    fn hit_title(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x + self.w + 2 * BORDER
            && py >= self.y && py < self.y + TITLE_H + BORDER
    }
    fn hit_frame(&self, px: i32, py: i32) -> bool {
        let (fx, fy, fw, fh) = self.frame();
        px >= fx && px < fx + fw && py >= fy && py < fy + fh
    }
}

struct Wm {
    info: sys::VideoInfo,
    /// Порядок = z-order: последнее окно рисуется поверх и получает клики первым.
    wins: Vec<Win>,
    next_id: u32,
    cursor: (i32, i32),
    buttons: u8,
    /// Тащим окно: (индекс, смещение курсора от угла рамки).
    drag: Option<(usize, i32, i32)>,
    focus: Option<u32>,
    /// Сколько байт временных буферов прочитано с прошлой уборки (см. `OP_ATTACH`).
    transient: usize,
}

impl Wm {
    // ── пиксели ────────────────────────────────────────────────────────────────────────
    //
    // Пишем ПРЯМО во фреймбуфер (он write-combining с Вехи 116.1 — 1,3+ ГБ/с на железе), без
    // теневого кадра: лишняя копия экрана стоила бы 4 МиБ памяти и второго прохода по ним.

    #[inline]
    fn pack(&self, c: (u8, u8, u8)) -> u32 {
        let mut out = 0u32;
        for (i, chan) in [c.0, c.1, c.2].iter().enumerate() {
            let (pos, size) = self.info.rgb[i];
            let size = size.clamp(1, 8);
            out |= ((*chan as u32) >> (8 - size)) << pos;
        }
        out
    }

    #[inline]
    fn put(&self, x: i32, y: i32, px: u32) {
        if x < 0 || y < 0 || x >= self.info.width as i32 || y >= self.info.height as i32 {
            return;
        }
        let bpp = self.info.bpp / 8;
        let dst = FB_VA + y as usize * self.info.pitch + x as usize * bpp;
        unsafe {
            match bpp {
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

    fn fill_rect(&self, x: i32, y: i32, w: i32, h: i32, c: (u8, u8, u8)) {
        let px = self.pack(c);
        for yy in y.max(0)..(y + h).min(self.info.height as i32) {
            for xx in x.max(0)..(x + w).min(self.info.width as i32) {
                self.put(xx, yy, px);
            }
        }
        fence();
    }

    /// Нарисовать строку битмапным шрифтом. Возвращает ширину нарисованного.
    fn text(&self, font: &mut BitmapFont, x: i32, y: i32, s: &str, c: (u8, u8, u8)) -> i32 {
        let px = self.pack(c);
        let mut pen = x;
        for ch in s.chars() {
            let g: RasterizedGlyph = font.rasterize(ch, RenderStyle::Regular);
            for gy in 0..g.height as i32 {
                for gx in 0..g.width as i32 {
                    if g.bitmap[(gy * g.width as i32 + gx) as usize] > 127 {
                        self.put(pen + gx, y + gy, px);
                    }
                }
            }
            pen += font.metrics().width as i32;
        }
        fence();
        pen - x
    }

    // ── композиция ─────────────────────────────────────────────────────────────────────

    /// Перерисовать прямоугольник экрана: стол, затем окна в порядке z, затем курсор.
    ///
    /// Это единственный путь, которым что-либо появляется на экране, — и именно поэтому
    /// перемещение окна стоит два таких вызова (старое место и новое), а не кадр.
    fn repaint(&self, font: &mut BitmapFont, x: i32, y: i32, w: i32, h: i32) {
        self.fill_rect(x, y, w, h, C_DESKTOP);
        for (i, win) in self.wins.iter().enumerate() {
            let (fx, fy, fw, fh) = win.frame();
            if fx + fw <= x || fy + fh <= y || fx >= x + w || fy >= y + h {
                continue; // не пересекается
            }
            self.draw_window(font, i, x, y, w, h);
        }
        self.draw_cursor();
    }

    /// Нарисовать окно, ограничив вывод прямоугольником `clip`.
    fn draw_window(&self, font: &mut BitmapFont, idx: usize, cx: i32, cy: i32, cw: i32, ch: i32) {
        let win = &self.wins[idx];
        let active = self.focus == Some(win.id);
        let (fx, fy, fw, fh) = win.frame();
        let inside = |x: i32, y: i32| x >= cx && x < cx + cw && y >= cy && y < cy + ch;

        // Рамка и титульная полоса.
        let border = self.pack(if active { C_ACCENT } else { C_BORDER });
        let title_bg = self.pack(if active { C_FRAME_ACTIVE } else { C_FRAME });
        for yy in fy..fy + fh {
            for xx in fx..fx + fw {
                if !inside(xx, yy) {
                    continue;
                }
                let on_border = xx == fx || xx == fx + fw - 1 || yy == fy || yy == fy + fh - 1;
                if on_border {
                    self.put(xx, yy, border);
                } else if yy < fy + BORDER + TITLE_H {
                    self.put(xx, yy, title_bg);
                }
            }
        }

        // Содержимое: копия пикселей клиента (RGBA по строкам).
        let (ox, oy) = win.content_at();
        if !win.pixels.is_empty() {
            for row in 0..win.h {
                let yy = oy + row;
                if yy < cy || yy >= cy + ch {
                    continue;
                }
                for col in 0..win.w {
                    let xx = ox + col;
                    if xx < cx || xx >= cx + cw {
                        continue;
                    }
                    let p = ((row * win.w + col) * 4) as usize;
                    if p + 2 < win.pixels.len() {
                        let px = self.pack((win.pixels[p], win.pixels[p + 1], win.pixels[p + 2]));
                        self.put(xx, yy, px);
                    }
                }
            }
        }
        fence();

        // Заголовок поверх полосы — рисуем после неё и только если он попал в clip.
        if fy + 3 >= cy - 16 && fy < cy + ch {
            let c = if active { C_TEXT } else { C_BORDER };
            self.text(font, fx + 6, fy + 3, &win.title, c);
        }
    }

    // ── курсор (тот же приём, что в `term`: он не часть кадра) ────────────────────────

    fn draw_cursor(&self) {
        let px_fill = self.pack(if self.buttons != 0 { C_ACCENT } else { (255, 255, 255) });
        let px_edge = self.pack((0, 0, 0));
        for (row, line) in CURSOR.iter().enumerate() {
            for (col, ch) in line.bytes().enumerate() {
                if ch == b' ' {
                    continue;
                }
                let px = if ch == b'#' { px_edge } else { px_fill };
                self.put(self.cursor.0 + col as i32, self.cursor.1 + row as i32, px);
            }
        }
        fence();
    }

    // ── ввод ───────────────────────────────────────────────────────────────────────────

    fn on_mouse(&mut self, e: &sys::MouseEvent, font: &mut BitmapFont) {
        let old = self.cursor;
        self.cursor.0 = (self.cursor.0 + e.dx as i32).clamp(0, self.info.width as i32 - 1);
        self.cursor.1 = (self.cursor.1 + e.dy as i32).clamp(0, self.info.height as i32 - 1);
        let was = self.buttons;
        self.buttons = e.buttons;

        // Нажатие: поднять окно, начать перетаскивание за титульную полосу.
        if was == 0 && e.buttons != 0 {
            let hit = self.wins.iter().rposition(|w| w.hit_frame(self.cursor.0, self.cursor.1));
            let lost = self.focus;
            match hit {
                Some(i) => {
                    // Фокус меняем ДО перерисовки: рамка и заголовок рисуются по нему, и
                    // порядок наоборот давал подсветку, отставшую на одно нажатие.
                    let id = self.wins[i].id;
                    self.focus = Some(id);
                    self.raise(i, font);
                    let i = self.wins.len() - 1; // после подъёма окно последнее
                    if self.wins[i].hit_title(self.cursor.0, self.cursor.1) {
                        self.drag =
                            Some((i, self.cursor.0 - self.wins[i].x, self.cursor.1 - self.wins[i].y));
                    }
                    self.unfocus_repaint(font, lost, Some(id));
                }
                None => {
                    self.focus = None;
                    self.unfocus_repaint(font, lost, None);
                }
            }
        }
        if e.buttons == 0 {
            self.drag = None;
        }

        // Перетаскивание — два прямоугольника: старое место и новое. Клиент об этом не знает
        // вовсе: его пиксели уже у нас, перерисовывать ему нечего.
        if let Some((i, gx, gy)) = self.drag {
            let (ox, oy, ow, oh) = self.wins[i].frame();
            self.wins[i].x = self.cursor.0 - gx;
            self.wins[i].y = self.cursor.1 - gy;
            self.repaint(font, ox, oy, ow, oh);
            let (nx, ny, nw, nh) = self.wins[i].frame();
            self.repaint(font, nx, ny, nw, nh);
        } else {
            // Просто движение — стереть курсор со старого места и нарисовать на новом.
            self.repaint(font, old.0, old.1, CUR_W, CUR_H);
            self.draw_cursor();
        }

        // Событие окну под курсором.
        if let Some(i) = self.wins.iter().rposition(|w| w.hit_frame(self.cursor.0, self.cursor.1)) {
            let (ox, oy) = self.wins[i].content_at();
            let (lx, ly) = ((self.cursor.0 - ox) as u16, (self.cursor.1 - oy) as u16);
            if was != e.buttons {
                let ev = [win::EV_BUTTON, lx as u8, (lx >> 8) as u8, ly as u8, (ly >> 8) as u8,
                          e.buttons, (e.buttons != 0) as u8, 0];
                self.send(i, ev, 7);
            } else {
                let ev = [win::EV_MOTION, lx as u8, (lx >> 8) as u8, ly as u8, (ly >> 8) as u8,
                          0, 0, 0];
                self.send(i, ev, 5);
            }
        }
    }

    fn key(&mut self, k: u8) {
        let Some(id) = self.focus else { return };
        let Some(i) = self.wins.iter().position(|w| w.id == id) else { return };
        self.send(i, [win::EV_KEY, k, 0, 0, 0, 0, 0, 0], 2);
    }

    /// Перерисовать окно, потерявшее фокус (его рамка обязана погаснуть).
    fn unfocus_repaint(&mut self, font: &mut BitmapFont, lost: Option<u32>, now: Option<u32>) {
        let Some(id) = lost else { return };
        if Some(id) == now {
            return;
        }
        if let Some(i) = self.wins.iter().position(|w| w.id == id) {
            let r = self.wins[i].frame();
            self.repaint(font, r.0, r.1, r.2, r.3);
        }
    }

    /// Поднять окно на верх стопки.
    fn raise(&mut self, i: usize, font: &mut BitmapFont) {
        let rect = self.wins[i].frame();
        if i + 1 != self.wins.len() {
            let w = self.wins.remove(i);
            self.wins.push(w);
        }
        // Перерисовываем всегда: даже если окно уже наверху, у него мог смениться фокус.
        self.repaint(font, rect.0, rect.1, rect.2, rect.3);
    }

    /// Отдать событие клиенту: сразу, если он ждёт, иначе в очередь.
    fn send(&mut self, i: usize, ev: [u8; 8], len: usize) {
        match self.wins[i].waiting.take() {
            Some(cap) => {
                sys::reply(cap, &ev[..len]);
            }
            None => {
                // Потолок разный по СМЫСЛУ события: движения мыши устаревают мгновенно (копить
                // их сотнями значит показывать клиенту прошлое), а клавиши терять нельзя —
                // человек их уже нажал. Строка, вставленная в консоль целиком, приезжает
                // десятками байт разом, и потолок в 16 съедал её середину.
                let cap = if ev[0] == win::EV_KEY { 256 } else { 16 };
                if self.wins[i].inbox.len() < cap {
                    let mut rec = [0u8; 8];
                    rec[..len].copy_from_slice(&ev[..len]);
                    rec[7] = len as u8;
                    self.wins[i].inbox.push(rec);
                }
            }
        }
    }

    // ── запросы клиентов ───────────────────────────────────────────────────────────────

    fn request(&mut self, m: &sys::Message, buf: &[u8], store: usize, font: &mut BitmapFont) {
        let op = m.op & 0xff;
        let len = m.len.min(buf.len());
        match op {
            win::OP_CREATE => {
                let w = u16::from_le_bytes([buf[0], buf[1]]).clamp(32, 1600) as i32;
                let h = u16::from_le_bytes([buf[2], buf[3]]).clamp(32, 1200) as i32;
                let title = core::str::from_utf8(&buf[4..len]).unwrap_or("окно");
                let id = self.next_id;
                self.next_id += 1;
                // Каскадом: каждое следующее окно правее и ниже — иначе они лягут друг на друга
                // и человек решит, что открылось одно.
                let n = self.wins.len() as i32;
                let win = Win {
                    id,
                    owner: m.sender,
                    x: 60 + n * 40,
                    y: 60 + n * 40,
                    w,
                    h,
                    title: String::from(title),
                    pixels: Vec::new(),
                    cid: [0u8; 32],
                    waiting: None,
                    inbox: Vec::new(),
                };
                let rect = win.frame();
                self.wins.push(win);
                self.focus = Some(id);
                sys::reply(m.reply_cap, &id.to_le_bytes());
                self.repaint(font, rect.0, rect.1, rect.2, rect.3);
            }
            win::OP_ATTACH => {
                let id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                let rx = u16::from_le_bytes([buf[4], buf[5]]) as i32;
                let ry = u16::from_le_bytes([buf[6], buf[7]]) as i32;
                let rw = u16::from_le_bytes([buf[8], buf[9]]) as i32;
                let rh = u16::from_le_bytes([buf[10], buf[11]]) as i32;
                let mut cid = [0u8; 32];
                cid.copy_from_slice(&buf[12..44]);
                if let Some(i) = self.wins.iter().position(|w| w.id == id) {
                    // Тот же content-id — содержимое то же, читать нечего. Это и есть выгода
                    // адресации по содержимому: «перерисовал в то же самое» стоит ноль.
                    if self.wins[i].cid != cid {
                        self.wins[i].cid = cid;
                        let (ww, wh) = (self.wins[i].w, self.wins[i].h);
                        if self.wins[i].pixels.len() != (ww * wh * 4) as usize {
                            self.wins[i].pixels = vec![0u8; (ww * wh * 4) as usize];
                        }
                        let need = (rw * rh * 4) as usize;
                        let mut px = vec![0u8; need];
                        let got = sys::obj_get(store, &cid, &mut px);
                        if got == need {
                            // Вклеиваем полосу на её место в копии окна.
                            for row in 0..rh {
                                let dy = ry + row;
                                if dy < 0 || dy >= wh {
                                    continue;
                                }
                                let src = (row * rw * 4) as usize;
                                let dst = ((dy * ww + rx) * 4) as usize;
                                let n = (rw * 4).min((ww - rx) * 4).max(0) as usize;
                                if dst + n <= self.wins[i].pixels.len() && src + n <= px.len() {
                                    self.wins[i].pixels[dst..dst + n]
                                        .copy_from_slice(&px[src..src + n]);
                                }
                            }
                        }
                        // Пиксели скопированы — объект больше не нужен НИКОМУ. Он не привязан
                        // корнем, значит уже мусор; считаем его и время от времени просим ядро
                        // прибраться, иначе куча ядра кончится (Веха 118: полный кадр каждое
                        // нажатие клавиши убивал систему за восемь букв).
                        self.transient += need;
                        if self.transient > 24 * 1024 * 1024 {
                            self.transient = 0;
                            sys::obj_gc(store);
                        }
                    }
                }
                sys::reply(m.reply_cap, &[]);
            }
            win::OP_COMMIT => {
                let id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                sys::reply(m.reply_cap, &[]);
                if let Some(i) = self.wins.iter().position(|w| w.id == id) {
                    let (ox, oy) = self.wins[i].content_at();
                    let dx = u16::from_le_bytes([buf[4], buf[5]]) as i32;
                    let dy = u16::from_le_bytes([buf[6], buf[7]]) as i32;
                    let dw = u16::from_le_bytes([buf[8], buf[9]]) as i32;
                    let dh = u16::from_le_bytes([buf[10], buf[11]]) as i32;
                    self.repaint(font, ox + dx, oy + dy, dw.max(1), dh.max(1));
                }
            }
            // Неблокирующий опрос: у клиента свой реактор, спать в нашем вызове он не может.
            win::OP_POLL => {
                let id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                let ev = self.wins.iter_mut().find(|w| w.id == id).and_then(|w| {
                    (!w.inbox.is_empty()).then(|| w.inbox.remove(0))
                });
                match ev {
                    Some(rec) => {
                        let n = (rec[7] as usize).min(7);
                        sys::reply(m.reply_cap, &rec[..n]);
                    }
                    None => {
                        sys::reply(m.reply_cap, &[]);
                    }
                }
            }
            win::OP_EVENT => {
                let id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                let Some(i) = self.wins.iter().position(|w| w.id == id) else {
                    sys::reply(m.reply_cap, &[]);
                    return;
                };
                // ОЧЕРЕДЬ, а не стек: `pop` брал с конца, и набранное приезжало задом наперёд
                // (а при переполнении — вперемешку). Стоимость `remove(0)` при потолке в
                // сотни записей несущественна, а порядок ввода — свойство, которое нельзя терять.
                let first = if self.wins[i].inbox.is_empty() {
                    None
                } else {
                    Some(self.wins[i].inbox.remove(0))
                };
                match first {
                    // Есть накопленное — отвечаем сразу.
                    Some(rec) => {
                        let n = rec[7] as usize;
                        sys::reply(m.reply_cap, &rec[..n.min(7)]);
                    }
                    // Пусто — ответ ОТКЛАДЫВАЕТСЯ: клиент спит в `SYS_CALL`, как в чтении stdin.
                    None => self.wins[i].waiting = Some(m.reply_cap),
                }
            }
            win::OP_DESTROY => {
                let id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                sys::reply(m.reply_cap, &[]);
                if let Some(i) = self.wins.iter().position(|w| w.id == id) {
                    let rect = self.wins[i].frame();
                    self.wins.remove(i);
                    self.repaint(font, rect.0, rect.1, rect.2, rect.3);
                }
            }
            // Чужой запрос — ответить пусто, а не молчать: молчание повесило бы вызвавшего.
            _ => {
                let _ = m.sender;
                sys::reply(m.reply_cap, &[]);
            }
        }
    }
}

/// Курсор — тот же, что в `term` (Веха 115): контур плюс тело.
const CUR_W: i32 = 12;
const CUR_H: i32 = 19;
const CURSOR: [&str; 19] = [
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

/// Барьер записи: пачки write-combining не должны залёживаться (Веха 116.1).
#[cfg(target_arch = "x86_64")]
fn fence() {
    unsafe { core::arch::asm!("sfence", options(nostack, preserves_flags)) };
}
#[cfg(not(target_arch = "x86_64"))]
fn fence() {}


#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    main_loop()
}

/// Право на экран: по имени, иначе перебором стартовых (как в `term`).
fn find_fb_cap() -> Option<usize> {
    sys::cap_named("FB")
        .filter(|&c| sys::video_info(c).is_some())
        .or_else(|| {
            (0..8)
                .map(sys::start_cap)
                .find(|&c| c != sys::NO_CAP && sys::video_info(c).is_some())
        })
}

/// Право на store (нужно и для чтения буферов, и для запуска клиентов).
fn store_cap() -> usize {
    sys::cap_named("STORE").unwrap_or_else(|| sys::start_cap(1))
}
