//! `taskmgr` — ДИСПЕТЧЕР ЗАДАЧ (Веха 153.5): лицо capability-модели к пользователю.
//!
//! В Windows/Linux «доверенный» — признак, приклеенный к процессу (подпись, путь, uid), его
//! можно подделать. У VOID доверия как признака нет: есть только то, что процесс реально держит
//! в своей таблице прав. Диспетчер не спрашивает программу, кто она, — он читает у ядра, ЧТО она
//! может ([[task-manager]]).
//!
//! - **вкладки система/пользователь** — признак не флаг, а ПРОИСХОЖДЕНИЕ: системное = поднято
//!   init'ом из конфига поколения. Свернул системное — осталось ровно то, что запустил ты.
//! - **имя не удостоверение** — программа запускается из store ПО CONTENT-ID; диспетчер показывает
//!   хэш того, что исполняется, а не имя (замаскироваться нечем: другой код → другой хэш).
//! - **права ГРАФОМ, а не списком** — процесс без права на сеть всё равно может ПОПРОСИТЬ того, у
//!   кого оно есть (confused deputy). Честная картинка — «эндпоинт к тому, у кого есть» (`→P<n>`).
//! - **что делает сейчас** — весь ввод-вывод идёт через IPC, значит его можно просто СЧИТАТЬ.
//! - **скальпель, а не топор** — «отнять» отбирает у процесса ОДНО право на ходу (`cap::revoke`
//!   наружу), а «рубильник сети» снимает сеть у всех пользовательских процессов разом.
//!
//! Список процессов сам под правом (`sysview`): у обычной программы его нет, поэтому она даже не
//! узнает, что рядом работает. Диспетчер ищет своё право по ВИДУ (13), не по позиции.

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

use ui::{Align, Font, Rect, Theme, Ui};

#[global_allocator]
static ALLOC: sys::heap::Heap<{ 4 * 1024 * 1024 }> = sys::heap::Heap::new();

const REC: usize = sys::PROC_REC; // 64 — запись процесса
const CAP_REC: usize = sys::PROC_CAP_REC; // 12 — запись права

fn say(s: &str) {
    sys::write_console(s.as_bytes());
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

#[derive(Default)]
struct Lay {
    tabs: Rect,
    col: Rect,
    list: Rect,
    bar: Rect,
    body: Rect,
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
    ls: ui::List,
    /// Вкладка: `true` — системные, `false` — пользовательские.
    tab_system: bool,
    caps: Vec<Capp>,
    stat: Option<sys::ProcStat>,
    /// Для какого pid прочитаны подробности (чтобы не читать каждый кадр — только при смене
    /// выбора и по таймеру обновления).
    detail_pid: Option<u16>,
    lay: Lay,
    /// pid сервера сети — для подписи рёбер и для рубильника.
    net_srv_pid: Option<u16>,
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

    /// Пересобрать список под текущую вкладку, сохранив выбор по pid.
    fn refilter(&mut self, keep: Option<u16>) {
        self.ls.hits = (0..self.procs.len())
            .filter(|&i| self.procs[i].system == self.tab_system)
            .collect();
        self.ls.refiltered();
        if let Some(pid) = keep {
            if let Some(pos) = self.ls.hits.iter().position(|&i| self.procs[i].pid == pid) {
                self.ls.sel = pos;
                self.ls.scroll_to_sel();
            }
        }
        self.detail_pid = None;
    }

    /// Перечитать процессы (по таймеру), сохранив вкладку и выбор.
    fn reload(&mut self) {
        let keep = self.current_pid();
        self.procs = self.read_procs();
        self.net_srv_pid =
            self.procs.iter().find(|p| p.name == "net-srv").map(|p| p.pid);
        self.refilter(keep);
    }

    /// Прочитать права и счётчики выбранного процесса, если ещё не прочитаны для его pid.
    fn sync_detail(&mut self) {
        let Some(pid) = self.current_pid() else {
            self.caps.clear();
            self.stat = None;
            self.detail_pid = None;
            return;
        };
        if self.detail_pid == Some(pid) {
            return;
        }
        self.detail_pid = Some(pid);
        self.caps = read_caps(self.sysview, pid);
        self.stat = sys::proc_stat(self.sysview, pid as usize);
    }

    /// Разрезать окно на места виджетов.
    fn measure(&self, font: &Font, th: &Theme) -> Lay {
        let font_h = font.line_h();
        let row_h = font_h + th.px(14);
        let mut all = Rect::new(0, 0, self.w, self.h).inset(th.pad);
        let tabs = all.cut_top(font_h + th.px(14));
        all.cut_top(th.gap);
        let mut foot = all.cut_bottom(font_h + th.px(10));
        let foot_btn = foot.cut_right(th.px(200));
        foot.cut_right(th.gap);
        all.cut_bottom(th.gap);
        let mut area = all;
        let list_w = (area.w / 3).clamp(th.px(240), th.px(380));
        let col = area.cut_left(list_w);
        area.cut_left(th.gap);
        let mut list = col;
        let bar = list.cut_right(th.px(6));
        let rows = (list.h / row_h).max(1) as usize;
        Lay {
            tabs,
            col,
            list,
            bar,
            body: area,
            foot_msg: foot,
            foot_btn,
            row_h,
            rows,
        }
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

    /// Рубильник сети: снять сетевые права у всех ПОЛЬЗОВАТЕЛЬСКИХ процессов (белый список —
    /// системные). Сеть = право на само устройство (`net`) ИЛИ эндпоинт к серверу сети. Политика
    /// целиком в диспетчере: ядро даёт лишь отзыв слота.
    fn killnet(&mut self) {
        let net = self.net_srv_pid;
        let mut count = 0usize;
        // Снимок pid'ов заранее: revoke не двигает процессы, но заимствование — чище.
        let targets: Vec<u16> =
            self.procs.iter().filter(|p| !p.system).map(|p| p.pid).collect();
        for pid in targets {
            for c in read_caps(self.sysview, pid) {
                let is_net = c.kind == 7 // Device(Net)
                    || (c.kind == 4 && net.is_some() && c.aux == net.unwrap());
                if is_net && sys::proc_revoke(self.sysview, pid as usize, c.slot as usize) {
                    count += 1;
                }
            }
        }
        self.flash = Some(alloc::format!("рубильник сети: снято прав — {}", count));
        self.detail_pid = None;
    }

    /// Нарисовать кадр. `true` — состояние изменилось прямо в кадре (клик), нужен ещё проход.
    fn paint(&mut self, u: &mut Ui, th: &Theme, lay: &Lay) -> bool {
        u.background(th.bg);
        // Веха 155 — права нет: сказать это ВСЛУХ. Пустые вкладки на месте списка процессов —
        // худший из возможных ответов: они выглядят как «ничего не работает», хотя означают
        // «мне не дано смотреть». Разница здесь принципиальная: у VOID отсутствие права — не
        // сбой, а нормальное состояние, и объяснить его должен тот, кто в него упёрся.
        if self.sysview == sys::NO_CAP {
            self.paint_denied(u, th, lay);
            return false;
        }
        let mut dirty = false;

        // ── вкладки ───────────────────────────────────────────────────────────────────────
        let mut tabs = lay.tabs;
        let tw = (tabs.w - th.gap) / 2;
        let t_sys = tabs.cut_left(tw);
        tabs.cut_left(th.gap);
        let t_usr = tabs;
        let sys_on = if self.tab_system { 256 } else { 0 };
        let usr_on = if self.tab_system { 0 } else { 256 };
        if u.tile(t_sys, "система", if u.hot(t_sys) { 256 } else { 0 }, sys_on) && !self.tab_system {
            self.tab_system = true;
            let keep = self.current_pid();
            self.refilter(keep);
            dirty = true;
        }
        if u.tile(t_usr, "пользователь", if u.hot(t_usr) { 256 } else { 0 }, usr_on)
            && self.tab_system
        {
            self.tab_system = false;
            let keep = self.current_pid();
            self.refilter(keep);
            dirty = true;
        }

        // ── список слева ──────────────────────────────────────────────────────────────────
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
            let p = &self.procs[i];
            let sel = if self.ls.top + k == self.ls.sel { 256 } else { 0 };
            let hot = if u.hot(rr) { 256 } else { 0 };
            let sub = alloc::format!("P{} · {}", p.pid, state_name(p.state));
            let letter = state_letter(p.state);
            if u.entry(rr, &p.name, &sub, letter, sel, hot) {
                self.ls.sel = self.ls.top + k;
            }
        }
        if self.ls.sel != was {
            dirty = true;
        }

        // ── подробности справа ─────────────────────────────────────────────────────────────
        if !u.c.clip().intersect(lay.body).is_empty() {
            dirty |= self.paint_detail(u, th, lay);
        }

        // ── подвал: сообщение + рубильник ──────────────────────────────────────────────────
        let msg = match &self.flash {
            Some(m) => m.clone(),
            None => alloc::format!(
                "{}: {} · sysview {}",
                if self.tab_system { "системных" } else { "пользовательских" },
                self.ls.hits.len(),
                if self.can_write { "чтение+управление" } else { "только чтение" }
            ),
        };
        let mcol = if self.flash.is_some() { th.accent } else { th.muted };
        u.label(lay.foot_msg, &msg, mcol, Align::Left);
        if self.can_write {
            let hot = if u.hot(lay.foot_btn) { 256 } else { 0 };
            if u.danger(lay.foot_btn, "рубильник сети", hot) {
                self.killnet();
                dirty = true;
            }
        }
        dirty
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

    /// Правая половина: content-id, происхождение, счётчики и граф прав с кнопками отзыва.
    fn paint_detail(&mut self, u: &mut Ui, th: &Theme, lay: &Lay) -> bool {
        let font_h = u.font.line_h();
        let inner = u.card(lay.body);
        let mut d = inner.inset(th.pad);
        let Some(i) = self.ls.current() else {
            u.label(d.cut_top(font_h), "процесс не выбран", th.muted, Align::Left);
            return false;
        };
        let (pid, ppid, system, linux, has_hash, state) = {
            let p = &self.procs[i];
            (p.pid, p.ppid, p.system, p.linux, p.has_hash, p.state)
        };
        let name = self.procs[i].name.clone();
        let hash = self.procs[i].hash;

        u.label(d.cut_top(font_h + th.px(4)), &name, th.text, Align::Left);
        // content-id целиком (или пометка «пакет»): это и есть настоящее имя того, что исполняется.
        if has_hash {
            let hex = hex64(&hash);
            u.label(d.cut_top(font_h), "content-id образа", th.muted, Align::Left);
            u.label(d.cut_top(font_h), &hex[..32], th.text, Align::Left);
            u.label(d.cut_top(font_h + th.px(4)), &hex[32..], th.text, Align::Left);
        } else {
            u.label(
                d.cut_top(font_h + th.px(4)),
                if linux { "образ из пакета Linux (единого хэша нет)" } else { "образ без хэша" },
                th.muted,
                Align::Left,
            );
        }
        u.row(d.cut_top(font_h + th.px(2)), "происхождение", if system { "системный (init из конфига)" } else { "пользовательский" });
        let par = if ppid == 0xFFFF { String::from("—") } else { alloc::format!("P{}", ppid) };
        u.row(d.cut_top(font_h + th.px(2)), "родитель", &par);
        u.row(d.cut_top(font_h + th.px(2)), "состояние", state_full(state));
        if let Some(st) = &self.stat {
            let a = alloc::format!("{} вызовов, {} Б", st.calls_made, st.bytes_sent);
            u.row(d.cut_top(font_h + th.px(2)), "сделал IPC", &a);
            let b = alloc::format!("{} вызовов, {} Б", st.calls_recv, st.bytes_recv);
            u.row(d.cut_top(font_h + th.px(2)), "принял IPC", &b);
            if st.holds_screen {
                u.row(d.cut_top(font_h + th.px(2)), "экран", "владеет");
            }
        }

        d.cut_top(th.px(4));
        u.hsep(d.cut_top(th.px(6)));
        d.cut_top(th.px(2));
        u.label(d.cut_top(font_h), "права (граф эндпоинтов):", th.muted, Align::Left);

        // Веха 155 — сказать вслух, что таблица прав ОБЩАЯ. Ядро заводит c-space по ИМЕНИ
        // программы (`cap::create_domain`), поэтому два экземпляра одного диспетчера смотрят в
        // одну таблицу: ниже будут и чужие права, а «отнять» отберёт их у обоих сразу. Молчать
        // об этом нельзя — вся ценность этого окна в том, что показанное соответствует правде.
        let twins: Vec<u16> =
            self.procs.iter().filter(|p| p.name == name && p.pid != pid).map(|p| p.pid).collect();
        if !twins.is_empty() {
            let mut s = String::from("общая таблица с ");
            for (k, t) in twins.iter().enumerate() {
                if k > 0 {
                    s.push_str(", ");
                }
                s.push_str(&alloc::format!("P{}", t));
            }
            u.label(d.cut_top(font_h), &s, th.accent, Align::Left);
            u.label(
                d.cut_top(font_h + th.px(2)),
                "(c-space заводится по имени)",
                th.muted,
                Align::Left,
            );
        }

        let mut acted: Option<u16> = None; // слот, у которого нажали «отнять»
        let btn_w = th.px(96);
        // Копию списка прав держим, чтобы отзыв не конфликтовал с заимствованием self.
        for k in 0..self.caps.len() {
            if d.h < font_h + th.px(4) {
                break;
            }
            let (slot, kind, rights, aux) = {
                let c = &self.caps[k];
                (c.slot, c.kind, c.rights, c.aux)
            };
            let mut rr = d.cut_top(font_h + th.px(6));
            let btn = if self.can_write { rr.cut_right(btn_w) } else { Rect::default() };
            if self.can_write {
                rr.cut_right(th.gap);
            }
            let mut txt = String::new();
            txt.push_str(kind_name(kind));
            txt.push_str(" [");
            txt.push_str(&rights_str(rights));
            txt.push(']');
            if aux != 0xFFFF {
                let who = self.procs.iter().find(|p| p.pid == aux);
                match who {
                    Some(p) => {
                        txt.push_str(&alloc::format!(" →P{} ({})", aux, p.name));
                    }
                    None => txt.push_str(&alloc::format!(" →P{}", aux)),
                }
            }
            u.label(rr, &txt, th.text, Align::Left);
            if self.can_write {
                let hot = if u.hot(btn) { 256 } else { 0 };
                if u.danger(btn, "отнять", hot) {
                    acted = Some(slot);
                }
            }
        }
        if let Some(slot) = acted {
            self.revoke(pid, slot);
            return true;
        }
        false
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

/// Короткое имя состояния (для подписи в списке).
fn state_name(s: u8) -> &'static str {
    match s {
        0 => "работает",
        1 | 2 => "ждёт IPC",
        3 => "ждёт ввода",
        4 => "ждёт ребёнка",
        5 => "ждёт нить",
        6 => "futex",
        7 => "ждёт IRQ",
        8 => "спит",
        _ => "?",
    }
}

/// Полное состояние (для подробностей).
fn state_full(s: u8) -> &'static str {
    match s {
        0 => "работает (готов)",
        1 => "ждёт запроса (RECV)",
        2 => "ждёт ответа (CALL)",
        3 => "ждёт ввода с консоли",
        4 => "ждёт завершения ребёнка",
        5 => "ждёт завершения нити",
        6 => "ждёт futex",
        7 => "ждёт прерывания устройства",
        8 => "спит по таймеру",
        _ => "неизвестно",
    }
}

/// Буква-значок процесса: сервер (ждёт RECV) или обычный.
fn state_letter(s: u8) -> &'static str {
    match s {
        1 => "IPC",
        7 => "IRQ",
        _ => "P",
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
    if let Some((c, rights)) = sys::start_cap_of_kind(13) {
        return (c, rights & 0x02 != 0); // WRITE — можно отзывать
    }
    let c = sys::win::grant(13);
    (c, sys::cap_info(c).is_some_and(|(_, r)| r & 0x02 != 0))
}

impl ui::Client for App {
    fn event(&mut self, e: Event, input: &ui::Input) -> ui::Scope {
        match e {
            Event::Key { sym: code, ch, down, .. } if down => {
                self.flash = None;
                if code == sym::ESCAPE {
                    return ui::Scope::No; // закрытие — дело композитора (Super+Q)
                }
                // Tab переключает вкладку.
                if code == sym::TAB {
                    self.tab_system = !self.tab_system;
                    let keep = self.current_pid();
                    self.refilter(keep);
                    return ui::Scope::All;
                }
                match self.ls.key(code, ch) {
                    ui::Hit::None => ui::Scope::No,
                    ui::Hit::Moved => ui::Scope::All,
                    ui::Hit::Query => ui::Scope::All,
                }
            }
            Event::Wheel { delta, .. } => {
                if self.ls.wheel(delta) {
                    ui::Scope::Part(self.lay.col)
                } else {
                    ui::Scope::No
                }
            }
            Event::Motion { .. } => {
                if input.held.is_some() {
                    return ui::Scope::Part(self.lay.col);
                }
                if self.ls.motion(input.ptr) {
                    ui::Scope::Part(self.lay.col)
                } else {
                    // Движение над кнопками правой половины меняет подсветку — перерисуем её.
                    ui::Scope::All
                }
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
        Some(String::from(if self.tab_system { "taskmgr" } else { "taskmgr user" }))
    }

    /// Обновляемся дважды в секунду: счётчики IPC и список процессов живые. Без права смотреть
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

    let (w, h) = (820u16, 560u16);
    let Some(mut surf) = Window::create(w, h, "Диспетчер задач") else {
        say("taskmgr: композитора нет (WM в окружении)\n");
        sys::exit(1);
    };

    let mut app = App {
        w: w as i32,
        h: h as i32,
        sysview,
        can_write,
        procs: Vec::new(),
        ls: ui::List::default(),
        tab_system: true,
        caps: Vec::new(),
        stat: None,
        detail_pid: None,
        lay: Lay::default(),
        net_srv_pid: None,
        flash: None,
    };
    // Аргумент `user` открывает сразу пользовательскую вкладку (так возвращает сеанс).
    if sys::argv::Argv::take().str(0).as_deref() == Some("user") {
        app.tab_system = false;
    }
    app.reload();

    ui::app::run(&mut surf, &th, &mut font, &mut app);
    sys::exit(0);
}
