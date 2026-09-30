//! vvsh — конфиг/язык VOID (ADR 0006, [[vvsh-config-layout]], [[vvsh-lang]]).
//!
//! Подкоманды (запуск через vsh: `run vvsh <под> …`; права наследуются от vsh, как install.rs —
//! start-cap 0 = posixfs-endpoint, 1 = store):
//!   `eval FILE`   — прочитать `.vv`, вычислить НА VOID, напечатать нормализованный конфиг (M1a/b).
//!   `init-config` — посеять конфиг `/etc/system/*.vv` либо СВЕРИТЬ его с шаблоном (Веха 191):
//!                   нетронутые файлы обновляются, правленые остаются, а про новые ключи в них
//!                   говорится вслух. `--force` — перезаписать всё, как раньше.
//!   `rebuild`     — вычислить `/etc/system/default.vv` → КОММИТ нового поколения `system/gen<N>`,
//!                   двинуть `system/current` (активно после ребута) (M1c) + собрать пакеты,
//!                   объявленные конфигом (`pkg sync`, Веха 112).
//!   `gens`        — показать поколения `system/gen*` и активное (декластер `roots`) (M1c).
//!
//! `rebuild`/`gens` работают со store (start-cap 1). PUT/SET_ROOT/LIST_ROOTS требуют WRITE (есть у
//! shell'а `store:*w*`); dedup и маркер current читают (GET_ROOT/GET → нужен READ): при отсутствии
//! READ ДЕГРАДИРУЕМ мягко (без dedup/маркера), не падаем.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

use void_user as sys;
use void_user::posix as px;

// Общий с `pkg` код формата архивов — подключён ПО ПУТИ, а не через библиотеку (почему именно
// так — в шапке самого файла).
#[allow(dead_code)] // потоковая распаковка нужна `pkg`, шеллу — нет
#[path = "../archive.rs"]
mod archive;
// Общий с `pkg` разбор списка корней store — по тому же доводу (Веха 107).
#[path = "../roots.rs"]
mod roots;
// Профиль пакетов — ради PATH: голое слово ищется и среди установленного (Веха 109).
#[allow(dead_code)] // писательская половина профиля нужна `pkg`, шеллу — чтение
#[path = "../profile.rs"]
mod profile;
use vvsh_core::{Env, EvalError, Value};

// ── глобальный аллокатор ──────────────────────────────────────────────────────
//
// Реализация вынесена в `void_user::heap` (Веха 95): она понадобилась второй программе —
// TLS-клиенту, — а копия аллокатора это то место, где расхождение замечают последним и по
// самым странным симптомам. Здесь остаётся только выбор размера арены.
//
// Lisp-REPL порождает временные значения на каждое выражение, и без возврата памяти шелл через
// N команд получил бы null (это и чинила Веха 89 свободным списком со слиянием).
//
// 16 МиБ вместо прежних 4 (Веха 105): распаковка пакета держит в куче сразу распакованный NAR и
// окно словаря LZMA — на прежней арене хватало ровно на игрушечные архивы. Арена ленивая
// (`SYS_MAP` по факту обращения), поэтому запас ничего не стоит, пока не понадобился.
#[global_allocator]
static ALLOC: sys::heap::Heap<{ 16 * 1024 * 1024 }> = sys::heap::Heap::new();

const DEFAULT_PATH: &[u8] = b"/etc/system/default.vv";
const CURRENT_ROOT: &[u8] = b"system/current";

// Цвета — как в vsh (зелёный жирный префикс, синий каталог, жёлтая команда в справке).
const C_PROMPT: &[u8] = b"\x1b[1;32m";
const C_DIR: &[u8] = b"\x1b[1;34m";
const C_CMD: &[u8] = b"\x1b[1;33m";
const C_RESET: &[u8] = b"\x1b[0m";

// ── текущий каталог сессии (глобальный: процесс однопоточный, гонок нет) ───────
struct Cwd {
    buf: UnsafeCell<[u8; 256]>,
    len: AtomicUsize,
}
unsafe impl Sync for Cwd {}
static CWD: Cwd = Cwd {
    buf: UnsafeCell::new([b'/'; 256]),
    len: AtomicUsize::new(1), // "/"
};

fn cwd_get(out: &mut [u8]) -> usize {
    let len = CWD.len.load(Ordering::Relaxed);
    let src = unsafe { &*CWD.buf.get() };
    let n = len.min(out.len());
    out[..n].copy_from_slice(&src[..n]);
    n
}

fn cwd_set(path: &[u8]) {
    let dst = unsafe { &mut *CWD.buf.get() };
    let n = path.len().min(dst.len());
    dst[..n].copy_from_slice(&path[..n]);
    CWD.len.store(n, Ordering::Relaxed);
    publish_cwd(&dst[..n]);
}

/// Объявить текущий каталог ДЕТЯМ — записью `CWD=` в собственное окружение (Веха 120.1).
///
/// Текущего каталога у процесса в VOID нет: его ведёт шелл. Пока он вёл его только для себя,
/// запущенная программа понимала относительный путь по-своему — `ved terminal.vv` после
/// `cd /etc/system` открывал пустой `/terminal.vv`, а сохранение создало бы там мусорный файл.
fn publish_cwd(path: &[u8]) {
    let mut buf = [0u8; 512];
    let n = sys::env(&mut buf).min(buf.len());
    let mut out = Vec::new();
    // Старую запись выбрасываем: окружение — список пар, и две записи `CWD=` означали бы, что
    // ответ зависит от того, кто первым дочитал до своей.
    for entry in buf[..n].split(|&b| b == 0) {
        if entry.is_empty() || entry.starts_with(b"CWD=") {
            continue;
        }
        out.extend_from_slice(entry);
        out.push(0);
    }
    out.extend_from_slice(b"CWD=");
    out.extend_from_slice(path);
    out.push(0);
    sys::set_env(&out);
}

/// Разрешить путь относительно cwd в АБСОЛЮТНЫЙ нормализованный (`.`/`..`/`//` схлопнуты).
fn resolve(rel: &[u8]) -> Vec<u8> {
    let mut cwdbuf = [0u8; 256];
    let cwdn = cwd_get(&mut cwdbuf);
    let mut comps: Vec<&[u8]> = Vec::new();
    if rel.first() != Some(&b'/') {
        for c in cwdbuf[..cwdn].split(|&b| b == b'/').filter(|c| !c.is_empty()) {
            comps.push(c);
        }
    }
    for c in rel.split(|&b| b == b'/') {
        match c {
            b"" | b"." => {}
            b".." => {
                comps.pop();
            }
            _ => comps.push(c),
        }
    }
    let mut out = Vec::new();
    if comps.is_empty() {
        out.push(b'/');
    } else {
        for c in &comps {
            out.push(b'/');
            out.extend_from_slice(c);
        }
    }
    out
}


// ── права: по ИМЕНИ, а не по номеру (Веха 99.2) ──────────────────────────────
//
// Стартовые права позиционны, и порядок задаёт строка конфига. Это уже стоило сломанной
// системы: в `gen3` экран стоял первым, `start_cap(0)` вернул фреймбуфер вместо файлового
// сервера — `ls` роняла шелл, `run` ничего не запускал ([[multiplexer]]).
//
// Теперь права ищутся по именам, которые init кладёт в окружение (`CAP_POSIXFS`, `CAP_STORE`,
// `CAP_NET-SRV`), с откатом на прежние позиции — старые конфиги без имён продолжают работать.
//
// Резолвим РОВНО ОДИН РАЗ на старте, а не при каждом обращении: `cap_named` разбирает окружение,
// и звать его из горячих путей шелла значило бы платить синкаллом за каждую команду.
static FS_CAP: AtomicUsize = AtomicUsize::new(usize::MAX);
static STORE_CAP: AtomicUsize = AtomicUsize::new(usize::MAX);
static NET_CAP: AtomicUsize = AtomicUsize::new(usize::MAX);
/// Веха 202.2 — канал к звуковому серверу. Ищется ПО ИМЕНИ и никогда по позиции: с сетью мы
/// этот урок уже оплатили заходом (см. ниже).
static SND_CAP: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Разобрать окружение и запомнить права. Зовётся первой строкой `_start`.
fn resolve_caps() {
    let by_name = |name: &str, fallback: usize| {
        sys::cap_named(name).unwrap_or_else(|| sys::start_cap(fallback))
    };
    FS_CAP.store(by_name("POSIXFS", 0), Ordering::Relaxed);
    STORE_CAP.store(by_name("STORE", 1), Ordering::Relaxed);
    // Веха 199.14 — СЕТЬ БЕРЁМ ТОЛЬКО ЕСЛИ ЭТО ДЕЙСТВИТЕЛЬНО КАНАЛ.
    //
    // Запасной путь `start_cap(2)` — позиционный, и он верен ровно для той строки конфига, где
    // канал к сети стоит третьим (`shell vsh endpoint:posixfs store:rwx endpoint:net-srv …`).
    // В оконном сеансе шелл рождается спавном композитора, набор прав у него другой, и по этой
    // позиции лежит что угодно. Дальше `ping` звал `SYS_CALL` на нём, ядро отвечало отказом — а
    // шелл печатал «нет ответа», как будто молчит адрес в сети. Ровно та ловушка, ради которой
    // Веха 99.1 завела имена прав, только теперь с сетью.
    //
    // Проверяем ВИД (`cap_info`): не эндпоинт — значит сети у нас нет, и говорить надо это.
    // СПЕРВА СПРАШИВАЕМ КОМПОЗИТОРА, и только потом смотрим на своё позиционное право.
    //
    // Порядок именно такой, и это не мелочь. Проверить у своего права ВИД мало: эндпоинтов у
    // шелла несколько (файловый сервер, композитор), все вида 4, и по позиции 2 в оконном сеансе
    // лежит один из них. Шелл брал его, `SYS_CALL` уходил не туда, и ответ «служба не ответила»
    // выглядел как поломка сети. Отличить сетевой канал от прочих по своим силам нельзя — адресат
    // известен только тому, кто знает, кто такой `net-srv`.
    //
    // А композитор это знает: он ищет службу по имени в списке процессов (`find_net_ep`) и потому
    // отдаёт ровно тот канал, который нужен. Своё право остаётся запасным путём — для шелла из
    // конфига поколения (`shell vsh … endpoint:net-srv …`), где композитора нет вовсе.
    let asked = sys::win::grant(4);
    // В ОКНЕ запасного пути нет: композитор не дал — значит не дал, и брать вместо канала к сети
    // что попало вида 4 (а там лежит либо он сам, либо файловый сервер) значит слать запросы не
    // туда и объяснять потом молчание сети. Запасной путь — только для шелла БЕЗ композитора.
    let net = match (asked, sys::win::endpoint()) {
        (a, _) if a != sys::NO_CAP => a,
        (_, Some(_)) => sys::NO_CAP,
        (_, None) => by_name("NET_SRV", 2),
    };
    // Звук — по имени, и без запасного позиционного пути: взять «что-то вида 4» значило бы
    // слать просьбы играть файловому серверу. Нет имени — нет звука, и это честный ответ.
    SND_CAP.store(sys::snd_cli::find_cap().unwrap_or(sys::NO_CAP), Ordering::Relaxed);
    let net_ok = net != sys::NO_CAP && matches!(sys::cap_info(net), Some((4, _)));
    NET_CAP.store(if net_ok { net } else { sys::NO_CAP }, Ordering::Relaxed);
    if !net_ok {
        // Молчать нельзя: «сети нет вовсе» и «сеть есть, но нам её не дали» лечатся по-разному,
        // и второе — строка в конфиге, которую иначе не найдёт никто.
        //
        // Веха 199.15 — печатаем ВСЮ свою пачку прав, а не только негодное. Догадка «по позиции
        // лежит что-то не то» стоила захода; список с видами называет положение дел целиком и
        // сразу, а стоит он одной строки на загрузку и только когда сети действительно нет.
        let mut s = alloc::string::String::from("[vvsh] канала к сети нет. Мои стартовые права:\n");
        for i in 0..12 {
            let c = sys::start_cap(i);
            if c == sys::NO_CAP {
                break;
            }
            match sys::cap_info_ex(c) {
                Some((k, r, peer)) => s.push_str(&alloc::format!(
                    "[vvsh]   {} → {} (вид {}, права {:#x}{})\n",
                    i, sys::cap_kind_name(k), k, r,
                    if k == 4 { alloc::format!(", адресат P{}", peer) } else { alloc::string::String::new() },
                )),
                None => s.push_str(&alloc::format!("[vvsh]   {} → пусто\n", i)),
            }
        }
        s.push_str("[vvsh] лечится строкой `desktop net bin/vvsh` в конфиге поколения + rebuild\n");
        sys::write_console(s.as_bytes());
    }
}

/// Файловый сервер (persona posixfs).
fn cap_fs() -> usize {
    FS_CAP.load(Ordering::Relaxed)
}
/// Объектный store: чтение/запись объектов и ЗАПУСК программ.
fn cap_store() -> usize {
    STORE_CAP.load(Ordering::Relaxed)
}
/// Сетевой сервер.
fn cap_net() -> usize {
    NET_CAP.load(Ordering::Relaxed)
}
/// Звуковой сервер. `NO_CAP` — звука в системе нет, и это нормальный случай.
fn cap_snd() -> usize {
    SND_CAP.load(Ordering::Relaxed)
}

// ── программа ───────────────────────────────────────────────────────────────
#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    resolve_caps();
    // Веха 178 — ЯЗЫК ВЫВОДА из конфига поколения: та же строка `ui("language", …)`, по которой
    // говорят панель и окна. Шелл берёт её отсюда, а не из своего файла, потому что язык — это
    // свойство системы, а не одной программы, и откатываться он обязан вместе с ней.
    //
    // Переведён только тот вывод, который читает ПОЛЬЗОВАТЕЛЬ: справка и ответы команд. Журнал
    // ядра остаётся русским — его читает тот, кто чинит систему.
    if let Some(text) = read_generation_text() {
        sys::i18n::set_from_config(&text);
    }
    // Каталог объявляем СРАЗУ, а не только при `cd`: программа, запущенная первой командой,
    // должна понимать относительный путь так же, как двадцатой.
    publish_cwd(b"/");
    let args = sys::argv::Argv::take();
    let mut argv = args.rest();
    let sub = argv.next().unwrap_or(&[]);

    // Веха 167 — АРГУМЕНТ-КАТАЛОГ: `vvsh /etc/system` открывает шелл ПРЯМО ТАМ.
    //
    // Подкомандой это делать нельзя (`vvsh cd /etc` уехало бы в разбор команд), а путь от
    // подкоманды отличается однозначно: он начинается с косой черты, а подкоманды — слова.
    // Нужно это файловому менеджеру: «открыть в терминале» без каталога открывает терминал не
    // там, куда человек смотрит, и весь смысл пункта в этом одном слове.
    if sub.starts_with(b"/") {
        if px::stat(cap_fs(), sub).is_some_and(|(d, _)| d) {
            // ДВА действия, а не одно: `cwd_set` — наш собственный каталог (по нему строится
            // приглашение и разрешаются относительные пути), `publish_cwd` — то же самое ДЕТЯМ.
            // Первый раз я позвал только второе, и шелл открылся в корне, честно объявив детям
            // чужой каталог.
            cwd_set(sub);
            publish_cwd(sub);
        }
        cmd_repl();
    }

    if sub == b"eval" {
        match argv.next() {
            Some(path) => cmd_eval(path),
            None => {
                sys::write("vvsh: eval: нужен путь к .vv-файлу\n".as_bytes());
                sys::exit(2);
            }
        }
    } else if sub == b"init-config" {
        cmd_init_config();
    } else if sub == b"rebuild" {
        cmd_rebuild();
    } else if sub == b"gens" {
        cmd_gens();
    } else if sub == b"repl" {
        cmd_repl();
    } else if sub.is_empty() {
        sys::write("vvsh - конфиг/язык VOID (ADR 0006)\n".as_bytes());
        sys::write("  vvsh repl         интерактивный Lisp-REPL (шелл; (exit) — назад в vsh)\n".as_bytes());
        sys::write("  vvsh eval FILE    вычислить .vv и напечатать нормализованный конфиг\n".as_bytes());
        sys::write("  vvsh init-config  посеять модульный конфиг /etc/system/*.vv\n".as_bytes());
        sys::write("  vvsh rebuild      /etc/system/default.vv → новое поколение (после ребута)\n".as_bytes());
        sys::write("  vvsh gens         показать поколения системы\n".as_bytes());
        sys::exit(0);
    } else {
        sys::write(sys::i18n::t("vvsh: неизвестная подкоманда: ").as_bytes());
        sys::write(sub);
        sys::write(b"\n");
        sys::exit(2);
    }
}

/// `eval FILE` — вычислить и напечатать нормализованный конфиг (без коммита).
fn cmd_eval(path: &[u8]) -> ! {
    let ep = cap_fs();
    let text = match read_config_text(ep, path) {
        Ok(t) => t,
        Err(code) => sys::exit(code),
    };
    let loader = FsLoader { ep, base: dirname(path) };
    match vvsh_core::build_config_with(&text, &loader) {
        Ok(out) => {
            sys::write(out.as_bytes());
            sys::exit(0);
        }
        Err(e) => fail(&e),
    }
}

/// `init-config` (подкоманда) — посеять конфиг и выйти. Логика — в [`run_init_config`] (её же
/// зовёт одноимённая команда REPL, чтобы не дублировать).
fn cmd_init_config() -> ! {
    let argv = sys::argv::Argv::take();
    let force = argv.rest().any(|w| w == b"--force");
    run_init_config(force);
    sys::exit(0);
}

/// Шаблон модуля конфига: куда пишем, что пишем и под каким корнем помним ПОСЕЯННОЕ.
///
/// Веха 191 — третье поле и есть вся суть. Зная, что мы посеяли в прошлый раз, можно ответить на
/// единственный вопрос, который здесь важен: **правил ли человек этот файл?** Если не правил —
/// обновить его безопасно и незачем спрашивать. Если правил — трогать нельзя ни при каких
/// обстоятельствах, и остаётся сказать, чего в нём не хватает.
///
/// Ответ даёт сам store: объект адресуется содержимым, поэтому «файл равен посеянному» — это
/// равенство двух content-id, а не сравнение текстов.
struct Tpl {
    path: &'static [u8],
    text: &'static str,
    seed: &'static str,
}

/// Посеять модульный конфиг в `/etc/system/` (posixfs, start-cap 0). Идемпотентно. Печатает итог.
const TPLS: [Tpl; 10] = [
    Tpl { path: b"/etc/system/net.vv", text: NET_VV, seed: "system/seed/net.vv" },
    Tpl { path: b"/etc/system/services.vv", text: SERVICES_VV, seed: "system/seed/services.vv" },
    Tpl {
        path: b"/etc/system/networking.vv",
        text: NETWORKING_VV,
        seed: "system/seed/networking.vv",
    },
    Tpl { path: b"/etc/system/terminal.vv", text: TERMINAL_VV, seed: "system/seed/terminal.vv" },
    Tpl { path: b"/etc/system/bar.vv", text: BAR_VV, seed: "system/seed/bar.vv" },
    Tpl { path: b"/etc/system/packages.vv", text: PACKAGES_VV, seed: "system/seed/packages.vv" },
    Tpl { path: b"/etc/system/apps.vv", text: APPS_VV, seed: "system/seed/apps.vv" },
    Tpl { path: b"/etc/system/autostart.vv", text: AUTOSTART_VV, seed: "system/seed/autostart.vv" },
    Tpl { path: b"/etc/system/hardware.vv", text: HARDWARE_VV, seed: "system/seed/hardware.vv" },
    Tpl { path: DEFAULT_PATH, text: DEFAULT_VV, seed: "system/seed/default.vv" },
];

/// Посеять конфиг ИЛИ сверить его с шаблоном (Веха 191).
///
/// До этой вехи команда просто перезаписывала все девять файлов. На чистой системе это верно, а
/// на живой — разрушительно, и потому ею никто не пользовался. Следствие вышло тихое и обидное:
/// **каждая веха, добавлявшая ключ, была невидима для уже установленных систем.** Ровно так
/// `ui("language", …)` от Вехи 178 не доехал до рабочего образа, и английский выглядел «выбран,
/// но не работает» — при полностью исправном механизме перевода.
///
/// Теперь у каждого файла три исхода, и решает их store:
///
/// - **файла нет** — пишем (новый модуль достаётся даром);
/// - **файл равен посеянному** — человек его не трогал, обновляем молча;
/// - **файл отличается** — НЕ ТРОГАЕМ и говорим, каких ключей в нём не хватает.
///
/// `--force` возвращает прежнее поведение: перезаписать всё. Оно осталось, но теперь его надо
/// попросить вслух.
fn run_init_config(force: bool) {
    let ep = cap_fs();
    let store = cap_store();
    px::mkdir(ep, b"/etc"); // идемпотентно: если есть — MAX, игнорируем
    px::mkdir(ep, b"/etc/system");

    let (mut written, mut updated, mut same, mut kept) = (0usize, 0usize, 0usize, 0usize);
    let mut bad = false;
    let mut news: Vec<(&[u8], Vec<String>)> = Vec::new();

    for t in &TPLS {
        let cur = read_file(ep, t.path);
        // Файл УЖЕ такой же, как шаблон, — писать нечего. Отдельный случай, а не «обновлён»:
        // сказать «обновлено 9» там, где не изменилось ничего, значит приучить не читать отчёт.
        let tpl_id = content_id(store, t.text.as_bytes());
        if let Some(c) = &cur {
            if tpl_id.is_some() && content_id(store, c) == tpl_id {
                if seed_id(store, t.seed) != tpl_id {
                    remember_seed(store, t.seed, t.text.as_bytes());
                }
                same += 1;
                continue;
            }
        }
        let untouched = match (&cur, seed_id(store, t.seed)) {
            (None, _) => true,                       // файла нет — писать можно
            (Some(c), Some(id)) => content_id(store, c) == Some(id),
            (Some(_), None) => false,                // посев не помним — считаем правленым
        };
        if force || untouched {
            // Веха 101 — КАЖДАЯ запись проверяется. Сев `terminal.vv` однажды доехал наполовину
            // и оборвался посреди буквы, а сообщение об успехе печаталось как ни в чём не бывало.
            if !px::echo_to(ep, t.path, t.text.as_bytes()) {
                sys::write(sys::i18n::t("vvsh: НЕ УДАЛОСЬ записать ").as_bytes());
                sys::write(t.path);
                sys::write(b"\n");
                bad = true;
                continue;
            }
            remember_seed(store, t.seed, t.text.as_bytes());
            if cur.is_none() {
                written += 1;
            } else {
                updated += 1;
            }
            continue;
        }
        // Правленый файл: не трогаем, но говорим, чего в нём нет.
        kept += 1;
        let text = cur.unwrap_or_default();
        let missing = missing_keys(t.text, &text);
        if !missing.is_empty() {
            news.push((t.path, missing));
        }
    }

    if bad {
        sys::write(
            sys::i18n::t("vvsh: конфиг посеян НЕПОЛНО — чинить до `rebuild`\n").as_bytes(),
        );
        return;
    }
    sys::write(
        tf("vvsh: конфиг сверен: создано {}, обновлено {}, без изменений {}, оставлено с правками {}\n", &[
            &alloc::format!("{}", written),
            &alloc::format!("{}", updated),
            &alloc::format!("{}", same),
            &alloc::format!("{}", kept),
        ])
        .as_bytes(),
    );
    for (path, keys) in &news {
        sys::write(b"  ");
        sys::write(path);
        sys::write(
            sys::i18n::t(" — ваши правки сохранены; в шаблоне появилось:\n").as_bytes(),
        );
        for k in keys {
            sys::write(alloc::format!("      {}…)\n", k).as_bytes());
        }
    }
    if !news.is_empty() {
        sys::write(
            sys::i18n::t(
                "  Дописать — руками, в нужный список: `ved <файл>` (^S сохранить, ^Q выход).\n\
                 Перезаписать файл шаблоном ЦЕЛИКОМ (правки пропадут): `init-config --force`.\n",
            )
            .as_bytes(),
        );
    }
    if written > 0 {
        sys::write(
            sys::i18n::t(
                "  Правь net.vv (true/false), terminal.vv (экран, клавиши), bar.vv (панель),\n\
                 packages.vv (пакеты) → `rebuild`.\n",
            )
            .as_bytes(),
        );
    }
}

/// Content-id посеянного в прошлый раз, если помним.
fn seed_id(store: usize, root: &str) -> Option<[u8; 32]> {
    let mut id = [0u8; 32];
    (sys::obj_get_root(store, root.as_bytes(), &mut id) == 32).then_some(id)
}

/// Content-id этого содержимого. Store адресуется содержимым, поэтому «положить» равное значит
/// получить тот же id и ни одного нового объекта.
fn content_id(store: usize, data: &[u8]) -> Option<[u8; 32]> {
    let mut id = [0u8; 32];
    (sys::obj_put(store, data, &mut id) == 0).then_some(id)
}

fn remember_seed(store: usize, root: &str, data: &[u8]) {
    if let Some(id) = content_id(store, data) {
        sys::obj_set_root(store, root.as_bytes(), &id);
    }
}

/// Ключи, которые есть в шаблоне и не УПОМЯНУТЫ в правленом файле.
///
/// Сравнение текстовое и намеренно грубое: ищем вхождения вида `имя("ключ"`. Разбирать
/// правленый файл языком нельзя — он программа, и «ключа нет» надо понимать как «о нём нигде не
/// написано», а не как «он не вычисляется». Закомментированный ключ считается упомянутым: человек
/// про него знает и решил не включать — напоминать об этом было бы шумом.
fn missing_keys(tpl: &str, cur: &[u8]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let b = tpl.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] != b'(' || i + 1 >= b.len() || b[i + 1] != b'"' {
            i += 1;
            continue;
        }
        // Имя функции — слово слева от скобки.
        let mut s = i;
        while s > 0 && (b[s - 1].is_ascii_alphanumeric() || b[s - 1] == b'_' || b[s - 1] == b'-') {
            s -= 1;
        }
        // Ключ — строка справа.
        let Some(end) = b[i + 2..].iter().position(|&c| c == b'"') else { break };
        let key = &b[s..i + 2 + end + 1];
        i += 2 + end + 1;
        if s == i || key.len() > 64 {
            continue;
        }
        let Ok(k) = core::str::from_utf8(key) else { continue };
        if out.iter().any(|x| x == k) {
            continue;
        }
        if !contains(cur, key) {
            out.push(String::from(k));
        }
    }
    out
}



/// `rebuild` (подкоманда) — собрать поколение и выйти. Логика — в [`run_rebuild`].
fn cmd_rebuild() -> ! {
    run_rebuild();
    sys::exit(0);
}

/// Вычислить `/etc/system/default.vv` → коммит нового поколения `system/gen<N>` → двинуть
/// `current`. Печатает итог/ошибку и ВОЗВРАЩАЕТСЯ (не выходит — годится и для REPL).
fn run_rebuild() {
    let ep = cap_fs();
    let scap = cap_store();
    let text = match read_config_text(ep, DEFAULT_PATH) {
        Ok(t) => t,
        Err(_) => {
            sys::write(
                sys::i18n::t("vvsh: нет /etc/system/default.vv — сначала `init-config`\n")
                    .as_bytes(),
            );
            return;
        }
    };
    let loader = FsLoader { ep, base: dirname(DEFAULT_PATH) };
    let norm = match vvsh_core::build_config_with(&text, &loader) {
        Ok(out) => out,
        Err(e) => {
            sys::write(sys::i18n::t("vvsh: ошибка: ").as_bytes());
            sys::write(e.as_bytes());
            sys::write(b"\n");
            return;
        }
    };
    // Веха 220.1 — СТРОГОСТЬ. Несоответствие в конфиге — ошибка СБОРКИ, а не строка «(пропуск)»
    // в журнале загрузки, которую на машине без COM-порта не читает никто.
    if !check_strict(scap, &norm) {
        return;
    }

    // Веха 219.1 — КОРНИ, НАЗВАННЫЕ КОНФИГОМ, обязаны существовать.
    //
    // Секретов в конфиге нет: пароль сети — объект store, а в строке сервиса стоит имя его корня
    // со знаком `@` (`arg:key=@wifi/upc`). Ровно поэтому конфиг можно показывать и копировать — и
    // ровно поэтому копия может назвать корень, которого на этой машине нет. Тогда это ошибка
    // СБОРКИ, а не загадка при подключении: замысел владельца и то, как ведёт себя NixOS —
    // ссылающаяся в никуда система не собирается вовсе.
    if !check_store_refs(scap, &norm) {
        return;
    }

    // Содержимое поколения — нормализованный текст (контент-адресуемо).
    let mut new_id = [0u8; 32];
    sys::obj_put(scap, norm.as_bytes(), &mut new_id);

    // Dedup: если конфиг уже в текущем поколении — не плодить (требует READ; иначе пропускаем).
    let cur = read_current_name(scap);
    if let Some(cn) = &cur {
        if let Some(cid) = gen_content_id(scap, cn) {
            if cid == new_id {
                sys::write(
                    tf("vvsh: нет изменений — конфиг уже в поколении {}\n", &[
                        core::str::from_utf8(cn).unwrap_or("?"),
                    ])
                    .as_bytes(),
                );
                // Пакеты синхронизируются ВСЁ РАВНО: «конфиг тот же» не значит «обещанное
                // выполнено». Прошлый `rebuild` мог не достать пакет (не было сети или индекса),
                // и тогда повторный `rebuild` — ровно то, чем человек это чинит.
                sync_packages();
                return;
            }
        }
    }

    // Веха 220.1 — СКАЗАТЬ, ЧТО ТЕРЯЕТСЯ. Не отказ: убрать сервис можно и нарочно. Но молча
    // система меняется только в одну сторону — становится беднее, а человек узнаёт об этом
    // последним. Ровно так дважды пропадала сеть.
    if let Some(cn) = &cur {
        if let Some(старый) = gen_content_id(scap, cn).and_then(|id| gen_text(scap, &id)) {
            let потери = vvsh_core::lost_entries(&старый, &norm);
            if !потери.is_empty() {
                sys::write(
                    tf("vvsh: ВНИМАНИЕ — по сравнению с {} теряется:\n", &[
                        core::str::from_utf8(cn).unwrap_or("?"),
                    ])
                    .as_bytes(),
                );
                for p in &потери {
                    sys::write(tf("vvsh:   {}\n", &[p]).as_bytes());
                }
            }
        }
    }

    // Новое поколение gen<N> (N = max существующих + 1) + активировать (current).
    let Some(num) = next_gen_number(scap) else {
        sys::write(
            sys::i18n::t(
                "vvsh: список корней store не читается целиком — номер поколения не выдумываем\n",
            )
            .as_bytes(),
        );
        return;
    };
    let name = alloc::format!("gen{}", num);
    let root = alloc::format!("system/{}", name);
    sys::obj_set_root(scap, root.as_bytes(), &new_id);
    let mut nm_id = [0u8; 32];
    sys::obj_put(scap, name.as_bytes(), &mut nm_id);
    sys::obj_set_root(scap, CURRENT_ROOT, &nm_id);

    // Фраза целиком, а не склейка: «было gen8» в другом языке стоит в другом месте.
    match &cur {
        Some(cn) => sys::write(
            tf("vvsh: собрано поколение {} (активно после ребута); было {}\n", &[
                &name,
                core::str::from_utf8(cn).unwrap_or("?"),
            ])
            .as_bytes(),
        ),
        None => sys::write(
            tf("vvsh: собрано поколение {} (активно после ребута)\n", &[&name]).as_bytes(),
        ),
    }
    sync_packages();
}

/// Арх-измерение корней программ: они лежат как `bin/<арх>/<имя>` (зеркало `prog_root` в ядре).
#[cfg(target_arch = "x86_64")]
const ARCH: &str = "x86_64";
#[cfg(target_arch = "riscv64")]
const ARCH: &str = "riscv64";

/// Веха 220.1 — ПРОВЕРИТЬ КОНФИГ СТРОГО. `false` — собирать нельзя, претензии напечатаны.
///
/// Владелец сформулировал правило прямо: «лучше давать по рукам программисту за любое
/// несоответствие, как это делает Rust» — и отдельно назвал класс, который ненавидит: молчаливые
/// ошибки. До этой вехи конфиг с опечаткой собирался, система поднималась, и единственным следом
/// была строка в журнале загрузки:
///
/// ```text
/// [init] mmio:wifii — устройство не найдено (пропуск)
/// ```
///
/// Проверяется двумя источниками. ПО ТЕКСТУ (`vvsh_core::check_config`, host-тесты) — вид и
/// буквы прав, имя устройства, ссылка на объявленный сервер. ПО STORE — есть ли названная
/// программа: `service имя` спавнит `bin/<арх>/имя`, и опечатка в нём до сих пор означала
/// молчаливо пропущенную строку конфига (в ядре `let Some(pid) = spawn(name) else { continue }`).
///
/// Чего НЕ проверяем: есть ли в ЭТОЙ машине названное устройство. Один конфиг ездит по разным
/// машинам, и `mmio:wifi` на машине без беспроводной карты — верная строка, а не ошибка.
fn check_strict(scap: usize, norm: &str) -> bool {
    let mut беды = vvsh_core::check_config(norm);
    // Программы — по списку корней store. Читается он один раз и целиком: частичный список
    // сказал бы «программы нет» о программе, которая есть.
    match roots::text(scap) {
        Some(list) => {
            for line in norm.lines() {
                let mut w = line.split_whitespace();
                let Some(kind) = w.next() else { continue };
                if !void_conf::spawns_program(kind) {
                    continue;
                }
                let Some(имя) = w.next() else { continue };
                let корень = alloc::format!("bin/{}/{}", ARCH, имя);
                if !roots::has(&list, корень.as_bytes()) {
                    беды.push(alloc::format!("{} {}: такой программы в store нет", kind, имя));
                }
            }
        }
        None => беды.push(alloc::string::String::from(
            "список корней store не читается — проверить программы нечем",
        )),
    }
    if беды.is_empty() {
        return true;
    }
    for b in &беды {
        sys::write(tf("vvsh: {}\n", &[b]).as_bytes());
    }
    sys::write(sys::i18n::t("vvsh: поколение не собрано\n").as_bytes());
    false
}

/// Веха 219.1 — проверить корни, названные конфигом. `false` — собирать нельзя, причина напечатана.
///
/// Список корней читается ОДИН раз и целиком: спрашивать по одному значило бы на полпути получить
/// другую картину, а частичный список здесь хуже отсутствующего — он даёт «корня нет» о корне,
/// который есть.
fn check_store_refs(scap: usize, norm: &str) -> bool {
    let refs = vvsh_core::store_refs(norm);
    if refs.is_empty() {
        return true;
    }
    let Some(list) = roots::text(scap) else {
        sys::write(
            sys::i18n::t(
                "vvsh: список корней store не читается целиком — проверить названные конфигом нечем\n",
            )
            .as_bytes(),
        );
        return false;
    };
    let mut плохих = 0;
    for r in refs {
        if r.is_empty() {
            sys::write(sys::i18n::t("vvsh: в конфиге `=@` без имени корня\n").as_bytes());
            плохих += 1;
        } else if !roots::has(&list, r.as_bytes()) {
            sys::write(tf("vvsh: нет корня {} — задай его `secret {}`\n", &[r, r]).as_bytes());
            плохих += 1;
        }
    }
    if плохих > 0 {
        sys::write(sys::i18n::t("vvsh: поколение не собрано\n").as_bytes());
        return false;
    }
    true
}

/// Достроить к поколению системы его пакеты (Веха 112).
///
/// Сборка системы — это не только строки для ядра: конфиг объявляет ещё и `packages …`. Достать
/// их умеет `pkg`, и зовём мы именно ЕГО — программой, а не куском кода внутри шелла. Довод тот
/// же, по которому `pkg` отделён от `vsh`: сеть, криптография и два распаковщика не должны жить
/// в процессе, который обязан пережить любую их ошибку. Полномочия `pkg` получает по
/// наследству, ничего сверх шелловских.
///
/// Сеть тут не обязательна: если всё объявленное уже в store, `sync` не сделает ни одной
/// загрузки. Незадача с пакетами НЕ отменяет собранного поколения системы — конфиг ядра и
/// терминала уже записан, и терять его из-за отвалившегося кэша было бы хуже, чем сказать
/// вслух, что пакеты не собрались.
fn sync_packages() {
    let code = px::spawn_args(cap_store(), b"pkg", b"sync\0");
    if code == usize::MAX {
        sys::write(
            sys::i18n::t("vvsh: pkg не запустился — пакеты конфига не собраны\n").as_bytes(),
        );
    } else if code != 0 {
        sys::write(
            tf("vvsh: pkg sync вернул [код {}] — пакеты не собраны\n", &[&alloc::format!("{}", code)])
                .as_bytes(),
        );
    }
}

/// `gens` (подкоманда) — перечислить поколения и выйти. Логика — в [`run_gens`].
fn cmd_gens() -> ! {
    run_gens();
    sys::exit(0);
}

/// Перечислить поколения `system/gen*` и пометить активное (`*`). Печатает; ВОЗВРАЩАЕТСЯ.
fn run_gens() {
    let scap = cap_store();
    let cur = read_current_name(scap);

    let Some(text) = roots::text(scap) else {
        sys::write(
            sys::i18n::t("vvsh: список корней store не прочитать (нужен store READ/WRITE)\n")
                .as_bytes(),
        );
        return;
    };
    let nums = roots::gen_numbers(&text, b"system/gen");

    sys::write(sys::i18n::t("поколения системы (активно — *):\n").as_bytes());
    if nums.is_empty() {
        sys::write(sys::i18n::t("  (нет собранных поколений — `rebuild`)\n").as_bytes());
    }
    for k in nums {
        let name = alloc::format!("gen{}", k);
        sys::write(b"  ");
        sys::write(name.as_bytes());
        if cur.as_deref() == Some(name.as_bytes()) {
            sys::write(" *".as_bytes());
        }
        sys::write(b"\n");
    }
    if cur.is_none() {
        sys::write(
            sys::i18n::t("  (активное поколение не прочитать — нужен store READ)\n").as_bytes(),
        );
    }
}

/// `repl` (S2a/S2b) — интерактивный шелл-REPL. Окружение ЖИВЁТ между строками (`(define x 5)` →
/// потом `(* x x)` → 25). Гибридный синтаксис: строка с `(`/`'` — Lisp-выражение (eval + печать
/// результата); иначе — КОМАНДА (голые слова, `ls /etc` ≡ `(ls "/etc")`; несвязанное имя → спавн
/// программы, как PATH). Запуск из vsh: `run vvsh repl`; выход — `(exit)`/`exit`/Ctrl-D → назад в
/// vsh (внешний спасательный шелл — если vvsh упадёт, он ловит обратно).
fn cmd_repl() -> ! {
    sys::write(C_PROMPT);
    sys::write(b"vvsh");
    sys::write(C_RESET);
    sys::write(
        sys::i18n::t(
            " — шелл VOID (ADR 0006/0013). `\\выражение` — вычислить, иначе команда. `help` — команды, `exit` — назад в vsh.\n",
        )
        .as_bytes(),
    );
    // Веха 99.3 — размер СВОЕГО окна, если мы живём в панели мультиплексора. Аналог TIOCSWINSZ,
    // только опрашиваемый: сигналов у нас нет, а сходить к хосту программа и так умеет.
    // Печатаем его в баннере не ради красоты — так сразу видно, что программа знает, куда рисует.
    if let Some((cols, rows)) = sys::stdio::win_size() {
        let mut line = alloc::string::String::new();
        use core::fmt::Write;
        let _ = write!(line, "{}: {cols}×{rows}\n", sys::i18n::t("окно"));
        sys::write(line.as_bytes());
    }
    let loader = vvsh_core::NoLoader;
    let programs = Programs;
    let interp = vvsh_core::Interp::new(&loader).with_runner(&programs);
    let env = shell_env(); // ПЕРСИСТЕНТНОЕ окружение сессии (чистые builtins + команды-эффекты)
    let mut line = [0u8; LINE_CAP];
    let mut hist = History::new();
    loop {
        let mut pbuf = [0u8; 320];
        let plen = build_prompt(&mut pbuf);
        let len = match read_line(&pbuf[..plen], &mut line, &hist) {
            Some(l) => l,
            None => {
                sys::write(b"\n");
                break; // EOF (Ctrl-D)
            }
        };
        let src = trim(&line[..len]);
        if src.is_empty() {
            continue;
        }
        hist.push(src);
        if src == b"exit" || src == b"quit" {
            break;
        }
        // Веха 102 (ADR 0013) — выражение открывает ведущий `\`, а не скобка. Раньше признаком
        // была `(`, но в новом синтаксисе скобка — это скобка ВЫЗОВА, и `ls(…)` неотличимо от
        // команды `ls`. Backslash выбран по свойству, которого нет у других кандидатов: ни одна
        // команда и ни один путь с него не начинаются, поэтому двусмысленности нет ни в одну
        // сторону. Всё прочее — команда, как и было (голые слова: `ls`, `ping 10.0.2.2`).
        if src[0] == b'\\' {
            expr_line(&interp, &env, trim(&src[1..])); // выражение vvsh
        } else {
            command_line(&interp, &env, src); // команда (голые слова)
        }
    }
    sys::write(sys::i18n::t("vvsh: выход из REPL — vsh продолжает\n").as_bytes());
    sys::exit(0);
}

/// Строка-ВЫРАЖЕНИЕ (после ведущего `\`): распарсить, вычислить каждую форму, напечатать
/// непустой результат.
fn expr_line(interp: &vvsh_core::Interp, env: &Env, src: &[u8]) {
    let text = match core::str::from_utf8(src) {
        Ok(t) => t,
        Err(_) => return sys::write(sys::i18n::t("ошибка: ввод не UTF-8\n").as_bytes()),
    };
    match vvsh_core::read_all(text) {
        Ok(forms) => {
            for f in &forms {
                match interp.eval(f, env) {
                    Ok(v) => render(&v),
                    Err(e) => print_err(&e),
                }
            }
        }
        Err(e) => {
            sys::write(sys::i18n::t("ошибка разбора: ").as_bytes());
            sys::write(e.0.as_bytes());
            sys::write(b"\n");
        }
    }
}

/// Строка-КОМАНДА (голые слова). Первое слово — имя, остальные — строковые аргументы. Разрешение:
/// связано с вызываемым (builtin/замыкание) → вызвать (это команда, вывод от неё); связано со
/// значением и без аргументов → показать (инспекция переменной); не связано → спавн программы (PATH).
fn command_line(interp: &vvsh_core::Interp, env: &Env, src: &[u8]) {
    // Веха 161 — слова режет [`vvsh_core::split_words`]: пробелы разделяют, КАВЫЧКИ СКЛЕИВАЮТ и
    // в аргумент не попадают. Раньше здесь стоял голый `split` по пробелу, и `ls "/etc"` шёл
    // спрашивать путь `/"/etc"` — молча пустой ответ, а у `cat` ещё и «файл не найден» про файл,
    // который был на месте.
    let text = match core::str::from_utf8(src) {
        Ok(t) => t,
        Err(_) => return sys::write(sys::i18n::t("vvsh: ввод не UTF-8\n").as_bytes()),
    };
    let owned = match vvsh_core::split_words(text) {
        Ok(w) => w,
        Err(m) => {
            sys::write("vvsh: ".as_bytes());
            sys::write(m.as_bytes());
            sys::write(b"\n");
            return;
        }
    };
    let words: alloc::vec::Vec<&[u8]> = owned.iter().map(|w| w.as_bytes()).collect();
    if words.is_empty() {
        return;
    }
    // Веха 221 — КОНВЕЙЕР ЦЕПОЧКОЙ, а не одним звеном.
    //
    // `klog |> grep wifi |> send 192.168.0.87 9000 > /etc/копия.txt` читается ровно так, как
    // звучит: каждая команда берёт текст предыдущей последним аргументом, а `>` в конце кладёт
    // итог в файл. Разбор — в [`run_pipeline`]; здесь только развилка «есть ли в строке знак».
    if words.iter().any(|w| *w == b">>" || *w == b"|>" || *w == b">") {
        return run_pipeline(interp, env, &words);
    }
    let head = match core::str::from_utf8(words[0]) {
        Ok(s) => s,
        Err(_) => return sys::write(sys::i18n::t("vvsh: имя команды не UTF-8\n").as_bytes()),
    };
    match env.lookup(head) {
        Some(v) if is_callable(&v) => match build_command_form(&words, env) {
            Ok(form) => match interp.eval(&form, env) {
                Ok(result) => render(&result), // вывод команды-данных (ls) рендерит хост
                Err(e) => print_err(&e),
            },
            Err(m) => {
                sys::write("vvsh: ".as_bytes());
                sys::write(m.as_bytes());
                sys::write(b"\n");
            }
        },
        Some(v) => {
            if words.len() == 1 {
                render(&v); // инспекция переменной
            } else {
                sys::write("vvsh: '".as_bytes());
                sys::write(words[0]);
                sys::write(sys::i18n::t("' — значение, а не команда (даны аргументы)\n").as_bytes());
            }
        }
        None => spawn_program(words[0], &words[1..]), // PATH: несвязанное имя → программа
    }
}

/// Веха 221 — КОНВЕЙЕР: `первая |> вторая |> третья [> файл]`.
///
/// ## Почему это переписано
///
/// Конвейер был на одно звено (`klog >> send`) и брал текст только у команд, ВОЗВРАЩАЮЩИХ его
/// значением. Половина команд его печатала — и владелец наткнулся на это ровно тогда, когда
/// конвейер был нужен по делу: `cat /etc/лог.txt >> send …` отправлял пустоту, потому что `cat`
/// печатал файл, а возвращал ничто. Это не мелочь в одной команде, а расхождение в устройстве:
/// **команда отдаёт текст, печатает его шелл** — правило было записано у `ls` и не соблюдено
/// у остальных.
///
/// ## Как теперь
///
/// - звенья разделяются `|>` или `>>` — это одно и то же; первый знак родной языку (там он уже
///   работает), второй выбрал владелец для командной строки, и отнимать его незачем;
/// - текст левого звена приезжает правому ПОСЛЕДНИМ аргументом (thread-last, как в языке);
/// - `> путь` в конце пишет итог в файл;
/// - звеном может быть и программа: её вывод шелл собирает, будучи хозяином её stdio
///   ([`run_captured`]).
///
/// Пустой ответ звена — ошибка ВСЛУХ, а не пустой файл на той стороне: молчаливая пустота и была
/// тем, что владелец разбирал руками.
fn run_pipeline(interp: &vvsh_core::Interp, env: &Env, words: &[&[u8]]) {
    // Хвост `> путь` отрезаем первым: он не звено, а место назначения.
    let mut тело = words;
    let mut файл: Option<alloc::vec::Vec<u8>> = None;
    if let Some(i) = words.iter().rposition(|w| *w == b">") {
        if i + 2 != words.len() {
            return sys::write(sys::i18n::t("vvsh: `>` хочет ПУТЬ и ничего после\n").as_bytes());
        }
        // Команды, разбирающие `>` сами (они пишут потоком, а не собранным текстом), остаются
        // при своём: их имя стоит первым, и звено у строки одно.
        let сама = matches!(core::str::from_utf8(words[0]), Ok("klog" | "echo" | "cat"))
            && !words[..i].iter().any(|w| *w == b">>" || *w == b"|>");
        if !сама {
            файл = Some(resolve(words[i + 1]));
            тело = &words[..i];
        }
    }
    // Звенья.
    let mut звенья: alloc::vec::Vec<&[&[u8]]> = alloc::vec::Vec::new();
    let mut от = 0usize;
    for (i, w) in тело.iter().enumerate() {
        if *w == b">>" || *w == b"|>" {
            звенья.push(&тело[от..i]);
            от = i + 1;
        }
    }
    звенья.push(&тело[от..]);
    if звенья.iter().any(|з| з.is_empty()) {
        return sys::write(
            sys::i18n::t("vvsh: у конвейера пустое звено — нужна команда с обеих сторон знака\n")
                .as_bytes(),
        );
    }
    // Гоним текст слева направо. Первое звено идёт без входа, каждое следующее получает текст
    // предыдущего последним аргументом.
    let mut текст: Option<String> = None;
    let последнее = звенья.len() - 1;
    for (n, звено) in звенья.iter().enumerate() {
        // Последнему звену текст нужен, только если итог никуда не уезжает дальше: иначе его
        // значение тоже надо взять, а не напечатать.
        let берём_значение = n < последнее || файл.is_some();
        let итог = run_stage(interp, env, звено, текст.as_deref(), берём_значение);
        match итог {
            Ok(t) => текст = t,
            Err(e) => return sys::write(e.as_bytes()),
        }
        if берём_значение && текст.is_none() {
            return sys::write(
                alloc::format!(
                    "vvsh: `{}` ничего не отдала — передавать дальше нечего\n",
                    String::from_utf8_lossy(звено[0]),
                )
                .as_bytes(),
            );
        }
    }
    // Итог: в файл либо на экран.
    let Some(path) = файл else { return };
    let Some(text) = текст else { return };
    let fd = px::open(cap_fs(), &path, px::O_TRUNC);
    if fd == usize::MAX {
        return sys::write(
            alloc::format!("vvsh: не открывается {}\n", String::from_utf8_lossy(&path)).as_bytes(),
        );
    }
    let n = px::write(cap_fs(), fd, text.as_bytes());
    px::close(cap_fs(), fd);
    sys::write(
        alloc::format!("{} {} {}\n", sys::i18n::t("записано"), n, sys::i18n::t("байт")).as_bytes(),
    );
}

/// Веха 221 — одно звено конвейера. `вход` — текст предыдущего звена (последним аргументом).
///
/// `нужен_текст` решает, что делать с результатом: отдать дальше или показать человеку. Печатает
/// звено только в последнем случае — иначе вывод оказался бы и на экране, и в файле.
fn run_stage(
    interp: &vvsh_core::Interp,
    env: &Env,
    звено: &[&[u8]],
    вход: Option<&str>,
    нужен_текст: bool,
) -> Result<Option<String>, String> {
    let head = core::str::from_utf8(звено[0]).unwrap_or("");
    // Программа: запускаем, собрав её вывод. Текста на вход ей передать пока нечем — у
    // перехваченной программы ввода нет (см. [`run_captured`]), и это честнее, чем сделать вид.
    if !env.lookup(head).is_some_and(|v| is_callable(&v)) {
        if вход.is_some() {
            return Err(alloc::format!(
                "vvsh: `{head}` — программа, а программе вход конвейера передать нечем\n"
            ));
        }
        let args: alloc::vec::Vec<&[u8]> = звено[1..].to_vec();
        let out = run_captured(звено[0], &args).ok_or_else(|| {
            alloc::format!(
                "{}{}\n",
                sys::i18n::t("vvsh: команда не найдена: "),
                head,
            )
        })?;
        if !нужен_текст {
            sys::write(out.as_bytes());
            return Ok(None);
        }
        return Ok(Some(out));
    }
    // Команда шелла: вычисляем форму, дописав вход последним аргументом.
    let form = build_command_form(звено, env).map_err(|e| alloc::format!("vvsh: {e}\n"))?;
    let form = match (вход, form) {
        (Some(t), Value::List(items)) => {
            let mut v = items.to_vec();
            v.push(Value::str(t));
            Value::list(v)
        }
        (_, f) => f,
    };
    match interp.eval(&form, env) {
        Ok(Value::Str(s)) => Ok(Some(String::from(&*s))),
        Ok(v) if !нужен_текст => {
            render(&v);
            Ok(None)
        }
        // Пустой список — это `nil`, то есть «команда сделала дело и текста не дала».
        Ok(Value::List(items)) if items.is_empty() => Ok(None),
        // Список (его отдаёт `ls`, `grep`) склеивается строками: так его и видит человек.
        Ok(Value::List(items)) => {
            let mut t = String::new();
            for it in items.iter() {
                match it {
                    Value::Str(s) => t.push_str(s),
                    other => t.push_str(&alloc::format!("{}", other)),
                }
                t.push('\n');
            }
            Ok(Some(t))
        }
        Ok(other) => Ok(Some(alloc::format!("{}", other))),
        Err(e) => Err(alloc::format!("vvsh: {}\n", e.0)),
    }
}

/// Веха 202.13/202.20 — запустить программу, СОБРАВ её вывод. `None` — программы нет.
///
/// Тот же приём, что у терминала и сторожа запуска `run`: хост чужого stdio даёт ребёнку право
/// на свой эндпоинт и объявляет `STDIO` (Веха 98). Здесь он нужен дважды — конвейеру (`>>`,
/// `>`) и языку: `\(fps 4)` тоже обязан получить вывод значением, иначе форма языка умеет
/// меньше, чем строка над ней.
fn run_captured(name: &[u8], args: &[&[u8]]) -> Option<alloc::string::String> {
    let mut blob = alloc::vec::Vec::new();
    for w in args {
        blob.extend_from_slice(w);
        blob.push(0);
    }
    let pid = sys::spawn_with_stdio(cap_store(), name, &blob, sys::self_endpoint()).or_else(|| {
        path_candidates(name)
            .into_iter()
            .find_map(|p| sys::spawn_with_stdio(cap_store(), p.as_bytes(), &blob, sys::self_endpoint()))
    });
    let pid = pid?;
    let mut out = alloc::string::String::new();
    let mut msg = [0u8; sys::stdio::CHUNK + 64];
    loop {
        // Спим коротко: пока ребёнок пишет — просыпаемся на его сообщения, пока молчит — на
        // свой срок, чтобы спросить, жив ли он. Блокирующего `wait` тут быть не может: мы его
        // stdio-хозяин, и уснув в ожидании смерти, повесили бы его на первом же `write`.
        if let Some(m) = sys::recv_timeout(&mut msg, 2_000_000) {
            match m.op & 0xff {
                sys::stdio::OP_STDOUT => {
                    let len = m.len.min(msg.len());
                    out.push_str(&alloc::string::String::from_utf8_lossy(&msg[..len]));
                    // Ответ пустой, но обязательный: он же и регулировка потока.
                    sys::reply(m.reply_cap, &[]);
                }
                // Ввода у перехваченной программы нет: клавиатура принадлежит шеллу, а не ей.
                // Пустой ответ — это конец ввода, и он честнее молчания: на молчании программа
                // повисла бы навсегда.
                sys::stdio::OP_STDIN => {
                    sys::reply(m.reply_cap, &[]);
                }
                sys::stdio::OP_WINSIZE => {
                    sys::reply(m.reply_cap, &[80, 0, 25, 0]);
                }
                _ => {
                    sys::reply(m.reply_cap, &[]);
                }
            }
            continue;
        }
        match sys::wait(pid, true) {
            sys::Wait::Running => {}
            _ => break, // вышел, или его забрал кто-то другой — ждать больше нечего
        }
    }
    Some(out)
}

/// Веха 202.20 — язык умеет запускать программы: `(fps 4)` — обычная форма.
///
/// Вывод приходит ЗНАЧЕНИЕМ, поэтому он сразу годится всему остальному языку: его можно
/// связать (`(define f (fps 4))`), передать дальше, отправить (`(send "192.168.0.223" 9000
/// (fps 4))`). Третий вид конвейера после этого не нужен: он был обходом ровно этой дыры.
struct Programs;

impl vvsh_core::Runner for Programs {
    fn run(&self, name: &str, args: &[Value]) -> Option<Result<Value, EvalError>> {
        // Аргументы — теми же строками, что у командной строки: число становится числом,
        // строка — собой. Так `(beep 440 200)` и `beep 440 200` значат одно и то же.
        let owned: alloc::vec::Vec<alloc::string::String> =
            args.iter().map(arg_string).collect();
        let refs: alloc::vec::Vec<&[u8]> = owned.iter().map(|s| s.as_bytes()).collect();
        run_captured(name.as_bytes(), &refs).map(|out| Ok(Value::str(&out)))
    }
}

/// Собрать форму применения `(имя "арг"…)` из слов команды (первое — символ, остальные — строки).
/// Аргумент `$name` подставляется значением Lisp-переменной `name` (шелл-переменные = Lisp-переменные;
/// несвязано → пустая строка, как в bash). Литеральный `$` — экранируй Lisp-режимом.
fn build_command_form(words: &[&[u8]], env: &Env) -> Result<Value, alloc::string::String> {
    let head = core::str::from_utf8(words[0]).map_err(|_| str_owned("имя команды не UTF-8"))?;
    let mut items = alloc::vec::Vec::with_capacity(words.len());
    items.push(Value::sym(head));
    for w in &words[1..] {
        if w.first() == Some(&b'$') && w.len() > 1 {
            let name = core::str::from_utf8(&w[1..]).map_err(|_| str_owned("$-имя не UTF-8"))?;
            let val = env.lookup(name).map(|v| arg_string(&v)).unwrap_or_default();
            items.push(Value::str(&val));
        } else {
            let s = core::str::from_utf8(w).map_err(|_| str_owned("аргумент не UTF-8"))?;
            items.push(Value::str(s));
        }
    }
    Ok(Value::list(items))
}

/// Значение → строка-аргумент команды: строка — как есть (без кавычек), прочее — каноничной формой.
fn arg_string(v: &Value) -> String {
    match v {
        Value::Str(s) => String::from(&**s),
        other => alloc::format!("{}", other),
    }
}

/// Спавн программы с NUL-разделёнными строковыми аргументами (наследует права shell'а через exec).
///
/// Порядок поиска (Веха 109) — сперва программы САМОЙ системы (корень store `bin/<имя>`), потом
/// **профиль**: `<пакет>/bin/<имя>` у каждого установленного пакета верхнего уровня. Свои раньше
/// чужих намеренно: пакет из nixpkgs не должен молча заслонять `vvsh` или `pkg`.
fn spawn_program(name: &[u8], arg_words: &[&[u8]]) {
    let mut blob = alloc::vec::Vec::new();
    for w in arg_words {
        blob.extend_from_slice(w);
        blob.push(0);
    }
    let mut code = px::spawn_args(cap_store(), name, &blob);
    if code == usize::MAX {
        // Кандидатов перебираем ВСЕХ по очереди, а не берём первого: в одном сторе спокойно
        // живут пакеты разных архитектур (у нас там и x86-, и riscv-glibc), и `bin/getconf`
        // есть у обоих — но запустится ровно свой.
        for path in path_candidates(name) {
            code = px::spawn_args(cap_store(), path.as_bytes(), &blob);
            if code != usize::MAX {
                break;
            }
        }
    }
    if code == usize::MAX {
        sys::write(sys::i18n::t("vvsh: команда не найдена: ").as_bytes());
        sys::write(name);
        sys::write(b"\n");
    } else if code != 0 {
        sys::write(tf("[код {}]\n", &[&alloc::format!("{}", code)]).as_bytes());
    }
}

/// Где в профиле может лежать программа `name`: `/nix/store/<пакет>/bin/<имя>` (Веха 109 — PATH).
///
/// Ищем только среди пакетов ВЕРХНЕГО УРОВНЯ: зависимости человек не устанавливал, и их `bin/` —
/// не его PATH (у nix ровно та же граница: профиль ссылается лишь на то, что просили).
///
/// Профилей с Вехи 112 два: поставленное руками и объявленное конфигом. Порядок задаёт
/// [`profile::path_items`] — здесь берётся первый кандидат, который запустился.
fn path_candidates(name: &[u8]) -> alloc::vec::Vec<alloc::string::String> {
    let ep = cap_fs();
    let mut out = alloc::vec::Vec::new();
    let Ok(name) = core::str::from_utf8(name) else { return out };
    for item in profile::path_items(cap_store()) {
        if !item.top {
            continue;
        }
        let path = alloc::format!("/nix/store/{}/bin/{}", item.base, name);
        if let Some((is_dir, _)) = px::stat(ep, path.as_bytes()) {
            if !is_dir {
                out.push(path);
            }
        }
    }
    out
}

fn is_callable(v: &Value) -> bool {
    matches!(v, Value::Builtin(..) | Value::Closure(_))
}

fn str_owned(s: &str) -> alloc::string::String {
    alloc::string::String::from(s)
}

fn print_err(e: &EvalError) {
    sys::write(sys::i18n::t("ошибка: ").as_bytes());
    sys::write(e.0.as_bytes());
    sys::write(b"\n");
}

/// Хост-рендеринг результата (модель «команды отдают значения, шелл рендерит на верхнем уровне»):
/// `()` — ничего (результат команд-«вывода»); список — по элементу на строку (строки без кавычек,
/// удобно для `ls`/конвейеров); прочее — каноничной формой.
fn render(v: &Value) {
    match v {
        Value::List(items) if items.is_empty() => {}
        Value::List(items) => {
            for it in items.iter() {
                render_atom(it);
            }
        }
        other => render_atom(other),
    }
}

/// Один атом результата: строки — без кавычек (шелл-дружелюбно), прочее — каноничной формой.
fn render_atom(v: &Value) {
    match v {
        Value::Str(s) => {
            sys::write(s.as_bytes());
            // Веха 221 — перевод строки только если его нет: содержимое файла (`cat`) им уже
            // кончается, и лишний давал бы пустую строку после каждого показа.
            if !s.ends_with('\n') {
                sys::write(b"\n");
            }
        }
        other => sys::write(alloc::format!("{}\n", other).as_bytes()),
    }
}

// ── команды-эффекты шелла (S2b): builtins в бинаре, дёргают синкаллы напрямую ──
// vvsh-core остаётся ЧИСТЫМ (конфиг использует `root_env`, эти команды — только в REPL). Caps
// приходят из `start_cap` (ambient процессу), вывод — `sys::write`; Host-trait не нужен.

/// Окружение шелл-сессии: чистые builtins vvsh-core + команды-эффекты (`ls`/`cat`/`echo`/`run`).
fn shell_env() -> Env {
    let env = vvsh_core::root_env();
    let cmds: &[(&'static str, fn(&[Value]) -> Result<Value, EvalError>)] = &[
        ("ls", sh_ls),
        ("cat", sh_cat),
        ("echo", sh_echo),
        ("run", sh_run),
        ("grep", sh_grep),
        ("cd", sh_cd),
        ("pwd", sh_pwd),
        ("log", sh_log),
        ("clear", sh_clear),
        ("help", sh_help),
        ("cp", sh_cp), // Веха 176 — копия мгновенна: то же содержимое под вторым именем
        ("date", sh_date), // Веха 86 — часы системы
        ("notify", sh_notify), // Веха 168 — сказать человеку
        ("random", sh_random), // Веха 86 — случайные байты от ядра
        // Веха 84 — перенос команд vsh в vvsh: файлы/каталоги, store, сеть, поколения.
        ("roots", sh_roots),
        ("mkdir", sh_mkdir),
        ("stat", sh_stat), // Веха 177 — вид, размер и время
        ("readlink", sh_readlink), // Веха 108.2 — симлинки есть только в дереве пакета
        ("rm", sh_rm),
        ("tail", sh_tail),
        ("mv", sh_mv),
        ("ping", sh_ping),
        ("resolve", sh_resolve), // Веха 92 — DNS
        // Веха 93 — TCP. Четыре примитива, из которых складывается обмен: соединиться, послать,
        // принять, закрыть. Клиент любого протокола пишется поверх них прямо в шелле.
        ("tcp-connect", sh_tcp_connect),
        ("tcp-send", sh_tcp_send),
        ("tcp-recv", sh_tcp_recv),
        ("tcp-close", sh_tcp_close),
        // Веха 94 — HTTP: скачать потоком в store и прочитать скачанное обратно.
        ("fetch", sh_fetch),
        ("blob", sh_blob),
        ("unroot", sh_unroot),
        ("thaw", sh_thaw),
        ("switch", sh_switch),
        ("klog", sh_klog),
        ("send", sh_send),
        // Веха 202.2 — звук: короткий сигнал. Веха 204 — громкость и миксер.
        ("beep", sh_beep),
        ("volume", sh_volume),
        // Веха 200 — заглянуть в регистры железа (см. `sh_mmio`/`sh_pci`).
        ("mmio", sh_mmio),
        ("pci", sh_pci),
        ("poweroff", sh_poweroff),
        ("reboot", sh_reboot),
        ("store-probe", sh_store_probe),
        ("nar-unpack", sh_nar_unpack),
        ("sysdef", sh_sysdef),
        ("rebuild", sh_rebuild),
        ("gens", sh_gens),
        ("init-config", sh_init_config),
        ("root", sh_root),
        ("root-del", sh_root_del),
        ("secret", sh_secret),
    ];
    for (name, f) in cmds {
        env.define(alloc::rc::Rc::from(*name), Value::Builtin(name, *f));
    }
    env
}

/// `(ls [путь])` — ВОЗВРАЩАЕТ список имён файлов каталога (по умолчанию `/`). Возврат значения, а не
/// печать: так `ls` течёт в конвейер `(| (ls) (grep "vv"))`, а на верхнем уровне REPL сам его рендерит.
fn sh_ls(args: &[Value]) -> Result<Value, EvalError> {
    let ep = cap_fs();
    let path = match args.first() {
        None => resolve(b""), // текущий каталог
        Some(Value::Str(s)) => resolve(s.as_bytes()),
        Some(other) => {
            return Err(EvalError::new(alloc::format!(
                "ls: путь — строка, дано {}",
                other.type_name()
            )))
        }
    };
    // Веха 161 — «нет такого пути» обязано ЗВУЧАТЬ. Пустой список от несуществующего каталога
    // неотличим от пустого каталога, и опечатка в пути ничем себя не выдавала: именно так
    // `ls "/etc"` (с кавычками в аргументе) целый день выглядел как «конфиг пропал».
    let shown = || String::from(core::str::from_utf8(&path).unwrap_or("?"));
    match px::stat(ep, &path) {
        Some((true, _)) => {}
        // POSIX: `ls файл` показывает сам файл, а не ошибку.
        Some((false, _)) => return Ok(Value::list(alloc::vec![Value::str(&shown())])),
        None => return Err(EvalError::new(alloc::format!("ls: нет такого пути: {}", shown()))),
    }
    // Буфер в куче и с запасом (Веха 108.2): каталог ПАКЕТА бывает в сотни имён — у glibc в
    // `lib/gconv` их 255, и на стековых 4 КиБ список снова начал бы упираться.
    let mut buf = alloc::vec![0u8; 64 * 1024];
    let (n, want) = px::readdir_ex(ep, &path, &mut buf);
    if want > n {
        // Молчаливое обрезание списка — ровно то, на чём эта система уже обжигалась (запись,
        // запрос, ответ, ввод с консоли). Пусть лучше режет глаз, чем врёт.
        sys::write(
            alloc::format!("  ! список каталога обрезан: {} из {} байт\n", n, want).as_bytes(),
        );
    }
    let mut items = alloc::vec::Vec::new();
    for name in buf[..n].split(|&b| b == b'\n') {
        if name.is_empty() {
            continue;
        }
        if let Ok(s) = core::str::from_utf8(name) {
            items.push(Value::str(s));
        }
    }
    Ok(Value::list(items))
}

/// `(grep "подстрока" список)` — оставить строки-элементы, содержащие подстроку. Для конвейеров:
/// `(| (ls) (grep "vv"))`. Подстрока — последним НЕ является; значение течёт списком-2-м аргументом.
fn sh_grep(args: &[Value]) -> Result<Value, EvalError> {
    let sub = match args.first() {
        Some(Value::Str(s)) => s.as_bytes(),
        _ => return Err(EvalError::new("grep: (grep \"подстрока\" список)")),
    };
    // Веха 221 — берём И СПИСОК, И ТЕКСТ. Список отдаёт `ls`, текст — `cat` и `klog`; конвейеру
    // приезжает то, что отдало предыдущее звено, и заставлять человека помнить, кто чем отвечает,
    // значит сделать конвейер непригодным ровно там, где он нужен: `cat лог |> grep wifi`.
    let mut out = alloc::vec::Vec::new();
    match args.get(1) {
        Some(Value::List(items)) => {
            for e in items.iter() {
                if let Value::Str(s) = e {
                    if contains(s.as_bytes(), sub) {
                        out.push(e.clone());
                    }
                }
            }
        }
        Some(Value::Str(text)) => {
            for line in text.lines() {
                if contains(line.as_bytes(), sub) {
                    out.push(Value::str(line));
                }
            }
        }
        _ => return Err(EvalError::new("grep: (grep \"подстрока\" текст-или-список)")),
    }
    Ok(Value::list(out))
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    needle.len() <= hay.len() && hay.windows(needle.len()).any(|w| w == needle)
}

/// `(cd [путь])` — сменить текущий каталог (без пути — в корень). Проверяет, что это каталог.
fn sh_cd(args: &[Value]) -> Result<Value, EvalError> {
    let ep = cap_fs();
    let target = match args.first() {
        None => alloc::vec![b'/'],
        Some(Value::Str(s)) => resolve(s.as_bytes()),
        Some(_) => return Err(EvalError::new("cd: путь — строка")),
    };
    match px::stat(ep, &target) {
        Some((true, _)) => {
            cwd_set(&target);
            Ok(Value::nil())
        }
        Some((false, _)) => Err(EvalError::new("cd: не каталог")),
        None => Err(EvalError::new("cd: нет такого каталога")),
    }
}

/// `(pwd)` — вернуть текущий каталог (строкой; хост его отрендерит).
fn sh_pwd(_args: &[Value]) -> Result<Value, EvalError> {
    let mut buf = [0u8; 256];
    let n = cwd_get(&mut buf);
    match core::str::from_utf8(&buf[..n]) {
        Ok(s) => Ok(Value::str(s)),
        Err(_) => Err(EvalError::new("pwd: путь не UTF-8")),
    }
}

/// Веха 168 — `(notify "заголовок" ["текст"])` — СКАЗАТЬ ЧЕЛОВЕКУ.
///
/// Уходит композитору, а он показывает всплывашку и кладёт в список у колокольчика. Имя
/// отправителя приписывает не шелл, а композитор, спросив у ядра, — поэтому назваться чужим
/// именем командой нельзя.
///
/// Без композитора (текстовое поколение) команда честно отказывает, а не молчит: уведомлению
/// негде появиться, и делать вид, что оно ушло, — вранье.
fn sh_notify(args: &[Value]) -> Result<Value, EvalError> {
    let title = match args.first() {
        Some(Value::Str(s)) => s.clone(),
        _ => return Err(EvalError::new("notify: (notify \"заголовок\" [\"текст\"])")),
    };
    let text = match args.get(1) {
        Some(Value::Str(s)) => &**s,
        _ => "",
    };
    if sys::win::notify(sys::win::NOTE_INFO, &title, text) {
        Ok(Value::nil())
    } else {
        Err(EvalError::new("notify: композитора нет — уведомлению негде появиться"))
    }
}

/// `(date)` — текущее время системы: `ГГГГ-ММ-ДД ЧЧ:ММ:СС UTC` (Веха 86, часы от прошивки).
/// Возвращает строку — значит годится и в конвейер, и как значение выражения.
fn sh_date(_args: &[Value]) -> Result<Value, EvalError> {
    let secs = sys::time_ns() / 1_000_000_000;
    let (y, mo, d, h, mi, s) = sys::civil_from_unix(secs);
    let mut buf = [0u8; 32];
    let mut n = 0;
    let mut put = |v: i64, width: usize, sep: u8| {
        let mut tmp = [0u8; 8];
        let mut len = 0;
        let mut x = v.max(0) as u64;
        loop {
            tmp[len] = b'0' + (x % 10) as u8;
            len += 1;
            x /= 10;
            if x == 0 {
                break;
            }
        }
        for _ in len..width {
            buf[n] = b'0';
            n += 1;
        }
        for i in (0..len).rev() {
            buf[n] = tmp[i];
            n += 1;
        }
        if sep != 0 {
            buf[n] = sep;
            n += 1;
        }
    };
    put(y, 4, b'-');
    put(mo as i64, 2, b'-');
    put(d as i64, 2, b' ');
    put(h as i64, 2, b':');
    put(mi as i64, 2, b':');
    put(s as i64, 2, 0);
    let text = core::str::from_utf8(&buf[..n]).unwrap_or("?");
    let mut out = alloc::string::String::from(text);
    out.push_str(" UTC");
    Ok(Value::str(&out))
}

/// `(random [N])` — N случайных байт (по умолчанию 8) шестнадцатеричной строкой (Веха 86).
/// Источник — ядро: аппаратный ГСЧ (`RDRAND` на x86) плюс пул событий; на riscv аппаратного
/// источника нет, поэтому там это НЕ криптографическое качество (см. `kernel/src/random.rs`).
fn sh_random(args: &[Value]) -> Result<Value, EvalError> {
    let n = match args.first() {
        None => 8usize,
        Some(Value::Int(v)) if *v > 0 && *v <= 64 => *v as usize,
        Some(_) => return Err(EvalError::new("random: нужно число байт 1..64")),
    };
    let mut buf = [0u8; 64];
    if sys::random(&mut buf[..n]) != n {
        return Err(EvalError::new("random: ядро не дало случайных байт"));
    }
    let mut out = alloc::string::String::new();
    for b in &buf[..n] {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    Ok(Value::str(&out))
}

/// `(clear)` — очистить экран (ANSI).
fn sh_clear(_args: &[Value]) -> Result<Value, EvalError> {
    sys::write(b"\x1b[2J\x1b[H");
    Ok(Value::nil())
}

/// Строка справки: жёлтая команда, выравнивание, описание.
/// Строка справки. Описание переводится ЗДЕСЬ (Веха 178) — одним местом на четыре десятка
/// строк: обернуть каждую значило бы сорок возможностей забыть одну.
/// Перевести и подставить значения (`{}` по порядку) — Веха 195.1.
///
/// Тот же приём, что у `ui::i18n::f1`, но без тулкита: шеллу не нужна остальная его половина, а
/// собирать фразу из переведённых обрывков нельзя — в другом языке другой порядок слов (см.
/// шапку `void_user::i18n`). Поэтому переводится ФРАЗА ЦЕЛИКОМ, а числа и имена встают в места.
fn tf(tmpl: &'static str, args: &[&str]) -> String {
    let mut out = String::new();
    let mut rest: &str = sys::i18n::t(tmpl);
    for a in args {
        match rest.find("{}") {
            Some(i) => {
                out.push_str(&rest[..i]);
                out.push_str(a);
                rest = &rest[i + 2..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

fn help_row(cmd: &[u8], desc: &'static str) {
    let desc = sys::i18n::t(desc);
    sys::write(b"  ");
    sys::write(C_CMD);
    sys::write(cmd);
    sys::write(C_RESET);
    for _ in 0..14usize.saturating_sub(cmd.len()) {
        sys::write(b" ");
    }
    sys::write(desc.as_bytes());
    sys::write(b"\n");
}

/// `(help)` — справка по vvsh (команды + краткая Lisp-шпаргалка).
fn sh_help(_args: &[Value]) -> Result<Value, EvalError> {
    sys::write(C_CMD);
    sys::write(b"VOID vvsh");
    sys::write(C_RESET);
    sys::write(sys::i18n::t(" — шелл VOID. Строка с ведущим `\\` — выражение, иначе команда.\n").as_bytes());
    help_row(b"ls [DIR]", "список файлов (каталог или текущий)");
    help_row(b"cat FILE", "показать содержимое (cat A > B — записать содержимым)");
    help_row(b"tail FILE", "последние ~32 байта файла");
    help_row(b"cd [DIR]", "сменить каталог (.. вверх, без арг — в корень)");
    help_row(b"pwd", "текущий каталог");
    help_row(b"mkdir DIR", "создать каталог");
    help_row(b"stat PATH", "вид, размер и время последнего изменения");
    help_row(b"rm PATH", "удалить файл или пустой каталог (rm \"-r\" — с содержимым)");
    help_row(b"mv OLD NEW", "переименовать/переместить файл или каталог");
    help_row(b"cp SRC DST", "копировать файл или каталог (мгновенно: то же содержимое)");
    help_row(b"echo TEXT", "напечатать ($x — переменная; TEXT > FILE — запись)");
    help_row(b"ved FILE", "экранный редактор: ^S сохранить, ^Q выход (программа)");
    help_row(b"grep SUB L", "фильтр строк списка (для конвейеров)");
    help_row(b"run NAME", "запустить программу из store (или просто NAME)");
    help_row(b"thaw NAME", "разморозить процесс из образа");
    help_row(b"ping IP", "ICMP-пинг адреса A.B.C.D");
    help_row(b"resolve NAME", "DNS: имя → адрес (возвращает строку)");
    help_row(b"tcp-connect IP P", "открыть TCP → хэндл (+ tcp-send/recv/close)");
    help_row(b"fetch URL [R]", "скачать по HTTP потоком в store (корень R)");
    help_row(b"blob R [OFF N]", "сводка/кусок скачанного (см. fetch)");
    help_row(b"unroot NAME", "отвязать сырой корень store");
    help_row(b"roots", "сырые корни store (bin/*, system/*, …)");
    help_row(b"init-config", "посеять /etc/system/*.vv");
    help_row(b"pkg", "пакеты nixpkgs: install/list/remove/rollback/gc (программа)");
    help_row("root ИМЯ [ЗНАЧЕНИЕ]".as_bytes(), "показать или задать корень store");
    help_row("root-del ИМЯ".as_bytes(), "снять корень");
    help_row("secret ИМЯ".as_bytes(), "задать корень, не показывая значения (пароли)");
    help_row(b"rebuild", "собрать поколение из /etc/system/*.vv");
    help_row(b"gens", "показать поколения системы (активно — *)");
    help_row(b"switch GEN", "выбрать поколение (после ребута)");
    help_row(b"sysdef GEN F", "задать поколение из файла-конфига");
    help_row(b"notify T [TXT]", "сказать человеку: всплывашка и колокольчик в панели");
    help_row(b"date", "текущее время системы (UTC)");
    help_row(b"random [N]", "N случайных байт от ядра (hex)");
    help_row(b"log on|off", "подробный трейс ядра ([ipc]/[obj]/…)");
    help_row(b"clear", "очистить экран");
    help_row(b"help", "эта справка");
    help_row(b"exit", "выйти в vsh (спасательный шелл)");
    help_row(b"klog [N]", "журнал ядра (N — последние строки); текст можно передать дальше");
    // Веха 199.22 — приёмник назван ЦЕЛИКОМ, с перенаправлением и `</dev/null`.
    //
    // Прежняя подсказка «там: nc -l P» стоила владельцу вечера. `nc` пишет принятое в файл
    // сразу, но САМ НЕ ВЫХОДИТ: получив конец потока от нас, он продолжает ждать конца своего
    // стандартного ввода — то есть терминала. Человек видит зависшую команду, жмёт Ctrl-C,
    // запускает заново — и запуск обнуляет файл, в который только что всё записалось. Снаружи
    // это выглядит как «лог приходит (`wc -c` его считает), а файл пустой».
    help_row(
        b"send IP P",
        "отправить по TCP. Там: nc -l P </dev/null > файл — иначе nc не выйдет сам",
    );
    help_row(b"beep [Hz] [ms]", "короткий сигнал (по умолчанию 880 Гц, 120 мс)");
    // Веха 200 — отладка железа на живой машине, без пересборки.
    help_row(b"mmio A...", "слова регистров по физ-адресам; `mmio A = V` — записать");
    help_row(b"pci B:D.F O...", "слова конфигурации PCI; `pci B:D.F O = V` — записать");
    help_row(b"poweroff", "выключить машину");
    help_row(b"reboot", "перезагрузить машину");
    help_row(b"store-probe", "замер: сколько store принимает за сессию (МиБ)");
    help_row(b"nar-unpack", "разложить NAR из корня store в файлы");
    // Справка обязана показывать ТОТ синтаксис, что понимает reader. Здесь висели S-выражения,
    // хотя с Вехи 102 (ADR 0013) язык инфиксный: `(define x 5)` шелл теперь не примет вовсе.
    sys::write(sys::i18n::t("  Выражение — с ведущим \\: \\x = 5 · \\ping(\"10.0.2.2\")\n").as_bytes());
    sys::write(sys::i18n::t("  Язык: x = 5 · |a| a + 1 · if c { a } else { b } · [1, 2] · map(f, L)\n").as_bytes());
    sys::write(sys::i18n::t("  Конвейер: \\ls() |> grep(\"vv\") |> count()\n").as_bytes());
    Ok(Value::nil())
}

/// `(log on|off)` — вкл/выкл подробный трейс ядра ([ipc]/[obj]/[mm]/…). По умолчанию выключен.
fn sh_log(args: &[Value]) -> Result<Value, EvalError> {
    let on = match args.first() {
        Some(Value::Str(s)) => matches!(&**s, "on" | "1" | "true" | "#t"),
        Some(Value::Bool(b)) => *b,
        None => return Err(EvalError::new("log: (log on) или (log off)")),
        Some(_) => return Err(EvalError::new("log: on|off")),
    };
    sys::log(on);
    Ok(Value::nil())
}

/// `(cat путь)` — вывести содержимое файла.
fn sh_cat(args: &[Value]) -> Result<Value, EvalError> {
    let ep = cap_fs();
    let path = match args.first() {
        Some(Value::Str(s)) => resolve(s.as_bytes()),
        _ => return Err(EvalError::new("cat: нужен путь-строка")),
    };
    // Веха 176 — `cat откуда > куда` пишет СОДЕРЖИМОЕ в файл. Тот же знак и тот же смысл, что у
    // `echo … > файл`, и нужен он там, где мгновенная копия невозможна: файл дерева пакета
    // (`/nix/store/…`) — узел чужого формата, корня `f<путь>` у него нет, и `cp` его не возьмёт.
    // Так содержимое пакета попадает в своё дерево — единственным способом, который у нас есть.
    let out = match args.iter().position(|a| matches!(a, Value::Str(s) if &**s == ">")) {
        Some(i) => match args.get(i + 1) {
            Some(Value::Str(pth)) => Some(resolve(pth.as_bytes())),
            _ => return Err(EvalError::new("cat: после > нужен путь")),
        },
        None => None,
    };
    match read_file(ep, &path) {
        Some(bytes) => {
            if let Some(dst) = out {
                if !px::echo_to(ep, &dst, &bytes) {
                    return Err(EvalError::new("cat: файл записан не полностью"));
                }
                return Ok(Value::nil());
            }
            // Веха 221 — ОТДАЁМ текст, а не печатаем его. Печатает его всё равно шелл (`render`),
            // и человеку ничего не изменилось; а вот конвейеру изменилось всё: `cat файл |> send`
            // раньше отправлял пустоту, потому что брать у `cat` было нечего.
            match String::from_utf8(bytes) {
                Ok(t) => Ok(Value::str(&t)),
                // Двоичный файл текстом не притворяется: конвейер из него ничего не сделает, а
                // печать испортит терминал. Говорим прямо.
                Err(_) => Err(EvalError::new(alloc::format!(
                    "cat: {} — не текст",
                    core::str::from_utf8(&path).unwrap_or("?")
                ))),
            }
        }
        // Веха 161 — путь В СООБЩЕНИИ: без него «файл не найден» винит файл, а спрашивали часто
        // не тот путь, который человек написал (кавычки, `..`, cwd).
        None => Err(EvalError::new(alloc::format!(
            "cat: файл не найден: {}",
            core::str::from_utf8(&path).unwrap_or("?")
        ))),
    }
}

/// `(echo арг…)` — напечатать аргументы через пробел (строки — как есть, прочее — каноничной
/// формой). Веха 84: голое слово `>` включает редирект — `echo текст > /файл` пишет в файл (как
/// в vsh). Записываемый текст — всё до `>`, склеенное пробелами; путь — слово после `>`.
fn sh_echo(args: &[Value]) -> Result<Value, EvalError> {
    // Редирект: найти аргумент-строку ">"; после него обязан быть путь.
    if let Some(i) = args.iter().position(|a| matches!(a, Value::Str(s) if &**s == ">")) {
        let path = match args.get(i + 1) {
            Some(Value::Str(p)) => resolve(p.as_bytes()),
            _ => return Err(EvalError::new("echo: после > нужен путь")),
        };
        let mut text = String::new();
        for (j, a) in args[..i].iter().enumerate() {
            if j > 0 {
                text.push(' ');
            }
            match a {
                Value::Str(s) => text.push_str(s),
                other => text.push_str(&alloc::format!("{}", other)),
            }
        }
        if !px::echo_to(cap_fs(), &path, text.as_bytes()) {
            // Веха 101: запись «наполовину» обязана быть ошибкой команды, а не тишиной.
            return Err(EvalError::new("echo: файл записан не полностью"));
        }
        return Ok(Value::nil());
    }
    // Веха 221 — та же правка, что у `cat`: слова склеиваются и ОТДАЮТСЯ. Печатает шелл.
    let mut text = String::new();
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            text.push(' ');
        }
        match a {
            Value::Str(s) => text.push_str(s),
            other => text.push_str(&alloc::format!("{}", other)),
        }
    }
    Ok(Value::str(&text))
}

/// `(run "имя" "арг"…)` — запустить программу из store, вернуть код выхода (число).
fn sh_run(args: &[Value]) -> Result<Value, EvalError> {
    let name = match args.first() {
        Some(Value::Str(s)) => s.clone(),
        _ => return Err(EvalError::new("run: имя программы — строка")),
    };
    let mut blob = alloc::vec::Vec::new();
    for a in &args[1..] {
        match a {
            Value::Str(s) => {
                blob.extend_from_slice(s.as_bytes());
                blob.push(0);
            }
            other => {
                return Err(EvalError::new(alloc::format!(
                    "run: аргумент — строка, дано {}",
                    other.type_name()
                )))
            }
        }
    }
    let code = px::spawn_args(cap_store(), name.as_bytes(), &blob);
    if code == usize::MAX {
        return Err(EvalError::new(alloc::format!("run: '{}' не запустилась", name)));
    }
    Ok(Value::Int(code as i64))
}

// ── команды vsh, перенесённые в vvsh (Веха 84) ──────────────────────────────────
// Модель прежняя: builtin дёргает синкаллы напрямую, права — из start_cap (0=posixfs, 1=store,
// 2=net). Каталожные команды возвращают `nil` (эффект — на экран/ФС), инспекционные — значение.

/// Первый аргумент как путь-строка, разрешённый относительно cwd. Общий помощник команд файлов.
fn arg_path(args: &[Value], usage: &str) -> Result<Vec<u8>, EvalError> {
    match args.first() {
        Some(Value::Str(s)) => Ok(resolve(s.as_bytes())),
        _ => Err(EvalError::new(alloc::string::String::from(usage))),
    }
}

/// `(roots)` — сырые корни store (короткий id + имя на строку). Store — start-cap 1.
fn sh_roots(_args: &[Value]) -> Result<Value, EvalError> {
    // Список читается ЦЕЛИКОМ (Веха 107): с пакетами корней стало много — по два на каждый путь
    // замыкания, — и фиксированный буфер молча резал вывод посреди строки.
    match roots::text(cap_store()) {
        Some(text) => sys::write(&text),
        None => sys::write(sys::i18n::t("нет корней (или нет прав на store)\n").as_bytes()),
    }
    Ok(Value::nil())
}

/// `(readlink путь)` — цель символической ссылки. Ссылки в VOID пока живут только в дереве
/// пакета под `/nix/store`: своих персоналия не заводит, а у настоящих пакетов их половина.
fn sh_readlink(args: &[Value]) -> Result<Value, EvalError> {
    let path = arg_path(args, "readlink: (readlink \"путь\")")?;
    let mut buf = [0u8; 1024];
    let n = px::readlink(cap_fs(), &path, &mut buf);
    if n == 0 {
        return Err(EvalError::new("не символическая ссылка (или нет такого пути)"));
    }
    match core::str::from_utf8(&buf[..n]) {
        Ok(s) => Ok(Value::str(s)),
        Err(_) => Err(EvalError::new("цель ссылки не UTF-8")),
    }
}

/// `(mkdir путь)` — создать каталог (относительно cwd).
/// `(stat "путь")` — что известно о записи: вид, размер и время последнего изменения.
///
/// Веха 177. Отдельной командой, а не столбцами в `ls`: `ls` возвращает СПИСОК ИМЁН и тем живёт
/// в конвейерах (`(| (ls) (grep "vv"))`). Приделать к именам ещё и колонки значило бы сломать
/// каждый такой конвейер ради одной подробности.
fn sh_stat(args: &[Value]) -> Result<Value, EvalError> {
    let path = arg_path(args, "stat: (stat \"путь\")")?;
    let shown = || String::from(core::str::from_utf8(&path).unwrap_or("?"));
    let Some((dir, size, _, when)) = px::stat_all(cap_fs(), &path) else {
        return Err(EvalError::new(alloc::format!("stat: нет такого пути: {}", shown())));
    };
    let kind = if dir { "каталог" } else { "файл" };
    let sz = if dir { String::new() } else { alloc::format!("  {} Б", size) };
    // Ноль — «времени нет», а не 1970 год: у всего, что легло в систему до Вехи 177, его просто
    // не записывали, и подписывать это датой было бы выдумкой.
    let at = if when == 0 {
        String::from("  время неизвестно")
    } else {
        let (y, mo, d, h, mi, s) = sys::civil_from_unix(when / 1_000_000_000);
        alloc::format!("  {y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
    };
    Ok(Value::str(&alloc::format!("{kind}{sz}{at}")))
}

fn sh_mkdir(args: &[Value]) -> Result<Value, EvalError> {
    let path = arg_path(args, "mkdir: (mkdir \"путь\")")?;
    if px::mkdir(cap_fs(), &path) != 0 {
        return Err(EvalError::new("mkdir не удался (уже есть? нет родителя?)"));
    }
    Ok(Value::nil())
}

/// `(rm путь)` — удалить файл или пустой каталог (относительно cwd).
/// `(rm путь)` — снять файл или ПУСТОЙ каталог. `(rm "-r" путь)` — вместе с содержимым.
///
/// Веха 175 — ключ отдельным аргументом, а не флагом у пути: вызов, который может потерять чужую
/// работу, обязан выглядеть иначе, чем обычный. Порядок как в мире Unix (`rm -r путь`), потому
/// что человек его уже знает.
fn sh_rm(args: &[Value]) -> Result<Value, EvalError> {
    let deep = matches!(args.first(), Some(Value::Str(s)) if &**s == "-r" || &**s == "-rf");
    let rest = if deep { &args[1..] } else { args };
    let path = arg_path(rest, "rm: (rm [\"-r\"] \"путь\")")?;
    let ep = cap_fs();
    let gone = if deep { px::unlink_all(ep, &path) } else { px::unlink(ep, &path) };
    if gone != 0 {
        return Err(EvalError::new(if deep {
            "rm -r не удался (нет такого пути? слишком большое дерево?)"
        } else {
            "rm не удался (нет файла? каталог не пуст? — тогда `rm \"-r\"`)"
        }));
    }
    Ok(Value::nil())
}

/// `(tail путь)` — последние ~32 байта файла (витрина lseek SEEK_END).
fn sh_tail(args: &[Value]) -> Result<Value, EvalError> {
    let ep = cap_fs();
    let path = arg_path(args, "tail: (tail \"путь\")")?;
    // stat до open: у posixfs open(mode 0) создал бы пустышку на опечатке пути.
    match px::stat(ep, &path) {
        Some((false, _)) => {}
        _ => return Err(EvalError::new("tail: нет такого файла")),
    }
    let fd = px::open(ep, &path, 0);
    if fd == usize::MAX {
        return Err(EvalError::new("tail: нет такого файла"));
    }
    px::seek(ep, fd, -32, px::SEEK_END);
    let mut tb = [0u8; 64];
    let k = px::read(ep, fd, &mut tb);
    px::close(ep, fd);
    sys::write(&tb[..k]);
    if k == 0 || tb[k - 1] != b'\n' {
        sys::write(b"\n");
    }
    Ok(Value::nil())
}

/// `(cp откуда куда)` — СКОПИРОВАТЬ файл или каталог (Веха 176).
///
/// Копия мгновенна и не занимает места: содержимое в VOID адресуется хэшем, поэтому копия — это
/// второе имя для тех же объектов. Каталог копируется со всем содержимым по той же причине.
///
/// Из дерева пакета (`/nix/store/…`) так копировать нельзя — там узлы чужого формата; для них
/// есть `cat откуда > куда`, копирующий содержимым.
fn sh_cp(args: &[Value]) -> Result<Value, EvalError> {
    match (args.first(), args.get(1)) {
        (Some(Value::Str(o)), Some(Value::Str(n))) => {
            let old = resolve(o.as_bytes());
            let new = resolve(n.as_bytes());
            if px::copy(cap_fs(), &old, &new) != 0 {
                return Err(EvalError::new(
                    "cp не удался: нет такого пути, цель занята, либо это файл из дерева пакета \
                     (для него `cat откуда > куда`)",
                ));
            }
            Ok(Value::nil())
        }
        _ => Err(EvalError::new("cp: (cp \"откуда\" \"куда\")")),
    }
}

/// `(mv старый новый)` — переименовать или переместить файл ИЛИ КАТАЛОГ (пути — от cwd).
///
/// Веха 175 — каталоги переехали наравне с файлами. Если цель — существующий каталог, содержимое
/// кладётся ВНУТРЬ него (`mv /a /b` при живом `/b` даёт `/b/a`), как и положено `mv`.
fn sh_mv(args: &[Value]) -> Result<Value, EvalError> {
    match (args.first(), args.get(1)) {
        (Some(Value::Str(o)), Some(Value::Str(n))) => {
            let old = resolve(o.as_bytes());
            let new = resolve(n.as_bytes());
            if px::rename(cap_fs(), &old, &new) != 0 {
                // Причин ровно три, и человеку стоит знать все: чего-то нет, что-то уже
                // занято, либо каталог просят переехать внутрь самого себя.
                return Err(EvalError::new(
                    "mv не удался: нет такого пути, либо цель занята, либо каталог переезжает \
                     внутрь себя",
                ));
            }
            Ok(Value::nil())
        }
        _ => Err(EvalError::new("mv: (mv \"старый\" \"новый\") — два пути")),
    }
}

/// `(ping "A.B.C.D")` — ICMP-пинг через сетевой сервер (start-cap 2). Возвращает RTT (мкс).
fn sh_ping(args: &[Value]) -> Result<Value, EvalError> {
    let ipstr = match args.first() {
        Some(Value::Str(s)) => s.clone(),
        _ => return Err(EvalError::new("ping: (ping \"A.B.C.D\")")),
    };
    let ip = match sys::net_cli::parse_ipv4(ipstr.as_bytes()) {
        Some(x) => x,
        None => return Err(EvalError::new("ping: неверный IP (нужно A.B.C.D)")),
    };
    let netep = cap_net();
    if netep == sys::NO_CAP {
        return Err(EvalError::new("ping: сети нет (net.vv = #f?)"));
    }
    let mut rep = [0u8; 5];
    let n = sys::call(netep, 0 /* OP_PING */, &ip, &mut rep);
    if n >= 5 && rep[0] == 0 {
        let rtt = u32::from_le_bytes([rep[1], rep[2], rep[3], rep[4]]);
        sys::write(tf("ответ от {}: {} мкс\n", &[&ipstr, &alloc::format!("{}", rtt)]).as_bytes());
        return Ok(Value::nil());
    }
    // Веха 199.13 — «служба не ответила» это НЕ «адрес молчит».
    //
    // Короткий ответ (или его отсутствие) означает, что до `net-srv` запрос не дошёл или он не
    // смог ответить, — то есть сети нет вовсе, а не «пакет ушёл и потерялся». Раньше оба случая
    // печатались как «нет ответа», и это увело поиск в карту, провод и роутер, когда отвечать
    // было уже некому: служба умерла, а ядро говорило об этом только при `log on`.
    if n < 5 {
        // Веха 199.14 — сказать, ЧТО у нас в руках. Отказ `SYS_CALL` бывает ровно по одной
        // причине — дескриптор не годится как эндпоинт, — и вид с адресатом называют её прямо,
        // не заставляя гадать между «служба умерла» и «право не то».
        let (kind, peer) = sys::cap_info_ex(netep).map_or((255, 0xffff), |(k, _, p)| (k, p));
        return Err(EvalError::new(alloc::format!(
            "ping: служба не ответила. Держим право {} — {} (вид {}, адресат P{})",
            netep, sys::cap_kind_name(kind), kind, peer,
        )));
    }
    // Отказы РАЗНЫЕ, и валить их в «нет ответа» — врать (Веха 135). «Не отзывается на ARP»
    // означает, что адрес в нашей подсети и там никого нет, — это другой диагноз и другое
    // лечение, чем «пакет ушёл, но молчат».
    Err(EvalError::new(match rep.first() {
        Some(1) => "ping: адрес в своей подсети не отзывается (никого по этому адресу)",
        Some(3) => "ping: сокеты пинга кончились — слишком много мёртвых адресов до перезагрузки",
        Some(7) => "ping: карта ещё не поднялась (или её нет) — стек ждёт драйвер",
        _ => "ping: нет ответа",
    }))
}

/// `(resolve "имя")` — Веха 92: спросить у DNS A-запись имени. ВОЗВРАЩАЕТ строку «A.B.C.D»,
/// а не печатает: адрес нужен как значение — `(ping (resolve "example.com"))` работает сразу.
fn sh_resolve(args: &[Value]) -> Result<Value, EvalError> {
    let name = match args.first() {
        Some(Value::Str(s)) => s.clone(),
        _ => return Err(EvalError::new("resolve: (resolve \"имя\")")),
    };
    let netep = cap_net();
    if netep == sys::NO_CAP {
        return Err(EvalError::new("resolve: сети нет (net.vv = #f?)"));
    }
    let mut rep = [0u8; 5];
    let n = sys::call(netep, 1 /* OP_RESOLVE */, name.as_bytes(), &mut rep);
    if n < 5 {
        return Err(EvalError::new("resolve: сервер не ответил"));
    }
    match rep[0] {
        0 => Ok(Value::str(&alloc::format!(
            "{}.{}.{}.{}",
            rep[1], rep[2], rep[3], rep[4]
        ))),
        1 => Err(EvalError::new("resolve: имя не разрешилось")),
        2 => Err(EvalError::new("resolve: DNS не ответил")),
        // Веха 136 — отдельный ответ, а не «имя не разрешилось»: пользователь обязан видеть
        // разницу между «такого имени нет» и «мы САМИ его не пустили». Иначе блокировщик
        // неотличим от поломки сети, и его будут чинить вместо того, чтобы настроить.
        5 => Err(EvalError::new("resolve: имя в списке блокировки (см. block= в net.vv)")),
        _ => Err(EvalError::new("resolve: сети нет")),
    }
}

/// Эндпоинт сетевого сервера (start-cap 2) — он же ПРАВО пользоваться сетью.
fn net_ep(who: &str) -> Result<usize, EvalError> {
    let ep = cap_net();
    if ep == sys::NO_CAP {
        return Err(EvalError::new(alloc::format!("{}: сети нет (net.vv = #f?)", who)));
    }
    Ok(ep)
}

/// Расшифровать код статуса сетевого сервера в человеческую ошибку.
fn net_err(who: &str, st: u8) -> EvalError {
    use sys::net_cli as p;
    EvalError::new(alloc::format!(
        "{}: {}",
        who,
        // Веха 220.2 — по одному факту на случай, без догадок в скобках. Прежние «(адрес отверг
        // соединение?)» и «(хэндл?)» были предположениями: система их не проверяла и знать не
        // могла, а вопросительный знак в отчёте — это перекладывание работы на читателя.
        match st {
            p::ST_ERR => "отказано",
            p::ST_TIMEOUT => "ответа нет",
            p::ST_EOF => "соединение закрыто другой стороной",
            // Веха 199.4 — «карты нет» отдельной строкой. Раньше этот случай приходил тем же
            // кодом, что негодный запрос, и человек с живой картой читал про «хэндл».
            p::ST_NODEV => "сетевой карты нет — стек ждёт драйвер",
            p::ST_OFF => "сеть выключена",
            _ => "негодный запрос",
        }
    ))
}

/// `(tcp-connect "A.B.C.D" порт)` — открыть TCP-соединение, ВЕРНУТЬ хэндл (число).
/// Вместе с `resolve` складывается сразу: `(tcp-connect (resolve "example.com") 80)`.
fn sh_tcp_connect(args: &[Value]) -> Result<Value, EvalError> {
    let (host, port) = match (args.first(), args.get(1)) {
        (Some(Value::Str(h)), Some(Value::Int(p))) if *p > 0 && *p < 65536 => (h.clone(), *p as u16),
        _ => return Err(EvalError::new("tcp-connect: (tcp-connect \"A.B.C.D\" порт)")),
    };
    let ip = match sys::net_cli::parse_ipv4(host.as_bytes()) {
        Some(x) => x,
        None => return Err(EvalError::new("tcp-connect: нужен адрес A.B.C.D (имя — через resolve)")),
    };
    match sys::net_cli::tcp_connect(net_ep("tcp-connect")?, ip, port) {
        Ok(h) => Ok(Value::Int(h as i64)),
        Err(st) => Err(net_err("tcp-connect", st)),
    }
}

/// `(tcp-send хэндл "данные")` — отправить; возвращает, сколько байт ПРИНЯЛ сервер (может быть
/// меньше — как `write(2)`; остаток шлёт вызывающий).
fn sh_tcp_send(args: &[Value]) -> Result<Value, EvalError> {
    let (h, data) = match (args.first(), args.get(1)) {
        (Some(Value::Int(h)), Some(Value::Str(d))) => (*h, d.clone()),
        _ => return Err(EvalError::new("tcp-send: (tcp-send хэндл \"данные\")")),
    };
    match sys::net_cli::tcp_send(net_ep("tcp-send")?, h as u8, data.as_bytes()) {
        Ok(n) => Ok(Value::Int(n as i64)),
        Err(st) => Err(net_err("tcp-send", st)),
    }
}

/// `(tcp-recv хэндл)` — принять очередной кусок, ВЕРНУТЬ строкой. Пустая строка — другая
/// сторона закрыла соединение (это не ошибка, а конец потока).
fn sh_tcp_recv(args: &[Value]) -> Result<Value, EvalError> {
    let h = match args.first() {
        Some(Value::Int(h)) => *h,
        _ => return Err(EvalError::new("tcp-recv: (tcp-recv хэндл)")),
    };
    let mut buf = [0u8; sys::net_cli::MAX_CHUNK];
    match sys::net_cli::tcp_recv(net_ep("tcp-recv")?, h as u8, &mut buf) {
        Ok(n) => Ok(Value::str(&alloc::string::String::from_utf8_lossy(&buf[..n]))),
        Err(sys::net_cli::ST_EOF) => Ok(Value::str("")),
        Err(st) => Err(net_err("tcp-recv", st)),
    }
}

/// `(tcp-close хэндл)` — закрыть аккуратно (FIN, не сброс).
fn sh_tcp_close(args: &[Value]) -> Result<Value, EvalError> {
    let h = match args.first() {
        Some(Value::Int(h)) => *h,
        _ => return Err(EvalError::new("tcp-close: (tcp-close хэндл)")),
    };
    match sys::net_cli::tcp_close(net_ep("tcp-close")?, h as u8) {
        Ok(()) => Ok(Value::nil()),
        // Веха 199.21 — закрытие ждёт подтверждения, поэтому отказы теперь разные: негодный
        // хэндл это одно, а «данные могли не доехать» — совсем другое.
        Err(st) if st == sys::net_cli::ST_TIMEOUT => Err(EvalError::new(
            "tcp-close: закрытие не подтвердилось — данные могли не доехать",
        )),
        Err(_) => Err(EvalError::new("tcp-close: негодный хэндл")),
    }
}

/// Шестнадцатеричное представление content-id (первые `n` байт) — для показа человеку.
fn hex_id(id: &[u8; 32], n: usize) -> alloc::string::String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = alloc::string::String::with_capacity(n * 2);
    for b in id.iter().take(n) {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

/// `(fetch "http://хост/путь" ["корень"])` — Веха 94: скачать ПОТОКОМ прямо в store.
/// Тело режется на куски-объекты, узел связывает их; ВОЗВРАЩАЕТ content-id узла строкой —
/// это Merkle-корень над содержимым, посчитанный самим устройством.
fn sh_fetch(args: &[Value]) -> Result<Value, EvalError> {
    let (url, root) = match (args.first(), args.get(1)) {
        (Some(Value::Str(u)), Some(Value::Str(r))) => (u.clone(), r.clone()),
        (Some(Value::Str(u)), None) => (u.clone(), alloc::rc::Rc::from("")),
        _ => return Err(EvalError::new("fetch: (fetch \"http://хост/путь\" [\"корень\"])")),
    };
    // Веха 95: https обслуживает ОТДЕЛЬНАЯ программа. TLS — это 105 крейтов чужого кода, и
    // давать им полномочия шелла незачем: у `httpsc` будут ровно сеть и store. Заодно шелл не
    // толстеет на полмегабайта криптографии.
    if url.as_bytes().len() > 8 && url.as_bytes()[..8].eq_ignore_ascii_case(b"https://") {
        if root.is_empty() {
            return Err(EvalError::new(
                "fetch: для https нужен корень — (fetch \"https://…\" \"dl/имя\")",
            ));
        }
        let mut argv = alloc::vec::Vec::new();
        argv.extend_from_slice(url.as_bytes());
        argv.push(0);
        argv.extend_from_slice(root.as_bytes());
        let code = sys::exec_args(cap_store(), b"httpsc", &argv);
        if code != 0 {
            return Err(EvalError::new("fetch: https не удался"));
        }
        // Итог печатает сам `httpsc`; content-id достаём из корня, чтобы `fetch` возвращал
        // одно и то же и для http, и для https.
        let mut id = [0u8; 32];
        if sys::obj_get_root(cap_store(), root.as_bytes(), &mut id) != 32 {
            return Err(EvalError::new("fetch: корень не появился"));
        }
        return Ok(Value::str(&hex_id(&id, 32)));
    }

    let netep = net_ep("fetch")?;
    let scap = cap_store();
    // Буферы даёт вызывающий: у библиотеки нет аллокатора, а у шелла есть.
    //
    // Веха 101 — список кусков БОЛЬШЕ не 512 записей. Прежний потолок (512 × 16 КиБ = 8 МиБ на
    // файл) был выбран «до пакетов», а пакеты как раз и начинаются с замыканий в десятки
    // мегабайт: `pkg fetch` упёрся бы в него на первом же настоящем пакете. Записей теперь
    // столько, сколько нужно (32 байта на кусок: 64 МиБ файла — 128 КиБ списка).
    // Настоящий потолок остался один и он честнее — сколько объектов принимает store за сессию
    // (см. `store-probe`).
    let mut chunk = alloc::vec![0u8; sys::http::CHUNK];
    let mut kids = alloc::vec![[0u8; 32]; 8192];
    let mut sink = sys::http::Sink { chunk: &mut chunk, kids: &mut kids };
    match sys::http::get(netep, scap, url.as_bytes(), root.as_bytes(), &mut sink) {
        Ok(f) => {
            sys::write(
                alloc::format!(
                    "скачано {} байт, кусков {}{}\n",
                    f.bytes,
                    f.chunks,
                    if root.is_empty() {
                        alloc::string::String::new()
                    } else {
                        alloc::format!(", корень {}", root)
                    }
                )
                .as_bytes(),
            );
            Ok(Value::str(&hex_id(&f.id, 32)))
        }
        Err(e) => Err(EvalError::new(alloc::format!("fetch: {}", e))),
    }
}

/// `(blob "корень" [смещение длина])` — прочитать скачанное обратно из store.
/// Без смещения печатает сводку (сколько байт, сколько кусков), со смещением ВОЗВРАЩАЕТ
/// кусок содержимого строкой — так проверяется, что приехало ровно то, что отдал сервер.
fn sh_blob(args: &[Value]) -> Result<Value, EvalError> {
    let name = match args.first() {
        Some(Value::Str(s)) => s.clone(),
        _ => return Err(EvalError::new("blob: (blob \"корень\" [смещение длина])")),
    };
    let scap = cap_store();
    let mut id = [0u8; 32];
    // `SYS_OBJ_GET_ROOT` отдаёт ЧИСЛО БАЙТ id (32), а не код возврата — 0 значит «нет корня».
    if sys::obj_get_root(scap, name.as_bytes(), &mut id) != 32 {
        return Err(EvalError::new("blob: нет такого корня"));
    }
    let mut manifest = [0u8; 512];
    let mlen = sys::obj_get(scap, &id, &mut manifest);
    if mlen == 0 || mlen == usize::MAX {
        return Err(EvalError::new("blob: узел не читается"));
    }
    let Some((total, nchunks, csize)) = sys::http::blob_info(&manifest[..mlen]) else {
        return Err(EvalError::new("blob: корень указывает не на блоб"));
    };
    let (off, len) = match (args.get(1), args.get(2)) {
        (Some(Value::Int(o)), Some(Value::Int(l))) if *o >= 0 && *l > 0 => (*o as usize, *l as usize),
        (None, None) => {
            sys::write(
                alloc::format!("{}: {} байт, кусков {}\n", name, total, nchunks).as_bytes(),
            );
            return Ok(Value::Int(total as i64));
        }
        _ => return Err(EvalError::new("blob: (blob \"корень\" смещение длина)")),
    };

    let mut kids = alloc::vec![[0u8; 32]; nchunks];
    if sys::obj_children(scap, &id, &mut kids) != nchunks {
        return Err(EvalError::new("blob: список кусков не сошёлся"));
    }
    // Куски одинаковой длины, кроме последнего, — значит нужный кусок ищется делением.
    let mut out = alloc::vec::Vec::new();
    let mut buf = alloc::vec![0u8; csize];
    let mut pos = off;
    while out.len() < len && pos < total {
        let ci = pos / csize;
        if ci >= nchunks {
            break;
        }
        let n = sys::obj_get(scap, &kids[ci], &mut buf);
        if n == 0 || n == usize::MAX {
            return Err(EvalError::new("blob: кусок не читается"));
        }
        let inside = pos % csize;
        if inside >= n {
            break;
        }
        let take = (n - inside).min(len - out.len());
        out.extend_from_slice(&buf[inside..inside + take]);
        pos += take;
    }
    Ok(Value::str(&alloc::string::String::from_utf8_lossy(&out)))
}

/// `(unroot "имя")` — отвязать СЫРОЙ корень store (то, что показывает `roots`).
///
/// Появилось вместе с `fetch`: тот заводит корни, а убрать их из шелла было нечем — скачанное
/// держалось бы вечно, ведь GC собирает только НЕдостижимое, а корень и есть достижимость.
/// Объекты исчезнут на ближайшей сборке, если на них больше никто не ссылается.
fn sh_unroot(args: &[Value]) -> Result<Value, EvalError> {
    let name = match args.first() {
        Some(Value::Str(s)) => s.clone(),
        _ => return Err(EvalError::new("unroot: (unroot \"имя-корня\")")),
    };
    // Системные корни через эту команду не трогаем: снести `system/current` или `bin/<arch>/vvsh`
    // значит остаться без загрузки или без шелла, а откатить это будет уже нечем.
    for guard in [b"system/".as_slice(), b"bin/".as_slice(), b"proc/".as_slice()] {
        if name.as_bytes().starts_with(guard) {
            return Err(EvalError::new("unroot: системные корни (system/, bin/, proc/) не трогаем"));
        }
    }
    if sys::obj_del_root(cap_store(), name.as_bytes()) == 0 {
        Ok(Value::nil())
    } else {
        Err(EvalError::new("unroot: нет такого корня (или нет права WRITE)"))
    }
}

/// `(thaw "имя")` — разморозить процесс из образа `proc/<arch>/имя` (start-cap 1 несёт EXEC).
fn sh_thaw(args: &[Value]) -> Result<Value, EvalError> {
    let name = match args.first() {
        Some(Value::Str(s)) => s.clone(),
        _ => return Err(EvalError::new("thaw: (thaw \"имя\")")),
    };
    let code = sys::restore(cap_store(), name.as_bytes());
    if code == usize::MAX {
        return Err(EvalError::new("thaw не удался (нет образа?)"));
    }
    Ok(Value::Int(code as i64))
}

/// `nar-unpack("корень", "/куда")` — разложить NAR из store в файлы (Веха 105).
///
/// Первая распаковка пакетного формата НА САМОЙ VOID. Архив берётся из store тремя видами —
/// объектом, блобом из кусков (так кладёт `fetch`) и в любом из них сжатым `.xz`, — а обход
/// (`void_nar`) отдаёт файлы по одному, и каждый сразу уезжает в персоналию. Держать дерево в
/// памяти целиком мы не можем и не пытаемся — ради этого у обхода и обратный вызов.
///
/// Пишем ЧЕРЕЗ файловый сервер, а не подделываем его корни: раскладка «файл = объект, каталог =
/// индекс» принадлежит ему, и лезть в неё за его спиной значило бы завести вторую правду.
fn sh_nar_unpack(args: &[Value]) -> Result<Value, EvalError> {
    let (root, dest) = match (args.first(), args.get(1)) {
        (Some(Value::Str(r)), Some(Value::Str(d))) => (r.clone(), d.clone()),
        _ => return Err(EvalError::new("nar-unpack: (\"корень\", \"/куда\")")),
    };
    let scap = cap_store();
    let ep = cap_fs();
    let mut id = [0u8; 32];
    if sys::obj_get_root(scap, root.as_bytes(), &mut id) != 32 {
        return Err(EvalError::new("nar-unpack: нет такого корня"));
    }
    let buf = archive::unpacked(scap, &id, true)
        .map_err(|e| EvalError::new(alloc::format!("nar-unpack: {}", e)))?;

    let base = alloc::string::String::from(dest.trim_end_matches('/'));
    px::mkdir(ep, base.as_bytes());
    let mut files = 0usize;
    let mut links = 0usize;
    let mut bytes = 0usize;
    let r = void_nar::walk(&buf, |e| {
        match e {
            void_nar::Entry::Dir { path } => {
                if !path.is_empty() {
                    px::mkdir(ep, alloc::format!("{}/{}", base, path).as_bytes());
                }
            }
            void_nar::Entry::File { path, data, .. } => {
                let full = if path.is_empty() {
                    base.clone()
                } else {
                    alloc::format!("{}/{}", base, path)
                };
                if !px::echo_to(ep, full.as_bytes(), data) {
                    // Веха 101 научила `echo_to` отвечать честно — грех не воспользоваться:
                    // недописанный файл обязан остановить распаковку, а не остаться огрызком.
                    return Err(void_nar::NarError(alloc::format!("не записался {}", full)));
                }
                files += 1;
                bytes += data.len();
            }
            // Симлинков в персоналии нет; молчать нельзя — иначе дерево тихо теряет часть себя.
            // Но и заваливать экран нельзя: настоящий пакет вроде `perl-env` — это сотня-другая
            // симлинков, за которыми не видно ничего. Первые несколько поимённо, остальные —
            // числом в итоговой строке.
            void_nar::Entry::Symlink { path, target } => {
                if links < 5 {
                    sys::write(
                        alloc::format!("  ! симлинк {} → {} пропущен\n", path, target).as_bytes(),
                    );
                }
                links += 1;
            }
        }
        Ok(())
    });
    if let Err(e) = r {
        return Err(EvalError::new(e.0));
    }
    sys::write(
        alloc::format!(
            "распаковано: файлов {} ({} Б){}\n",
            files,
            bytes,
            if links > 0 { alloc::format!(", симлинков пропущено {}", links) } else { String::new() },
        )
        .as_bytes(),
    );
    Ok(Value::Int(files as i64))
}

/// `(store-probe [МиБ])` — сколько store принимает за сессию (Веха 101, замер перед пакетами).
///
/// Кладёт объекты по 64 КиБ с РАЗНЫМ содержимым (одинаковые схлопнулись бы дедупом и ничего не
/// измерили) и считает, сколько удалось. Вопрос практический: NAR настоящего пакета — десятки
/// мегабайт, а объекты живут в куче ЯДРА (16 МиБ арены), и упереться в это лучше здесь, чем на
/// середине пакетной фазы. Останавливается на первом отказе или на заданном пределе (по
/// умолчанию 64 МиБ).
fn sh_store_probe(args: &[Value]) -> Result<Value, EvalError> {
    let limit_mib = match args.first() {
        Some(Value::Int(n)) if *n > 0 => *n as usize,
        _ => 64,
    };
    let scap = cap_store();
    const PIECE: usize = 64 * 1024;
    let mut buf = alloc::vec![0u8; PIECE];
    let mut id = [0u8; 32];
    let mut done = 0usize;
    let pieces = limit_mib * 1024 * 1024 / PIECE;
    for i in 0..pieces {
        // Уникальная «соль» в начале куска: содержимое обязано отличаться, иначе store честно
        // вернёт тот же объект и замер покажет бесконечность.
        buf[..8].copy_from_slice(&(i as u64).to_le_bytes());
        if sys::obj_put(scap, &buf, &mut id) != 0 {
            sys::write(
                alloc::format!(
                    "store-probe: отказ на {} МиБ ({} объектов по 64 КиБ)\n",
                    done / (1024 * 1024),
                    i,
                )
                .as_bytes(),
            );
            return Ok(Value::Int((done / (1024 * 1024)) as i64));
        }
        done += PIECE;
    }
    sys::write(
        alloc::format!("store-probe: принято {} МиБ без отказа\n", done / (1024 * 1024)).as_bytes(),
    );
    Ok(Value::Int((done / (1024 * 1024)) as i64))
}

/// Прочитать журнал ядра целиком. Общее для [`sh_klog_save`] и [`sh_klog_send`].
///
/// Буфер большой намеренно: журнал загрузки на живой машине — это сотни строк про PCI,
/// контроллеры и драйверы, и обрезать его именно там, где началось интересное, было бы
/// издевательством.
fn klog_text() -> Result<String, EvalError> {
    let mut buf = alloc::vec![0u8; 128 * 1024];
    let (got, lost) = sys::klog(&mut buf);
    if got == 0 {
        return Err(EvalError::new("журнал ядра пуст или недоступен"));
    }
    buf.truncate(got);
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if lost > 0 {
        // Сказать вслух, что начало не поместилось: иначе человек будет искать в журнале то,
        // чего в нём уже нет, и винить себя.
        text.push_str(&alloc::format!("\n[klog] начало журнала потеряно: {} байт\n", lost));
    }
    Ok(text)
}

/// `(klog [N])` — журнал ядра ТЕКСТОМ (Веха 199.3), необязательно последние `N` строк.
///
/// Встроенная команда, а не программа `klog`, ровно по одной причине: значение можно передать
/// дальше — `klog > /etc/log.txt` кладёт его в файл, `klog >> send …` отправляет по сети. Вывод
/// на экран при этом прежний: печатает его шелл, как и любое возвращённое значение.
fn sh_klog(args: &[Value]) -> Result<Value, EvalError> {
    // `klog > файл` — тот же знак и смысл, что у `echo` и `cat`.
    let out = match args.iter().position(|a| matches!(a, Value::Str(s) if &**s == ">")) {
        Some(i) => match args.get(i + 1) {
            Some(Value::Str(p)) => Some(resolve(p.as_bytes())),
            _ => return Err(EvalError::new("klog: после > нужен путь")),
        },
        None => None,
    };
    let mut text = klog_text()?;
    // Число первым аргументом — сколько ПОСЛЕДНИХ строк оставить.
    let lines = match args.first() {
        Some(Value::Int(n)) if *n > 0 => Some(*n as usize),
        Some(Value::Str(s)) => s.trim().parse::<usize>().ok().filter(|n| *n > 0),
        _ => None,
    };
    if let Some(n) = lines {
        let keep: alloc::vec::Vec<&str> = text.lines().rev().take(n).collect();
        text = keep.iter().rev().fold(String::new(), |mut acc, l| {
            acc.push_str(l);
            acc.push('\n');
            acc
        });
    }
    if let Some(path) = out {
        if !px::echo_to(cap_fs(), &path, text.as_bytes()) {
            return Err(EvalError::new("klog: файл записан не полностью"));
        }
        sys::write(tf("журнал сохранён: {} байт\n", &[&alloc::format!("{}", text.len())]).as_bytes());
        return Ok(Value::nil());
    }
    Ok(Value::str(&text))
}

/// `(send "A.B.C.D" порт "текст")` — отправить текст по TCP (Веха 199.3).
///
/// Обычно текст приходит слева по конвейеру: `klog >> send 192.168.0.87 9000`, а на той стороне
/// достаточно `nc -l 9000 > log.txt`. Это единственный способ вынести диагностику с машины без
/// COM-порта — и он же первая настоящая проверка сети на чужом железе: доехало, значит работают
/// и карта, и стек.
fn sh_send(args: &[Value]) -> Result<Value, EvalError> {
    // Номер порта принимаем и числом, и строкой: в командной строке шелла всё, что набрал
    // человек, приходит строкой, и требовать от него кавычек с обратным слэшем ради типа —
    // это язык, объясняющийся своей реализацией.
    let port = match args.get(1) {
        Some(Value::Int(p)) if *p > 0 && *p < 65536 => *p as u16,
        Some(Value::Str(p)) => match p.trim().parse::<u16>() {
            Ok(p) if p > 0 => p,
            _ => return Err(EvalError::new("send: порт — число 1..65535")),
        },
        _ => return Err(EvalError::new("send: (send \"A.B.C.D\" порт \"текст\")")),
    };
    let host = match args.first() {
        Some(Value::Str(h)) => h.clone(),
        _ => return Err(EvalError::new("send: (send \"A.B.C.D\" порт \"текст\")")),
    };
    let ip = match sys::net_cli::parse_ipv4(host.as_bytes()) {
        Some(x) => x,
        None => return Err(EvalError::new("send: нужен адрес A.B.C.D (имя — через resolve)")),
    };
    // Текст — третьим аргументом либо слева по конвейеру `>>` (он же кладётся последним).
    let text = match args.get(2) {
        Some(Value::Str(t)) => String::from(&**t),
        Some(other) => alloc::format!("{}", other),
        None => return Err(EvalError::new("send: нечего отправлять (слева нужен `>>`)")),
    };
    let ep = net_ep("send")?;
    let h = match sys::net_cli::tcp_connect(ep, ip, port) {
        Ok(h) => h,
        // Веха 199.20 — ТИШИНА И ОТКАЗ это разные вещи, и человеку надо сказать какая. Закрытый
        // порт отвечает отказом (`ST_ERR`); `ST_TIMEOUT` значит, что наш запрос ушёл и не
        // вернулось ничего.
        //
        // Веха 220.2 — и БОЛЬШЕ НИЧЕГО. Прежде здесь стояли четыре строки догадок: фаервол,
        // настройка NixOS, изоляция клиентов на роутере. Владелец об этом не спрашивал; сказать
        // факт и замолчать — его работа, а не наша.
        Err(st) if st == sys::net_cli::ST_TIMEOUT => {
            return Err(EvalError::new(alloc::format!(
                "send: {} молчит — запрос ушёл, ответа нет (отказ выглядел бы иначе)",
                host,
            )))
        }
        Err(st) => return Err(net_err("send", st)),
    };
    // Шлём кусками: сервер принимает столько, сколько готов, и остаток — наша забота (как у
    // `write(2)`). Без этого цикла ушёл бы только первый кусок, а выглядело бы как «журнал
    // обрезан на ровном месте».
    let bytes = text.as_bytes();
    let mut sent = 0usize;
    while sent < bytes.len() {
        match sys::net_cli::tcp_send(ep, h, &bytes[sent..]) {
            Ok(0) => break,
            Ok(n) => sent += n,
            Err(st) => {
                let _ = sys::net_cli::tcp_close(ep, h);
                return Err(net_err("send", st));
            }
        }
    }
    // Веха 199.21 — «отправлено» говорим ТОЛЬКО ПОСЛЕ закрытия, и не раньше.
    //
    // Раньше эта строка печаталась сразу за последним `tcp_send`, то есть сообщала, сколько
    // байт принял БУФЕР, и выглядела как отчёт о доставке. Владелец получал «отправлено 14834
    // байт» и пустой файл на другой машине: `nc` копит принятое и сбрасывает по концу потока, а
    // конца не было, пока наш FIN не доехал. Теперь закрытие ждёт подтверждения (`net-srv`
    // отвечает по факту `Closed`), и «отправлено» значит «доставлено».
    match sys::net_cli::tcp_close(ep, h) {
        Ok(()) => {
            // Веха 199.22 — говорим ОБА числа. Отдали меньше, чем было, — это обрезанная
            // передача, и она обязана быть видна: `Ok(0) => break` выше срабатывает, когда
            // сервер перестал принимать, и прежняя строка сообщала об этом как об успехе.
            if sent < bytes.len() {
                sys::write(
                    alloc::format!(
                        "send: доставлено {} байт из {} — передача ОБРЕЗАНА (сервер перестал принимать)\n",
                        sent, bytes.len(),
                    )
                    .as_bytes(),
                );
                return Ok(Value::nil());
            }
            sys::write(tf("отправлено: {} байт\n", &[&alloc::format!("{}", sent)]).as_bytes());
            Ok(Value::nil())
        }
        Err(st) if st == sys::net_cli::ST_TIMEOUT => Err(EvalError::new(alloc::format!(
            "send: {} байт отдано сети, но закрытие не подтвердилось — доставка под вопросом.\n\
             Другая сторона могла не получить конец потока: если она копит принятое в буфере\n\
             (`nc … > файл`), файл окажется пустым или обрезанным.",
            sent,
        ))),
        Err(st) => Err(net_err("send", st)),
    }
}

// ── Веха 200 — ЗАГЛЯНУТЬ В ЖЕЛЕЗО ──────────────────────────────────────────────────────────
//
// Зачем это в шелле. Фаза драйверов (Вехи 191–199) шла циклами «пересобрал → записал на флешку →
// загрузился → отправил журнал», и раз за разом выяснялось, что не хватает ОДНОГО ЧИСЛА из
// регистра: предел кадра у сетевой карты, состояние порта у контроллера USB, флаги ошибок шины.
// Каждое такое число стоило круга — а с этими двумя командами спрашивается на живой машине
// одной строкой.
//
// Право спрашиваем у композитора (как выключение и сеть): в оконном сеансе своё оно шеллу не
// достаётся, а конфиг называет получателя строкой `desktop hwprobe bin/vvsh`.
fn cap_hw(who: &str) -> Result<usize, EvalError> {
    let c = sys::win::cap_or_grant(15);
    if c == sys::NO_CAP {
        return Err(EvalError::new(alloc::format!(
            "{}: нет права на регистры — нужна строка `desktop hwprobe bin/vvsh` в конфиге",
            who,
        )));
    }
    Ok(c)
}

/// Разобрать число: `0x…` шестнадцатеричное, иначе десятичное. Адреса регистров человек читает
/// из спецификаций в шестнадцатеричном виде, и требовать перевода было бы издевательством.
fn num(v: &Value) -> Option<u64> {
    match v {
        Value::Int(i) => Some(*i as u64),
        Value::Str(s) => {
            let t = s.trim();
            match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                Some(h) => u64::from_str_radix(h, 16).ok(),
                None => t.parse::<u64>().ok(),
            }
        }
        _ => None,
    }
}

/// `(beep [частота] [миллисекунды])` — короткий сигнал. Без аргументов — 880 Гц на 120 мс.
///
/// Команда шелла, а не программа: звук нужен ровно там, где длинная работа кончилась и человек
/// смотрит в другую сторону (`rebuild && beep`), — и заводить ради двух чисел отдельный процесс
/// значило бы платить за него больше, чем стоит сам сигнал.
fn sh_beep(args: &[Value]) -> Result<Value, EvalError> {
    let ep = cap_snd();
    if ep == sys::NO_CAP {
        return Err(EvalError::new(
            "beep: звука нет — ни строки `service hda` в поколении, ни звуковой карты в машине",
        ));
    }
    let hz = args.first().and_then(num).unwrap_or(880) as u32;
    let ms = args.get(1).and_then(num).unwrap_or(120) as u32;
    match sys::snd_cli::beep(ep, hz, ms) {
        sys::snd_cli::ST_OK => Ok(Value::nil()),
        sys::snd_cli::ST_NO_SOUND => Err(EvalError::new(
            "beep: звука в этой машине нет (сервер `hda` не поднялся — нет звуковой карты)",
        )),
        _ => Err(EvalError::new("beep: частота 20..20000 Гц, длительность больше нуля")),
    }
}

/// Веха 204 — `volume` БЕЗ АРГУМЕНТОВ показывает миксер, `volume(N)` ставит общую громкость,
/// `volume(номер, N)` — громкость одного голоса.
///
/// Тот же миксер, что в меню панели, только словами: меню читает и пишет ровно эти же две
/// операции сервера. Терминальный путь нужен не для красоты — им проверяется звук на машине,
/// где панели может и не быть (`mode = "term"`), и им же видно то, чего в меню не показывают:
/// номера голосов.
fn sh_volume(args: &[Value]) -> Result<Value, EvalError> {
    let ep = cap_snd();
    if ep == sys::NO_CAP {
        return Err(EvalError::new(
            "volume: звука нет — ни строки `service hda` в поколении, ни звуковой карты в машине",
        ));
    }
    // Один аргумент — общая громкость, два — громкость голоса. Ноль как номер голоса не
    // годится: им обозначен мастер, и `volume(0, 50)` значило бы то же, что `volume(50)`.
    let nums: Vec<u64> = args.iter().filter_map(num).collect();
    match nums.len() {
        0 => {}
        1 => {
            let v = nums[0].min(100) as u8;
            if sys::snd_cli::volume(ep, 0, v) != sys::snd_cli::ST_OK {
                return Err(EvalError::new("volume: сервер звука не принял громкость"));
            }
        }
        _ => {
            let (id, v) = (nums[0].clamp(1, 65535) as u16, nums[1].min(100) as u8);
            if sys::snd_cli::volume(ep, id, v) != sys::snd_cli::ST_OK {
                return Err(EvalError::new("volume: такого голоса у сервера нет (он уже смолк?)"));
            }
        }
    }
    let Some(st) = sys::snd_cli::state(ep) else {
        return Err(EvalError::new("volume: сервер звука не ответил"));
    };
    let mut out = alloc::format!("общая {} % → {}\n", st.master, st.out());
    if st.voices().is_empty() {
        out.push_str("сейчас никто не играет\n");
    }
    for v in st.voices() {
        out.push_str(&alloc::format!("  {:>3}  {:>3} %  {}\n", v.id, v.vol, v.name()));
    }
    Ok(Value::str(&out))
}

/// `(mmio адрес…)` — слова регистров устройства по физическим адресам; `(mmio адрес = значение)`
/// — ЗАПИСАТЬ.
///
/// Веха 200.2 — адресов можно несколько, и результат ВОЗВРАЩАЕТСЯ текстом, а не печатается.
/// Снятый с железа набор чисел человек не должен переписывать руками: `mmio … >> send` уносит
/// его целиком (см. `pipe_into` — конвейер берёт текст только у того, кто его отдаёт значением).
/// Запись отделена знаком `=`, иначе второй адрес и записываемое значение неразличимы.
fn sh_mmio(args: &[Value]) -> Result<Value, EvalError> {
    const USAGE: &str = "mmio: (mmio \"0xdfc08000\" …) либо (mmio \"0xdfc08000\" = значение)";
    if args.is_empty() {
        return Err(EvalError::new(USAGE));
    }
    let hw = cap_hw("mmio")?;
    if let Some(i) = args.iter().position(|a| matches!(a, Value::Str(s) if &**s == "=")) {
        if i != 1 || args.len() != 3 {
            return Err(EvalError::new("mmio: записывается один адрес за раз"));
        }
        let (Some(pa), Some(v)) = (num(&args[0]), args.get(2).and_then(num)) else {
            return Err(EvalError::new(USAGE));
        };
        if !sys::hw_write(hw, pa as usize, v as u32) {
            return Err(EvalError::new(
                "mmio: записать не удалось (нет права `w`, адрес не выровнен или это ОЗУ)",
            ));
        }
        return Ok(Value::str(&alloc::format!("{:#x} ← {:#010x}", pa, v)));
    }
    let mut out = String::new();
    for a in args {
        let Some(pa) = num(a) else {
            return Err(EvalError::new(USAGE));
        };
        match sys::hw_read(hw, pa as usize) {
            Some(v) => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&alloc::format!("{:#x}: {:#010x} ({})", pa, v, v));
            }
            // Отказ здесь значит вполне определённое, и это стоит сказать: чаще всего человек
            // целится в оперативную память, а её ядро закрывает нарочно. Адрес называем: при
            // списке из пяти регистров «не удалось» без имени виновника бесполезно.
            None => {
                return Err(EvalError::new(alloc::format!(
                    "mmio {:#x}: прочитать не удалось — адрес не выровнен по слову либо это ОЗУ (его нельзя)",
                    pa
                )))
            }
        }
    }
    Ok(Value::str(&out))
}

/// `(pci "шина:устройство.функция" смещение [значение])` — слово конфигурации PCI.
fn sh_pci(args: &[Value]) -> Result<Value, EvalError> {
    let Some(Value::Str(loc)) = args.first() else {
        return Err(EvalError::new(PCI_USAGE));
    };
    // `шина:устройство.функция` — та же запись, которой устройства называет опись шины в журнале
    // ядра. Человек копирует строку оттуда, а не считает биты.
    let (bus, rest) = loc.split_once(':').unwrap_or(("0", loc));
    let (dev, func) = rest.split_once('.').unwrap_or((rest, "0"));
    let parse = |s: &str| u16::from_str_radix(s.trim(), 16).ok();
    let (Some(b), Some(d), Some(f)) = (parse(bus), parse(dev), parse(func)) else {
        return Err(EvalError::new("pci: адрес вида \"04:00.0\" (шестнадцатеричный)"));
    };
    if d > 31 || f > 7 {
        return Err(EvalError::new("pci: устройство 0..1f, функция 0..7"));
    }
    let bdf = b << 8 | d << 3 | f;
    if args.len() < 2 {
        return Err(EvalError::new(PCI_USAGE));
    }
    let hw = cap_hw("pci")?;
    // Смещений, как и адресов у `mmio`, может быть несколько; запись отделена знаком `=`.
    if let Some(i) = args.iter().position(|a| matches!(a, Value::Str(s) if &**s == "=")) {
        if i != 2 || args.len() != 4 {
            return Err(EvalError::new("pci: записывается одно смещение за раз"));
        }
        let (Some(off), Some(v)) = (num(&args[1]), args.get(3).and_then(num)) else {
            return Err(EvalError::new(PCI_USAGE));
        };
        if !sys::hw_pci_write(hw, bdf, off as usize, v as u32) {
            return Err(EvalError::new(
                "pci: записать не удалось (нет права `w` либо смещение не то)",
            ));
        }
        return Ok(Value::str(&alloc::format!("{} +{:#x} ← {:#010x}", loc, off, v)));
    }
    let mut out = String::new();
    for a in &args[1..] {
        let Some(off) = num(a) else {
            return Err(EvalError::new(PCI_USAGE));
        };
        match sys::hw_pci_read(hw, bdf, off as usize) {
            Some(v) => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&alloc::format!("{} +{:#x}: {:#010x}", loc, off, v));
            }
            None => {
                return Err(EvalError::new(alloc::format!(
                    "pci +{:#x}: прочитать не удалось — смещение 0..0xfc, кратное четырём",
                    off
                )))
            }
        }
    }
    Ok(Value::str(&out))
}

/// Как звать `pci` — в одном месте: строка нужна четырежды, и расходиться копиям незачем.
const PCI_USAGE: &str = "pci: (pci \"04:00.0\" \"0x04\" …) либо (pci \"04:00.0\" \"0x04\" = значение)";

/// `(poweroff)` — выключить машину (Веха 101). Нужно право `power` из конфига: выключение —
/// одностороннее действие над всей системой, и оно названо правом, а не считается общедоступным.
fn sh_poweroff(_args: &[Value]) -> Result<Value, EvalError> {
    sys::write(sys::i18n::t("выключаю машину…\n").as_bytes());
    // Веха 157 — в тексте право приходит из конфига стартовым, а в ОКНЕ его приходится просить у
    // композитора: с Вехи 154 выключение помечено «не наследуется», и шелл, запущенный терминалом,
    // получал бы его только вместе со всеми окнами разом.
    let pc = sys::win::cap_or_grant(10);
    if pc != sys::NO_CAP {
        sys::power_off(pc);
    }
    Err(EvalError::new(
        "poweroff: нет права `power` — в оконном режиме нужна строка `desktop power bin/vvsh`",
    ))
}

/// `(reboot)` — перезагрузить машину (Веха 197). Право то же, что у выключения (`power`), и по
/// той же причине: разница лишь в том, поднимется ли система обратно.
///
/// Нужна эта команда прежде всего после `rebuild`: поколение становится активным только на
/// следующей загрузке, и до сих пор «перезагрузись» означало выключить машину и включить её
/// руками — а на ноутбуке ещё и дойти до кнопки.
fn sh_reboot(_args: &[Value]) -> Result<Value, EvalError> {
    sys::write(sys::i18n::t("перезагружаю машину…\n").as_bytes());
    let pc = sys::win::cap_or_grant(10);
    if pc != sys::NO_CAP {
        sys::reboot(pc);
    }
    Err(EvalError::new(
        "reboot: нет права `power` — в оконном режиме нужна строка `desktop power bin/vvsh`",
    ))
}

fn sh_switch(args: &[Value]) -> Result<Value, EvalError> {
    let name = match args.first() {
        Some(Value::Str(s)) => s.clone(),
        _ => return Err(EvalError::new("switch: (switch \"gen\")")),
    };
    let scap = cap_store();
    let mut id = [0u8; 32];
    if sys::obj_put(scap, name.as_bytes(), &mut id) == 0
        && sys::obj_set_root(scap, CURRENT_ROOT, &id) == 0
    {
        sys::write(tf("поколение {} выбрано — перезагрузи машину\n", &[&name]).as_bytes());
        Ok(Value::nil())
    } else {
        Err(EvalError::new("switch не удался (нет права WRITE на store?)"))
    }
}

/// `(sysdef "gen" "файл")` — зарегистрировать содержимое файла как поколение `system/gen`.
fn sh_sysdef(args: &[Value]) -> Result<Value, EvalError> {
    let (gname, fname) = match (args.first(), args.get(1)) {
        (Some(Value::Str(g)), Some(Value::Str(f))) => (g.clone(), f.clone()),
        _ => return Err(EvalError::new("sysdef: (sysdef \"gen\" \"файл\")")),
    };
    let ep = cap_fs();
    let scap = cap_store();
    let path = resolve(fname.as_bytes());
    let data = match read_file(ep, &path) {
        Some(d) => d,
        None => return Err(EvalError::new("sysdef: нет такого файла")),
    };
    let mut root = Vec::with_capacity(7 + gname.len());
    root.extend_from_slice(b"system/");
    root.extend_from_slice(gname.as_bytes());
    let mut id = [0u8; 32];
    if sys::obj_put(scap, &data, &mut id) == 0 && sys::obj_set_root(scap, &root, &id) == 0 {
        sys::write(
            alloc::format!("поколение записано: {} (switch {}, затем ребут)\n", gname, gname)
                .as_bytes(),
        );
        Ok(Value::nil())
    } else {
        Err(EvalError::new("sysdef не удался"))
    }
}

/// `(rebuild)` — собрать поколение из `/etc/system/default.vv` (та же логика, что у подкоманды).
fn sh_rebuild(_args: &[Value]) -> Result<Value, EvalError> {
    run_rebuild();
    Ok(Value::nil())
}

// ── корни store прямой командой (Веха 218) ──────────────────────────────────
//
// Корни можно было только ПОСМОТРЕТЬ (`roots`). Завести — нечем, и это упиралось в стену всякий
// раз, когда рядом с конфигом должно лежать что-то, чему в конфиге не место: пароль сети, список
// блокировки, закладки файлового менеджера.
//
// Замысел владельца: завести корень командой, а в конфиге назвать его ИМЯ. Тогда конфиг можно
// показывать и копировать — в нём ссылка, а не секрет.

/// `(root "имя")` — показать значение; `(root "имя" "значение")` — задать.
fn sh_root(args: &[Value]) -> Result<Value, EvalError> {
    let Some(Value::Str(name)) = args.first() else {
        return Err(EvalError::new("root: (root \"имя\" [\"значение\"])"));
    };
    let scap = cap_store();
    match args.get(1) {
        Some(Value::Str(val)) => put_root(scap, name.as_bytes(), val.as_bytes()),
        None => show_root(scap, name.as_bytes()),
        _ => Err(EvalError::new("root: значение — строка")),
    }
}

/// `(root-del "имя")` — снять корень. Содержимое переживёт снятие до сборки мусора.
fn sh_root_del(args: &[Value]) -> Result<Value, EvalError> {
    let Some(Value::Str(name)) = args.first() else {
        return Err(EvalError::new("root-del: (root-del \"имя\")"));
    };
    if sys::obj_del_root(cap_store(), name.as_bytes()) == 0 {
        sys::write(tf("корень {} снят\n", &[name]).as_bytes());
        Ok(Value::nil())
    } else {
        Err(EvalError::new("root-del: такого корня нет (или нет права WRITE на store)"))
    }
}

/// `(secret "имя")` — спросить значение и НЕ ПОКАЗЫВАТЬ его.
///
/// Отдельная команда, а не ключ к `root`, ровно по одной причине: пароль, набранный обычной
/// командой, остаётся на экране и в истории строк. Здесь его не видно ни там, ни там.
fn sh_secret(args: &[Value]) -> Result<Value, EvalError> {
    let Some(Value::Str(name)) = args.first() else {
        return Err(EvalError::new("secret: (secret \"имя\")"));
    };
    let mut buf = [0u8; 256];
    let n = read_hidden("значение (скрыто): ".as_bytes(), &mut buf);
    if n == 0 {
        return Err(EvalError::new("secret: пусто — корень не тронут"));
    }
    let r = put_root(cap_store(), name.as_bytes(), &buf[..n]);
    buf.fill(0); // не оставлять пароль в стеке дольше, чем нужно
    r
}

fn put_root(scap: usize, name: &[u8], value: &[u8]) -> Result<Value, EvalError> {
    let mut id = [0u8; 32];
    if sys::obj_put(scap, value, &mut id) == 0 && sys::obj_set_root(scap, name, &id) == 0 {
        sys::write(tf("корень {} задан ({} Б)\n", &[
            core::str::from_utf8(name).unwrap_or("?"),
            &alloc::format!("{}", value.len()),
        ]).as_bytes());
        Ok(Value::nil())
    } else {
        Err(EvalError::new("root: не записалось (нет права WRITE на store?)"))
    }
}

fn show_root(scap: usize, name: &[u8]) -> Result<Value, EvalError> {
    let mut id = [0u8; 32];
    if sys::obj_get_root(scap, name, &mut id) != 32 {
        return Err(EvalError::new("root: такого корня нет"));
    }
    let mut buf = [0u8; 4096];
    let n = sys::obj_get(scap, &id, &mut buf);
    if n == usize::MAX {
        return Err(EvalError::new("root: значение не читается (нет права READ на store?)"));
    }
    sys::write(&buf[..n.min(buf.len())]);
    if n > 0 && buf[n.min(buf.len()) - 1] != b'\n' {
        sys::write(b"\n");
    }
    Ok(Value::nil())
}

/// Прочитать строку, НЕ отображая набранное. Свой маленький читатель, а не ключ к редактору
/// строк: тому нужны история, курсор и перерисовка — то есть ровно то, чего здесь быть не должно.
fn read_hidden(prompt: &[u8], out: &mut [u8]) -> usize {
    sys::write(prompt);
    let mut n = 0usize;
    let mut inb = [0u8; 16];
    loop {
        let got = sys::read_stdin(&mut inb);
        if got == 0 {
            break;
        }
        for &b in &inb[..got] {
            match b {
                b'\n' | b'\r' => {
                    sys::write(b"\n");
                    return n;
                }
                // Веха 220.2 — ЗВЁЗДОЧКА ЗА ЗНАК, по просьбе владельца. Пустая строка не отличает
                // «набрано ничего» от «клавиатура не доходит», и набирать пароль вслепую вдвойне
                // неприятно там, где ошибку увидишь только через минуту, на подключении.
                //
                // Показывается ДЛИНА, а не знаки: длину пароля и так видно тому, кто стоит рядом
                // и считает нажатия, а вот сами знаки — нет.
                0x08 | 0x7f => {
                    if n > 0 {
                        n -= 1;
                        // Забой в терминале не стирает — он двигает курсор; стираем пробелом.
                        sys::write(b"\x08 \x08");
                    }
                }
                _ if n < out.len() => {
                    out[n] = b;
                    n += 1;
                    sys::write(b"*");
                }
                _ => {}
            }
        }
    }
    sys::write(b"\n");
    n
}

/// `(gens)` — показать поколения системы.
fn sh_gens(_args: &[Value]) -> Result<Value, EvalError> {
    run_gens();
    Ok(Value::nil())
}

/// `(init-config [--force])` — посеять либо СВЕРИТЬ `/etc/system/*.vv` с шаблоном (Веха 191).
///
/// `--force` принимается и отсюда: сама подсказка команды советует набрать именно это, и
/// отправлять человека запускать `vvsh` отдельной программой ради собственного совета было бы
/// издевательством. Произнесённое вслух намерение — это и есть набранное слово.
fn sh_init_config(args: &[Value]) -> Result<Value, EvalError> {
    let force = args.iter().any(|a| matches!(a, Value::Str(s) if &**s == "--force"));
    run_init_config(force);
    Ok(Value::nil())
}


// ── редактор строки (S2c ч.2): история ↑/↓, курсор ←/→/Home/End, backspace/Delete ──
// Байт-ориентированный (курсор в колонках=байтах — ASCII точен; многобайтные символы редактируются
// грубо, но для команд/путей хватает). vsh (спасательный шелл) НЕ трогаем — свой редактор здесь.

const HISTN: usize = 8;
/// Веха 101 — было 256, и этого не хватало ровно там, где важнее всего: редактора файлов у нас
/// нет, `.vv` правится командой `echo … > файл`, а модуль конфига в одну строку длиннее 256 байт
/// запросто (`terminal.vv` — полторы тысячи). Строка обрывалась МОЛЧА. 1 КиБ × 8 записей истории
/// = 8 КиБ на стеке при 256 КиБ у процесса — запас есть.
const LINE_CAP: usize = 1024;

struct History {
    buf: [[u8; LINE_CAP]; HISTN],
    len: [usize; HISTN],
    head: usize,  // следующий слот записи
    count: usize, // сохранено (≤ HISTN)
}

impl History {
    fn new() -> Self {
        History { buf: [[0; LINE_CAP]; HISTN], len: [0; HISTN], head: 0, count: 0 }
    }
    fn push(&mut self, line: &[u8]) {
        if line.is_empty() {
            return;
        }
        if self.count > 0 {
            let last = (self.head + HISTN - 1) % HISTN;
            if self.buf[last][..self.len[last]] == *line {
                return; // не дублировать подряд
            }
        }
        let n = line.len().min(LINE_CAP);
        self.buf[self.head][..n].copy_from_slice(&line[..n]);
        self.len[self.head] = n;
        self.head = (self.head + 1) % HISTN;
        if self.count < HISTN {
            self.count += 1;
        }
    }
    fn get(&self, back: usize) -> Option<&[u8]> {
        if back == 0 || back > self.count {
            return None;
        }
        let slot = (self.head + HISTN - back) % HISTN;
        Some(&self.buf[slot][..self.len[slot]])
    }
}

/// Прочитать строку с редактированием. `None` — EOF (Ctrl-D на пустой). `hb` — просмотр истории.
fn read_line(prompt: &[u8], line: &mut [u8], hist: &History) -> Option<usize> {
    let mut llen = 0usize;
    let mut pos = 0usize;
    let mut esc = 0u8; // 0 обычный, 1 после ESC, 2 после ESC[
    let mut hb = 0usize; // индекс истории (0 — свежая строка)
    let mut inb = [0u8; 16];
    sys::write(prompt);
    loop {
        let n = sys::read_stdin(&mut inb);
        if n == 0 {
            return if llen == 0 { None } else { Some(llen) };
        }
        for &b in &inb[..n] {
            match esc {
                1 => esc = if b == b'[' { 2 } else { 0 },
                2 => {
                    if b.is_ascii_digit() || b == b';' {
                        continue; // параметр CSI — копим до финального байта
                    }
                    esc = 0;
                    match b {
                        b'C' => {
                            if pos < llen {
                                pos = next_char(line, pos, llen);
                                sys::write(b"\x1b[C");
                            }
                        }
                        b'D' => {
                            if pos > 0 {
                                pos = prev_char(line, pos);
                                sys::write(b"\x1b[D");
                            }
                        }
                        b'H' => {
                            pos = 0;
                            redraw(prompt, line, llen, pos);
                        }
                        b'F' => {
                            pos = llen;
                            redraw(prompt, line, llen, pos);
                        }
                        b'A' => {
                            if hb < hist.count {
                                hb += 1;
                                if let Some(h) = hist.get(hb) {
                                    llen = h.len().min(line.len());
                                    line[..llen].copy_from_slice(&h[..llen]);
                                    pos = llen;
                                    redraw(prompt, line, llen, pos);
                                }
                            }
                        }
                        b'B' => {
                            if hb > 1 {
                                hb -= 1;
                                if let Some(h) = hist.get(hb) {
                                    llen = h.len().min(line.len());
                                    line[..llen].copy_from_slice(&h[..llen]);
                                }
                            } else {
                                hb = 0;
                                llen = 0;
                            }
                            pos = llen;
                            redraw(prompt, line, llen, pos);
                        }
                        b'~' => {
                            if pos < llen {
                                let e = next_char(line, pos, llen);
                                line.copy_within(e..llen, pos);
                                llen -= e - pos;
                                redraw(prompt, line, llen, pos);
                            }
                        }
                        _ => {}
                    }
                }
                _ => match b {
                    b'\r' | b'\n' => {
                        sys::write(b"\r\n");
                        return Some(llen);
                    }
                    0x1b => esc = 1,
                    0x7f | 0x08 => {
                        if pos > 0 {
                            let p = prev_char(line, pos);
                            line.copy_within(pos..llen, p);
                            llen -= pos - p;
                            pos = p;
                            redraw(prompt, line, llen, pos);
                        }
                    }
                    0x04 => {
                        if llen == 0 {
                            return None; // Ctrl-D на пустой строке — EOF
                        }
                    }
                    0x03 => {
                        sys::write(b"^C\r\n"); // Ctrl-C — отменить строку
                        return Some(0);
                    }
                    c if c >= 0x20 => {
                        if llen < line.len() {
                            line.copy_within(pos..llen, pos + 1);
                            line[pos] = c;
                            llen += 1;
                            pos += 1;
                            if pos == llen {
                                // Эхо ЦЕЛЫМ символом: терминал рисует кадр между записями, и
                                // половина кириллической буквы успевала мелькнуть на экране «?».
                                let st = prev_char(line, pos);
                                if pos - st == utf8_len(line[st]) {
                                    sys::write(&line[st..pos]);
                                }
                            } else {
                                redraw(prompt, line, llen, pos);
                            }
                        }
                    }
                    _ => {}
                },
            }
        }
    }
}

/// Веха 143.2 — граница символа СЛЕВА от `i`. Строка ввода живёт в байтах, а редактируется по
/// СИМВОЛАМ: кириллица в UTF-8 занимает два байта, и шаг в байт оставлял половину буквы —
/// терминал рисовал на её месте «?», а стиралась она со второго нажатия.
fn prev_char(line: &[u8], i: usize) -> usize {
    let mut j = i;
    while j > 0 {
        j -= 1;
        // Продолжения UTF-8 — байты вида 10xxxxxx; начало символа — любой другой.
        if line[j] & 0xC0 != 0x80 {
            break;
        }
    }
    j
}

/// Граница символа СПРАВА от `i` (для Delete и стрелки вправо).
fn next_char(line: &[u8], i: usize, llen: usize) -> usize {
    let mut j = (i + 1).min(llen);
    while j < llen && line[j] & 0xC0 == 0x80 {
        j += 1;
    }
    j
}

/// Сколько ЗНАКОМЕСТ занимает кусок строки: байты-продолжения места не занимают. Нужно сдвигу
/// курсора — тот считает колонки, а не байты.
fn cols(part: &[u8]) -> usize {
    part.iter().filter(|&&b| b & 0xC0 != 0x80).count()
}

/// Сколько байт в символе по его первому байту.
fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// Перерисовать строку ввода целиком: в начало, приглашение, содержимое, стереть хвост, вернуть
/// курсор — ОДНОЙ записью.
///
/// Одной, а не пятью, и это не экономия. Каждая `write` — вызов к терминалу, и тот успевает
/// нарисовать кадр МЕЖДУ ними: на экране мелькало промежуточное состояние, где возврат каретки
/// уже сделан, а строка ещё не напечатана. Со стороны это «при стирании курсор прыгает в начало
/// строки и возвращается» — давняя жалоба владельца, и причина оказалась не в терминале.
fn redraw(prompt: &[u8], line: &[u8], llen: usize, pos: usize) {
    // Приглашение (320) + строка (LINE_CAP) + управляющие последовательности. Больше `CHUNK`
    // stdio всё равно поедет двумя сообщениями, но это уже длина строки, а не наша щедрость.
    let mut buf = [0u8; 1400];
    let mut i = 0usize;
    append(&mut buf, &mut i, b"\r");
    append(&mut buf, &mut i, prompt);
    append(&mut buf, &mut i, &line[..llen]);
    append(&mut buf, &mut i, b"\x1b[K"); // стереть до конца строки
    if pos < llen {
        // Влево — на число ЗНАКОМЕСТ, а не байт: иначе на кириллице курсор уезжал вдвое дальше.
        csi_num(&mut buf, &mut i, cols(&line[pos..llen]), b'D');
    }
    sys::write(&buf[..i]);
}

/// Дописать управляющую последовательность `ESC[<n><fin>` в буфер (сдвиг курсора и подобное).
fn csi_num(out: &mut [u8], i: &mut usize, n: usize, fin: u8) {
    if n == 0 {
        return;
    }
    append(out, i, b"\x1b[");
    let mut tmp = [0u8; 10];
    let mut t = 0;
    let mut m = n;
    while m > 0 {
        tmp[t] = b'0' + (m % 10) as u8;
        t += 1;
        m /= 10;
    }
    while t > 0 {
        t -= 1;
        append(out, i, &tmp[t..t + 1]);
    }
    append(out, i, &[fin]);
}

fn append(out: &mut [u8], i: &mut usize, bytes: &[u8]) {
    for &b in bytes {
        if *i < out.len() {
            out[*i] = b;
            *i += 1;
        }
    }
}

/// Собрать цветное приглашение `vvsh<cwd>> ` (зелёный `vvsh`, синий каталог) — как у vsh.
/// ANSI-коды нулевой ширины, потому редактор строки считает колонки верно.
fn build_prompt(out: &mut [u8]) -> usize {
    let mut i = 0;
    append(out, &mut i, C_PROMPT);
    append(out, &mut i, b"vvsh");
    append(out, &mut i, C_RESET);
    append(out, &mut i, C_DIR);
    let mut cwd = [0u8; 256];
    let n = cwd_get(&mut cwd);
    append(out, &mut i, &cwd[..n]);
    append(out, &mut i, C_RESET);
    append(out, &mut i, b"> ");
    i
}

// ── помощники store ──────────────────────────────────────────────────────────

/// Текст активного поколения целиком: `system/current` → имя → `system/<имя>`. Нужен языку
/// вывода (Веха 178); ровно тот же текст читают панель, композитор и терминал.
fn read_generation_text() -> Option<alloc::string::String> {
    let scap = cap_store();
    let name = read_current_name(scap)?;
    let id = gen_content_id(scap, &name)?;
    let mut buf = alloc::vec![0u8; 64 * 1024];
    let n = sys::obj_get(scap, &id, &mut buf);
    if n == 0 || n > buf.len() {
        return None;
    }
    buf.truncate(n);
    alloc::string::String::from_utf8(buf).ok()
}

/// Имя активного поколения (значение корня `system/current`). `None` — нет/нет READ.
fn read_current_name(scap: usize) -> Option<Vec<u8>> {
    let mut id = [0u8; 32];
    if sys::obj_get_root(scap, CURRENT_ROOT, &mut id) != 32 {
        return None;
    }
    let mut buf = [0u8; 64];
    let n = sys::obj_get(scap, &id, &mut buf);
    if n == 0 {
        return None;
    }
    Some(trim(&buf[..n]).to_vec())
}

/// Content-id поколения `system/<имя>`. `None` — нет/нет READ.
/// Веха 220.1 — текст поколения по его content-id. `None` — не читается (нет права либо объект
/// больше буфера; конфиг в двадцать килобайт — это уже не конфиг).
fn gen_text(scap: usize, id: &[u8; 32]) -> Option<String> {
    let mut buf = alloc::vec![0u8; 20 * 1024];
    let (got, full) = sys::obj_get_ex(scap, id, &mut buf);
    if got == 0 || got != full {
        return None;
    }
    buf.truncate(got);
    String::from_utf8(buf).ok()
}

fn gen_content_id(scap: usize, gen: &[u8]) -> Option<[u8; 32]> {
    let mut root = Vec::with_capacity(7 + gen.len());
    root.extend_from_slice(b"system/");
    root.extend_from_slice(gen);
    let mut id = [0u8; 32];
    if sys::obj_get_root(scap, &root, &mut id) == 32 {
        Some(id)
    } else {
        None
    }
}

/// Максимальный N среди корней `system/gen<N>` + 1 (нумерация поколений). LIST_ROOTS — по WRITE.
///
/// `None` — список корней не прочитан целиком: номер тогда НЕ выдумывается. Иначе «не увидели
/// gen3» стало бы «собираем gen3 заново», то есть затиранием существующего поколения.
fn next_gen_number(scap: usize) -> Option<u32> {
    let text = roots::text(scap)?;
    Some(roots::gen_numbers(&text, b"system/gen").last().copied().unwrap_or(0) + 1)
}

fn trim(mut s: &[u8]) -> &[u8] {
    while let [f, rest @ ..] = s {
        if f.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    while let [rest @ .., l] = s {
        if l.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    s
}

// ── помощники файлов ─────────────────────────────────────────────────────────

/// Прочитать конфиг-файл в текст (UTF-8). `Err(код-выхода)` с уже напечатанной причиной.
fn read_config_text(ep: usize, path: &[u8]) -> Result<String, usize> {
    let src = match read_file(ep, path) {
        Some(s) => s,
        None => {
            sys::write(sys::i18n::t("vvsh: не удалось прочитать файл: ").as_bytes());
            sys::write(path);
            sys::write(b"\n");
            return Err(1);
        }
    };
    match String::from_utf8(src) {
        Ok(t) => Ok(t),
        Err(_) => {
            sys::write(sys::i18n::t("vvsh: файл не UTF-8\n").as_bytes());
            Err(1)
        }
    }
}

fn fail(msg: &str) -> ! {
    sys::write(sys::i18n::t("vvsh: ошибка: ").as_bytes());
    sys::write(msg.as_bytes());
    sys::write(b"\n");
    sys::exit(1);
}

/// Прочитать файл posixfs целиком в `Vec<u8>`. `None` — файла нет, это каталог (`stat` до
/// `open`, чтобы опечатка в пути не плодила пустышку — у posixfs `open` создаёт файл) ЛИБО чтение
/// оборвалось на середине.
///
/// Оборвалось — значит `None`, а не половина файла. Причин две, и обе серьёзные:
///
/// 1. `px::read` отвечает [`usize::MAX`], когда вызов не состоялся вовсе (сервер умер, право
///    отозвано). Это значение уходило прямо в `&chunk[..k]` и роняло ШЕЛЛ паникой — то есть
///    смерть posixfs посреди чтения уносила с собой и того, кто читал.
/// 2. На прочитанном считается КОНФИГ ПОКОЛЕНИЯ (`sysdef`, `import`). Половина конфига — это не
///    «меньше настроек», а другая система: вычислится она молча и до конца.
fn read_file(ep: usize, path: &[u8]) -> Option<Vec<u8>> {
    match px::stat(ep, path) {
        Some((is_dir, _)) if !is_dir => {}
        _ => return None,
    }
    let fd = px::open(ep, path, 0);
    if fd == usize::MAX {
        return None;
    }
    let mut out = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let k = px::read(ep, fd, &mut chunk);
        if k == usize::MAX {
            px::close(ep, fd);
            return None;
        }
        if k == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..k]);
    }
    px::close(ep, fd);
    Some(out)
}

/// Каталог пути (всё до последнего '/', включительно). Без '/' — пусто (относительно корня).
fn dirname(path: &[u8]) -> Vec<u8> {
    match path.iter().rposition(|&b| b == b'/') {
        Some(i) => path[..=i].to_vec(),
        None => Vec::new(),
    }
}

/// Загрузчик модулей `import` поверх posixfs (M1b). Имя резолвится относительно `base` (каталога
/// корневого файла); имя, начинающееся с '/', — абсолютный путь.
struct FsLoader {
    ep: usize,
    base: Vec<u8>,
}

impl vvsh_core::ModuleLoader for FsLoader {
    fn load(&self, name: &str) -> Result<String, String> {
        let nb = name.as_bytes();
        let mut path = Vec::new();
        if nb.first() == Some(&b'/') {
            path.extend_from_slice(nb);
        } else {
            path.extend_from_slice(&self.base);
            path.extend_from_slice(nb);
        }
        match read_file(self.ep, &path) {
            Some(bytes) => {
                String::from_utf8(bytes).map_err(|_| alloc::format!("модуль '{}' не UTF-8", name))
            }
            None => Err(alloc::format!("модуль '{}' не найден", name)),
        }
    }
}

// ── содержимое сеянного конфига (`init-config`) ──────────────────────────────
// Модули независимы (каждый вычисляется в своём окружении) и возвращают свой ВКЛАД; default.vv их
// сливает `append`. Тумблер сети вынесен в net.vv (правишь `#t`/`#f` — короткая правка).

// Шаблоны `/etc/system/*.vv` уехали в `vvsh_core::templates` (Веха 216): это ДАННЫЕ, и их надо
// проверять тестом, а тестов в `no_std`-бинаре не бывает. Разбор — в шапке того модуля.
use vvsh_core::templates::*;

