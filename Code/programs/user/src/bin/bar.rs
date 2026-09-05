//! `bar` — панель и МЕНЮ (Веха 140; тулкит `void-ui` Вехой 144; меню и движение Вехой 145).
//!
//! Второй клиент слоя после обоев и первый, кому нужны КЛИКИ: столы в панели переключаются
//! мышью. Занятая зона у неё настоящая — окна начинаются под панелью, а не заезжают под неё
//! ([[layers]]).
//!
//! ## Почему панель — программа, а не часть композитора
//!
//! Тот же довод, что у обоев, и он стал сильнее: панель ходит за состоянием по протоколу
//! (`OP_STATUS`), рисует шрифтом и знает про календарь. Три чужих умения в программе, где лежат
//! буферы ВСЕХ окон. Здесь падение панели — это исчезнувшая полоска сверху и вернувшееся окнам
//! место; в композиторе оно было бы чёрным экраном.
//!
//! ## Откуда панель знает, что показывать
//!
//! Не опросом. `OP_WATCH` подписывает её на `EV_STATUS` — «состояние сменилось», — и подробности
//! она берёт `OP_STATUS`'ом уже по делу. Часы идут отдельно: минута не событие композитора,
//! поэтому панель спит в `next_event_timeout` ровно до следующей минуты и просыпается либо к ней,
//! либо раньше — от события. Опрос в цикле стоил бы системе простоя (см. Веху 139.3, где ровно
//! это жгло целое ядро).
//!
//! Часы показывают **UTC**: часовых поясов в VOID нет вовсе, а врать про местное время хуже, чем
//! честно показать всемирное. Если у машины нет RTC, время идёт с загрузки — панель говорит об
//! этом вслух при старте, иначе «02:14» выглядело бы как сломанные часы, а не как их отсутствие.
//!
//! ## Веха 144 — острова вместо полосы
//!
//! Панель больше не полоса во всю ширину: это скруглённые острова на обоях (так же выглядит
//! оболочка, которой владелец пользуется сегодня, — `IMG/mk7q48w.png`). Отсюда два следствия,
//! из-за которых веха и не свелась к перекраске:
//!
//! - поверхность стала ПРОЗРАЧНОЙ, а значит композитору понадобилось смешивание по альфе
//!   (`win::LAYER_ALPHA`) — до этого он копировал кадр слоя строками;
//! - перерисовывать всю поверхность на каждую минуту стало жалко, поэтому панель считает
//!   **подпись** каждого острова и трогает только тот, у которого она изменилась.
//!
//! ## Веха 145 — меню и движение
//!
//! Меню (центр управления) живёт ЗДЕСЬ ЖЕ, а не отдельной программой, и поверхность у него та же:
//! на время открытия панель просит буфер во весь экран (`OP_REBUF` — у слоя это и есть смена
//! размера), рисует карточку под собой, а закрывшись, сжимается обратно. Три причины, и первая
//! решающая:
//!
//! 1. **Ждать событий можно только на ОДНОЙ поверхности.** `next_event` спит внутри вызова; будь
//!    у панели две поверхности, ей пришлось бы опрашивать обе в цикле — то есть не спать вовсе,
//!    а это ровно тот простой в 100 %, который чинила Веха 139.3.
//! 2. **Клик мимо меню закрывает его сам собой**: поверхность накрывает экран, значит этот клик
//!    приходит нам. Отдельному окошку меню он не пришёл бы никогда — попадания вне себя клиент не
//!    видит, и «щёлкни по пустому месту» пришлось бы просить у композитора отдельной операцией.
//! 3. Занятая зона при этом не меняется (она задана при создании слоя), поэтому окна НЕ ЕДУТ:
//!    экран накрывает прозрачное стекло, а раскладка о нём не знает.
//!
//! Цена названа честно: пока меню открыто, композитор смешивает по альфе полноэкранный слой над
//! всем, что перерисовывается. Меню живёт секунды, и это дешевле, чем второй событийный цикл.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use void_user as sys;
use void_user::win::{self, Event, Window};

// Тулкит — библиотека, и панель пользуется не всем, что в нём есть: следующий потребитель
// (лаунчер, Веха 146) возьмёт остальное. Поэтому неиспользованное здесь не ошибка.
#[allow(dead_code)]
#[path = "../ui/mod.rs"]
mod ui;
use ui::{Align, Font, Motion, Rect, Theme, Ui};

// Аватар устройства приезжает объектом store — тем же путём, что картинка обоев (Веха 145.1).
#[allow(dead_code)]
#[path = "../obj.rs"]
mod obj;

/// Потолок на размер аватара при распаковке — тот же, что у обоев: картинку выбирает человек, и
/// «слишком большая» обязано быть ответом, а не падением в аллокаторе.
const MAX_PIXELS: u64 = 17_000_000;

/// Куча: настоящий шрифт приезжает файлом на мегабайты, и глифы кэшируются растрами. Куча
/// ленивая — под неё берётся адресное окно, а страницы приходят по мере нужды.
#[global_allocator]
static ALLOC: sys::heap::Heap<{ 24 * 1024 * 1024 }> = sys::heap::Heap::new();

/// Сборка системы — её же показывает `wm` в консоли. Меню обязано отвечать на «что у меня
/// установлено», и брать это из чужих рук ему неоткуда.
const BUILD: &str = env!("VOID_BUILD");

/// Сборка КОРОТКО — только слепок (`aa1ceb8+`), без даты. Дата сборки повторяет соседнюю строку
/// «дата» через день-два и при этом делает строку самой длинной в карточке, то есть задаёт её
/// ширину. Слепок опознаёт сборку однозначно, дата — нет.
fn build_short() -> &'static str {
    match BUILD.split_once(' ') {
        Some((hash, _)) => hash,
        None => BUILD,
    }
}

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let Some((sw, sh)) = win::screen() else {
        say("bar: композитора нет (WM в окружении) — панели не на чем висеть\n");
        sys::exit(1);
    };

    // Тема и шрифт — ДО поверхности: от них зависит высота панели, а высоту надо назвать в
    // запросе. Размер, посчитанный после, пришлось бы менять вторым вызовом на глазах у человека.
    let generation = ui::conf::generation().unwrap_or_default();
    let th = Theme::from_config(&generation);
    let mut font = Font::load(th.font.as_deref(), th.font_px);

    // Сказать, что панель решила. Молчание здесь стоило бы человеку получаса: «почему шрифт не
    // тот» и «почему всё мелкое» — вопросы, ответ на которые панель знает, а он нет.
    let anim_ms = ui::anim::duration_from_config(&generation);
    let mut bar = Bar::new(&th, font.line_h(), sw as i32, sh as i32, anim_ms, &generation);
    say(&alloc::format!(
        "bar: шрифт {} {} px, масштаб {}%, высота {} px, движение {} мс\n",
        if font.ttf() { "из пакета" } else { "встроенный 8×16" },
        th.font_px,
        th.scale,
        bar.h,
        anim_ms,
    ));
    // Кто эта машина — вслух, по той же причине, что и шрифт: «почему в меню написано VOID, а не
    // моё имя» — вопрос, ответ на который панель знает, а человек нет.
    say(&alloc::format!(
        "bar: раскладка {} — слева: {} · середина: {} · справа: {}\n",
        if bar.from_conf { "из bar.vv" } else { "по умолчанию (в конфиге нет строк bar)" },
        slot_names(&bar.left),
        slot_names(&bar.center),
        slot_names(&bar.right),
    ));
    say(&alloc::format!(
        "bar: устройство «{}», аватар {}\n",
        bar.device,
        bar.avatar_root.as_deref().unwrap_or("не задан (знак системы в кружке)"),
    ));

    let spec = win::Layer {
        layer: win::LAYER_TOP,
        anchor: win::ANCHOR_TOP | win::ANCHOR_LEFT | win::ANCHOR_RIGHT,
        // Занятая зона равна ПОЛОСЕ панели и больше не меняется никогда: меню растит поверхность,
        // но не зону — иначе окна разъезжались бы на каждое открытие меню. Вогнутые уголки под
        // полосой тоже места не занимают: они ЛОЖАТСЯ на верхние углы стола, в этом их смысл.
        exclusive: bar.strip as u16,
        // Веха 144 — острова скруглены, значит углы у них прозрачные. Без этого флага композитор
        // скопировал бы кадр как есть и нарисовал вокруг островов чёрный прямоугольник.
        alpha: true,
        // Клавиатура панели не нужна: меню открывается мышью, а набирать в нём нечего. Заберёт
        // её — и текст перестанет доходить до окна в фокусе (Веха 146).
        kbd: false,
    };
    let Some(mut surf) = Window::layer(spec, sw, bar.h as u16, "панель") else {
        say("bar: композитор не дал поверхность слоя\n");
        sys::exit(1);
    };
    if !surf.watch() {
        say("bar: композитор не принял подписку на состояние — столы показаны не будут\n");
    }

    bar.fetch(&surf);
    // Право обзора — ПОСЛЕ поверхности: просьба идёт композитору по IPC, и до того как он
    // выдал нам слой, отвечать на неё ему нечем.
    bar.take_sysview();
    if year_now() < 2000 {
        say("bar: часов у машины нет — время идёт с загрузки (см. SYS_TIME)\n");
    }

    ui::app::run(&mut surf, &th, &mut font, &mut bar);
    sys::exit(0);
}

impl ui::Client for Bar {
    fn event(&mut self, e: Event, input: &ui::Input) -> ui::Scope {
        match e {
            // Состояние сменилось — спросить композитор ПЕРЕД кадром. Не здесь: событий подряд
            // может прийти несколько, а ответ на все один.
            Event::Status => {
                self.stale = true;
                ui::Scope::All
            }
            // Отвечаем ТОЛЬКО на нажатие: реагировать и на отпускание значило бы два
            // переключения на один щелчок.
            Event::Button { x, y, down: true, .. } => {
                self.ptr = Some((x as i32, y as i32));
                ui::Scope::All
            }
            Event::Motion { .. } => {
                // Курсор мог уйти с панели — цикл говорит это через `None` (Веха 144). Пока он
                // стоит на месте, кадра не надо: подписи островов не изменились бы всё равно.
                if input.ptr == self.ptr {
                    return ui::Scope::No;
                }
                self.ptr = input.ptr;
                ui::Scope::All
            }
            Event::Resize { w, h } => {
                self.screen(w as i32, h as i32);
                ui::Scope::All
            }
            _ => ui::Scope::No,
        }
    }

    /// Перед кадром: спросить состояние и подогнать размер поверхности.
    fn before(&mut self, surf: &mut Window) -> ui::Scope {
        if core::mem::take(&mut self.stale) {
            self.fetch(surf);
        }
        let was = self.grown;
        self.sync_surface(surf);
        // Буфер сменился — в нём нет ничего, и кадр обязан быть полным.
        if self.grown != was { ui::Scope::All } else { ui::Scope::No }
    }

    fn draw(&mut self, u: &mut Ui) -> ui::Scope {
        self.paint(u);
        ui::Scope::No
    }

    /// После кадра: исполнить решённое. `true` от [`Bar::act`] означает «нужен новый кадр ПРЯМО
    /// СЕЙЧАС» — клик по кнопке меню меняет РАЗМЕР поверхности, и рисовать надо уже в новую.
    fn after(&mut self, surf: &Window) -> ui::Scope {
        if self.act(surf) { ui::Scope::All } else { ui::Scope::No }
    }

    fn wake(&mut self) -> Option<u32> {
        // Пока что-то движется — просыпаться кадрами; иначе спать до минуты. Композитор будит
        // раньше своим событием, и это ровно то, чего мы ждём.
        //
        // Веха 159 — с метриками спать до минуты нельзя: загрузка процессора считается МЕЖДУ
        // замерами, и без второго замера её не существует. Секунда — цена этого: одно
        // пробуждение в секунду против шестидесяти кадров, которые панель и так рисует, когда
        // что-то движется. Перерисовки при этом чаще не станет — остров сравнивает подпись, и
        // при тех же числах ни один пиксель не тронется.
        Some(match () {
            _ if self.busy() => ui::anim::FRAME_MS,
            _ if self.sysview != sys::NO_CAP => ms_to_next_minute().min(METRIC_MS),
            _ => ms_to_next_minute(),
        })
    }

    /// Срок вышел: перерисовать, если сменилась минута, изменились метрики ИЛИ идёт движение.
    fn tick(&mut self) -> ui::Scope {
        let m = minute_now();
        let metrics = self.sample();
        if m != self.minute || metrics || self.busy() {
            self.minute = m;
            return ui::Scope::All;
        }
        ui::Scope::No
    }
}

/// Остров панели: где он и что на нём было нарисовано в прошлый раз.
///
/// Подпись — не украшение, а вся суть перерисовки по damage: сравнить одно число дешевле, чем
/// сравнивать состояние по полям, и невозможно забыть добавить в сравнение новое поле — оно
/// просто не попадёт в подпись, и это видно в одном месте.
///
/// Веха 145 — в подпись входят и АНИМИРОВАННЫЕ величины (место капсулы, прозрачность подсветки).
/// Из этого само собой следует правильное поведение: пока значение едет, подпись меняется каждый
/// кадр и остров перерисовывается; доехало — подпись замерла, и панель замолчала. Ни одного
/// «если идёт анимация, рисовать всё» в коде нет.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Isle {
    rect: Rect,
    sig: u64,
}

/// Номера анимируемых величин ([[void-ui]]: ключ назначает клиент, и вот он весь).
const A_IND_X: u32 = 1;
const A_IND_W: u32 = 2;
const A_TITLE: u32 = 3;
const A_MENU: u32 = 4;
const A_LANG: u32 = 5;
const A_SYS: u32 = 6;
const A_POWER: u32 = 9;
/// Веха 168 — подсветка колокольчика.
const A_NOTES: u32 = 10;
/// Подсветка пилюль столов: `A_PILL + номер стола`.
const A_PILL: u32 = 16;

/// Веха 159 — как часто панель переспрашивает числа про машину, мс.
const METRIC_MS: u32 = 1000;

/// Что панель показывает и где у неё что нарисовано.
struct Bar {
    h: i32,
    /// Веха 158.2 — высота ПОЛОСЫ панели. Меньше `h`: под полосой живут ещё вогнутые уголки,
    /// они рисуются на нашей поверхности, но места у окон НЕ занимают (зона равна полосе).
    strip: i32,
    sw: i32,
    sh: i32,
    /// Веха 148.3 — композитор сказал, что состояние сменилось, а спросить его мы ещё не успели.
    /// Флагом, а не запросом на месте: событий подряд приходит несколько, а ответ на все один.
    stale: bool,
    /// Минута, которую показывают часы. Ею решается, стоит ли кадра истёкший срок.
    minute: u64,
    space: u8,
    spaces: u8,
    /// Раскладка клавиатуры: 0 — US, 1 — RU (Веха 143).
    layout: u8,
    /// Композитор в ОБЗОРЕ (Веха 145): меню там некому закрыть мышью — слоям в обзоре её не
    /// отдают, — поэтому оно закрывается само.
    overview: bool,
    /// Заголовок окна в фокусе, как его назвал композитор, и тот, что НАРИСОВАН: заголовок
    /// меняется в два такта — старый гаснет, новый загорается.
    title: String,
    shown: String,
    /// Где курсор в координатах поверхности. `None` — не над ней.
    ptr: Option<(i32, i32)>,
    /// Острова прошлого кадра.
    isles: [Isle; SLOTS + 1],
    /// Веха 160 — раскладка ряда из `bar.vv`: что стоит слева, посередине и справа.
    left: Vec<Slot>,
    center: Vec<Slot>,
    right: Vec<Slot>,
    /// Раскладка пришла ИЗ КОНФИГА, а не подставлена умолчанием. Разница видна только в
    /// журнале — на экране «как в bar.vv» и «как по умолчанию» выглядят одинаково, пока их
    /// не развели, и отличить одно от другого снаружи нечем.
    from_conf: bool,
    /// Веха 159 — право обзора МАШИНЫ (`desktop sysview bar`), последний замер и то, что
    /// из него показано. `NO_CAP` — права не дали: остров метрик тогда не рождается вовсе.
    ///
    /// Показывать «—» вместо чисел было бы хуже пустоты: пустое место не обещает ничего, а
    /// прочерк обещает число, которого не будет.
    sysview: usize,
    prev: sys::SysInfo,
    cpu: u32,
    ram: u32,
    /// Кадра ещё не было: холст надо очистить ЦЕЛИКОМ.
    fresh: bool,
    /// Меню: какое открыто (цель) и растянута ли поверхность на весь экран (факт).
    open: Menu,
    grown: bool,
    /// Веха 168 — сколько уведомлений (из снимка состояния) и сам список, прочитанный при
    /// открытии меню. Список не держим постоянно: он нужен ровно тогда, когда на него смотрят.
    notes: u8,
    notes_buf: Vec<u8>,
    /// Какое полотно СЕЙЧАС нарисовано. Отличается от `open` ровно на время ухода: пока оно
    /// уезжает, `open` уже `None`, а рисовать надо то же самое — иначе меню на прощание
    /// подменяет содержимое.
    showing: Menu,
    /// Подпись СОДЕРЖИМОГО меню прошлого кадра — без доли выезда. Ею кадр отличает «меню едет»
    /// от «в меню изменилось написанное»: первое стоит одной полоски, второе — всего полотна.
    card_body: u64,
    /// Выключение подтверждается ВТОРЫМ нажатием. Диалога в тулките нет, и заводить его ради
    /// одной кнопки — заводить окно поверх окна; кнопка, меняющая надпись, честнее и дешевле.
    confirm: bool,
    /// Имя активного поколения — оно же надпись на кнопке меню: система, которой ты пользуешься,
    /// называет себя сама.
    gen: String,
    /// Веха 145.1 — КТО ЭТА МАШИНА: имя устройства и корень store с картинкой. Пользователей в
    /// VOID нет, поэтому «чьё это» — про устройство. Без картинки в кружке рисуется знак системы
    /// (Веха 158), а не первая буква имени: имя и так написано рядом.
    device: String,
    avatar_root: Option<String>,
    /// Распакованный аватар под размер кружка и признак «пробовали уже». Пробуем ОДИН раз и по
    /// первому открытию меню: распаковка картинки на старте панели задержала бы весь экран.
    avatar: Option<void_img::Image>,
    avatar_tried: bool,
    /// Сторона кружка аватара в пикселях — считается от шрифта, как и всё в карточке.
    av_px: u32,
    /// Сколько корней в store и полностью ли влез список (иначе «37+»).
    roots: (u32, bool),
    /// Что решил последний кадр.
    go: Option<u8>,
    flip: bool,
    /// Какое меню просят открыть-закрыть.
    toggle: Option<Menu>,
    power: bool,
    /// Кадр изменил то, ЧТО РИСУЕТСЯ, уже после того как посчитал подпись (сегодня это только
    /// подтверждение выключения). Такому изменению нужен ещё один кадр, и попросить его больше
    /// некому: события от композитора не будет — оно ничего в системе не меняло.
    again: bool,
    mo: Motion,
}

/// Веха 168 — какое полотно свисает с полосы. Их два, и они взаимоисключающи: два открытых
/// меню — это два ответа на вопрос «что сейчас делает панель», и человеку пришлось бы гадать,
/// какое из них слушает его щелчок.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Menu {
    None,
    /// Меню оболочки: кнопка поколения.
    Shell,
    /// Уведомления: колокольчик.
    Notes,
}

/// Веха 160 — ОСТРОВ ПАНЕЛИ как выбор человека: что стоит и в каком порядке, решает `bar.vv`
/// (строки `bar группа остров`). До этой вехи порядок был зашит в код, и «убрать часы» или
/// «переставить столы вправо» стоило пересборки системы.
///
/// Числа этого перечисления — индексы в [`Bar::isles`]: остров сравнивает свою подпись с
/// прошлым кадром по нему, и вторая таблица «кто под каким номером» была бы обязана совпадать
/// с этой (на таких вторых таблицах панель уже стояла — см. `pills` до тулкита).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Slot {
    Clock = 0,
    Lang = 1,
    Metrics = 2,
    Spaces = 3,
    Title = 4,
    Gen = 5,
    /// Веха 168 — колокольчик уведомлений.
    Notes = 6,
}

/// Сколько всего островов знает панель. Карточка меню идёт следом отдельным индексом: она не
/// остров ряда — её нельзя ни переставить, ни убрать, она принадлежит кнопке поколения.
const SLOTS: usize = 7;

impl Slot {
    fn parse(s: &str) -> Option<Slot> {
        Some(match s {
            "clock" => Slot::Clock,
            "lang" => Slot::Lang,
            "metrics" => Slot::Metrics,
            "spaces" => Slot::Spaces,
            "title" => Slot::Title,
            "gen" => Slot::Gen,
            "notes" => Slot::Notes,
            _ => return None,
        })
    }
    fn at(self) -> usize {
        self as usize
    }
}

/// Раскладка панели из конфига поколения.
///
/// Конфиг БЕЗ единой строки `bar` — это поколение старше Вехи 160: там раскладку никто не
/// выбирал, и брать её пустой значило бы стереть панель после обновления системы. Поэтому
/// умолчание применяется ко всем трём группам сразу, а не к каждой по отдельности: пустая
/// группа в новом конфиге — законный выбор («часов мне не надо»), и путать её с «не сказано»
/// нельзя.
fn layout_from(text: &str) -> (Vec<Slot>, Vec<Slot>, Vec<Slot>, bool) {
    let mut any = false;
    let (mut l, mut c, mut r) = (Vec::new(), Vec::new(), Vec::new());
    for e in void_conf::of(text, "bar") {
        any = true;
        let Some(slot) = Slot::parse(e.tail().trim()) else { continue };
        let group = match e.key() {
            "left" => &mut l,
            "center" => &mut c,
            "right" => &mut r,
            _ => continue,
        };
        // Один остров дважды — это опечатка, и второй его экземпляр нарисовался бы поверх
        // первого своим же прямоугольником: у острова одна запись в таблице подписей.
        if !group.contains(&slot) {
            group.push(slot);
        }
    }
    if any {
        (l, c, r, true)
    } else {
        (
            alloc::vec![Slot::Clock, Slot::Lang, Slot::Metrics, Slot::Spaces],
            alloc::vec![Slot::Title],
            alloc::vec![Slot::Notes, Slot::Gen],
            false,
        )
    }
}

/// Имена островов группы через пробел — для журнала. Пустая группа так и говорит: «пусто».
fn slot_names(v: &[Slot]) -> String {
    if v.is_empty() {
        return String::from("пусто");
    }
    let mut s = String::new();
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(match x {
            Slot::Clock => "clock",
            Slot::Lang => "lang",
            Slot::Metrics => "metrics",
            Slot::Spaces => "spaces",
            Slot::Title => "title",
            Slot::Gen => "gen",
            Slot::Notes => "notes",
        });
    }
    s
}

/// Карточка меню в [`Bar::isles`] — сразу за островами ряда.
const I_CARD: usize = SLOTS;

impl Bar {
    fn new(th: &Theme, line_h: i32, sw: i32, sh: i32, anim_ms: u64, gen_text: &str) -> Bar {
        // Имя устройства из конфига; без него — «VOID». Пустым его оставлять нельзя: шапка меню
        // без единого слова выглядит недорисованной.
        let (left, center, right, from_conf) = layout_from(gen_text);
        let device = ui::conf::device(gen_text, "name").unwrap_or_else(|| "VOID".to_string());
        let avatar = ui::conf::device(gen_text, "avatar");
        // Высота считается ОТ ШРИФТА и от темы: разъехаться им нельзя (см. `bar_wanted` в `wm`).
        let isle_h = line_h + 2 * th.px(5);
        // Поля вокруг островов — по макету: остров 21 в полосе 25, то есть по два пикселя
        // сверху и снизу. Было шесть, и панель выходила заметно выше нарисованной.
        let strip = isle_h + 2 * th.px(2);
        // Поверхность выше полосы ровно на уголки: они рисуются на ней, но зону не занимают.
        let h = strip + ui::panel_fillet(strip);
        Bar {
            h,
            strip,
            sw,
            sh,
            stale: false,
            minute: minute_now(),
            space: 0,
            spaces: 1,
            layout: 0,
            overview: false,
            title: String::new(),
            shown: String::new(),
            ptr: None,
            isles: [Isle::default(); SLOTS + 1],
            left,
            center,
            right,
            from_conf,
            sysview: sys::NO_CAP,
            prev: sys::SysInfo::default(),
            cpu: 0,
            ram: 0,
            fresh: true,
            open: Menu::None,
            grown: false,
            notes: 0,
            notes_buf: Vec::new(),
            showing: Menu::None,
            card_body: 0,
            confirm: false,
            gen: ui::conf::generation_name().unwrap_or_else(|| "VOID".to_string()),
            device: device.clone(),
            avatar_root: avatar,
            avatar: None,
            avatar_tried: false,
            // Сторона знака в шапке — ровно две строки текста: тот же расчёт, что в
            // [`Bar::metrics`] (`head - 2*pad`), и разъехаться им нельзя.
            av_px: (2 * line_h).max(8) as u32,
            roots: (0, true),
            go: None,
            flip: false,
            toggle: None,
            power: false,
            again: false,
            mo: Motion::new(anim_ms),
        }
    }

    /// Забыть нарисованное: следующий кадр перерисует всё (смена размера поверхности).
    fn forget(&mut self) {
        self.isles = [Isle::default(); SLOTS + 1];
        self.fresh = true;
    }

    /// Композитор назначил поверхности другой размер (якорь растянул её или упёрся в экран).
    fn screen(&mut self, w: i32, h: i32) {
        self.sw = w;
        if self.grown {
            self.sh = h;
        } else {
            self.h = h;
        }
        self.forget();
    }

    /// Есть ли незаконченное дело: движение, полусменённый заголовок или несовпавший размер
    /// поверхности. По нему решается, просыпаться ли кадрами.
    fn busy(&self) -> bool {
        self.mo.moving() || self.shown != self.title || self.want_grown() != self.grown
    }

    /// Нужна ли сейчас полноэкранная поверхность: меню открыто ИЛИ ещё доигрывает закрытие.
    fn want_grown(&self) -> bool {
        self.open != Menu::None || self.mo.peek(A_MENU) > 0
    }

    /// Спросить композитор: стол, сколько столов, раскладка, обзор, заголовок окна в фокусе.
    fn fetch(&mut self, surf: &Window) {
        let mut buf = [0u8; win::TITLE_MAX];
        let Some(st) = surf.status(&mut buf) else { return };
        self.space = st.space;
        self.spaces = st.spaces.max(1);
        self.layout = st.layout;
        self.overview = st.overview;
        self.notes = st.notes;
        self.title = String::from(core::str::from_utf8(&buf[..st.title_len]).unwrap_or(""));
        // В обзоре мышь слоям не отдают вовсе — закрыть меню было бы нечем.
        if self.overview {
            self.open = Menu::None;
        }
    }

    /// Поверхность растёт на весь экран, когда открывается меню, и сжимается, когда оно ушло.
    ///
    /// Отдельным шагом, а не внутри рисования: смена буфера — разговор с композитором (`OP_REBUF`),
    /// и делать её посреди кадра значило бы рисовать в область, которую уже отпустили.
    fn sync_surface(&mut self, surf: &mut Window) {
        let want = self.want_grown();
        if want == self.grown {
            return;
        }
        let h = if want { self.sh } else { self.h };
        if !surf.resize_buf(self.sw as u16, h as u16) {
            say("bar: композитор не дал буфер под меню — меню не открыть\n");
            self.open = Menu::None;
            return;
        }
        self.grown = want;
        self.forget();
        if want {
            // Меню начинает выезд ОТТУДА, где его не видно. Без этого первый кадр показал бы
            // карточку уже на месте, и «выехало» превратилось бы в «возникло».
            self.mo.set(A_MENU, 0);
        }
    }

    /// Исполнить то, что решил кадр. `true` — состояние изменилось так, что нужен новый кадр
    /// ПРЯМО СЕЙЧАС.
    ///
    /// Отдельно от рисования намеренно: переключение стола вызывает у композитора новое состояние,
    /// а значит и новое событие нам — делать это посреди кадра значило бы рисовать по данным,
    /// которые уже устарели.
    fn act(&mut self, surf: &Window) -> bool {
        if let Some(n) = self.go.take() {
            surf.switch_space(n);
        }
        if core::mem::take(&mut self.flip) {
            // Через композитор, а не `SYS_KEYMAP` самой: право переключать раскладку у владельца
            // ЭКРАНА, а это он. Панель пробовала звать ядро напрямую — и получала отказ, потому
            // что она композитору не ровня, а ребёнок (Веха 144, найдено на первом же клике).
            surf.switch_layout();
        }
        if core::mem::take(&mut self.power) {
            say("bar: выключение по кнопке меню\n");
            // Веха 157 — своё право, а нет его — попросить у композитора. Панель перестала
            // получать выключение наследством в Вехе 154 (право композитора помечено «не
            // наследуется», иначе его имело бы каждое окно), и кнопка молча перестала работать.
            let pc = sys::win::cap_or_grant(10);
            if pc == sys::NO_CAP {
                say("bar: права `power` нет — нужна строка `desktop power bar` в конфиге\n");
            } else {
                sys::power_off(pc);
                // Вернулись — значит права не хватило: сказать вслух, а не молчать кнопкой.
                say("bar: выключить не вышло — право есть, но машина продолжает работу\n");
            }
        }
        if let Some(want) = core::mem::take(&mut self.toggle) {
            // Нажали на то же — закрыли; на другое — переехали. Два открытых полотна сразу
            // человеку пришлось бы различать по содержимому, а не по тому, куда он нажал.
            self.open = if self.open == want { Menu::None } else { want };
            if self.open != Menu::None {
                self.showing = self.open;
            }
            match self.open {
                Menu::Shell => {
                    // Сведения собираются в момент ОТКРЫТИЯ: держать их свежими постоянно
                    // значило бы ходить в store каждую минуту ради того, на что никто не смотрит.
                    self.roots = store_roots();
                    self.confirm = false;
                    self.load_avatar();
                }
                // Список тоже читается при открытии — по той же причине.
                Menu::Notes => {
                    let mut buf = alloc::vec![0u8; 8 * 1024];
                    let n = sys::win::notes_read(&mut buf);
                    buf.truncate(n);
                    self.notes_buf = buf;
                }
                Menu::None => {}
            }
            return true;
        }
        core::mem::take(&mut self.again)
    }

    fn paint(&mut self, u: &mut Ui) {
        self.mo.begin(sys::monotonic_ns());
        let confirm_was = self.confirm;
        let (th, click) = (u.th.clone(), u.click());
        let th = &th;
        let (w, h) = (u.c.w, u.c.h);
        let margin = th.px(2);
        let isle_h = self.strip - 2 * margin;
        let clock = clock_text();
        let date = date_text();
        let lang = if self.layout == 0 { "EN" } else { "RU" };
        let ptr = self.ptr;
        let hot = |r: Rect| ptr.is_some_and(|(x, y)| r.contains(x, y));

        // ── раскладка ряда: сперва посчитать, потом рисовать ───────────────────────────────
        //
        // Считаем всё до единого пикселя ДО первого касания холста: остров, чья ширина зависит
        // от соседа, иначе рисовался бы по вчерашним числам.
        let pill_gap = th.px(4);
        let mut widths = Vec::with_capacity(self.spaces as usize);
        let mut spaces_w = 2 * th.pad;
        for i in 0..self.spaces {
            let label = alloc::format!("{}", i + 1);
            let pw = (u.font.width(&label) + th.px(14)).max(isle_h - th.px(8));
            if i > 0 {
                spaces_w += pill_gap;
            }
            spaces_w += pw;
            widths.push((label, pw));
        }
        // ── ширины островов ────────────────────────────────────────────────────────────────
        //
        // Ширина НОЛЬ значит «острова нет»: заголовка при пустом окне, метрик без права обзора.
        // Это не то же самое, что не назвать остров в конфиге, — там его нет по воле человека,
        // здесь ему нечего показать сейчас.
        let lang_w = u.font.width(lang) + 2 * th.pad;
        let clock_w = u.font.width(&clock);
        let date_w = u.font.width(&date);
        let ico = isle_h - 2 * th.px(5);
        let num_w = u.font.width("100%");
        let title_w = u.font.width(&self.shown) + 2 * th.pad;
        let note_text = alloc::format!("{}", self.notes.min(99));
        let note_w = u.font.width(&note_text) + th.px(4);
        let gen_w = u.font.width(&self.gen) + 2 * th.pad;
        // Все ширины сняты со шрифта ЗАРАНЕЕ: измерение строки просит шрифт изменяемо (глиф
        // может лечь в кэш), а замыкание, которое так делает, нельзя звать из `map`.
        let width = |s: Slot| -> i32 {
            match s {
                Slot::Clock => 2 * th.pad + clock_w + th.px(6) + date_w,
                Slot::Lang => lang_w,
                // Веха 159 — метрик нет вовсе без права обзора: прочерк обещал бы число,
                // которого не будет, а пустое место не обещает ничего.
                Slot::Metrics if self.sysview == sys::NO_CAP => 0,
                Slot::Metrics => 2 * th.pad + 2 * (ico + th.px(4) + num_w) + th.px(10),
                Slot::Spaces => spaces_w,
                Slot::Title if self.shown.is_empty() => 0,
                Slot::Title => title_w,
                Slot::Gen => gen_w,
                // Веха 168 — колокольчик: знак, а при накопившемся — ещё и число рядом.
                Slot::Notes => ico + 2 * th.pad + if self.notes > 0 { note_w } else { 0 },
            }
        };

        // ── расстановка: слева направо, справа налево, остаток — середине ──────────────────
        let mut at = [Rect::ZERO; SLOTS];
        let mut x = margin;
        for &s in &self.left {
            let iw = width(s);
            if iw <= 0 {
                continue;
            }
            at[s.at()] = Rect::new(x, margin, iw, isle_h);
            x += iw + margin;
        }
        let left_end = x - margin;
        let mut xr = w - margin;
        for &s in self.right.iter().rev() {
            let iw = width(s);
            if iw <= 0 {
                continue;
            }
            xr -= iw;
            at[s.at()] = Rect::new(xr, margin, iw, isle_h);
            xr -= margin;
        }
        let right_start = xr + margin;

        // Середина живёт ОСТАТКОМ. Ужимается при этом только заголовок: он один умеет быть
        // любой длины, и резать вместо него часы значило бы получить «19:4» на узком экране.
        let room = right_start - left_end - 2 * margin;
        let mut mid: Vec<(Slot, i32)> = self.center.iter().map(|&s| (s, width(s))).collect();
        mid.retain(|&(_, iw)| iw > 0);
        let gaps = margin * (mid.len() as i32 - 1).max(0);
        let mut total: i32 = mid.iter().map(|&(_, iw)| iw).sum::<i32>() + gaps;
        if total > room {
            if let Some(t) = mid.iter_mut().find(|(s, _)| *s == Slot::Title) {
                t.1 = (t.1 - (total - room)).max(0);
                total = mid.iter().map(|&(_, iw)| iw).sum::<i32>() + gaps;
            }
        }
        if total > 0 && room >= th.px(60) && total <= room {
            let mut cx = ((w - total) / 2).clamp(left_end + margin, right_start - margin - total);
            for (s, iw) in mid {
                at[s.at()] = Rect::new(cx, margin, iw, isle_h);
                cx += iw + margin;
            }
        }
        let (r_isle, m_isle, l_isle, s_isle) = (
            at[Slot::Clock.at()],
            at[Slot::Metrics.at()],
            at[Slot::Spaces.at()],
            at[Slot::Gen.at()],
        );
        let lang_isle = at[Slot::Lang.at()];
        let n_isle = at[Slot::Notes.at()];
        let t_isle = at[Slot::Title.at()];

        // Заголовок меняется в ДВА ТАКТА: старый гаснет, подменяется и загорается новый. Смена
        // текста на полной яркости читается как рывок — рядом с едущим окном это особенно заметно.
        if self.shown != self.title && (self.shown.is_empty() || self.mo.peek(A_TITLE) == 0) {
            self.shown = self.title.clone();
            self.mo.set(A_TITLE, 0);
        }
        let fading = !self.shown.is_empty() && self.shown != self.title;
        let title_a = self.mo.val(A_TITLE, if fading { 0 } else { 256 }).clamp(0, 256) as u32;


        // ── движение ───────────────────────────────────────────────────────────────────────
        let mut pills = Vec::with_capacity(widths.len());
        {
            let mut x = l_isle.x + th.pad;
            for (_, pw) in &widths {
                pills.push(Rect::new(x, l_isle.y + th.px(4), *pw, isle_h - 2 * th.px(4)));
                x += pw + pill_gap;
            }
        }
        let live = pills.get(self.space as usize).copied().unwrap_or(Rect::ZERO);
        if self.fresh {
            // Первый кадр — капсула СРАЗУ на месте: панель не должна начинать жизнь с влёта.
            self.mo.set(A_IND_X, live.x);
            self.mo.set(A_IND_W, live.w);
        }
        let ind = Rect::new(
            self.mo.val(A_IND_X, live.x),
            live.y,
            self.mo.val(A_IND_W, live.w),
            live.h,
        );
        let pill_hot: Vec<u32> = pills
            .iter()
            .enumerate()
            .map(|(i, r)| self.mo.val(A_PILL + i as u32, if hot(*r) { 256 } else { 0 }) as u32)
            .collect();
        let lang_hot = self.mo.val(A_LANG, if hot(lang_isle) { 256 } else { 0 }) as u32;
        let sys_hot = self.mo.val(A_SYS, if hot(s_isle) { 256 } else { 0 }) as u32;
        let notes_hot = self.mo.val(A_NOTES, if hot(n_isle) { 256 } else { 0 }) as u32;

        // ── карточка меню ──────────────────────────────────────────────────────────────────
        let menu_t =
            self.mo.val(A_MENU, if self.open == Menu::None { 0 } else { 256 }).clamp(0, 256) as u32;
        // Место — окончательное, а на экране столько, сколько вытянулось. Содержимое считается
        // по ПЕРВОМУ: строки, съезжающие вверх по мере выезда, читались бы как второе движение
        // внутри первого, и это ровно то, чем «выехало» отличается от «уехало и приехало».
        let card = self.card_rect(th, u.font);
        let sheet = self.sheet(card, menu_t);
        let card_clip = Rect::new(0, self.strip - margin, w, h - (self.strip - margin));
        // Место считается ДО опроса движения: `self.mo` берётся изменяемо, а прямоугольник —
        // из `self`, и в одном выражении эти два заимствования спорят.
        let pow_r = self.power_rect(th, &*u.font, card, sheet);
        let pow_hot = self.mo.val(A_POWER, if hot(pow_r) { 256 } else { 0 }) as u32;
        let uptime = uptime_text();

        // Веха 165 — подпись СОДЕРЖИМОГО меню, без доли выезда. По ней кадр отличает «меню
        // просто едет» от «в меню изменилось написанное»: в первом случае перерисовать надо
        // одну полоску у нижнего края, во втором — всё полотно.
        let card_body = sig(&[
            self.showing as u64,
            self.notes_buf.len() as u64,
            sig(&self.notes_buf.iter().map(|&b| b as u64).collect::<Vec<_>>()),
            self.confirm as u64,
            self.space as u64,
            self.spaces as u64,
            self.roots.0 as u64,
            self.avatar.is_some() as u64,
            sig_str(&self.device),
            sig_str(&uptime),
            sig_str(&clock),
            pow_hot as u64,
        ]);

        // ── что перерисовывать ─────────────────────────────────────────────────────────────
        // Порядок в таблице — порядок [`Slot`], а не порядок на экране: подпись сравнивается
        // с прошлым кадром по индексу острова, и переставленный конфигом остров обязан
        // сравниваться сам с собой, а не с соседом.
        let want = [
            Isle { rect: r_isle, sig: sig(&[sig_str(&clock), sig_str(&date)]) },
            Isle { rect: lang_isle, sig: sig(&[self.layout as u64, lang_hot as u64]) },
            Isle { rect: m_isle, sig: sig(&[self.cpu as u64, self.ram as u64]) },
            Isle {
                rect: l_isle,
                sig: sig(&[
                    self.spaces as u64,
                    ind.x as u64,
                    ind.w as u64,
                    sig(&pill_hot.iter().map(|&v| v as u64).collect::<Vec<_>>()),
                ]),
            },
            Isle { rect: t_isle, sig: sig(&[sig_str(&self.shown), title_a as u64]) },
            Isle { rect: s_isle, sig: sig(&[sig_str(&self.gen), sys_hot as u64]) },
            Isle {
                rect: n_isle,
                sig: sig(&[self.notes as u64, notes_hot as u64, (self.open == Menu::Notes) as u64]),
            },
            Isle {
                rect: if menu_t == 0 { Rect::ZERO } else { sheet },
                sig: sig(&[menu_t as u64, card_body]),
            },
        ];
        // Клик всегда рисует всё: он меняет и то, что под курсором, и то, что было активным, —
        // а «что именно» знает уже сам виджет, а не эта таблица.
        let all = click.is_some();
        let redraw: [bool; SLOTS + 1] = core::array::from_fn(|i| all || want[i] != self.isles[i]);
        if !redraw.iter().any(|&x| x) {
            return;
        }
        // Едет — и только едет: содержимое то же, поверхность не сменилась, клика не было.
        let anim_only = !all && !self.fresh && card_body == self.card_body;
        self.card_body = card_body;

        // Стереть старое место острова вместе с новым: остров, ставший уже, оставил бы за собой
        // кусок себя прежнего — на прозрачной поверхности это не «след», а мусор поверх обоев.
        // Карточка стирается не здесь, а под своим клипом: она одна умеет вылезать за панель.
        if core::mem::take(&mut self.fresh) {
            u.clear_all();
            // Веха 158.2 — полоса панели со стыками у краёв экрана. Только на свежей
            // поверхности: дальше её возвращает под острова `wipe`, а перерисовывать полосу
            // целиком на каждое тиканье часов значило бы трогать весь ряд ради двух цифр.
            u.panel(w, self.strip);
        } else {
            for i in 0..I_CARD {
                if redraw[i] {
                    u.wipe(self.isles[i].rect.union(want[i].rect));
                }
            }
        }

        if redraw[Slot::Spaces.at()] && !l_isle.is_empty() {
            u.island(l_isle);
            // Капсула — ОДНА на ряд и едет; пилюли только подписывают её собой.
            u.indicator(ind, ind.h / 2);
            for (i, (label, _)) in widths.iter().enumerate() {
                // Попадание клика считает САМ виджет — по тому прямоугольнику, по которому он
                // и нарисован. До тулкита панель вела вторую таблицу клеток, и это была не
                // экономия, а два расчёта «где что», обязанных совпасть (обзор композитора уже
                // расходился так — Веха 123).
                let on = pills[i].cover_x(ind);
                if u.pill(pills[i], label, pill_hot[i], on) {
                    self.go = Some(i as u8);
                }
            }
        }

        if redraw[Slot::Title.at()] && !t_isle.is_empty() {
            u.fade(title_a);
            let inner = u.island(t_isle);
            u.label(inner, &self.shown, th.text, Align::Center);
            u.fade(256);
        }

        if redraw[Slot::Clock.at()] && !r_isle.is_empty() {
            // Порядок по макету: сперва время, потом дата приглушённой.
            let mut inner = u.island(r_isle);
            u.label(inner.cut_left(clock_w), &clock, th.text, Align::Left);
            inner.cut_left(th.px(6));
            u.label(inner, &date, th.muted, Align::Left);
        }

        if redraw[Slot::Lang.at()] && !lang_isle.is_empty() {
            // Раскладка — свой остров (Веха 160): её можно переставить или убрать, не трогая
            // часы. В макете её нет вовсе, но клавиатура двуязычная, и молча терять переключение
            // ради точности картинки — плохой размен.
            let inner = u.island(lang_isle);
            if u.button(inner, lang, lang_hot) | u.clicked(lang_isle) {
                self.flip = true;
            }
        }

        if redraw[Slot::Metrics.at()] && !m_isle.is_empty() {
            let mut inner = u.island(m_isle);
            let iy = m_isle.y + (isle_h - ico) / 2;
            for (art, val) in [(ui::icon::CPU, self.cpu), (ui::icon::RAM, self.ram)] {
                let ix = inner.cut_left(ico).x;
                u.icon(Rect::new(ix, iy, ico, ico), art, th.muted);
                inner.cut_left(th.px(4));
                let text = alloc::format!("{val}%");
                u.label(inner.cut_left(num_w), &text, th.text, Align::Left);
                inner.cut_left(th.px(10));
            }
        }

        if redraw[Slot::Gen.at()] && !s_isle.is_empty() {
            let inner = u.island(s_isle);
            // Нажимается ВЕСЬ остров, а не только надпись: целиться в четыре буквы, когда рядом
            // есть очевидная карточка, — это заставлять человека мериться с пикселями.
            let pressed = u.button(inner, &self.gen, sys_hot) | u.clicked(s_isle);
            if pressed {
                self.toggle = Some(Menu::Shell);
            }
        }

        if redraw[Slot::Notes.at()] && !n_isle.is_empty() {
            // Веха 168 — КОЛОКОЛЬЧИК. Число рядом, а не поверх знака: цифра на знаке в шрифте
            // 8×16 превращается в кляксу, а рядом она читается.
            let inner = u.island(n_isle);
            let mut d = inner;
            let ir = d.cut_left(ico);
            let lit = self.notes > 0;
            let col = if self.open == Menu::Notes || lit { th.text } else { th.muted };
            u.icon(Rect::new(ir.x, n_isle.y + (isle_h - ico) / 2, ico, ico), ui::icon::BELL, col);
            if lit {
                u.label(d, &note_text, th.text, Align::Right);
            }
            if u.clicked(n_isle) {
                self.toggle = Some(Menu::Notes);
            }
        }

        if redraw[I_CARD] {
            // Стереть надо и УГОЛКИ карточки: они торчат за её прямоугольник (слева сверху и
            // справа снизу), и без запаса от прошлого кадра остался бы их след.
            let fil = ui::panel_fillet(self.strip);
            let was = self.isles[I_CARD].rect;
            // Веха 165 — на чистом движении меняется ОДНА ПОЛОСКА у нижнего края: раскрытие
            // клипом ничего не двигает, и всё, что выше края, уже нарисовано правильно. Запас
            // вверх на скругление — угол полотна уехал вниз и стал серединой; вниз на уголок —
            // вогнутый стык торчит за прямоугольник.
            let touched = if anim_only && !was.is_empty() {
                let lo = was.bottom().min(sheet.bottom()) - th.radius;
                let hi = was.bottom().max(sheet.bottom()) + fil;
                Rect::new(card.x - fil, lo, card.w + fil, hi - lo)
            } else {
                let g = was.union(want[I_CARD].rect);
                Rect::new(g.x - fil, g.y, g.w + fil, g.h + fil)
            };
            // Карточка режется краем панели: она выезжает ИЗ-ПОД неё, а не поверх её островов.
            u.clip(card_clip.intersect(touched));
            u.clear(touched);
            // Веха 162 — и вернуть уголки САМОЙ ПОЛОСЫ: карточка прижата к правому краю экрана,
            // то есть накрывает правый стык полосы со столом. Под клипом это стоит двух уголков,
            // а не перерисовки ряда: выше `strip - margin` клип не пускает.
            u.panel(w, self.strip);
            if !sheet.is_empty() {
                // Полотно рисуется целиком (уголки торчат за него), а СОДЕРЖИМОЕ режется по
                // вытянутому: коробки и строки лежат на своих окончательных местах и
                // открываются по мере того, как полотно до них доходит.
                u.dropdown(sheet, fil);
                u.clip(card_clip.intersect(touched).intersect(sheet));
                self.draw_card(u, th, card, sheet, &uptime, &clock, pow_hot);
                u.clip(card_clip.intersect(touched));
            }
            u.clip(Rect::new(0, 0, w, h));
        }

        // Щелчок мимо всего — закрыть меню. Работает потому, что открытая поверхность накрывает
        // экран: клик по чужому окну приходит НАМ, а не ему. Пока меню закрыто, поверхность —
        // полоска панели, и мимо неё щёлкнуть нельзя вовсе.
        if self.open != Menu::None && self.toggle.is_none() {
            if let Some((cx, cy)) = click {
                // По ВЫТЯНУТОМУ, а не по окончательному: пока полотно едет, «внутри меню» — это
                // то, что человек видит, а не то, где меню будет через треть секунды.
                let inside = sheet.contains(cx, cy)
                    || n_isle.contains(cx, cy)
                    || l_isle.contains(cx, cy)
                    || t_isle.contains(cx, cy)
                    || r_isle.contains(cx, cy)
                    || s_isle.contains(cx, cy);
                if !inside {
                    self.toggle = Some(self.open);
                }
            }
        }

        // Виджет мог поменять картинку прямо в этом кадре (кнопка выключения «взвелась»), а
        // подпись посчитана ДО него. Просим ещё кадр: иначе кнопка осталась бы невзведённой на
        // экране до ближайшей минуты — найдено проверкой, курсор при этом стоял неподвижно, и
        // разбудить панель было нечему.
        self.again |= self.confirm != confirm_was;

        self.isles = want;
    }

    // ── меню ───────────────────────────────────────────────────────────────────────────────

    /// Веха 159 — взять право обзора машины и сделать первый замер.
    ///
    /// Через КОМПОЗИТОРА (`win::cap_or_grant`), а не наследством: панель — его ребёнок, но
    /// право обзора не наследуется, и отдаёт он его только тому, кого назвал конфиг
    /// (`desktop sysview bar`). Не дали — панель говорит об этом вслух и живёт без метрик.
    fn take_sysview(&mut self) {
        let c = sys::win::cap_or_grant(13);
        if c == sys::NO_CAP {
            say("bar: права обзора нет — метрик не будет (строка `desktop sysview bar` в конфиге)\n");
            return;
        }
        self.sysview = c;
        if let Some(i) = sys::sysinfo(c) {
            self.prev = i;
            self.ram = i.ram_percent();
        }
    }

    /// Новый замер. `true` — показанные числа изменились, нужен кадр.
    fn sample(&mut self) -> bool {
        if self.sysview == sys::NO_CAP {
            return false;
        }
        let Some(now) = sys::sysinfo(self.sysview) else { return false };
        // Слишком близкие замеры не считаем: на промежутке короче полусекунды разница времён
        // сравнима с квантом вытеснения, и «загрузка» скакала бы от 0 до 100 на ровном месте.
        if now.uptime_ns.saturating_sub(self.prev.uptime_ns) < 500_000_000 {
            return false;
        }
        let (cpu, ram) = (now.cpu_percent(&self.prev), now.ram_percent());
        self.prev = now;
        let changed = (cpu, ram) != (self.cpu, self.ram);
        self.cpu = cpu;
        self.ram = ram;
        changed
    }

    /// Веха 145.1 — распаковать аватар устройства. Один раз за жизнь панели и по первому открытию
    /// меню: картинку выбирает человек, она может быть на мегабайты, и платить за неё при старте
    /// панели значило бы задерживать весь экран ради того, на что ещё никто не смотрит.
    ///
    /// Приезжает он объектом store, как обои: корень называет конфиг (`device("avatar", …)`),
    /// файловой системы для этого не нужно вовсе.
    fn load_avatar(&mut self) {
        if self.avatar_tried {
            return;
        }
        self.avatar_tried = true;
        let Some(root) = self.avatar_root.clone() else { return };
        // Путь читается файловым сервером, имя — корнем store. Тот же уговор, что у шрифта, и по
        // той же причине: картинка человека может лежать и объектом, и файлом внутри пакета, а
        // требовать от него «сперва положи в store» значит требовать инструмента, которого у
        // него под рукой нет.
        let bytes = if root.starts_with('/') {
            match ui::font::read_path(&root) {
                Some(b) => b,
                None => {
                    say(&alloc::format!("bar: аватар «{root}»: файла нет\n"));
                    return;
                }
            }
        } else {
            let Some(store) = ui::conf::store_cap() else {
                say("bar: аватар не прочитать — нет права на store\n");
                return;
            };
            match obj::read(store, root.as_bytes()) {
                Ok(b) => b,
                Err(e) => {
                    say(&alloc::format!("bar: аватар «{root}»: {e}\n"));
                    return;
                }
            }
        };
        let img = match void_img::decode(&bytes, MAX_PIXELS) {
            Ok(i) => i,
            Err(e) => {
                say(&alloc::format!("bar: аватар «{root}»: {e}\n"));
                return;
            }
        };
        let (iw, ih) = (img.w, img.h);
        // `cover`, а не `scaled`: аватар квадратный, а снимок обычно нет — вписывать его целиком
        // значит оставить поля внутри кружка.
        match img.cover(self.av_px, self.av_px) {
            Ok(a) => {
                say(&alloc::format!("bar: аватар {iw}×{ih} → {0}×{0}\n", self.av_px));
                self.avatar = Some(a);
            }
            Err(e) => say(&alloc::format!("bar: аватар «{root}»: {e}\n")),
        }
    }

    /// Меры карточки: строка сведений, высота ШАПКИ и поле внутри коробки.
    ///
    /// Шапка — ровно две строки текста плюс поля, как в макете (там 37 при содержимом 31): имя
    /// машины и время работы под ним. Всё остальное считается от них, чтобы карточка оставалась
    /// соразмерной себе при любом кегле.
    fn metrics(th: &Theme, font: &Font) -> (i32, i32, i32) {
        let row = font.line_h() + th.px(6);
        let pad = th.px(3);
        let head = 2 * font.line_h() + 2 * pad;
        (row, head, pad)
    }

    /// Веха 168 — ПОЛОТНО УВЕДОМЛЕНИЙ: что накопилось, свежее сверху.
    ///
    /// Каждое — своя карточка: заголовок, под ним «от кого» и текст, справа крестик. «От кого» —
    /// имя, которое назвало ЯДРО, а не то, которым программа представилась: подписаться чужим
    /// именем в уведомлении должно быть так же невозможно, как выпросить чужое право.
    fn draw_notes(&mut self, u: &mut Ui, th: &Theme, card: Rect) {
        let font_h = u.font.line_h();
        let (_, _, pad) = Self::metrics(th, &*u.font);
        let m = th.px(5);
        let mut d = card.inset(m);
        let note_h = 2 * font_h + 2 * pad;
        // Список читается из буфера, снятого при открытии: он не меняется, пока смотрят, и
        // перечитывать его каждый кадр значило бы дёргать композитор шестьдесят раз в секунду.
        let buf = core::mem::take(&mut self.notes_buf);
        let mut drop_id: Option<u32> = None;
        let mut shown = 0usize;
        for n in win::notes(&buf) {
            if shown >= Self::NOTES_SHOWN || d.h < note_h {
                break;
            }
            shown += 1;
            let r = d.cut_top(note_h);
            d.cut_top(m);
            u.card(r);
            let mut c = r.inset(pad);
            let x = c.cut_right(font_h);
            let title = String::from_utf8_lossy(n.title);
            let from = String::from_utf8_lossy(n.from);
            let text = String::from_utf8_lossy(n.text);
            let col = if n.level == win::NOTE_WARN { th.danger } else { th.text };
            u.label(Rect::new(c.x, c.y, c.w, font_h), &title, col, Align::Left);
            // Вторая строка — «от кого» и текст вместе: у уведомления обычно одна короткая
            // фраза, и отдавать ей отдельную строку значило бы растить полотно вдвое ради
            // пустоты.
            let sub = if from.is_empty() {
                text.to_string()
            } else {
                alloc::format!("{from}: {text}")
            };
            u.label(Rect::new(c.x, c.y + font_h, c.w, font_h), &sub, th.muted, Align::Left);
            let hot = if u.hot(x) { 256 } else { 0 };
            if u.icon_button(x, ui::icon::CLOSE, hot, false) {
                drop_id = Some(n.id);
            }
        }
        let total = win::notes(&buf).count();
        self.notes_buf = buf;
        // Подвал: «убрать все» — и сколько не поместилось. Молча спрятанный хвост списка это
        // ровно та ложь, которой в системе быть не должно.
        let foot = d.cut_top(font_h + th.px(4));
        if shown == 0 {
            u.label(foot, "уведомлений нет", th.muted, Align::Left);
        } else {
            if total > shown {
                let more = alloc::format!("ещё {}", total - shown);
                u.label(foot, &more, th.muted, Align::Left);
            }
            let btn = Rect::new(foot.right() - th.px(90), foot.y, th.px(90), foot.h);
            let hot = if u.hot(btn) { 256 } else { 0 };
            if u.button(btn, "убрать все", hot) {
                drop_id = Some(0);
            }
        }
        if let Some(id) = drop_id {
            win::note_drop(id);
            let mut nb = alloc::vec![0u8; 8 * 1024];
            let n = win::notes_read(&mut nb);
            nb.truncate(n);
            self.notes_buf = nb;
            self.again = true;
        }
    }

    /// Сколько уведомлений показываем разом. Больше — полотно перестаёт помещаться на экран
    /// ноутбука, а история всё равно не читается «вся»: человек смотрит последние.
    const NOTES_SHOWN: usize = 6;

    /// Сколько строк в коробке сведений. Число живёт одним местом: по нему считается и высота
    /// карточки, и то, что в неё влезает.
    const INFO_ROWS: i32 = 6;

    /// Самая широкая пара «подпись — значение» из тех, что окажутся в коробке сведений. Ширину
    /// карточки задаёт она, а не выбранные наугад две строки: строка, которую забыли посчитать,
    /// налезает значением на подпись — обе выравниваются по своим краям и молча встречаются
    /// посередине.
    fn widest_row(font: &mut Font, gap: i32) -> i32 {
        [
            ("сборка", build_short()),
            ("поколение", "gen00"),
            ("время", "00:00"),
            ("дата", "00.00.0000"),
            ("столов", "00, сейчас 00"),
            ("корней в store", "00000+"),
        ]
        .iter()
        .map(|(k, v)| font.width(k) + gap + font.width(v))
        .max()
        .unwrap_or(0)
    }

    /// Где карточка меню, когда она ОТКРЫТА ЦЕЛИКОМ. Движение сюда не входит — им занят
    /// [`Bar::sheet`].
    ///
    /// Веха 162 — карточка прижата к ПРАВОМУ КРАЮ ЭКРАНА и к низу полосы, без полей: по макету
    /// это не отдельное окошко, а продолжение панели вниз ([`ui::Ui::dropdown`]). Поля были бы
    /// видны насквозь как щель между меню и краем, а вогнутому стыку не на чем стоять.
    fn card_rect(&self, th: &Theme, font: &mut Font) -> Rect {
        let (row, head, pad) = Self::metrics(th, &*font);
        let m = th.px(5); // поле полотна вокруг коробок — из макета
        // Веха 168 — у полотна уведомлений свои меры: оно шире (в нём текст, а не пары
        // «подпись — значение») и ровно такой высоты, сколько накопилось.
        if self.showing == Menu::Notes {
            let w = th.px(320).min(self.sw - th.px(20));
            let n = win::notes(&self.notes_buf).count().min(Self::NOTES_SHOWN).max(1) as i32;
            let note_h = 2 * font.line_h() + 2 * pad;
            let h = 2 * m + n * (note_h + m) + row + m;
            return Rect::new(self.sw - w, self.strip, w, h);
        }
        let w = (Self::widest_row(font, th.gap) + 2 * th.pad + 2 * m).max(th.px(200));
        let h = 2 * m + head + m + (2 * pad + Self::INFO_ROWS * row);
        // От низа ПОЛОСЫ, а не поверхности: под полосой у нас теперь ещё вогнутые уголки, и
        // считать от них значило бы отодвинуть карточку от панели на их радиус.
        Rect::new(self.sw - w, self.strip, w, h)
    }

    /// Веха 165 — сколько полотна ВЫТЯНУЛОСЬ из полосы: `t` (0..256) — доля хода.
    ///
    /// ## Почему не прозрачность
    ///
    /// До этой вехи меню проявлялось: подъём на палец плюс общая альфа кадра. Решение владельца —
    /// отказаться от проявления вовсе. Довод не про вкус: панель у нас **вещество**, а не слайд.
    /// Полоса, из которой меню растёт, никуда не девается; скругления и вогнутые стыки заведены
    /// ровно затем, чтобы это чтение работало ([[void-figma-design]]). Проявление же говорит
    /// «здесь появилось второе окно» — то самое, чем меню не является.
    ///
    /// ## И заодно оно ДЕШЕВЛЕ
    ///
    /// Прозрачность гнала весь кадр по медленной дороге: цвет с альфой < 255 не заливается
    /// строками ([`ui::paint::Canvas::fill`]), а идёт по пикселю через `blend`, и на пустой
    /// поверхности — по САМОЙ медленной его ветке, с делением на каждый канал. Двести тысяч
    /// пикселей карточки, шестьдесят раз в секунду. Вытягивание не стоит ничего: содержимое
    /// режется клипом, а нарисовано ровно столько, сколько видно.
    fn sheet(&self, card: Rect, t: u32) -> Rect {
        Rect::new(card.x, card.y, card.w, card.h * t.min(256) as i32 / 256)
    }

    /// Коробки макета внутри карточки: шапка и сведения. ОДИН расчёт на всех — рисование и
    /// попадания берут места отсюда, а не считают их каждый по-своему.
    fn boxes(&self, th: &Theme, font: &Font, card: Rect) -> (Rect, Rect) {
        let (_, head, _) = Self::metrics(th, font);
        let m = th.px(5);
        let mut c = card.inset(m);
        let top = c.cut_top(head);
        c.cut_top(m); // зазор между коробками
        (top, c)
    }

    /// Место круглой кнопки выключения — в шапке, справа, как в макете. Считается ТЕМ ЖЕ кодом,
    /// что и рисование ([`Bar::draw_card`] режет те же коробки): два расчёта «где что» — это два
    /// случая разойтись, на которых система уже стояла (обзор, Веха 123).
    ///
    /// Веха 165 — «есть ли кнопка» решает ВЫТЯНУТОЕ полотно, а не флаг «меню открыто». Флаг
    /// гаснет в момент щелчка, а полотно ещё треть секунды уезжает — с ним вместе обязана
    /// уезжать и кнопка, иначе она пропадает рывком за кадр до всего остального.
    fn power_rect(&self, th: &Theme, font: &Font, card: Rect, sheet: Rect) -> Rect {
        if card.is_empty() || sheet.is_empty() {
            return Rect::ZERO;
        }
        let (_, head, pad) = Self::metrics(th, font);
        let (top, _) = self.boxes(th, font, card);
        let ico = head - 2 * pad;
        // 23 из 31 в макете: кнопка чуть меньше строки, иначе круг упирается в края коробки.
        let btn = (ico - th.px(8)).max(th.px(14));
        let inner = top.inset(pad);
        Rect::new(inner.right() - btn, inner.y + (ico - btn) / 2, btn, btn)
    }

    /// Карточка центра управления.
    ///
    /// Показывается ТОЛЬКО то, у чего есть источник. Звука в VOID нет вовсе (драйвера HDA нет),
    /// батареи нет (ACPI разобран лишь до выключения), сети в меню нет (служба есть, но опрашивать
    /// её отсюда — это ещё один протокол). Пустой ползунок громкости ради красоты запрещён тем же
    /// правилом, которым запрещён спиннер после смерти процесса ([[0016-void-ui-toolkit]]).
    ///
    /// **Переключателя раскладки здесь нет** (решение владельца, Веха 145.1): он и так в панели,
    /// в двух сантиметрах выше, и второй его экземпляр — это два места, где одно и то же надо
    /// поддерживать. Меню пока почти пустое, и это честнее, чем набить его повторами.
    ///
    /// «Карточки владельца» из плана здесь нет и не будет: владельцев в VOID не существует —
    /// система сознательно без юзеров и root. Вместо человека — УСТРОЙСТВО: его имя и аватар из
    /// конфига (`device("name", …)`, `device("avatar", …)`), а под ним время работы.
    ///
    /// Веха 162 — раскладка по макету: ПОЛОТНО цвета полосы, на нём две коробки. Шапка (иконка,
    /// имя, время работы, выключение) и сведения. Средний ряд из трёх переключателей в макете
    /// есть, а здесь его нет: за ним нет ни звука, ни Wi-Fi, ни яркости — рисовать кнопку, под
    /// которой ничего, запрещено тем же правилом, что и пустой ползунок громкости.
    fn draw_card(
        &mut self,
        u: &mut Ui,
        th: &Theme,
        card: Rect,
        sheet: Rect,
        uptime: &str,
        clock: &str,
        pow_hot: u32,
    ) {
        if self.showing == Menu::Notes {
            self.draw_notes(u, th, card);
            return;
        }
        let (row, head_h, pad) = Self::metrics(th, &*u.font);
        let (top, info) = self.boxes(th, &*u.font, card);

        // ── шапка: кто эта машина ──────────────────────────────────────────────────────────
        u.card(top);
        let mut h = top.inset(pad);
        let ico = head_h - 2 * pad;
        let art = h.cut_left(ico);
        match self.avatar.as_ref() {
            // Своя картинка из конфига — кружком, как было.
            Some(a) => u.avatar(art, Some((&a.px[..], a.w as i32, a.h as i32))),
            // Без неё — знак системы прямо на коробке: в макете это перечёркнутый глаз без
            // подложки, и подложка тут не «фон иконки», а лишний кружок вокруг неё.
            None => u.icon(art, ui::icon::LOGO, th.muted),
        }
        h.cut_left(th.px(4));
        let round = self.power_rect(th, &*u.font, card, sheet);
        h.cut_right(round.w + th.px(4));
        let line = u.font.line_h();
        u.label(Rect::new(h.x, h.y, h.w, line), &self.device, th.text, Align::Left);
        // Вторая строка шапки — время работы, а на втором шаге выключения ПРЕДУПРЕЖДЕНИЕ: место
        // одно, и подпись у кнопки, которая сейчас погасит машину, важнее уптайма.
        let (sub, col) = if self.confirm {
            ("нажми ещё раз", th.danger)
        } else {
            (uptime, th.muted)
        };
        u.label(Rect::new(h.x, h.y + line, h.w, line), sub, col, Align::Left);
        // Нажать можно только по ВИДИМОЙ кнопке. Клип режет рисование, но не попадание: пока
        // полотно не дотянулось до шапки, кнопки выключения на экране нет — а клик по ней
        // прошёл бы. Полсекунды невидимого выключателя под курсором — ровно тот сорт «нажал не
        // туда», который человек себе объяснить не сможет.
        if u.power_button(round, pow_hot, self.confirm) && round.bottom() <= sheet.bottom() {
            if self.confirm {
                self.power = true;
            } else {
                self.confirm = true;
            }
        }

        // ── сведения ───────────────────────────────────────────────────────────────────────
        u.card(info);
        let mut c = info.inset(pad);
        let (year, mo, d, _, _, _) = sys::civil_from_unix(sys::time_ns() / 1_000_000_000);
        u.row(c.cut_top(row), "сборка", build_short());
        u.row(c.cut_top(row), "поколение", &self.gen);
        // Время и дата — РАЗНЫМИ строками: вместе они были самой длинной строкой карточки и
        // растягивали её вдвое против макета ради одного значения.
        u.row(c.cut_top(row), "время", clock);
        u.row(c.cut_top(row), "дата", &alloc::format!("{d:02}.{mo:02}.{year}"));
        u.row(
            c.cut_top(row),
            "столов",
            &alloc::format!("{}, сейчас {}", self.spaces, self.space as u32 + 1),
        );
        u.row(
            c.cut_top(row),
            "корней в store",
            &if self.roots.1 {
                alloc::format!("{}", self.roots.0)
            } else {
                alloc::format!("{}+", self.roots.0)
            },
        );
    }
}

/// Сколько корней в store и влез ли список целиком.
///
/// Это единственный «индикатор диска», который в VOID имеет смысл: файловой системы с занятым
/// местом у неё нет, а корни — то, из чего система состоит (поколения, пакеты, картинки).
fn store_roots() -> (u32, bool) {
    let Some(cap) = ui::conf::store_cap() else { return (0, true) };
    let mut buf = alloc::vec![0u8; 64 * 1024];
    let Some((got, want)) = sys::obj_list_roots_ex(cap, &mut buf) else { return (0, true) };
    let n = buf[..got.min(buf.len())].iter().filter(|&&b| b == b'\n').count() as u32;
    (n, want <= got)
}

/// Подпись набора чисел (FNV-1a). Нужна только для сравнения «то же самое или нет».
fn sig(vals: &[u64]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for v in vals {
        for b in v.to_le_bytes() {
            h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
        }
    }
    h
}

fn sig_str(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in s.as_bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// Часы «ЧЧ:ММ» по UTC.
fn clock_text() -> String {
    let (_, _, _, hh, mm, _) = sys::civil_from_unix(sys::time_ns() / 1_000_000_000);
    alloc::format!("{hh:02}:{mm:02}")
}

/// Дата «ДД.ММ» — рядом с часами и приглушённо: она нужна реже времени, но искать её в другом
/// месте человеку не должно приходиться.
fn date_text() -> String {
    let (_, mo, d, _, _, _) = sys::civil_from_unix(sys::time_ns() / 1_000_000_000);
    alloc::format!("{d:02}.{mo:02}")
}

/// Сколько система работает. Монотонными часами, а не разницей календарного времени: RTC у машины
/// может не быть вовсе, и тогда календарь идёт с нуля, а этот счётчик всё равно верен.
fn uptime_text() -> String {
    let s = sys::monotonic_ns() / 1_000_000_000;
    let (h, m) = (s / 3600, s / 60 % 60);
    if h > 0 {
        alloc::format!("{h} ч {m} мин")
    } else if s >= 60 {
        alloc::format!("{m} мин")
    } else {
        alloc::format!("{s} с")
    }
}

fn minute_now() -> u64 {
    sys::time_ns() / 60_000_000_000
}

fn year_now() -> i64 {
    sys::civil_from_unix(sys::time_ns() / 1_000_000_000).0
}

/// Сколько миллисекунд до смены минуты. Не меньше секунды: ноль означал бы бесконечное
/// просыпание на границе, а лишняя секунда на часах без секундной стрелки не видна.
fn ms_to_next_minute() -> u32 {
    let secs = sys::time_ns() / 1_000_000_000 % 60;
    ((60 - secs) * 1000).max(1000) as u32
}

fn say(s: &str) {
    sys::write(s.as_bytes());
}
