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
    /// МАСТЕР: человек отвечает на вопросы. Какой именно — [`App::step`].
    Wizard,
    /// Кадр «работаю» уже нарисован — дело идёт следующим шагом ([`ui::Client::after`]).
    /// `true` — ОБНОВЛЕНИЕ (данные остаются), `false` — установка со стиранием.
    Working(bool),
    /// Кончилось: текст итога и удалось ли.
    Done(String, bool),
}

/// Шаг мастера. Карта внизу экрана — это они же, по порядку.
///
/// Перечислением, а не цепочкой условий: шаг «шифрование диска» встанет сюда ОДНОЙ строкой, и ни
/// карта, ни кнопки, ни раскладка об этом не узнают. Макет владельца рисует карту с самого
/// начала именно затем, чтобы человек видел, сколько ещё осталось, — а значит число шагов обязано
/// быть данными, а не формой кода.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Логотип и язык — первое, что видно. Язык здесь же, а не отдельным экраном: пустой экран
    /// с одним вопросом выглядит как ошибка, а знак системы без вопроса — как заставка.
    Lang,
    /// Куда ставим.
    Disk,
    /// Чем система встречает и как её зовут.
    System,
    /// Сводка выбранного и само действие.
    Install,
}

const STEPS: [Step; 4] = [Step::Lang, Step::Disk, Step::System, Step::Install];

impl Step {
    /// Подпись в карте шагов.
    fn title(self) -> &'static str {
        match self {
            Step::Lang => ui::t("Язык"),
            Step::Disk => ui::t("Диск"),
            Step::System => ui::t("Система"),
            Step::Install => ui::t("Установка"),
        }
    }
}

/// Язык интерфейса: что показываем и что пишем в конфиг.
const LANGS: [(&str, &str); 2] = [("Русский", "ru"), ("English", "en")];

/// Чем система встречает. Третье поле — объяснение: выбор между «окнами» и «терминалом» человек
/// делает один раз и навсегда, и угадывать по одному слову его заставлять нельзя.
const MODES: [(&str, &str, &str); 3] = [
    ("Окна", "wm", "Композитор, окна и мышь. Обычный рабочий стол."),
    ("Терминал", "term", "Терминал во весь экран, без окон. Нужен пиксельный экран."),
    ("Шелл", "vsh", "Текстовый шелл в консоли ядра. Работает где угодно."),
];

struct App {
    w: i32,
    h: i32,
    disks: Vec<Disk>,
    sel: usize,
    stage: Stage,
    store: usize,
    /// Номер шага в [`STEPS`].
    step: usize,
    lang: usize,
    /// Открыт ли список языков (у [`ui::Ui::select`] памяти нет — она здесь).
    lang_open: bool,
    mode: usize,
    /// Имя машины. Пустое — не писать в конфиг вовсе (у машины останется безымянность).
    name: ui::Edit,
}

impl App {
    /// Можно ли ставить на выбранный диск.
    fn target_ok(&self) -> bool {
        self.disks.get(self.sel).is_some_and(|d| !d.live)
    }

    /// Можно ли ОБНОВИТЬ выбранный диск (Веха 226.8). Условие то же, что у ядра: VOID на нём
    /// уже стоит, и это не тот диск, с которого система работает сейчас.
    fn can_update(&self) -> bool {
        self.disks.get(self.sel).is_some_and(|d| d.void && !d.live)
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

    fn step(&self) -> Step {
        STEPS[self.step.min(STEPS.len() - 1)]
    }

    /// Пускать ли дальше с этого шага. `None` — можно; `Some(причина)` — нет, и причину человек
    /// обязан прочитать РЯДОМ С КНОПКОЙ, а не узнать по тому, что она не нажимается.
    fn blocked(&self) -> Option<&'static str> {
        match self.step() {
            Step::Disk if self.disks.is_empty() => Some(ui::t("Дисков не найдено — ставить некуда.")),
            Step::Disk if !self.target_ok() => {
                Some(ui::t("С этого диска работает система прямо сейчас."))
            }
            _ => None,
        }
    }

    /// Веха 227.1 — **ПОЖЕЛАНИЕ УСТАНОВЩИКА**: то, что человек выбрал, в грамматике конфига.
    ///
    /// Едет в ядро вместе с номером диска и ложится в store цели; первая загрузка установленной
    /// системы сеет по нему ИСХОДНИК `/etc/system/*.vv`. Не поколение: поколение — вычисленный
    /// результат, и первая же пересборка вернула бы умолчания образа.
    fn wish(&self) -> String {
        let mut s = String::new();
        s.push_str(&alloc::format!("mode {}\n", MODES[self.mode].1));
        s.push_str(&alloc::format!("language {}\n", LANGS[self.lang].1));
        let name = self.name.text.trim();
        if !name.is_empty() {
            s.push_str(&alloc::format!("name {}\n", name));
        }
        s
    }
}

impl ui::Client for App {
    fn event(&mut self, e: Event, _input: &ui::Input) -> ui::Scope {
        if !matches!(self.stage, Stage::Wizard) {
            return ui::Scope::No;
        }
        let changed = match e {
            Event::Key { sym: code, ch, mods, down } if down => match (self.step(), code) {
                (Step::Disk, sym::DOWN) => self.move_sel(1),
                (Step::Disk, sym::UP) => self.move_sel(-1),
                // Имя машины набирают клавиатурой, и все прочие клавиши на этом шаге — его.
                (Step::System, _) => {
                    self.name.key(code, ch, mods);
                    true
                }
                _ => false,
            },
            Event::Resize { w, h } => {
                self.w = w as i32;
                self.h = h as i32;
                true
            }
            // Клики и наведение разбираются при рисовании: тулкит immediate-mode, и попадание
            // считает тот же код, что рисует (иначе они разойдутся — Веха 123).
            Event::Button { down, .. } if down => true,
            Event::Motion { .. } => true,
            _ => false,
        };
        if changed {
            ui::Scope::All
        } else {
            ui::Scope::No
        }
    }

    fn draw(&mut self, u: &mut Ui) -> ui::Scope {
        let bg = u.th.desktop;
        u.background(bg);
        let pad = u.th.pad * 2;
        let line = u.font.line_h();
        let mut all = Rect::new(0, 0, self.w, self.h).inset(pad);

        // ── карта шагов: внизу, «стрелочкоподобными» блоками (макет владельца) ──
        let map = all.cut_bottom(line * 3);
        all.cut_bottom(u.th.gap);
        let at = match &self.stage {
            Stage::Wizard => self.step,
            // Работа и итог — это всё ещё последний шаг: карта не должна гаснуть ровно тогда,
            // когда человек больше всего хочет знать, где он.
            _ => STEPS.len() - 1,
        };
        let names: Vec<&str> = STEPS.iter().map(|s| s.title()).collect();
        u.steps(map, &names, at);

        let card = u.card(all);
        let mut body = card.inset_xy(0, pad);
        let (tx, mu) = (u.th.text, u.th.muted);

        match &self.stage {
            Stage::Working(обновление) => {
                let слово = if *обновление { ui::t("обновляю…") } else { ui::t("ставлю…") };
                let mut mid = body;
                let bar = mid.cut_top(mid.h / 2 + line);
                u.label(bar, слово, tx, Align::Center);
                // Полоса хода честная ровно настолько, насколько честно ядро: оно не говорит,
                // сколько секторов уже записано, и выдумывать проценты мы не станем. Полоса
                // показывает, что дело ИДЁТ, и доезжает до конца к последнему кадру.
                let w = (mid.w / 2).min(360);
                let line_r = Rect::new(mid.x + (mid.w - w) / 2, mid.y, w, line * 2);
                let ход = u.anim(ANIM_WORK, 256).clamp(0, 256) as u32;
                u.progress(line_r, ход);
                return ui::Scope::No;
            }
            Stage::Done(text, ok) => {
                let col = if *ok { tx } else { u.th.danger };
                let head = body.cut_top(line * 2);
                u.label(head, if *ok { ui::t("Готово") } else { ui::t("Не вышло") }, col, Align::Center);
                body.cut_top(line);
                wrap(u, body.x, body.y, body.w, text, mu);
                return ui::Scope::No;
            }
            Stage::Wizard => {}
        }

        // ── кнопки «назад» и «дальше»: ВНИЗУ карточки, справа ──────────────────
        let mut again = false;
        let bar = body.cut_bottom(line * 2);
        body.cut_bottom(pad);
        let last = self.step + 1 == STEPS.len();
        let mut right = bar;
        let w = (right.w / 3).min(u.text_w(ui::t("Стереть и поставить")) + pad * 4);
        let next_r = right.cut_right(w);
        right.cut_right(u.th.gap);
        let back_r = right.cut_right(w);
        if self.step > 0 {
            let hot = if u.hot(back_r) { 256 } else { 0 };
            if u.button(back_r, ui::t("Назад"), hot) {
                self.step -= 1;
                self.lang_open = false;
                again = true;
            }
        }
        match self.blocked() {
            Some(why) => {
                // Кнопки нет вовсе, а на её месте — причина. Серая кнопка, которая не
                // нажимается, объясняет ровно ничего, и человек жмёт её до тех пор, пока не
                // решит, что сломано.
                u.label(next_r.union(back_r), why, u.th.danger, Align::Right);
            }
            None if last => {
                let hot = if u.hot(next_r) { 256 } else { 0 };
                let (text, upd) = if self.can_update() {
                    (ui::t("Обновить"), true)
                } else {
                    (ui::t("Стереть и поставить"), false)
                };
                let нажата =
                    if upd { u.action(next_r, text, hot) } else { u.danger(next_r, text, hot) };
                if нажата {
                    self.stage = Stage::Working(upd);
                    return ui::Scope::All; // кадр «ставлю…» — ДО самой установки
                }
            }
            None => {
                let hot = if u.hot(next_r) { 256 } else { 0 };
                if u.action(next_r, ui::t("Дальше"), hot) {
                    self.step += 1;
                    self.lang_open = false;
                    again = true;
                }
            }
        }

        // ── содержимое шага ────────────────────────────────────────────────────
        let mut язык: Option<Rect> = None; // список языков рисуется ПОСЛЕДНИМ (он поверх всего)
        match self.step() {
            Step::Lang => {
                // Знак системы крупно: это первый экран, и он обязан сказать, куда человек попал.
                //
                // Блок считается целиком и ставится ПО СЕРЕДИНЕ карточки, а не сверху. Сверху он
                // выглядел бы началом списка, под которым что-то ещё будет, — а под ним ничего
                // нет и не должно быть: на первом экране ровно один вопрос.
                let d = (body.h / 3).min(body.w / 4).min(line * 8);
                let всего = d + line * 5;
                body.cut_top(((body.h - всего) / 2).max(0));
                let logo = body.cut_top(d);
                u.icon(Rect::new(logo.x + (logo.w - d) / 2, logo.y, d, d), ui::icon::LOGO, tx);
                u.label(body.cut_top(line * 2), ui::t("Установка VOID"), tx, Align::Center);
                u.label(body.cut_top(line), ui::t("Язык установки и системы"), mu, Align::Center);
                body.cut_top(u.th.gap);
                let w = (body.w / 2).min(320);
                язык = Some(Rect::new(body.x + (body.w - w) / 2, body.y, w, line * 2));
            }
            Step::Disk => {
                u.label(body.cut_top(line * 2), ui::t("Куда поставить систему"), tx, Align::Center);
                let row_h = line * 3;
                let mut picked = None;
                for i in 0..self.disks.len() {
                    if body.h < row_h + line * 4 {
                        break;
                    }
                    let row = body.cut_top(row_h);
                    let (name, sub, sel) = {
                        let d = &self.disks[i];
                        let mark = if d.live {
                            ui::t(" · с него работает система")
                        } else if d.void {
                            ui::t(" · здесь уже есть VOID")
                        } else {
                            ""
                        };
                        (
                            alloc::format!("{}  ·  {}", d.model, d.size),
                            alloc::format!(
                                "{}{}",
                                ui::f1(ui::t("диск {}"), &alloc::format!("{}", d.slot)),
                                mark
                            ),
                            if i == self.sel { 256 } else { 0 },
                        )
                    };
                    let hot = if u.hot(row) { 256 } else { 0 };
                    if u.entry(row, &name, &sub, "", sel, hot) {
                        picked = Some(i);
                    }
                    body.cut_top(u.th.gap);
                }
                if let Some(i) = picked {
                    self.sel = i;
                    again = true;
                }
                if self.disks.is_empty() {
                    wrap(u, body.x, body.y, body.w, ui::t("SATA-дисков не найдено. Ставить некуда."), mu);
                }
            }
            Step::System => {
                u.label(body.cut_top(line * 2), ui::t("Чем система встречает"), tx, Align::Center);
                let tiles = body.cut_top(line * 3);
                let mut row = tiles;
                let w = row.w / MODES.len() as i32;
                for (i, (имя, _, _)) in MODES.iter().enumerate() {
                    let mut cell = row.cut_left(w);
                    if i + 1 < MODES.len() {
                        cell.w -= u.th.gap;
                    }
                    let hot = if u.hot(cell) { 256 } else { 0 };
                    let on = if i == self.mode { 256 } else { 0 };
                    if u.tile(cell, ui::t(имя), hot, on) {
                        self.mode = i;
                        again = true;
                    }
                }
                body.cut_top(u.th.gap);
                wrap(u, body.x, body.y, body.w, ui::t(MODES[self.mode].2), mu);
                body.cut_top(line * 3);
                u.label(body.cut_top(line * 2), ui::t("Как зовут эту машину"), tx, Align::Center);
                let w = (body.w / 2).min(320);
                let f = Rect::new(body.x + (body.w - w) / 2, body.y, w, line * 2);
                let (click, drag) = u.edit_field(f, &self.name, ui::t("без пробелов, можно пусто"));
                if let Some(a) = click {
                    self.name.put_caret(a);
                    again = true;
                }
                if let Some(a) = drag {
                    self.name.drag_caret(a);
                }
                body.cut_top(line * 3);
                wrap(
                    u,
                    body.x,
                    body.y,
                    body.w,
                    ui::t(
                        "Пользователей в VOID нет — ни root, ни учётных записей. На вопрос «чьё \
                         это» отвечает имя машины.",
                    ),
                    mu,
                );
            }
            Step::Install => {
                u.label(body.cut_top(line * 2), ui::t("Проверь и подтверди"), tx, Align::Center);
                let d = self.disks.get(self.sel);
                let строки = [
                    (ui::t("Диск"), d.map_or(String::new(), |d| alloc::format!("{}  ·  {}", d.model, d.size))),
                    (ui::t("Сеанс"), String::from(ui::t(MODES[self.mode].0))),
                    (ui::t("Язык"), String::from(LANGS[self.lang].0)),
                    (
                        ui::t("Имя машины"),
                        if self.name.text.trim().is_empty() {
                            String::from(ui::t("не задано"))
                        } else {
                            String::from(self.name.text.trim())
                        },
                    ),
                ];
                for (ключ, знач) in строки {
                    let row = body.cut_top(line * 2);
                    u.label(row, ключ, mu, Align::Left);
                    u.label(row, &знач, tx, Align::Right);
                    u.c.fill(Rect::new(row.x, row.bottom(), row.w, 1), u.th.text.with_a(0x18));
                }
                body.cut_top(line);
                let опасно = !self.can_update();
                wrap(
                    u,
                    body.x,
                    body.y,
                    body.w,
                    if опасно {
                        ui::t(
                            "Диск будет стёрт целиком: таблица разделов, все разделы, все файлы. \
                             Отменить это нельзя.",
                        )
                    } else {
                        ui::t(
                            "На этом диске уже есть VOID. Обновятся ядро и загрузчик; файлы, \
                             поколения и пакеты останутся на месте.",
                        )
                    },
                    if опасно { u.th.danger } else { mu },
                );
            }
        }

        // Список языков — ПОСЛЕДНИМ в кадре: он ложится поверх всего нарисованного.
        if let Some(r) = язык {
            let имена: Vec<&str> = LANGS.iter().map(|(имя, _)| *имя).collect();
            match u.select(r, &имена, self.lang, self.lang_open) {
                ui::Pick::Toggle => {
                    self.lang_open = !self.lang_open;
                    again = true;
                }
                ui::Pick::Take(i) => {
                    self.lang = i;
                    self.lang_open = false;
                    // Язык меняется НЕМЕДЛЕННО: дальше человек читает про стирание диска, и
                    // читать это он должен на выбранном языке, а не после перезагрузки.
                    sys::i18n::set(LANGS[i].1 == "en");
                    again = true;
                }
                ui::Pick::Away => {
                    self.lang_open = false;
                    again = true;
                }
                ui::Pick::None => {}
            }
        }

        if again {
            ui::Scope::All
        } else {
            ui::Scope::No
        }
    }

    /// После кадра — сама установка. Отдельно от рисования намеренно: она занимает секунды, и
    /// делать её внутри кадра значило бы показать человеку замерший экран без единого слова.
    fn after(&mut self, _surf: &Window) -> ui::Scope {
        let Stage::Working(обновление) = self.stage else {
            return ui::Scope::No;
        };
        let slot = self.disks.get(self.sel).map(|d| d.slot).unwrap_or(usize::MAX);
        // Веха 227.4 — ПРИЧИНУ отказа спрашиваем у ядра и показываем здесь же. Отсылка к `klog`
        // была верна ровно до машины с настоящим железом: на ноутбуке владельца журнал забит
        // драйверами сети, и одна строка отказа в нём не находится.
        let mut why = [0u8; 160];
        let вышло = if обновление {
            // Обновление конфига НЕ КАСАЕТСЯ: система на диске уже настроена, и переписать ей
            // выбор установщика значило бы отнять у человека то, что он настраивал сам.
            sys::update(self.store, slot, &mut why).is_some()
        } else {
            sys::install(self.store, slot, &self.wish(), &mut why).is_some()
        };
        let причина = sys::err_text(&why);
        let итог = String::from(match (обновление, вышло) {
                (true, true) => ui::t(
                    "VOID обновлён. Файлы, поколения и пакеты остались на месте. \
                     Выключи машину, вынь носитель и включи снова.",
                ),
                (false, true) => ui::t(
                    "VOID установлен. Выключи машину, вынь носитель и включи снова — \
                     система поднимется с диска. Первая загрузка сама посеет конфиг с тем, \
                     что здесь выбрано.",
                ),
                (true, false) => ui::t(
                    "Обновить не вышло. Причину ядро сказало в журнал: `klog`. \
                     Данные на диске при этом не тронуты.",
                ),
                (false, false) => ui::t(
                    "Установка не состоялась. Чаще всего это отсутствие образа установки — то \
                     есть загрузка не с носителя.",
                ),
            });
        // Причина от ядра дописывается к общему объяснению, а не заменяет его: она точная, но
        // короткая («раскладка диска не совпадает с образом»), и одной ей человек не поймёт,
        // что делать дальше.
        self.stage = Stage::Done(
            if вышло || причина.is_empty() {
                итог
            } else {
                // Одним абзацем, без переноса: `wrap` считает перевод строки обычным пробелом
                // (он разбирает текст по словам), и пустая строка здесь всё равно пропала бы —
                // но выглядело бы это как недоделка, а не как решение.
                alloc::format!("{} {}: {}", итог, ui::t("Ядро ответило"), причина)
            },
            вышло,
        );
        ui::Scope::All
    }
}

/// Имя движущейся величины для [`ui::Ui::anim`]: полоса хода установки.
const ANIM_WORK: u32 = 1;

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

/// Без композитора: список словами, установка и ОБНОВЛЕНИЕ по номеру.
fn text_mode(store: usize, disks: &[Disk], arg: Option<usize>, обновить: bool) -> ! {
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
            sys::write(
                "\nПоставить:  install <номер>       ДИСК БУДЕТ СТЁРТ ЦЕЛИКОМ\n\
Обновить:   install update <номер>  данные останутся на месте\n"
                    .as_bytes(),
            );
            sys::exit(0)
        }
        Some(slot) => {
            // Веха 222 — два разных действия и два разных предупреждения. Путать их нельзя ни
            // на слово: одно стирает диск, другое обещает этого не делать.
            let mut why = [0u8; 160];
            let итог = if обновить {
                sys::write(
                    alloc::format!(
                        "install: обновляю диск {} — файлы, поколения и пакеты останутся…\n",
                        slot
                    )
                    .as_bytes(),
                );
                sys::update(store, slot, &mut why)
            } else {
                sys::write(
                    alloc::format!("install: ставлю на диск {} — он будет стёрт целиком…\n", slot)
                        .as_bytes(),
                );
                // Словами пожелания не передают: `install <номер>` ставит ровно то, что в
                // образе. Спрашивать про режим и язык в консоли, где человек уже назвал диск
                // числом, значило бы устроить второй мастер в неподходящем месте.
                sys::install(store, slot, "", &mut why)
            };
            match итог {
                Some(_) => {
                    sys::write(
                        "Готово. Выключи машину, вынь носитель и включи снова.\n".as_bytes(),
                    );
                    sys::exit(0)
                }
                None => {
                    // Причина — ЗДЕСЬ ЖЕ. Отсылка к журналу была отговоркой: ядро причину знает
                    // точно, а в журнале её ещё надо найти среди строк драйверов.
                    let причина = sys::err_text(&why);
                    if причина.is_empty() {
                        sys::write("Не вышло — причина в журнале ядра (`klog`).\n".as_bytes());
                    } else {
                        sys::write(alloc::format!("Не вышло: {}\n", причина).as_bytes());
                    }
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
    // Веха 222 — `install update <номер>` обновляет, `install <номер>` ставит. Слово впереди
    // числа, а не ключ после него: человек сперва решает ЧТО делает, и только потом — с чем.
    let обновить = argv.str(0) == Some("update");
    let arg = argv
        .str(if обновить { 1 } else { 0 })
        .and_then(|s| s.parse::<usize>().ok());

    // Обновление словами и делается словами: окно со списком дисков — про выбор, а человек
    // выбор уже сделал, назвав действие и номер.
    if обновить {
        text_mode(store, &disks, arg, обновить);
    }
    let (w, h) = (720u16, 520u16);
    let Some(mut surf) = Window::create(w, h, ui::t("Установка VOID")) else {
        text_mode(store, &disks, arg, обновить);
    };
    // Номер, названный словами, слушаемся и в окне: человек уже сказал, куда ставить.
    let sel = arg.and_then(|s| disks.iter().position(|d| d.slot == s)).unwrap_or(0);
    let mut app = App {
        w: w as i32,
        h: h as i32,
        disks,
        sel,
        stage: Stage::Wizard,
        store,
        step: 0,
        lang: 0,
        lang_open: false,
        mode: 0,
        name: ui::Edit::tail(String::new()),
    };
    ui::app::run(&mut surf, &th, &mut font, &mut app);
    sys::exit(0);
}
