//! `install` — установщик VOID на SATA-диск (Веха 48; окно и выбор диска — Веха 174).
//!
//! Сеется ТОЛЬКО на живом носителе: признак — загрузочный модуль GRUB с образом диска
//! ([`boot_module`]). На установленной системе команды «стереть диск» просто нет, и это решение
//! владельца, а не забывчивость: обновляют VOID с той же флешки, с которой ставили.
//!
//! ## Почему окно, и почему со списком
//!
//! Прежний установщик был одной строкой в консоли: «ставлю на диск» — на какой, он не говорил,
//! потому что и не знал: ядро брало первый порт AHCI, на котором нашёлся диск. На машине с одним
//! диском это работало, на машине с двумя — это лотерея, а ставка в ней вся память человека.
//!
//! Теперь ядро перечисляет ВСЕ диски ([`sys::disks`]), а окно показывает модель, размер и две
//! пометки, каждая из которых меняет решение: «здесь уже есть VOID» и «с этого диска работает
//! система». Второй установщик не предлагает вовсе — ядро на него и не поставит.
//!
//! ## Без экрана
//!
//! Композитора может не быть (текстовый сеанс, riscv), и тогда программа работает списком и
//! номером: `install` печатает диски, `install 1` ставит на диск номер 1. Это не запасной путь
//! «на всякий случай», а тот же самый выбор, записанный словами.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use void_user as sys;
use void_user::win::{sym, Event, Window};

#[path = "../ui/mod.rs"]
mod ui;

use ui::{Align, Rect, Ui};

#[global_allocator]
static ALLOC: sys::heap::Heap<{ 512 * 1024 }> = sys::heap::Heap::new();

/// Сколько дисков показываем. Столько же перечисляет ядро (`arch::MAX_DISKS`).
const MAX_DISKS: usize = 8;

/// Диск глазами программы: то же, что отдало ядро, но строками.
struct Disk {
    slot: usize,
    model: String,
    size: String,
    void: bool,
    live: bool,
}

/// Размер по-человечески: секторы по 512 Б → ГиБ, а мелочь — в МиБ.
fn human(sectors: u64) -> String {
    let mib = sectors / 2048;
    if mib >= 1024 {
        alloc::format!("{}.{} ГиБ", mib / 1024, (mib % 1024) * 10 / 1024)
    } else {
        alloc::format!("{} МиБ", mib)
    }
}

fn read_disks(store: usize) -> Vec<Disk> {
    let mut raw: Vec<sys::DiskInfo> = Vec::new();
    for _ in 0..MAX_DISKS {
        raw.push(sys::DiskInfo {
            slot: 0,
            sectors: 0,
            model: [0; 40],
            model_len: 0,
            void: false,
            live: false,
        });
    }
    let n = sys::disks(store, &mut raw);
    raw.into_iter()
        .take(n)
        .map(|d| Disk {
            slot: d.slot,
            model: core::str::from_utf8(&d.model[..d.model_len])
                .unwrap_or("диск")
                .trim()
                .into(),
            size: human(d.sectors),
            void: d.void,
            live: d.live,
        })
        .collect()
}

/// Что сейчас на экране.
enum Stage {
    /// Выбираем диск.
    Pick,
    /// Кадр «ставлю» уже нарисован — установка идёт следующим шагом ([`ui::Client::after`]).
    Working,
    /// Кончилось: текст итога и удалось ли.
    Done(String, bool),
}

struct App {
    w: i32,
    h: i32,
    disks: Vec<Disk>,
    sel: usize,
    stage: Stage,
    store: usize,
}

impl App {
    /// Можно ли ставить на выбранный диск.
    fn target_ok(&self) -> bool {
        self.disks.get(self.sel).is_some_and(|d| !d.live)
    }

    fn move_sel(&mut self, by: i32) -> bool {
        if self.disks.is_empty() {
            return false;
        }
        let n = self.disks.len() as i32;
        let next = (self.sel as i32 + by).clamp(0, n - 1) as usize;
        let moved = next != self.sel;
        self.sel = next;
        moved
    }
}

impl ui::Client for App {
    fn event(&mut self, e: Event, _input: &ui::Input) -> ui::Scope {
        if !matches!(self.stage, Stage::Pick) {
            return ui::Scope::No;
        }
        let moved = match e {
            Event::Key { sym: code, down, .. } if down => match code {
                sym::DOWN => self.move_sel(1),
                sym::UP => self.move_sel(-1),
                _ => false,
            },
            Event::Resize { w, h } => {
                self.w = w as i32;
                self.h = h as i32;
                true
            }
            // Клики разбираются при рисовании: тулкит immediate-mode, и попадание в строку
            // считает тот же код, что её рисует (иначе они разойдутся — Веха 123).
            Event::Button { down, .. } if down => true,
            _ => false,
        };
        if moved {
            ui::Scope::All
        } else {
            ui::Scope::No
        }
    }

    fn draw(&mut self, u: &mut Ui) -> ui::Scope {
        let bg = u.th.bg;
        u.background(bg);
        let pad = u.th.pad * 2;
        let line = u.font.line_h();
        let mut r = Rect::new(0, 0, self.w, self.h).inset(pad);

        let title = r.cut_top(line * 2);
        let (tx, mu) = (u.th.text, u.th.muted);
        u.label(title, "Куда поставить систему", tx, Align::Center);

        match &self.stage {
            Stage::Working => {
                u.label(r, "ставлю…", mu, Align::Center);
                return ui::Scope::No;
            }
            Stage::Done(text, ok) => {
                let col = if *ok { u.th.text } else { u.th.danger };
                let mut body = r;
                let head = body.cut_top(line * 2);
                u.label(head, if *ok { "Готово" } else { "Не вышло" }, col, Align::Center);
                wrap(u, body.x, body.y, body.w, text, mu);
                return ui::Scope::No;
            }
            Stage::Pick => {}
        }

        if self.disks.is_empty() {
            wrap(u, r.x, r.y, r.w, "SATA-дисков не найдено. Ставить некуда.", mu);
            return ui::Scope::No;
        }

        // ── список дисков ──────────────────────────────────────────────────
        let row_h = line * 3;
        let mut picked = None;
        for i in 0..self.disks.len() {
            if r.h < row_h {
                break;
            }
            let row = r.cut_top(row_h);
            let (name, sub, sel) = {
                let d = &self.disks[i];
                let mark = if d.live {
                    " · с него работает система"
                } else if d.void {
                    " · здесь уже есть VOID"
                } else {
                    ""
                };
                (
                    alloc::format!("{}  ·  {}", d.model, d.size),
                    alloc::format!("диск {}{}", d.slot, mark),
                    if i == self.sel { 256 } else { 0 },
                )
            };
            let hot = if u.hot(row) { 256 } else { 0 };
            if u.entry(row, &name, &sub, "", sel, hot) {
                picked = Some(i);
            }
            r.cut_top(u.th.gap);
        }
        if let Some(i) = picked {
            self.sel = i;
        }

        // ── предупреждение и кнопка ────────────────────────────────────────
        let btn = r.cut_bottom(line * 2);
        let warn = r.cut_bottom(line * 3);
        if self.target_ok() {
            wrap(
                u,
                warn.x,
                warn.y,
                warn.w,
                "Диск будет стёрт целиком: таблица разделов, все разделы, все файлы. \
                 Отменить это нельзя.",
                u.th.danger,
            );
            let hot = if u.hot(btn) { 256 } else { 0 };
            if u.danger(btn, "Стереть и поставить", hot) {
                self.stage = Stage::Working;
                return ui::Scope::All; // кадр «ставлю…» — ДО самой установки
            }
        } else {
            wrap(
                u,
                warn.x,
                warn.y,
                warn.w,
                "На этот диск поставить нельзя: с него работает система прямо сейчас.",
                mu,
            );
        }
        ui::Scope::No
    }

    /// После кадра — сама установка. Отдельно от рисования намеренно: она занимает секунды, и
    /// делать её внутри кадра значило бы показать человеку замерший экран без единого слова.
    fn after(&mut self, _surf: &Window) -> ui::Scope {
        if !matches!(self.stage, Stage::Working) {
            return ui::Scope::No;
        }
        let slot = self.disks.get(self.sel).map(|d| d.slot).unwrap_or(usize::MAX);
        self.stage = match sys::install(self.store, slot) {
            Some(_) => Stage::Done(
                String::from(
                    "VOID установлен. Выключи машину, вынь носитель и включи снова — \
                     система поднимется с диска. Первая загрузка сама посеет конфиг.",
                ),
                true,
            ),
            None => Stage::Done(
                String::from(
                    "Установка не состоялась. Причину ядро сказало в журнал: `klog`. \
                     Чаще всего это отсутствие образа установки — то есть загрузка не с носителя.",
                ),
                false,
            ),
        };
        ui::Scope::All
    }
}

/// Перенос абзаца по словам. Возвращает `y` следующей строки.
fn wrap(u: &mut Ui, x: i32, y: i32, w: i32, s: &str, col: ui::Rgba) -> i32 {
    let line = u.font.line_h();
    let mut y = y;
    let mut cur = String::new();
    for word in s.split_whitespace() {
        let probe =
            if cur.is_empty() { String::from(word) } else { alloc::format!("{cur} {word}") };
        if u.text_w(&probe) > w && !cur.is_empty() {
            u.label(Rect::new(x, y, w, line), &cur, col, Align::Left);
            y += line;
            cur = String::from(word);
        } else {
            cur = probe;
        }
    }
    if !cur.is_empty() {
        u.label(Rect::new(x, y, w, line), &cur, col, Align::Left);
        y += line;
    }
    y
}

/// Без композитора: список словами и установка по номеру.
fn text_mode(store: usize, disks: &[Disk], arg: Option<usize>) -> ! {
    match arg {
        None => {
            sys::write("install: куда ставить? Диски машины:\n".as_bytes());
            for d in disks {
                let mark = if d.live {
                    " — с него работает система, ставить нельзя"
                } else if d.void {
                    " — здесь уже есть VOID"
                } else {
                    ""
                };
                sys::write(
                    alloc::format!("  {}  {}  {}{}\n", d.slot, d.model, d.size, mark).as_bytes(),
                );
            }
            if disks.is_empty() {
                sys::write("  (ни одного)\n".as_bytes());
            }
            sys::write("\nПоставить: install <номер>. ДИСК БУДЕТ СТЁРТ ЦЕЛИКОМ.\n".as_bytes());
            sys::exit(0)
        }
        Some(slot) => {
            sys::write(
                alloc::format!("install: ставлю на диск {} — он будет стёрт целиком…\n", slot)
                    .as_bytes(),
            );
            match sys::install(store, slot) {
                Some(_) => {
                    sys::write(
                        "Готово. Выключи машину, вынь носитель и включи снова.\n".as_bytes(),
                    );
                    sys::exit(0)
                }
                None => {
                    sys::write("Не вышло — причина в журнале ядра (`klog`).\n".as_bytes());
                    sys::exit(1)
                }
            }
        }
    }
}

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let (_generation, th, mut font) = ui::app::boot();
    // Право на store с WRITE — то же, что у шелла: установка меняет содержимое store целиком, и
    // по силе это запись. Наследуется от того, кто нас запустил (`cap::endow`).
    let store = ui::conf::store_cap().unwrap_or_else(|| sys::start_cap(1));
    let disks = read_disks(store);

    let argv = sys::argv::Argv::take();
    let arg = argv.str(0).and_then(|s| s.parse::<usize>().ok());

    let (w, h) = (720u16, 520u16);
    let Some(mut surf) = Window::create(w, h, "Установка VOID") else {
        text_mode(store, &disks, arg);
    };
    // Номер, названный словами, слушаемся и в окне: человек уже сказал, куда ставить.
    let sel = arg.and_then(|s| disks.iter().position(|d| d.slot == s)).unwrap_or(0);
    let mut app = App { w: w as i32, h: h as i32, disks, sel, stage: Stage::Pick, store };
    ui::app::run(&mut surf, &th, &mut font, &mut app);
    sys::exit(0);
}
