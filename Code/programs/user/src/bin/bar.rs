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
        "bar: устройство «{}», аватар {}\n",
        bar.device,
        bar.avatar_root.as_deref().unwrap_or("не задан (буква в кружке)"),
    ));

    let spec = win::Layer {
        layer: win::LAYER_TOP,
        anchor: win::ANCHOR_TOP | win::ANCHOR_LEFT | win::ANCHOR_RIGHT,
        // Занятая зона равна высоте ПАНЕЛИ и больше не меняется никогда: меню растит поверхность,
        // но не зону — иначе окна разъезжались бы на каждое открытие меню.
        exclusive: bar.h as u16,
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
        Some(if self.busy() { ui::anim::FRAME_MS } else { ms_to_next_minute() })
    }

    /// Срок вышел: перерисовать, если сменилась минута ИЛИ если идёт движение (кадр анимации).
    fn tick(&mut self) -> ui::Scope {
        let m = minute_now();
        if m != self.minute || self.busy() {
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
/// Подсветка пилюль столов: `A_PILL + номер стола`.
const A_PILL: u32 = 16;

/// Что панель показывает и где у неё что нарисовано.
struct Bar {
    h: i32,
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
    isles: [Isle; 5],
    /// Кадра ещё не было: холст надо очистить ЦЕЛИКОМ.
    fresh: bool,
    /// Меню: открыто ли (цель) и растянута ли поверхность на весь экран (факт).
    open: bool,
    grown: bool,
    /// Выключение подтверждается ВТОРЫМ нажатием. Диалога в тулките нет, и заводить его ради
    /// одной кнопки — заводить окно поверх окна; кнопка, меняющая надпись, честнее и дешевле.
    confirm: bool,
    /// Имя активного поколения — оно же надпись на кнопке меню: система, которой ты пользуешься,
    /// называет себя сама.
    gen: String,
    /// Веха 145.1 — КТО ЭТА МАШИНА: имя устройства, его первая буква (запасной аватар) и корень
    /// store с картинкой. Пользователей в VOID нет, поэтому «чьё это» — про устройство.
    device: String,
    /// Имя пришло ИЗ КОНФИГА, а не подставлено. Нужно шапке: под безымянной машиной писать
    /// «VOID <сборка>» вторым «VOID» подряд — это выглядеть сломанным, а не скромным.
    named: bool,
    letter: String,
    avatar_root: Option<String>,
    /// Распакованный аватар под размер кружка и признак «пробовали уже». Пробуем ОДИН раз и по
    /// первому открытию меню: распаковка картинки на старте панели задержала бы весь экран.
    avatar: Option<void_img::Image>,
    avatar_tried: bool,
    /// Сторона кружка аватара в пикселях — считается от шрифта, как и всё в карточке.
    av_px: u32,
    /// Сколько корней в store и полностью ли влез список (иначе «37+»).
    roots: (u32, bool),
    /// Место переключателя раскладки в панели — оно известно только тому кадру, который его
    /// нарисовал, а подсветка нужна следующему.
    lang_at: Rect,
    /// Что решил последний кадр.
    go: Option<u8>,
    flip: bool,
    toggle: bool,
    power: bool,
    /// Кадр изменил то, ЧТО РИСУЕТСЯ, уже после того как посчитал подпись (сегодня это только
    /// подтверждение выключения). Такому изменению нужен ещё один кадр, и попросить его больше
    /// некому: события от композитора не будет — оно ничего в системе не меняло.
    again: bool,
    mo: Motion,
}

/// Индексы островов в [`Bar::isles`].
const I_SPACES: usize = 0;
const I_TITLE: usize = 1;
const I_CLOCK: usize = 2;
const I_SYS: usize = 3;
const I_CARD: usize = 4;

impl Bar {
    fn new(th: &Theme, line_h: i32, sw: i32, sh: i32, anim_ms: u64, gen_text: &str) -> Bar {
        // Имя устройства из конфига; без него — «VOID». Пустым его оставлять нельзя: шапка меню
        // без единого слова выглядит недорисованной.
        let named = ui::conf::device(gen_text, "name");
        let device = named.clone().unwrap_or_else(|| "VOID".to_string());
        let avatar = ui::conf::device(gen_text, "avatar");
        // Высота считается ОТ ШРИФТА и от темы: разъехаться им нельзя (см. `bar_wanted` в `wm`).
        let isle_h = line_h + 2 * th.px(5);
        let h = isle_h + 2 * th.px(6);
        Bar {
            h,
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
            isles: [Isle::default(); 5],
            fresh: true,
            open: false,
            grown: false,
            confirm: false,
            gen: ui::conf::generation_name().unwrap_or_else(|| "VOID".to_string()),
            device: device.clone(),
            named: named.is_some(),
            letter: device.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default(),
            avatar_root: avatar,
            avatar: None,
            avatar_tried: false,
            // Кружок в две строки высотой минус поле — тот же расчёт, что у шапки карточки.
            av_px: (2 * (line_h + th.px(6)) - 2 * th.px(2)).max(8) as u32,
            roots: (0, true),
            lang_at: Rect::ZERO,
            go: None,
            flip: false,
            toggle: false,
            power: false,
            again: false,
            mo: Motion::new(anim_ms),
        }
    }

    /// Забыть нарисованное: следующий кадр перерисует всё (смена размера поверхности).
    fn forget(&mut self) {
        self.isles = [Isle::default(); 5];
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
        self.open || self.mo.peek(A_MENU) > 0
    }

    /// Спросить композитор: стол, сколько столов, раскладка, обзор, заголовок окна в фокусе.
    fn fetch(&mut self, surf: &Window) {
        let mut buf = [0u8; win::TITLE_MAX];
        let Some(st) = surf.status(&mut buf) else { return };
        self.space = st.space;
        self.spaces = st.spaces.max(1);
        self.layout = st.layout;
        self.overview = st.overview;
        self.title = String::from(core::str::from_utf8(&buf[..st.title_len]).unwrap_or(""));
        // В обзоре мышь слоям не отдают вовсе — закрыть меню было бы нечем.
        if self.overview {
            self.open = false;
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
            self.open = false;
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
        if core::mem::take(&mut self.toggle) {
            self.open = !self.open;
            if self.open {
                // Сведения собираются в момент ОТКРЫТИЯ: держать их свежими постоянно значило бы
                // ходить в store каждую минуту ради того, на что никто не смотрит.
                self.roots = store_roots();
                self.confirm = false;
                self.load_avatar();
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
        let margin = th.px(6);
        let isle_h = self.h - 2 * margin;
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
        let l_isle = Rect::new(margin, margin, spaces_w, isle_h);

        // Кнопка меню — самая правая: это «начало» оболочки, и звать её надо там, где рука её
        // ищет. Надпись — имя поколения: система называет себя тем, чем она сейчас является.
        let sys_w = u.font.width(&self.gen) + 2 * th.pad;
        let s_isle = Rect::new(w - margin - sys_w, margin, sys_w, isle_h);

        let lang_w = u.font.width(lang) + th.px(12);
        let clock_w = u.font.width(&clock);
        let date_w = u.font.width(&date);
        let r_w = 2 * th.pad + lang_w + th.px(8) + th.line + th.px(8) + clock_w + th.px(6) + date_w;
        let r_isle = Rect::new(s_isle.x - margin - r_w, margin, r_w, isle_h);

        // Заголовок меняется в ДВА ТАКТА: старый гаснет, подменяется и загорается новый. Смена
        // текста на полной яркости читается как рывок — рядом с едущим окном это особенно заметно.
        if self.shown != self.title && (self.shown.is_empty() || self.mo.peek(A_TITLE) == 0) {
            self.shown = self.title.clone();
            self.mo.set(A_TITLE, 0);
        }
        let fading = !self.shown.is_empty() && self.shown != self.title;
        let title_a = self.mo.val(A_TITLE, if fading { 0 } else { 256 }).clamp(0, 256) as u32;

        // Заголовок посередине — тем местом, что осталось между островами. Пустой заголовок
        // острова не рождает: пустая карточка посреди панели выглядела бы поломкой.
        let room = r_isle.x - l_isle.right() - 2 * margin;
        let t_isle = if self.shown.is_empty() || room < th.px(60) {
            Rect::ZERO
        } else {
            let tw = (u.font.width(&self.shown) + 2 * th.pad).min(room);
            let x = ((w - tw) / 2).clamp(l_isle.right() + margin, r_isle.x - margin - tw);
            Rect::new(x, margin, tw, isle_h)
        };

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
        let lang_r = self.lang_at;
        let lang_hot = self.mo.val(A_LANG, if hot(lang_r) { 256 } else { 0 }) as u32;
        let sys_hot = self.mo.val(A_SYS, if hot(s_isle) { 256 } else { 0 }) as u32;

        // ── карточка меню ──────────────────────────────────────────────────────────────────
        let menu_t = self.mo.val(A_MENU, if self.open { 256 } else { 0 }).clamp(0, 256) as u32;
        let card = self.card_rect(th, u.font, menu_t);
        let card_clip = Rect::new(0, self.h - margin, w, h - (self.h - margin));
        // Место считается ДО опроса движения: `self.mo` берётся изменяемо, а прямоугольник —
        // из `self`, и в одном выражении эти два заимствования спорят.
        let pow_r = self.power_rect(th, &*u.font, card);
        let pow_hot = self.mo.val(A_POWER, if hot(pow_r) { 256 } else { 0 }) as u32;
        let uptime = uptime_text();

        // ── что перерисовывать ─────────────────────────────────────────────────────────────
        let want = [
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
            Isle {
                rect: r_isle,
                sig: sig(&[
                    self.layout as u64,
                    sig_str(&clock),
                    sig_str(&date),
                    lang_hot as u64,
                ]),
            },
            Isle { rect: s_isle, sig: sig(&[sig_str(&self.gen), sys_hot as u64]) },
            Isle {
                rect: if menu_t == 0 { Rect::ZERO } else { card },
                sig: sig(&[
                    menu_t as u64,
                    self.confirm as u64,
                    self.space as u64,
                    self.spaces as u64,
                    self.roots.0 as u64,
                    self.avatar.is_some() as u64,
                    sig_str(&self.device),
                    sig_str(&uptime),
                    sig_str(&clock),
                    pow_hot as u64,
                ]),
            },
        ];
        // Клик всегда рисует всё: он меняет и то, что под курсором, и то, что было активным, —
        // а «что именно» знает уже сам виджет, а не эта таблица.
        let all = click.is_some();
        let redraw: [bool; 5] = core::array::from_fn(|i| all || want[i] != self.isles[i]);
        if !redraw.iter().any(|&x| x) {
            return;
        }

        // Стереть старое место острова вместе с новым: остров, ставший уже, оставил бы за собой
        // кусок себя прежнего — на прозрачной поверхности это не «след», а мусор поверх обоев.
        // Карточка стирается не здесь, а под своим клипом: она одна умеет вылезать за панель.
        if core::mem::take(&mut self.fresh) {
            u.clear_all();
        } else {
            for i in I_SPACES..I_CARD {
                if redraw[i] {
                    u.clear(self.isles[i].rect.union(want[i].rect));
                }
            }
        }

        if redraw[I_SPACES] {
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

        if redraw[I_TITLE] && !t_isle.is_empty() {
            u.fade(title_a);
            let inner = u.island(t_isle);
            u.label(inner, &self.shown, th.text, Align::Center);
            u.fade(256);
        }

        if redraw[I_CLOCK] {
            let mut inner = u.island(r_isle);
            let lang_at = inner.cut_left(lang_w);
            if u.button(lang_at, lang, lang_hot) {
                self.flip = true;
            }
            self.lang_at = lang_at;
            inner.cut_left(th.px(8));
            u.sep(inner.cut_left(th.line).inset_xy(0, th.px(5)));
            inner.cut_left(th.px(8));
            u.label(inner.cut_left(clock_w), &clock, th.text, Align::Left);
            inner.cut_left(th.px(6));
            u.label(inner, &date, th.muted, Align::Left);
        }

        if redraw[I_SYS] {
            let inner = u.island(s_isle);
            // Нажимается ВЕСЬ остров, а не только надпись: целиться в четыре буквы, когда рядом
            // есть очевидная карточка, — это заставлять человека мериться с пикселями.
            let pressed = u.button(inner, &self.gen, sys_hot) | u.clicked(s_isle);
            if pressed {
                self.toggle = true;
            }
        }

        if redraw[I_CARD] {
            // Карточка режется краем панели: она выезжает ИЗ-ПОД неё, а не поверх её островов.
            u.clip(card_clip);
            u.clear(self.isles[I_CARD].rect.union(want[I_CARD].rect));
            if menu_t > 0 {
                u.fade(menu_t);
                self.draw_card(u, th, card, &uptime, &clock, pow_hot);
                u.fade(256);
            }
            u.clip(Rect::new(0, 0, w, h));
        }

        // Щелчок мимо всего — закрыть меню. Работает потому, что открытая поверхность накрывает
        // экран: клик по чужому окну приходит НАМ, а не ему. Пока меню закрыто, поверхность —
        // полоска панели, и мимо неё щёлкнуть нельзя вовсе.
        if self.open && !self.toggle {
            if let Some((cx, cy)) = click {
                let inside = card.contains(cx, cy)
                    || l_isle.contains(cx, cy)
                    || t_isle.contains(cx, cy)
                    || r_isle.contains(cx, cy)
                    || s_isle.contains(cx, cy);
                if !inside {
                    self.toggle = true;
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

    /// Высота строки сведений и высота плитки — считаются от шрифта, как и всё остальное.
    fn metrics(th: &Theme, font: &Font) -> (i32, i32, i32) {
        let row = font.line_h() + th.px(6);
        let sep = th.px(11);
        let tile = font.line_h() + th.px(12);
        (row, sep, tile)
    }

    /// Где сейчас карточка меню. `t` — насколько она проявилась (0..256): выезд это сдвиг вверх,
    /// гаснущий вместе с прозрачностью, а не «появилась целиком».
    fn card_rect(&self, th: &Theme, font: &mut Font, t: u32) -> Rect {
        let (row, sep, btn) = Self::metrics(th, &*font);
        let w = (font.width("корней в store") + font.width("00:00 · 00.00.0000") + th.gap
            + 2 * th.pad)
            .max(th.px(240));
        let h = 2 * th.pad + 2 * row + sep + 5 * row + sep + btn;
        let margin = th.px(6);
        // Выезд: подняться на палец и опуститься. Больший ход читается как «упало сверху».
        let lift = th.px(18) * (256 - t as i32) / 256;
        Rect::new(self.sw - margin - w, self.h - lift, w, h)
    }

    /// Место круглой кнопки выключения. Считается ТЕМ ЖЕ кодом, что и рисование
    /// ([`Bar::draw_card`] режет тот же прямоугольник теми же кусками): два расчёта «где что» —
    /// это два случая разойтись, на которых система уже стояла (обзор, Веха 123).
    fn power_rect(&self, th: &Theme, font: &Font, card: Rect) -> Rect {
        if card.is_empty() || !self.open {
            return Rect::ZERO;
        }
        let (row, sep, btn) = Self::metrics(th, font);
        let mut c = card.inset_xy(th.pad, th.pad);
        c.cut_top(2 * row + sep + 5 * row + sep);
        let foot = c.cut_top(btn);
        Rect::new(foot.right() - btn, foot.y, btn, btn)
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
    /// конфига (`device("name", …)`, `device("avatar", …)`), а под ними сборка системы.
    fn draw_card(
        &mut self,
        u: &mut Ui,
        th: &Theme,
        card: Rect,
        uptime: &str,
        clock: &str,
        pow_hot: u32,
    ) {
        let (row, sep, btn) = Self::metrics(th, &*u.font);
        u.card(card);
        let mut c = card.inset_xy(th.pad, th.pad);

        // ── шапка: кто эта машина ──────────────────────────────────────────────────────────
        let mut head = c.cut_top(2 * row);
        let av = head.cut_left(2 * row);
        let img = self.avatar.as_ref().map(|a| (&a.px[..], a.w as i32, a.h as i32));
        u.avatar(av.inset(th.px(2)), img, &self.letter);
        head.cut_left(th.gap);
        let name = Rect::new(head.x, head.y, head.w, row);
        let sub = Rect::new(head.x, head.y + row, head.w, row);
        u.label(name, &self.device, th.text, Align::Left);
        let sub_text =
            if self.named { alloc::format!("VOID {BUILD}") } else { BUILD.to_string() };
        u.label(sub, &sub_text, th.muted, Align::Left);
        u.hsep(c.cut_top(sep));

        // ── сведения ───────────────────────────────────────────────────────────────────────
        let (year, mo, d, _, _, _) = sys::civil_from_unix(sys::time_ns() / 1_000_000_000);
        u.row(c.cut_top(row), "поколение", &self.gen);
        u.row(c.cut_top(row), "работает", uptime);
        u.row(c.cut_top(row), "время", &alloc::format!("{clock} · {d:02}.{mo:02}.{year}"));
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
        u.hsep(c.cut_top(sep));

        // ── выключение ─────────────────────────────────────────────────────────────────────
        let mut foot = c.cut_top(btn);
        let round = foot.cut_right(btn);
        foot.cut_right(th.gap); // подпись не должна упираться в кнопку
        if self.confirm {
            // Подпись только на втором шаге: у кнопки со знаком её быть не должно, а у кнопки,
            // которая сейчас выключит машину, — обязана.
            u.label(foot, "нажми ещё раз", th.danger, Align::Right);
        }
        if u.power_button(round, pow_hot, self.confirm) {
            if self.confirm {
                self.power = true;
            } else {
                self.confirm = true;
            }
        }
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
