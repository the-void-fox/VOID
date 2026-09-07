//! `taskmgr` — ДИСПЕТЧЕР ЗАДАЧ (Веха 153.5): лицо capability-модели к пользователю.
//!
//! В Windows/Linux «доверенный» — признак, приклеенный к процессу (подпись, путь, uid), его
//! можно подделать. У VOID доверия как признака нет: есть только то, что процесс реально держит
//! в своей таблице прав. Диспетчер не спрашивает программу, кто она, — он читает у ядра, ЧТО она
//! может ([[task-manager]]).
//!
//! - **вкладки приложения/службы** — признак не флаг, а ПРОИСХОЖДЕНИЕ: служба = поднято init'ом
//!   из конфига поколения. Открыл «приложения» — там ровно то, что запустил ты.
//! - **имя не удостоверение** — программа запускается из store ПО CONTENT-ID; диспетчер показывает
//!   хэш того, что исполняется, а не имя (замаскироваться нечем: другой код → другой хэш).
//! - **права ГРАФОМ, а не списком** — процесс без права на сеть всё равно может ПОПРОСИТЬ того, у
//!   кого оно есть (confused deputy). Честная картинка — «эндпоинт к тому, у кого есть» (`→P<n>`).
//! - **что делает сейчас** — весь ввод-вывод идёт через IPC, значит его можно просто СЧИТАТЬ.
//! - **скальпель, а не топор** — «отнять» отбирает у процесса ОДНО право на ходу (`cap::revoke`
//!   наружу). А вот ОБЩИЙ выключатель сети правами не орудует вовсе, и это решение, а не
//!   упрощение: отзыв необратим по устройству (поколение слота), значит тумблер на нём честно
//!   выключал бы и обманывал при включении. Выключатель живёт в сервере сети (Веха 166).
//!
//! Список процессов сам под правом (`sysview`): у обычной программы его нет, поэтому она даже не
//! узнает, что рядом работает. Диспетчер ищет своё право по ВИДУ (13), не по позиции.
//!
//! ## Веха 164 — по макету
//!
//! Три вкладки вместо двух: **производительность**, **приложения**, **службы**. Процессы —
//! ТАБЛИЦЕЙ (имя, PID, доля процессора, куча, состояние), под ней права выбранного процесса и
//! пояснение к выбранному праву: список видов прав ничего не говорит человеку, который видит
//! `sysview [rw]` впервые.
//!
//! Вкладка производительности показывает устройства ПЛИТКАМИ. Живых из них две — процессор и
//! память (Вехи 159 и 163); у остальных вместо числа сказано, ЧЕГО не хватает, и это разные
//! вещи: у диска и сети драйверы есть (система с диска грузится, сеть получает адрес), но
//! счётчиков ввода-вывода ядро не ведёт; видеокарты и датчиков нет вовсе. «Нет драйверов» и
//! «драйвер есть, счётчиков нет» — не одно и то же, и путать их значило бы врать в обе стороны.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use void_user as sys;
use void_user::win::{sym, Event, Window};

#[allow(dead_code)]
#[path = "../ui/mod.rs"]
mod ui;

use ui::{Align, Font, Rect, Rgba, Theme, Ui};

#[global_allocator]
static ALLOC: sys::heap::Heap<{ 4 * 1024 * 1024 }> = sys::heap::Heap::new();

const REC: usize = sys::PROC_REC; // 64 — запись процесса
const CAP_REC: usize = sys::PROC_CAP_REC; // 12 — запись права

/// Сколько замеров загрузки помнит график. При обновлении дважды в секунду это минута.
const HIST: usize = 120;

/// Веха 168.2 — имя движущейся величины у тумблера сети (см. [`ui::Ui::anim`]). Величина одна,
/// и константа тоже одна: числа, названные прямо в вызове, — это первый шаг к тому, чтобы два
/// виджета поехали под одним именем.
const A_NET: u32 = 1;

fn say(s: &str) {
    sys::write_console(s.as_bytes());
}

/// Вкладка.
#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Perf,
    Apps,
    Services,
}

impl Tab {
    fn name(self) -> &'static str {
        match self {
            Tab::Perf => "производительность",
            Tab::Apps => "приложения",
            Tab::Services => "службы",
        }
    }
}

/// Один процесс (снимок для одного кадра).
struct Proc {
    pid: u16,
    ppid: u16,
    system: bool,
    linux: bool,
    has_hash: bool,
    state: u8,
    hash: [u8; 32],
    name: String,
}

/// Одно право процесса (ребро графа, если endpoint/reply).
struct Capp {
    slot: u16,
    kind: u8,
    rights: u32,
    aux: u16,
}

/// Устройство на вкладке производительности. `live` — есть ли под ним источник; если нет, вместо
/// числа стоит причина, и она РАЗНАЯ у «драйвера нет» и «драйвер есть, счётчиков нет».
struct Dev {
    name: &'static str,
    why: &'static str,
}

/// Плитки в том порядке, в каком они стоят в макете. Первые две живые, у остальных источника
/// нет — и сказано, какого именно.
const DEVS: [Dev; 6] = [
    Dev { name: "процессор", why: "" },
    Dev { name: "память", why: "" },
    Dev { name: "диск", why: "счётчиков нет" },
    Dev { name: "сеть", why: "счётчиков нет" },
    Dev { name: "видео", why: "нет драйверов" },
    Dev { name: "датчики", why: "нет драйверов" },
];

#[derive(Default)]
struct Lay {
    tabs: Rect,
    /// Таблица процессов и её шапка (вкладки «приложения»/«службы»).
    head: Rect,
    list: Rect,
    bar: Rect,
    /// Права выбранного процесса и пояснение к выбранному праву.
    caps: Rect,
    capinfo: Rect,
    /// Плитки устройств, подробности выбранного и график (вкладка производительности).
    tiles: Rect,
    detail: Rect,
    graph: Rect,
    foot_msg: Rect,
    foot_btn: Rect,
    row_h: i32,
    rows: usize,
}

struct App {
    w: i32,
    h: i32,
    sysview: usize,
    /// Право включает WRITE — можно отзывать (иначе диспетчер только смотрит).
    can_write: bool,
    procs: Vec<Proc>,
    /// Счётчики каждого процесса и его доля процессора — параллельно [`App::procs`].
    stats: Vec<Option<sys::ProcStat>>,
    cpu: Vec<u32>,
    /// Прошлый замер: (pid, накопленное время) и когда сняли. Доля — разность двух замеров, и
    /// одного не хватает никогда ([[task-manager]], Веха 163).
    prev: Vec<(u16, u64)>,
    prev_at: u64,
    ls: ui::List,
    tab: Tab,
    caps: Vec<Capp>,
    /// Выбранное ПРАВО в списке прав — под пояснение справа.
    cap_sel: usize,
    /// Для какого pid прочитаны права (чтобы не читать каждый кадр).
    detail_pid: Option<u16>,
    /// Числа про машину целиком и прошлый их замер — для загрузки и графика.
    info: Option<sys::SysInfo>,
    info_prev: Option<sys::SysInfo>,
    /// Веха 165 — история У КАЖДОГО устройства, а не одна на всех: в макете окошко графика стоит
    /// рядом с каждой плиткой, и внизу — увеличенная версия ТОЙ ЖЕ кривой. Пустая история —
    /// не «ноль», а «источника нет»: окошко у диска и видео так и остаётся пустым, и это ровно
    /// то, что про них правда ([[void-taskmgr]]).
    hist: Vec<Vec<u8>>,
    /// Выбранная плитка устройства.
    dev: usize,
    lay: Lay,
    /// pid сервера сети — для подписи рёбер и для выключателя.
    net_srv_pid: Option<u16>,
    /// Веха 166 — служит ли сеть (ответ сервера). `None` — спросить не у кого. Спрашивается на
    /// каждом перечитывании, а не помнится: выключить сеть могли и не отсюда.
    net: Option<bool>,
    /// Канал к серверу сети и «искали уже»: просить его у композитора надо ОДИН раз, а не
    /// дважды в секунду вместе с перечитыванием списка.
    net_ep: usize,
    net_tried: bool,
    /// Ответ на последнее действие (отзыв/рубильник) — в подвале.
    flash: Option<String>,
}

impl App {
    /// Прочитать снимок всех живых процессов.
    fn read_procs(&self) -> Vec<Proc> {
        let mut out = Vec::new();
        let mut buf = vec![0u8; REC * 64];
        let Some(total) = sys::proc_list(self.sysview, &mut buf) else { return out };
        let shown = total.min(buf.len() / REC);
        for k in 0..shown {
            let r = &buf[k * REC..k * REC + REC];
            let flags = u16::from_le_bytes([r[4], r[5]]);
            let nlen = (r[7] as usize).min(24);
            let mut hash = [0u8; 32];
            hash.copy_from_slice(&r[8..40]);
            out.push(Proc {
                pid: u16::from_le_bytes([r[0], r[1]]),
                ppid: u16::from_le_bytes([r[2], r[3]]),
                system: flags & 0x01 != 0,
                linux: flags & 0x02 != 0,
                has_hash: flags & 0x04 != 0,
                state: r[6],
                hash,
                name: String::from_utf8_lossy(&r[40..40 + nlen]).into_owned(),
            });
        }
        out
    }

    /// pid текущего выбранного процесса (в отфильтрованном списке).
    fn current_pid(&self) -> Option<u16> {
        self.ls.current().map(|i| self.procs[i].pid)
    }

    /// Показывать ли на этой вкладке системные процессы.
    fn want_system(&self) -> bool {
        self.tab == Tab::Services
    }

    /// Пересобрать список под текущую вкладку, сохранив выбор по pid.
    fn refilter(&mut self, keep: Option<u16>) {
        let want = self.want_system();
        self.ls.hits = (0..self.procs.len()).filter(|&i| self.procs[i].system == want).collect();
        self.ls.refiltered();
        if let Some(pid) = keep {
            if let Some(pos) = self.ls.hits.iter().position(|&i| self.procs[i].pid == pid) {
                self.ls.sel = pos;
                self.ls.scroll_to_sel();
            }
        }
        self.detail_pid = None;
    }

    /// Перечитать процессы и их счётчики (по таймеру), сохранив вкладку и выбор.
    ///
    /// Счётчики снимаются У ВСЕХ, а не только у выбранного: доля процессора стоит в КОЛОНКЕ,
    /// и посчитать её одному значило бы оставить таблицу с одной живой строкой.
    fn reload(&mut self) {
        let keep = self.current_pid();
        self.procs = self.read_procs();
        self.net_srv_pid = self.procs.iter().find(|p| p.name == "net-srv").map(|p| p.pid);
        if !self.net_tried && self.net_srv_pid.is_some() {
            self.net_tried = true;
            self.resolve_net_ep();
        }
        self.net = self.net_on();

        let now = sys::monotonic_ns();
        let dt = now.saturating_sub(self.prev_at);
        let mut snap = Vec::with_capacity(self.procs.len());
        self.stats.clear();
        self.cpu.clear();
        for p in &self.procs {
            let st = sys::proc_stat(self.sysview, p.pid as usize);
            let was = self.prev.iter().find(|(pid, _)| *pid == p.pid).map(|(_, ns)| *ns);
            // Доля есть только у того, кого мы уже видели: у процесса, родившегося между
            // замерами, «прошлого» нет вовсе, и придумывать ему ноль нельзя.
            let pc = match (&st, was) {
                (Some(s), Some(old)) if dt > 0 => {
                    ((s.run_ns.saturating_sub(old).saturating_mul(100) / dt) as u32).min(100)
                }
                _ => 0,
            };
            snap.push((p.pid, st.as_ref().map_or(0, |s| s.run_ns)));
            self.stats.push(st);
            self.cpu.push(pc);
        }
        self.prev = snap;
        self.prev_at = now;

        // Числа про машину целиком — и точка на графике.
        self.info_prev = self.info.take();
        self.info = sys::sysinfo(self.sysview);
        // Замер идёт в историю ТОГО устройства, к которому относится. Загрузка процессора —
        // разность двух замеров (первого одного не хватает никогда), занятая память — число
        // само по себе.
        if let (Some(n), Some(p)) = (&self.info, &self.info_prev) {
            self.push_hist(0, n.cpu_percent(p).min(100) as u8);
        }
        if let Some(n) = &self.info {
            self.push_hist(1, n.ram_percent().min(100) as u8);
        }
        self.refilter(keep);
    }

    /// Добавить замер в историю устройства `k`, подвинув окно на минуту.
    fn push_hist(&mut self, k: usize, v: u8) {
        let Some(h) = self.hist.get_mut(k) else { return };
        if h.len() == HIST {
            h.remove(0);
        }
        h.push(v);
    }

    /// Прочитать права выбранного процесса, если ещё не прочитаны для его pid.
    fn sync_detail(&mut self) {
        let Some(pid) = self.current_pid() else {
            self.caps.clear();
            self.detail_pid = None;
            return;
        };
        if self.detail_pid == Some(pid) {
            return;
        }
        self.detail_pid = Some(pid);
        self.caps = read_caps(self.sysview, pid);
        self.cap_sel = self.cap_sel.min(self.caps.len().saturating_sub(1));
    }

    /// Разрезать окно на места виджетов.
    fn measure(&self, font: &Font, th: &Theme) -> Lay {
        let font_h = font.line_h();
        let row_h = font_h + th.px(8);
        let mut all = Rect::new(0, 0, self.w, self.h).inset(th.pad);
        let tabs = all.cut_top(font_h + th.px(12));
        all.cut_top(th.gap);
        let mut lay = Lay { tabs, row_h, ..Lay::default() };
        if self.tab == Tab::Perf {
            // Веха 165 — высота полотна считается ТЕМ ЖЕ кодом, что и места плиток
            // ([`App::tile_rect`]). До этой вехи их было две штуки: полотно резалось по одной
            // формуле, плитки расставлялись по другой, — и нижний ряд вылезал за край на
            // половину зазора. Разъехаться двум расчётам «где что» тут было ровно нечему
            // помешать: оба выглядели правильными по отдельности.
            lay.tiles = all.cut_top(Self::tiles_h(th, font_h));
            all.cut_top(th.gap);
            // Подробности лежат ПРЯМО НА ФОНЕ, без карточки: в макете под ними нет подложки —
            // между полотном плиток и полем графика идёт голый фон окна.
            lay.detail = all.cut_top(Self::detail_h(th, font_h));
            all.cut_top(th.gap);
            lay.graph = all;
            return lay;
        }
        let mut foot = all.cut_bottom(font_h + th.px(10));
        lay.foot_btn = foot.cut_right(th.px(200));
        foot.cut_right(th.gap);
        lay.foot_msg = foot;
        all.cut_bottom(th.gap);
        // Таблица сверху, права под ней — как в макете (там таблица занимает верхние две пятых).
        let mut table = all.cut_top((all.h * 2 / 5).max(row_h * 3 + th.pad * 2));
        all.cut_top(th.gap);
        let inner = table.inset(th.pad);
        lay.head = Rect::new(inner.x, inner.y, inner.w, row_h);
        let mut body = Rect::new(inner.x, inner.y + row_h + th.px(4), inner.w, 0);
        body.h = table.bottom() - th.pad - body.y;
        lay.bar = body;
        lay.list = lay.bar.cut_left(lay.bar.w - th.px(6));
        table.h = 0; // сам прямоугольник карточки восстановим при рисовании
        lay.rows = (lay.list.h / row_h).max(1) as usize;
        // Права: список слева, пояснение справа (в макете — ровно так). Пополам, а не 5/9:
        // пояснение — это ТЕКСТ, и узкая колонка режет его многоточием на первом же слове.
        let mut caps = all;
        lay.caps = caps.cut_left((caps.w - th.gap) / 2);
        caps.cut_left(th.gap);
        lay.capinfo = caps;
        lay
    }

    /// Сколько плиток в ряду. Три — как в макете; при более узком окне сетка не ужимается в две,
    /// потому что ширину окна назначает лента композитора, а не человек, и «иногда две, иногда
    /// три» читалось бы как сбой раскладки.
    const TILE_COLS: usize = 3;

    /// Высота одной плитки: имя, подпись и значение — три строки текста плюс поля.
    fn tile_h(th: &Theme, font_h: i32) -> i32 {
        3 * font_h + th.px(10)
    }

    /// Высота полотна плиток целиком: поля по краям, ряды и зазоры между ними.
    fn tiles_h(th: &Theme, font_h: i32) -> i32 {
        let rows = DEVS.len().div_ceil(Self::TILE_COLS) as i32;
        2 * th.px(6) + rows * Self::tile_h(th, font_h) + (rows - 1) * th.px(9)
    }

    /// Высота блока подробностей: строка-заголовок и сетка пар в две колонки.
    fn detail_h(th: &Theme, font_h: i32) -> i32 {
        (font_h + th.px(6)) + 4 * (font_h + th.px(3))
    }

    /// Место плитки `k` внутри полотна. ЕДИНСТВЕННЫЙ расчёт: им же меряется высота полотна.
    fn tile_rect(th: &Theme, tiles: Rect, font_h: i32, k: usize) -> Rect {
        let (pad, gap) = (th.px(6), th.px(9));
        let inner = tiles.inset(pad);
        let cols = Self::TILE_COLS as i32;
        let cw = (inner.w - (cols - 1) * gap) / cols;
        let ch = Self::tile_h(th, font_h);
        let (cx, cy) = ((k % Self::TILE_COLS) as i32, (k / Self::TILE_COLS) as i32);
        Rect::new(inner.x + cx * (cw + gap), inner.y + cy * (ch + gap), cw, ch)
    }

    /// Прямоугольник карточки таблицы — по шапке и списку (карточка рисуется вокруг них).
    fn table_card(&self, th: &Theme, lay: &Lay) -> Rect {
        let top = lay.head.y - th.pad;
        let bot = lay.list.bottom() + th.pad;
        Rect::new(lay.head.x - th.pad, top, lay.head.w + 2 * th.pad, bot - top)
    }

    /// Отозвать право у процесса `pid` в слоте `slot` и обновить подробности.
    fn revoke(&mut self, pid: u16, slot: u16) {
        if sys::proc_revoke(self.sysview, pid as usize, slot as usize) {
            self.flash = Some(alloc::format!("отнято: P{} слот {}", pid, slot));
        } else {
            self.flash = Some(String::from("отзыв не удался"));
        }
        self.detail_pid = None; // перечитать граф на следующем проходе
    }

    /// Свой канал к СЕРВЕРУ СЕТИ — один раз за жизнь окна.
    ///
    /// Ищется по АДРЕСАТУ (`cap_info_ex`), а не по виду: вид у всех эндпоинтов один (4), и
    /// «первый попавшийся вид 4» — это файловый сервер. Своего нет — просим у композитора, тем
    /// же порядком, что и право обзора (Веха 155): он отдаёт названному в конфиге
    /// (`desktop net taskmgr`), а имя просителя спрашивает у ядра.
    fn resolve_net_ep(&mut self) {
        let Some(net) = self.net_srv_pid else { return };
        let mine = (0..32)
            .map(sys::start_cap)
            .take_while(|&c| c != sys::NO_CAP)
            .find(|&c| matches!(sys::cap_info_ex(c), Some((4, _, aux)) if aux == net));
        self.net_ep = match mine {
            Some(c) => c,
            None => match sys::win::grant(4) {
                c if matches!(sys::cap_info_ex(c), Some((4, _, aux)) if aux == net) => c,
                _ => sys::NO_CAP,
            },
        };
        if self.net_ep == sys::NO_CAP {
            say("taskmgr: канала к серверу сети нет — выключателя не будет\n");
            say("taskmgr: выдаётся строкой `desktop net taskmgr` в конфиге поколения\n");
        }
    }

    /// Веха 166 — **служит ли сеть сейчас**. Спрашивается у СЕРВЕРА, а не помнится флагом:
    /// диспетчер могли закрыть и открыть заново, выключить сеть могли и не из него.
    /// `None` — спросить не у кого (нет канала или сервер молчит).
    fn net_on(&self) -> Option<bool> {
        let ep = (self.net_ep != sys::NO_CAP).then_some(self.net_ep)?;
        let mut rep = [0u8; 1];
        let n = sys::call(ep, sys::net_cli::OP_NET_SWITCH, &[], &mut rep);
        // `usize::MAX` — вызов НЕ СОСТОЯЛСЯ, и это не «ответ длиной MAX». Проверять одним
        // `n >= 1` нельзя: MAX проходит его насквозь, и мёртвый канал читался бы как «сеть
        // работает» (так и было — тумблер горел, а сервер запроса не видел).
        (n != usize::MAX && n >= 1).then(|| rep[0] == sys::net_cli::ST_OK)
    }

    /// Веха 166 — щёлкнуть выключателем сети.
    ///
    /// ## Почему выключатель НЕ отбирает права
    ///
    /// Первым делом это было сделано отзывом — как «рубильник» до него: снять сетевые права у
    /// всех пользовательских процессов. Выключается так прекрасно, а вот включить обратно
    /// нельзя, и не из-за нехватки сискола: **отзыв необратим по устройству**. Он бумкает
    /// поколение слота, и дескриптор, который программа держит в переменной, мёртв навсегда.
    /// Вручить право заново ядро научить можно (пробовал), но лечь оно обязано в НОВЫЙ слот, а
    /// сказать программе его номер нечем: стартовые права — это снимок, сделанный при запуске.
    /// Тумблер на отзыве честно выключал бы и обманывал при включении — а тумблер, который
    /// обманывает в одну сторону, хуже кнопки, которая честно делает одно ([[void-cap-inherit]]).
    ///
    /// Поэтому выключатель живёт В СЕРВЕРЕ СЕТИ: права никто не трогает, сервер просто перестаёт
    /// обслуживать — и начинает снова. Тем же процессам, без перезапуска.
    ///
    /// Отзыв при этом никуда не делся и остаётся правильным инструментом для ДРУГОГО вопроса:
    /// «отобрать сеть вот у этого одного» — знаком у каждого права выбранного процесса.
    fn switch_net(&mut self, on: bool) {
        let Some(ep) = (self.net_ep != sys::NO_CAP).then_some(self.net_ep) else {
            self.flash = Some(String::from("сети нет: у диспетчера нет канала к серверу"));
            return;
        };
        let mut rep = [0u8; 1];
        let n = sys::call(ep, sys::net_cli::OP_NET_SWITCH, &[on as u8], &mut rep);
        self.flash = Some(if n == 0 || n == usize::MAX {
            String::from("сервер сети не ответил")
        } else if on {
            String::from("сеть включена: сервер снова обслуживает")
        } else {
            String::from("сеть выключена: сервер не обслуживает никого")
        });
    }

    /// Нарисовать кадр. `true` — состояние изменилось прямо в кадре (клик), нужен ещё проход.
    fn paint(&mut self, u: &mut Ui, th: &Theme, lay: &Lay) -> bool {
        // Веха 164 — фон окна ТЕМНЕЕ карточек, как в макете. Раньше и фон, и карточки были одним
        // цветом (`bg`), и карточка была видна только по тому, что на ней написано: сетка плиток,
        // таблица и график сливались в один лист.
        u.background(th.band);
        // Веха 155 — права нет: сказать это ВСЛУХ. Пустые вкладки на месте списка процессов —
        // худший из возможных ответов: они выглядят как «ничего не работает», хотя означают
        // «мне не дано смотреть». Разница здесь принципиальная: у VOID отсутствие права — не
        // сбой, а нормальное состояние, и объяснить его должен тот, кто в него упёрся.
        if self.sysview == sys::NO_CAP {
            self.paint_tabs(u, th, lay);
            self.paint_denied(u, th, lay);
            return false;
        }
        let mut dirty = self.paint_tabs(u, th, lay);
        if self.tab == Tab::Perf {
            dirty |= self.paint_perf(u, th, lay);
            return dirty;
        }
        dirty |= self.paint_table(u, th, lay);
        dirty |= self.paint_caps(u, th, lay);

        // ── подвал: сообщение + рубильник ──────────────────────────────────────────────────
        let msg = match &self.flash {
            Some(m) => m.clone(),
            // Про диск и видео сказано на вкладке производительности, у самих плиток: строка
            // подвала — не место для оговорок, она обрежется первой.
            None => alloc::format!(
                "{}: {} · sysview: {}",
                if self.want_system() { "служб" } else { "приложений" },
                self.ls.hits.len(),
                if self.can_write { "чтение+управление" } else { "только чтение" }
            ),
        };
        let mcol = if self.flash.is_some() { th.accent } else { th.muted };
        u.label(lay.foot_msg, &msg, mcol, Align::Left);
        if self.can_write {
            // Веха 166 — ТУМБЛЕР вместо кнопки (просьба владельца): у сети два положения, и
            // выключатель без «обратно» — это не выключатель, а порча.
            let hot = if u.hot(lay.foot_btn) { 256 } else { 0 };
            match self.net {
                Some(on) => {
                    let label = if on { "сеть работает" } else { "сеть выключена" };
                    if u.toggle(lay.foot_btn, label, on, hot, A_NET) {
                        self.switch_net(!on);
                        self.net = self.net_on();
                        dirty = true;
                    }
                }
                // Сервера сети нет вовсе (или канала к нему): тумблер не рисуем. Выключатель,
                // за которым ничего, — та же ложь, что пустой ползунок громкости в меню.
                None => {
                    u.label(lay.foot_btn, "сети нет", th.muted, Align::Right);
                }
            }
        }
        dirty
    }

    /// Ряд вкладок — по центру, как в макете.
    fn paint_tabs(&mut self, u: &mut Ui, th: &Theme, lay: &Lay) -> bool {
        let mut dirty = false;
        let tabs = [Tab::Perf, Tab::Apps, Tab::Services];
        // Ширина — по самой длинной подписи, чтобы пилюли были одинаковы и не прыгали.
        let w = tabs.iter().map(|t| u.font.width(t.name())).max().unwrap_or(0) + 2 * th.pad;
        let total = 3 * w + 2 * th.gap;
        let mut x = lay.tabs.x + (lay.tabs.w - total) / 2;
        for t in tabs {
            let r = Rect::new(x, lay.tabs.y, w, lay.tabs.h);
            x += w + th.gap;
            let on = if self.tab == t { 256 } else { 0 };
            let hot = if u.hot(r) { 256 } else { 0 };
            if u.tile(r, t.name(), hot, on) && self.tab != t {
                self.tab = t;
                let keep = self.current_pid();
                self.refilter(keep);
                dirty = true;
            }
        }
        dirty
    }

    /// Таблица процессов: шапка колонок и строки.
    fn paint_table(&mut self, u: &mut Ui, th: &Theme, lay: &Lay) -> bool {
        u.card(self.table_card(th, lay));
        let cols = Cols::new(th, lay.head);
        // ── шапка колонок ──────────────────────────────────────────────────────────────────
        let h = lay.head;
        cols.label(u, h, "имя", "PID", "ЦП", "куча", "состояние", th.muted, th.muted);
        u.hsep(Rect::new(h.x, h.bottom(), h.w, th.px(2)));

        if let Some(t) = u.scrollbar(
            lay.bar.inset_xy(th.px(1), th.px(2)),
            self.ls.top,
            lay.rows,
            self.ls.hits.len(),
            u.held(),
        ) {
            self.ls.top = t;
        }
        let was = self.ls.sel;
        let mut list = lay.list;
        for k in 0..lay.rows {
            let rr = list.cut_top(lay.row_h);
            let Some(&i) = self.ls.hits.get(self.ls.top + k) else { continue };
            let sel = self.ls.top + k == self.ls.sel;
            let hot = u.hot(rr);
            if sel || hot {
                let bg = if sel {
                    th.accent
                } else {
                    th.text.with_a(0x14)
                };
                let c = u.tint(bg);
                u.c.rrect(rr, th.radius.min(rr.h / 2), c);
            }
            let (name, pid, state, cpu, heap) = {
                let p = &self.procs[i];
                let heap = self.stats[i].as_ref().map_or(0, |s| s.heap_bytes() / 1024);
                (p.name.clone(), p.pid, state_name(p.state), self.cpu[i], heap)
            };
            let (fg, dim) = if sel { (th.on_accent, th.on_accent) } else { (th.text, th.muted) };
            cols.label(
                u,
                rr,
                &name,
                &alloc::format!("{pid}"),
                &alloc::format!("{cpu} %"),
                &alloc::format!("{heap} КиБ"),
                state,
                fg,
                dim,
            );
            if u.clicked(rr) {
                self.ls.sel = self.ls.top + k;
            }
        }
        self.ls.sel != was
    }

    /// Права выбранного процесса (слева) и пояснение к выбранному праву (справа).
    fn paint_caps(&mut self, u: &mut Ui, th: &Theme, lay: &Lay) -> bool {
        let font_h = u.font.line_h();
        let inner = u.card(lay.caps);
        let mut d = inner.inset(th.pad);
        let Some(i) = self.ls.current() else {
            u.label(d.cut_top(font_h), "процесс не выбран", th.muted, Align::Left);
            u.card(lay.capinfo);
            return false;
        };
        let (pid, name, system, linux, has_hash, hash) = {
            let p = &self.procs[i];
            (p.pid, p.name.clone(), p.system, p.linux, p.has_hash, p.hash)
        };
        u.label(
            d.cut_top(font_h + th.px(2)),
            &alloc::format!("права процесса {name} (P{pid})"),
            th.text,
            Align::Left,
        );
        u.hsep(d.cut_top(th.px(4)));
        d.cut_top(th.px(2));

        let mut acted: Option<u16> = None; // слот, у которого нажали «отнять»
        let mut picked: Option<usize> = None;
        // Веха 165 — по макету отзыв это ЗНАК, а не слово: у строки права своя ширина в три
        // десятка знаков, и подпись «отнять» рядом занимала треть её.
        let btn_w = font_h + th.px(4);
        let row_h = font_h + th.px(6);
        for k in 0..self.caps.len() {
            if d.h < row_h {
                break;
            }
            let (slot, kind, rights, aux) = {
                let c = &self.caps[k];
                (c.slot, c.kind, c.rights, c.aux)
            };
            let mut rr = d.cut_top(row_h);
            if u.clicked(rr) {
                picked = Some(k);
            }
            if k == self.cap_sel {
                let c = u.tint(th.text.with_a(0x18));
                u.c.rrect(rr, th.radius.min(rr.h / 2), c);
            }
            rr = rr.inset_xy(th.px(4), 0);
            let btn = if self.can_write { rr.cut_right(btn_w) } else { Rect::default() };
            if self.can_write {
                rr.cut_right(th.gap);
            }
            // Строка КОРОТКАЯ: имя процесса-цели живёт в пояснении справа, а здесь оно только
            // отъедало место у кнопки и обрывалось многоточием на самом номере.
            let mut txt = String::new();
            txt.push_str(kind_name(kind));
            txt.push_str(" [");
            txt.push_str(&rights_str(rights));
            txt.push(']');
            if aux != 0xFFFF {
                txt.push_str(&alloc::format!(" →P{aux}"));
            }
            u.label(rr, &txt, th.text, Align::Left);
            if self.can_write {
                let hot = if u.hot(btn) { 256 } else { 0 };
                if u.icon_button(btn, ui::icon::REVOKE, hot, true) {
                    acted = Some(slot);
                }
            }
        }

        // ── справа: что это право значит и чей это процесс ─────────────────────────────────
        let inner = u.card(lay.capinfo);
        let mut d = inner.inset(th.pad);
        match self.caps.get(self.cap_sel) {
            Some(c) => {
                let (kind, rights, aux, slot) = (c.kind, c.rights, c.aux, c.slot);
                u.label(d.cut_top(font_h + th.px(2)), kind_name(kind), th.text, Align::Left);
                u.hsep(d.cut_top(th.px(4)));
                d.cut_top(th.px(2));
                // Пояснение — не украшение: `sysview [rw]` человеку, впервые открывшему
                // диспетчер, не говорит ничего, а именно из таких строк и состоит система.
                for line in kind_help(kind) {
                    if d.h < font_h {
                        break;
                    }
                    u.label(d.cut_top(font_h + th.px(1)), line, th.muted, Align::Left);
                }
                d.cut_top(th.px(4));
                u.row(d.cut_top(font_h + th.px(2)), "слот", &alloc::format!("{slot}"));
                u.row(d.cut_top(font_h + th.px(2)), "права", &rights_full(rights));
                if aux != 0xFFFF {
                    let who = match self.procs.iter().find(|p| p.pid == aux) {
                        Some(p) => alloc::format!("P{} ({})", aux, p.name),
                        None => alloc::format!("P{}", aux),
                    };
                    u.row(d.cut_top(font_h + th.px(2)), "цель", &who);
                }
            }
            None => {
                u.label(d.cut_top(font_h), "прав нет", th.muted, Align::Left);
                u.label(
                    d.cut_top(font_h + th.px(2)),
                    "процесс не может ничего вне себя",
                    th.muted,
                    Align::Left,
                );
            }
        }
        d.cut_top(th.px(4));
        u.hsep(d.cut_top(th.px(4)));
        d.cut_top(th.px(2));
        u.row(
            d.cut_top(font_h + th.px(2)),
            "происхождение",
            if system { "служба (init)" } else { "приложение" },
        );
        if has_hash {
            // По шестнадцать знаков в строке: колонка узкая, а хэш — то самое, ради чего
            // диспетчер и не верит именам; обрезать его многоточием нельзя.
            let hex = hex64(&hash);
            u.label(d.cut_top(font_h), "content-id образа", th.muted, Align::Left);
            for k in 0..4 {
                if d.h < font_h {
                    break;
                }
                u.label(d.cut_top(font_h), &hex[k * 16..k * 16 + 16], th.text, Align::Left);
            }
        } else {
            u.label(
                d.cut_top(font_h),
                if linux { "образ из пакета Linux" } else { "образ без хэша" },
                th.muted,
                Align::Left,
            );
        }

        if let Some(k) = picked {
            self.cap_sel = k;
            return true;
        }
        if let Some(slot) = acted {
            self.revoke(pid, slot);
            return true;
        }
        false
    }

    /// Вкладка производительности: плитки устройств, подробности выбранного и его график.
    ///
    /// Веха 165 — по макету у КАЖДОЙ плитки своё окошко кривой, а внизу — та же кривая крупно.
    /// Одного графика «загрузка процессора» на всю вкладку в макете нет и не было: он отвечал
    /// на вопрос про процессор независимо от того, какую плитку человек выбрал.
    fn paint_perf(&mut self, u: &mut Ui, th: &Theme, lay: &Lay) -> bool {
        let font_h = u.font.line_h();
        let mut dirty = false;
        u.card(lay.tiles);
        for (k, dev) in DEVS.iter().enumerate() {
            let r = Self::tile_rect(th, lay.tiles, font_h, k);
            let sel = k == self.dev;
            let hot = u.hot(r);
            // Подложки у плитки в макете нет — полотно под ней сплошное, а выбранное различается
            // яркостью рамки окошка. Подсветка под курсором своя: макет её не рисует вовсе, но
            // без неё непонятно, что плитка нажимается.
            if hot && !sel {
                let c = u.tint(th.text.with_a(0x10));
                u.c.rrect(r, th.radius, c);
            }
            let mut d = r.inset(th.px(3));
            // Окошко кривой — слева, как в макете (там 69×54 при плитке 69×63).
            let gh = d.h;
            let gr = d.cut_left(gh * 5 / 4);
            d.cut_left(th.px(6));
            let br = u.tint(if sel { th.text } else { th.muted });
            u.c.rrect_bordered(gr, th.px(3), th.line.max(1), Rgba::CLEAR, br);
            let hist = self.hist.get(k).map(|h| h.as_slice()).unwrap_or(&[]);
            graph(u, th, gr.inset(th.px(2)), hist, false);
            let (val, sub, col) = self.dev_value(k);
            let name = if sel { th.text } else { th.muted };
            u.label(d.cut_top(font_h), dev.name, name, Align::Left);
            u.label(d.cut_top(font_h), &sub, th.muted, Align::Left);
            u.label(d.cut_top(font_h), &val, col, Align::Left);
            if u.clicked(r) && !sel {
                self.dev = k;
                dirty = true;
            }
        }

        // ── подробности выбранного: заголовок и сетка пар в две колонки ────────────────────
        let mut d = lay.detail;
        let head = d.cut_top(font_h + th.px(6));
        u.label(head, DEVS[self.dev].name, th.text, Align::Left);
        let (val, sub, col) = self.dev_value(self.dev);
        u.label(head, &alloc::format!("{sub} · {val}"), col, Align::Right);
        let rh = font_h + th.px(3);
        let pairs = self.detail_rows();
        if pairs.is_empty() {
            // Источника нет — вместо пустой сетки причина. Строки разные у «драйвера нет» и
            // «драйвер есть, счётчиков нет»: писать одно вместо другого значит врать в обе
            // стороны ([[void-taskmgr]]).
            for line in dev_help(self.dev) {
                if d.h < font_h {
                    break;
                }
                u.label(d.cut_top(rh), line, th.muted, Align::Left);
            }
        } else {
            let rows = pairs.len().div_ceil(2);
            // Зазор между колонками — ШИРЕ обычного: в паре значение прижато вправо, а подпись
            // соседней колонки — влево, и на обычном зазоре они встречаются в одно слово
            // («2 % из них простой»).
            let cgap = 3 * th.pad;
            let colw = (d.w - cgap) / 2;
            for (i, (k, v)) in pairs.iter().enumerate() {
                let (cx, cy) = (i / rows, i % rows);
                let r = Rect::new(d.x + cx as i32 * (colw + cgap), d.y + cy as i32 * rh, colw, rh);
                if r.bottom() <= d.bottom() {
                    u.row(r, k, v);
                }
            }
        }

        // ── график выбранного устройства ──────────────────────────────────────────────────
        let inner = u.card(lay.graph);
        let mut g = Rect::new(inner.x, lay.graph.y + th.pad, inner.w, lay.graph.h - 2 * th.pad);
        let title = g.cut_top(font_h);
        u.label(title, &alloc::format!("{}, последняя минута", DEVS[self.dev].name), th.muted, Align::Left);
        g.cut_top(th.px(4));
        let hist = self.hist.get(self.dev).map(|h| h.as_slice()).unwrap_or(&[]);
        if hist.len() < 2 {
            let why = if DEVS[self.dev].why.is_empty() { "замеров ещё нет" } else { DEVS[self.dev].why };
            u.label(g, why, th.muted, Align::Left);
            return dirty;
        }
        u.label(title, "100 %", th.muted, Align::Right);
        graph(u, th, g, hist, true);
        dirty
    }

    /// Пары «подпись — значение» для блока подробностей. Пусто — источника нет, и вместо сетки
    /// надо написать, какого именно ([`dev_help`]).
    fn detail_rows(&self) -> Vec<(String, String)> {
        let s = |a: &str, b: String| (String::from(a), b);
        match (self.dev, &self.info) {
            (0, info) => {
                let load = match (info, &self.info_prev) {
                    (Some(n), Some(p)) => alloc::format!("{} %", n.cpu_percent(p)),
                    _ => String::from("—"),
                };
                let mut v = alloc::vec![s("загрузка", load), s("архитектура", String::from(ARCH))];
                if let Some(n) = info {
                    v.push(s("время работы", dur_text(n.uptime_ns)));
                    v.push(s("из них простой", dur_text(n.idle_ns)));
                    v.push(s("процессов", alloc::format!("{}", n.procs)));
                    // Веха 170 — ЯДРА двумя числами: сколько машина объявила и на скольких
                    // работает система. Здесь стояло зашитое «1 (SMP нет)» — оно было верным
                    // ровно до этой вехи и, что важнее, ничего не измеряло.
                    //
                    // Пока поднятые ядра запаркованы, «4, работает 1» — это и есть правда.
                    // Написать «4» одним числом значило бы пообещать то, чего система не делает.
                    let (c, up, run) =
                        (n.cores.max(1), n.cores_up.max(1), n.cores_sched.max(1));
                    let mut text = alloc::format!("{c}");
                    if up < c {
                        // Ядро есть, а поднять его не вышло — это неисправность, и молчать о
                        // ней нельзя.
                        text.push_str(&alloc::format!(", поднято {up}"));
                    }
                    if run < up {
                        text.push_str(&alloc::format!(" (работает {run})"));
                    }
                    v.push(s("ядер", text));
                }
                v
            }
            (1, Some(n)) => {
                let mib = |b: u64| alloc::format!("{} МиБ", b / (1024 * 1024));
                alloc::vec![
                    s("всего", mib(n.ram_total)),
                    s("занято", mib(n.ram_used)),
                    s("свободно", mib(n.ram_total.saturating_sub(n.ram_used))),
                    s("занято, доля", alloc::format!("{} %", n.ram_percent())),
                    s("страница", String::from("4 КиБ")),
                    s("подкачки", String::from("нет (и не будет)")),
                ]
            }
            _ => Vec::new(),
        }
    }

    /// Значение плитки: (число, подпись, цвет числа).
    fn dev_value(&self, k: usize) -> (String, String, Rgba) {
        let th_muted = Rgba::hex(0x8c8c8c);
        match k {
            0 => match (&self.info, &self.info_prev) {
                (Some(n), Some(p)) => (
                    alloc::format!("{} %", n.cpu_percent(p)),
                    String::from(ARCH),
                    Rgba::hex(0xdcdcdd),
                ),
                _ => (String::from("—"), String::from(ARCH), th_muted),
            },
            1 => match &self.info {
                Some(n) => (
                    alloc::format!("{} %", n.ram_percent()),
                    alloc::format!("{} из {} МиБ", n.ram_used / 1048576, n.ram_total / 1048576),
                    Rgba::hex(0xdcdcdd),
                ),
                None => (String::from("—"), String::from("нет права"), th_muted),
            },
            _ => (String::from("—"), String::from(DEVS[k].why), th_muted),
        }
    }

    /// Веха 155 — окно без права обзора: что именно не дано и чем это выдаётся.
    fn paint_denied(&mut self, u: &mut Ui, th: &Theme, lay: &Lay) {
        let font_h = u.font.line_h();
        let mut all = Rect::new(0, 0, self.w, self.h).inset(th.pad);
        all.cut_top(lay.tabs.h + th.gap);
        let inner = u.card(all);
        let mut d = inner.inset(th.pad);
        u.label(d.cut_top(font_h + th.px(6)), "нет права обзора процессов", th.text, Align::Left);
        u.hsep(d.cut_top(th.px(6)));
        d.cut_top(th.px(4));
        // Строки КОРОТКИЕ намеренно: ширину окна назначает композитор (колонка ленты), и текст,
        // сверстанный под 820 точек запроса, обрезался бы многоточием ровно там, где важное.
        // Кавычек-ёлочек здесь нет намеренно: в шрифте оболочки их глифов нет, и на экране они
        // выходят посторонними буквами (проверено — «все» читалось как «овсе»).
        for line in [
            "Диспетчер показывает не всю систему,",
            "а ровно то, что выдано ему самому.",
            "Права `sysview` у него нет — поэтому",
            "списки пусты. Это не значит, что",
            "никого нет: это значит, что смотреть",
            "не дано.",
            "",
            "Выдать — в конфиге поколения:",
            "  shell wm … sysview:rwg!",
            "     право обзора композитору,",
            "     детям НЕ наследуется",
            "  desktop sysview taskmgr",
            "     кому он отдаёт его по просьбе",
            "",
            "Затем `rebuild` и перезагрузка.",
            "Без второй строки не отдаст никому —",
            "это правильное состояние по умолчанию.",
        ] {
            if d.h < font_h {
                break;
            }
            let col = if line.starts_with("  ") { th.accent } else { th.muted };
            u.label(d.cut_top(font_h + th.px(2)), line, col, Align::Left);
        }
    }
}

/// Веха 165 — **КРИВАЯ ЗАМЕРОВ**: столбик на замер, свежие справа.
///
/// Одна функция и на окошко в плитке, и на большой график внизу — это одна и та же кривая,
/// показанная в двух размерах, и рисовать её дважды разным кодом значило бы завести два ответа
/// на вопрос «какая была загрузка».
///
/// График НЕ РАСТЯГИВАЕТСЯ на всю ширину: он рисует то, что замерено. Иначе первые секунды после
/// запуска выглядели бы как минута наблюдений. В узкое окошко влезает столько последних замеров,
/// сколько влезает, — с конца, потому что человек смотрит на «сейчас».
fn graph(u: &mut Ui, th: &Theme, r: Rect, hist: &[u8], hair: bool) {
    if r.w <= 0 || r.h <= 0 {
        return;
    }
    if hair {
        // Половина и потолок — волосками: без них плоская линия у самого низа неотличима от
        // пустого поля, а «плоско» и «нет данных» — разные ответы.
        let c = u.tint(th.text.with_a(0x18));
        for part in [0, 1] {
            let y = r.y + r.h * part / 2;
            u.c.fill(Rect::new(r.x, y, r.w, th.line.max(1)), c);
        }
    }
    let bw = (r.w / HIST as i32).max(1);
    let fit = (r.w / bw).max(1) as usize;
    let from = hist.len().saturating_sub(fit);
    let col = u.tint(th.accent);
    for (k, &v) in hist[from..].iter().enumerate() {
        let x = r.x + k as i32 * bw;
        // Не меньше точки: ноль процентов — тоже замер, и он обязан быть виден.
        let h = (r.h * v as i32 / 100).max(1);
        u.c.fill(Rect::new(x, r.bottom() - h, (bw - 1).max(1), h), col);
    }
}

/// Колонки таблицы: считаются ОДИН раз и служат и шапке, и строкам — иначе заголовок и данные
/// разъезжаются, и это тот самый случай двух расчётов «где что», на которых система уже стояла.
struct Cols {
    pid: i32,
    cpu: i32,
    heap: i32,
    state: i32,
    gap: i32,
}

impl Cols {
    fn new(th: &Theme, r: Rect) -> Cols {
        let gap = th.px(8);
        let state = th.px(150).min(r.w / 4);
        Cols { pid: th.px(56), cpu: th.px(56), heap: th.px(80), state, gap }
    }

    /// Разложить пять значений по колонкам. Имя занимает всё, что осталось слева.
    #[allow(clippy::too_many_arguments)]
    fn label(
        &self,
        u: &mut Ui,
        r: Rect,
        name: &str,
        pid: &str,
        cpu: &str,
        heap: &str,
        state: &str,
        fg: Rgba,
        dim: Rgba,
    ) {
        let mut row = r.inset_xy(u.th.pad, 0);
        let st = row.cut_right(self.state);
        // Двойной зазор перед «состоянием»: оно выровнено ВЛЕВО, а «куча» — вправо, и на одном
        // зазоре два соседних значения слипались в одно слово («0 КиБ ждёт IPC»).
        row.cut_right(2 * self.gap);
        let hp = row.cut_right(self.heap);
        row.cut_right(self.gap);
        let cp = row.cut_right(self.cpu);
        row.cut_right(self.gap);
        let pd = row.cut_right(self.pid);
        row.cut_right(self.gap);
        u.label(row, name, fg, Align::Left);
        u.label(pd, pid, dim, Align::Right);
        u.label(cp, cpu, fg, Align::Right);
        u.label(hp, heap, fg, Align::Right);
        u.label(st, state, dim, Align::Left);
    }
}

/// Прочитать права процесса `pid` через ядро.
fn read_caps(sysview: usize, pid: u16) -> Vec<Capp> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; CAP_REC * 96];
    let Some(total) = sys::proc_caps(sysview, pid as usize, &mut buf) else { return out };
    let shown = total.min(buf.len() / CAP_REC);
    for k in 0..shown {
        let r = &buf[k * CAP_REC..k * CAP_REC + CAP_REC];
        out.push(Capp {
            slot: u16::from_le_bytes([r[0], r[1]]),
            kind: r[2],
            rights: u32::from_le_bytes([r[4], r[5], r[6], r[7]]),
            aux: u16::from_le_bytes([r[8], r[9]]),
        });
    }
    out
}

/// Имя вида цели — общий словарь с ядром (`cap::info_kind`).
fn kind_name(k: u8) -> &'static str {
    match k {
        1 => "store",
        2 => "root",
        3 => "value",
        4 => "endpoint",
        5 => "reply",
        6 => "диск",
        7 => "сеть",
        8 => "mmio",
        9 => "dma",
        10 => "power",
        11 => "shm",
        12 => "irq",
        13 => "sysview",
        _ => "?",
    }
}

/// Веха 164 — ЧТО ЭТО ПРАВО ЗНАЧИТ, человеческими словами. Список видов без пояснения — это
/// та же непрозрачность, от которой capability-модель и уходит: видеть `mmio [rw]` и не знать,
/// что за ним, ничем не лучше, чем не видеть ничего.
fn kind_help(k: u8) -> &'static [&'static str] {
    // Строки короткие намеренно: ширину окна назначает композитор, и длинная фраза обрезалась
    // бы многоточием на первом же слове.
    match k {
        1 => &["объекты системы:", "класть и читать по", "content-id"],
        2 => &["именованный корень —", "вход в поколение", "или файл"],
        3 => &["одно значение в store"],
        4 => &["канал к процессу:", "можно звать его и", "просить за себя"],
        5 => &["ответить на один", "вызов; живёт до", "ответа"],
        6 => &["диск целиком: секторы", "мимо store"],
        7 => &["сетевая карта: кадры", "мимо служб"],
        8 => &["регистры устройства —", "прямое управление", "железом"],
        9 => &["память для устройства:", "оно пишет в неё само"],
        10 => &["выключить машину"],
        11 => &["общая память с другим", "процессом (так ездят", "кадры окон)"],
        12 => &["прерывания устройства:", "спать до сигнала"],
        13 => &["видеть процессы и их", "права, а с `w` —", "отзывать их"],
        _ => &["вид неизвестен этой", "сборке"],
    }
}

/// Права словами (для панели пояснения).
fn rights_full(r: u32) -> String {
    if r == 0 {
        return String::from("никаких");
    }
    let mut s = String::new();
    for (bit, word) in [
        (1u32, "чтение"),
        (2, "запись"),
        (16, "запуск"),
        (8, "отправка"),
        (4, "передача"),
    ] {
        if r & bit != 0 {
            if !s.is_empty() {
                s.push_str(", ");
            }
            s.push_str(word);
        }
    }
    s
}

/// Права буквами (READ 1 · WRITE 2 · GRANT 4 · SEND 8 · EXEC 16).
fn rights_str(r: u32) -> String {
    if r == 0 {
        return String::from("-");
    }
    let mut s = String::new();
    for (bit, ch) in [(1u32, 'r'), (2, 'w'), (16, 'x'), (8, 's'), (4, 'g')] {
        if r & bit != 0 {
            s.push(ch);
        }
    }
    s
}

/// Веха 164 — чего именно не хватает под плиткой устройства. Разные причины — разные слова:
/// у диска и сети драйверы РАБОТАЮТ (система с диска грузится, адрес по DHCP приезжает), нет
/// счётчиков; видеокарты и датчиков нет вовсе.
fn dev_help(k: usize) -> &'static [&'static str] {
    match k {
        2 => &[
            "Драйверы диска есть и работают: система",
            "грузится с него, store живёт на нём.",
            "Счётчиков ввода-вывода ядро не ведёт —",
            "спросить, сколько прочитано, не у кого.",
        ],
        3 => &[
            "Драйверы сети есть и работают: адрес",
            "приезжает по DHCP, пакеты ходят.",
            "Счётчики держит служба сети, а не ядро,",
            "и отдельного протокола к ней у диспетчера нет.",
        ],
        4 => &[
            "Драйверов видеокарты НЕТ вовсе.",
            "Композитор рисует процессором, в буфер",
            "кадра от загрузчика. Ускорения нет,",
            "и показывать под этой плиткой нечего.",
        ],
        _ => &[
            "Драйверов датчиков НЕТ вовсе: ни температур,",
            "ни оборотов кулеров, ни частот. Ни ACPI, ни",
            "hwmon в VOID пока не разобраны — число здесь",
            "было бы выдумкой.",
        ],
    }
}

/// Короткое имя состояния (для колонки таблицы).
fn state_name(s: u8) -> &'static str {
    match s {
        0 => "готов",
        1 => "ждёт IPC",
        2 => "ждёт ответа",
        3 => "ждёт ребёнка",
        4 => "завершён",
        5 => "ждёт ввода",
        6 => "ждёт futex",
        7 => "ждёт IRQ",
        8 => "спит",
        _ => "неизвестно",
    }
}

/// Веха 163 — длительность человеку: миллисекунды, пока их немного, дальше секунды с десятой,
/// а с минуты — минуты. Наносекунд не показываем никогда.
fn dur_text(ns: u64) -> String {
    let ms = ns / 1_000_000;
    if ms < 10_000 {
        alloc::format!("{ms} мс")
    } else if ms < 600_000 {
        alloc::format!("{},{} с", ms / 1000, ms % 1000 / 100)
    } else {
        alloc::format!("{} мин {} с", ms / 60_000, ms % 60_000 / 1000)
    }
}

/// Content-id строкой в 64 знака.
fn hex64(id: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in id {
        s.push_str(&alloc::format!("{:02x}", b));
    }
    s
}

/// Архитектура сборки — то единственное про процессор, что диспетчер знает наверняка.
#[cfg(target_arch = "x86_64")]
const ARCH: &str = "x86_64";
#[cfg(target_arch = "riscv64")]
const ARCH: &str = "riscv64";

/// Найти своё право обзора: сперва среди СТАРТОВЫХ по ВИДУ (Sysview = 13), а если его там нет —
/// попросить у композитора (Веха 155). Возвращает `(дескриптор, можно ли отзывать)`.
///
/// Два источника, потому что диспетчер запускают двумя способами. Из конфига поколения
/// (`shell taskmgr sysview:rw …`) право приезжает стартовым — как у любого сервиса. Но обычно
/// его открывают из меню, то есть спавнит его композитор, а ребёнок получает копию прав РОДИТЕЛЯ:
/// выдать право так значило бы выдать его каждому окну разом. Поэтому в оконном режиме право
/// у композитора помечено «не наследуется» (Веха 154), а диспетчер просит его отдельно — и
/// получает, если конфиг поколения назвал диспетчер по имени (`desktop sysview taskmgr`).
fn find_sysview() -> (usize, bool) {
    let c = sys::win::cap_or_grant(13);
    (c, sys::cap_info(c).is_some_and(|(_, r)| r & 0x02 != 0)) // WRITE — можно отзывать
}

impl ui::Client for App {
    fn event(&mut self, e: Event, input: &ui::Input) -> ui::Scope {
        match e {
            Event::Key { sym: code, ch, down, .. } if down => {
                self.flash = None;
                if code == sym::ESCAPE {
                    return ui::Scope::No; // закрытие — дело композитора (Super+Q)
                }
                // Tab переключает вкладку по кругу.
                if code == sym::TAB {
                    self.tab = match self.tab {
                        Tab::Perf => Tab::Apps,
                        Tab::Apps => Tab::Services,
                        Tab::Services => Tab::Perf,
                    };
                    let keep = self.current_pid();
                    self.refilter(keep);
                    return ui::Scope::All;
                }
                if self.tab == Tab::Perf {
                    return ui::Scope::No;
                }
                match self.ls.key(code, ch) {
                    ui::Hit::None => ui::Scope::No,
                    ui::Hit::Moved => ui::Scope::All,
                    ui::Hit::Query => ui::Scope::All,
                }
            }
            Event::Wheel { delta, .. } => {
                if self.tab != Tab::Perf && self.ls.wheel(delta) {
                    ui::Scope::All
                } else {
                    ui::Scope::No
                }
            }
            Event::Motion { .. } => {
                if self.tab != Tab::Perf {
                    self.ls.motion(input.ptr);
                }
                // Подсветка живёт и в таблице, и в плитках, и на кнопках — перерисовываем всё.
                ui::Scope::All
            }
            Event::Button { .. } => ui::Scope::All,
            Event::Resize { w, h } => {
                self.w = w as i32;
                self.h = h as i32;
                self.ls.scroll_to_sel();
                ui::Scope::All
            }
            _ => ui::Scope::No,
        }
    }

    /// Веха 151 — вернуться после перезагрузки на ту же вкладку.
    fn persist(&mut self) -> Option<String> {
        Some(String::from(match self.tab {
            Tab::Perf => "taskmgr perf",
            Tab::Apps => "taskmgr user",
            Tab::Services => "taskmgr",
        }))
    }

    /// Обновляемся дважды в секунду: счётчики и список процессов живые. Без права смотреть
    /// не на что — тогда и будильник не заводим: спящее окно не должно будить систему впустую.
    fn wake(&mut self) -> Option<u32> {
        (self.sysview != sys::NO_CAP).then_some(500)
    }

    fn tick(&mut self) -> ui::Scope {
        self.reload();
        ui::Scope::All
    }

    fn draw(&mut self, u: &mut Ui) -> ui::Scope {
        let th = u.th.clone();
        self.lay = self.measure(u.font, &th);
        self.ls.measure(self.lay.list, self.lay.row_h, self.lay.rows);
        self.sync_detail();
        let lay = core::mem::take(&mut self.lay);
        let dirty = self.paint(u, &th, &lay);
        self.lay = lay;
        if dirty { ui::Scope::All } else { ui::Scope::No }
    }
}

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let (_generation, th, mut font) = ui::app::boot();

    let (sysview, can_write) = find_sysview();
    if sysview == sys::NO_CAP {
        say("taskmgr: нет права обзора (sysview) — ни в старте, ни от композитора\n");
        say("taskmgr: выдаётся строкой `desktop sysview taskmgr` в конфиге поколения\n");
    }

    let (w, h) = (820u16, 620u16);
    let Some(mut surf) = Window::create(w, h, "Диспетчер задач") else {
        say("taskmgr: композитора нет (WM в окружении)\n");
        sys::exit(1);
    };

    // Аргумент выбирает вкладку — так возвращает сеанс (Веха 151).
    let av = sys::argv::Argv::take();
    let tab = match av.str(0) {
        Some("user") => Tab::Apps,
        Some("perf") => Tab::Perf,
        _ => Tab::Services,
    };

    let mut app = App {
        w: w as i32,
        h: h as i32,
        sysview,
        can_write,
        procs: Vec::new(),
        stats: Vec::new(),
        cpu: Vec::new(),
        prev: Vec::new(),
        prev_at: 0,
        ls: ui::List::default(),
        tab,
        caps: Vec::new(),
        cap_sel: 0,
        detail_pid: None,
        info: None,
        info_prev: None,
        hist: DEVS.iter().map(|_| Vec::new()).collect(),
        dev: 0,
        lay: Lay::default(),
        net_srv_pid: None,
        net: None,
        net_ep: sys::NO_CAP,
        net_tried: false,
        flash: None,
    };
    app.reload();

    ui::app::run(&mut surf, &th, &mut font, &mut app);
    sys::exit(0);
}
