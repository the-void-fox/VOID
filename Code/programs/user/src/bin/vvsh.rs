//! vvsh — конфиг/язык VOID (ADR 0006, [[vvsh-config-layout]], [[vvsh-lang]]).
//!
//! Подкоманды (запуск через vsh: `run vvsh <под> …`; права наследуются от vsh, как install.rs —
//! start-cap 0 = posixfs-endpoint, 1 = store):
//!   `eval FILE`   — прочитать `.vv`, вычислить НА VOID, напечатать нормализованный конфиг (M1a/b).
//!   `init-config` — посеять модульный конфиг `/etc/system/*.vv` (правишь его → `rebuild`) (M1c).
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

/// Разобрать окружение и запомнить права. Зовётся первой строкой `_start`.
fn resolve_caps() {
    let by_name = |name: &str, fallback: usize| {
        sys::cap_named(name).unwrap_or_else(|| sys::start_cap(fallback))
    };
    FS_CAP.store(by_name("POSIXFS", 0), Ordering::Relaxed);
    STORE_CAP.store(by_name("STORE", 1), Ordering::Relaxed);
    NET_CAP.store(by_name("NET_SRV", 2), Ordering::Relaxed);
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

// ── программа ───────────────────────────────────────────────────────────────
#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    resolve_caps();
    // Каталог объявляем СРАЗУ, а не только при `cd`: программа, запущенная первой командой,
    // должна понимать относительный путь так же, как двадцатой.
    publish_cwd(b"/");
    let mut abuf = [0u8; 256];
    let n = sys::args(&mut abuf).min(abuf.len());
    let mut argv = abuf[..n].split(|&b| b == 0).filter(|s| !s.is_empty());
    let _name = argv.next();
    let sub = argv.next().unwrap_or(&[]);

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
        sys::write("vvsh: неизвестная подкоманда: ".as_bytes());
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
    run_init_config();
    sys::exit(0);
}

/// Посеять модульный конфиг в `/etc/system/` (posixfs, start-cap 0). Идемпотентно. Печатает итог.
fn run_init_config() {
    let ep = cap_fs();
    px::mkdir(ep, b"/etc"); // идемпотентно: если есть — MAX, игнорируем
    px::mkdir(ep, b"/etc/system");
    // Веха 101 — КАЖДАЯ запись проверяется. Сев `terminal.vv` (1.5 КиБ) однажды доехал
    // наполовину и оборвался посреди буквы, а сообщение об успехе печаталось как ни в чём не
    // бывало; виноватым тогда выглядел конфиг, а не запись.
    let files: [(&[u8], &str); 6] = [
        (b"/etc/system/net.vv", NET_VV),
        (b"/etc/system/services.vv", SERVICES_VV),
        (b"/etc/system/networking.vv", NETWORKING_VV),
        (b"/etc/system/terminal.vv", TERMINAL_VV),
        (b"/etc/system/packages.vv", PACKAGES_VV),
        (DEFAULT_PATH, DEFAULT_VV),
    ];
    let mut bad = false;
    for (path, text) in files {
        if !px::echo_to(ep, path, text.as_bytes()) {
            sys::write("vvsh: НЕ УДАЛОСЬ записать ".as_bytes());
            sys::write(path);
            sys::write(b"\n");
            bad = true;
        }
    }
    if bad {
        sys::write("vvsh: конфиг посеян НЕПОЛНО — чинить до `rebuild`\n".as_bytes());
        return;
    }
    sys::write(
        "vvsh: посеян модульный конфиг /etc/system/*.vv. Правь net.vv (true/false),\n\
         terminal.vv (режим экрана, клавиши), packages.vv (пакеты) → `rebuild`.\n\
         Править — редактором: `ved /etc/system/terminal.vv` (^S сохранить, ^Q выход).\n"
            .as_bytes(),
    );
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
            sys::write("vvsh: нет /etc/system/default.vv — сначала `init-config`\n".as_bytes());
            return;
        }
    };
    let loader = FsLoader { ep, base: dirname(DEFAULT_PATH) };
    let norm = match vvsh_core::build_config_with(&text, &loader) {
        Ok(out) => out,
        Err(e) => {
            sys::write("vvsh: ошибка: ".as_bytes());
            sys::write(e.as_bytes());
            sys::write(b"\n");
            return;
        }
    };
    // Содержимое поколения — нормализованный текст (контент-адресуемо).
    let mut new_id = [0u8; 32];
    sys::obj_put(scap, norm.as_bytes(), &mut new_id);

    // Dedup: если конфиг уже в текущем поколении — не плодить (требует READ; иначе пропускаем).
    let cur = read_current_name(scap);
    if let Some(cn) = &cur {
        if let Some(cid) = gen_content_id(scap, cn) {
            if cid == new_id {
                sys::write("vvsh: нет изменений — конфиг уже в поколении ".as_bytes());
                sys::write(cn);
                sys::write(b"\n");
                // Пакеты синхронизируются ВСЁ РАВНО: «конфиг тот же» не значит «обещанное
                // выполнено». Прошлый `rebuild` мог не достать пакет (не было сети или индекса),
                // и тогда повторный `rebuild` — ровно то, чем человек это чинит.
                sync_packages();
                return;
            }
        }
    }

    // Новое поколение gen<N> (N = max существующих + 1) + активировать (current).
    let Some(num) = next_gen_number(scap) else {
        sys::write("vvsh: список корней store не читается целиком — номер поколения не выдумываем\n".as_bytes());
        return;
    };
    let name = alloc::format!("gen{}", num);
    let root = alloc::format!("system/{}", name);
    sys::obj_set_root(scap, root.as_bytes(), &new_id);
    let mut nm_id = [0u8; 32];
    sys::obj_put(scap, name.as_bytes(), &mut nm_id);
    sys::obj_set_root(scap, CURRENT_ROOT, &nm_id);

    sys::write("vvsh: собрано поколение ".as_bytes());
    sys::write(name.as_bytes());
    sys::write(" (активно после ребута)".as_bytes());
    if let Some(cn) = &cur {
        sys::write("; было ".as_bytes());
        sys::write(cn);
    }
    sys::write(b"\n");
    sync_packages();
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
        sys::write("vvsh: pkg не запустился — пакеты конфига не собраны\n".as_bytes());
    } else if code != 0 {
        sys::write(alloc::format!("vvsh: pkg sync вернул [код {}] — пакеты не собраны\n", code).as_bytes());
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
        sys::write("vvsh: список корней store не прочитать (нужен store READ/WRITE)\n".as_bytes());
        return;
    };
    let nums = roots::gen_numbers(&text, b"system/gen");

    sys::write("поколения системы (активно — *):\n".as_bytes());
    if nums.is_empty() {
        sys::write("  (нет собранных поколений — `rebuild`)\n".as_bytes());
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
        sys::write("  (активное поколение не прочитать — нужен store READ)\n".as_bytes());
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
    sys::write(" — шелл VOID (ADR 0006/0013). `\\выражение` — вычислить, иначе команда. `help` — команды, `exit` — назад в vsh.\n".as_bytes());
    // Веха 99.3 — размер СВОЕГО окна, если мы живём в панели мультиплексора. Аналог TIOCSWINSZ,
    // только опрашиваемый: сигналов у нас нет, а сходить к хосту программа и так умеет.
    // Печатаем его в баннере не ради красоты — так сразу видно, что программа знает, куда рисует.
    if let Some((cols, rows)) = sys::stdio::win_size() {
        let mut line = alloc::string::String::new();
        use core::fmt::Write;
        let _ = write!(line, "окно: {cols}×{rows} знакомест\n");
        sys::write(line.as_bytes());
    }
    let loader = vvsh_core::NoLoader;
    let interp = vvsh_core::Interp::new(&loader);
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
    sys::write("vvsh: выход из REPL — vsh продолжает\n".as_bytes());
    sys::exit(0);
}

/// Строка-ВЫРАЖЕНИЕ (после ведущего `\`): распарсить, вычислить каждую форму, напечатать
/// непустой результат.
fn expr_line(interp: &vvsh_core::Interp, env: &Env, src: &[u8]) {
    let text = match core::str::from_utf8(src) {
        Ok(t) => t,
        Err(_) => return sys::write("ошибка: ввод не UTF-8\n".as_bytes()),
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
            sys::write("ошибка разбора: ".as_bytes());
            sys::write(e.0.as_bytes());
            sys::write(b"\n");
        }
    }
}

/// Строка-КОМАНДА (голые слова). Первое слово — имя, остальные — строковые аргументы. Разрешение:
/// связано с вызываемым (builtin/замыкание) → вызвать (это команда, вывод от неё); связано со
/// значением и без аргументов → показать (инспекция переменной); не связано → спавн программы (PATH).
fn command_line(interp: &vvsh_core::Interp, env: &Env, src: &[u8]) {
    let words: alloc::vec::Vec<&[u8]> = src
        .split(|&b| b == b' ' || b == b'\t')
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() {
        return;
    }
    let head = match core::str::from_utf8(words[0]) {
        Ok(s) => s,
        Err(_) => return sys::write("vvsh: имя команды не UTF-8\n".as_bytes()),
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
                sys::write("' — значение, а не команда (даны аргументы)\n".as_bytes());
            }
        }
        None => spawn_program(words[0], &words[1..]), // PATH: несвязанное имя → программа
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
        sys::write("vvsh: команда не найдена: ".as_bytes());
        sys::write(name);
        sys::write(b"\n");
    } else if code != 0 {
        sys::write(alloc::format!("[код {}]\n", code).as_bytes());
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
    sys::write("ошибка: ".as_bytes());
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
            sys::write(b"\n");
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
        ("date", sh_date), // Веха 86 — часы системы
        ("random", sh_random), // Веха 86 — случайные байты от ядра
        // Веха 84 — перенос команд vsh в vvsh: файлы/каталоги, store, сеть, поколения.
        ("roots", sh_roots),
        ("mkdir", sh_mkdir),
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
        ("poweroff", sh_poweroff),
        ("store-probe", sh_store_probe),
        ("nar-unpack", sh_nar_unpack),
        ("sysdef", sh_sysdef),
        ("rebuild", sh_rebuild),
        ("gens", sh_gens),
        ("init-config", sh_init_config),
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
    let lst = match args.get(1) {
        Some(Value::List(items)) => items,
        _ => return Err(EvalError::new("grep: второй аргумент — список")),
    };
    let mut out = alloc::vec::Vec::new();
    for e in lst.iter() {
        if let Value::Str(s) = e {
            if contains(s.as_bytes(), sub) {
                out.push(e.clone());
            }
        }
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
fn help_row(cmd: &[u8], desc: &str) {
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
    sys::write(" — шелл VOID. Строка с ведущим `\\` — выражение, иначе команда.\n".as_bytes());
    help_row(b"ls [DIR]", "список файлов (каталог или текущий)");
    help_row(b"cat FILE", "показать содержимое файла");
    help_row(b"tail FILE", "последние ~32 байта файла");
    help_row(b"cd [DIR]", "сменить каталог (.. вверх, без арг — в корень)");
    help_row(b"pwd", "текущий каталог");
    help_row(b"mkdir DIR", "создать каталог");
    help_row(b"rm PATH", "удалить файл (или пустой каталог)");
    help_row(b"mv OLD NEW", "переименовать файл");
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
    help_row(b"rebuild", "собрать поколение из /etc/system/*.vv");
    help_row(b"gens", "показать поколения системы (активно — *)");
    help_row(b"switch GEN", "выбрать поколение (после ребута)");
    help_row(b"sysdef GEN F", "задать поколение из файла-конфига");
    help_row(b"date", "текущее время системы (UTC)");
    help_row(b"random [N]", "N случайных байт от ядра (hex)");
    help_row(b"log on|off", "подробный трейс ядра ([ipc]/[obj]/…)");
    help_row(b"clear", "очистить экран");
    help_row(b"help", "эта справка");
    help_row(b"exit", "выйти в vsh (спасательный шелл)");
    help_row(b"poweroff", "выключить машину");
    help_row(b"store-probe", "замер: сколько store принимает за сессию (МиБ)");
    help_row(b"nar-unpack", "разложить NAR из корня store в файлы");
    sys::write("  Lisp: (define x 5) · (lambda (a) …) · (if c t e) · (map f L) · (filter p L)\n".as_bytes());
    sys::write("  Конвейер: (| (ls) (grep \"vv\") count)\n".as_bytes());
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
    match read_file(ep, &path) {
        Some(bytes) => {
            if !bytes.is_empty() {
                sys::write(&bytes);
                if *bytes.last().unwrap() != b'\n' {
                    sys::write(b"\n");
                }
            }
            Ok(Value::nil())
        }
        None => Err(EvalError::new("cat: файл не найден")),
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
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            sys::write(b" ");
        }
        match a {
            Value::Str(s) => sys::write(s.as_bytes()),
            other => sys::write(alloc::format!("{}", other).as_bytes()),
        }
    }
    sys::write(b"\n");
    Ok(Value::nil())
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
        None => sys::write("нет корней (или нет прав на store)\n".as_bytes()),
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
fn sh_mkdir(args: &[Value]) -> Result<Value, EvalError> {
    let path = arg_path(args, "mkdir: (mkdir \"путь\")")?;
    if px::mkdir(cap_fs(), &path) != 0 {
        return Err(EvalError::new("mkdir не удался (уже есть? нет родителя?)"));
    }
    Ok(Value::nil())
}

/// `(rm путь)` — удалить файл или пустой каталог (относительно cwd).
fn sh_rm(args: &[Value]) -> Result<Value, EvalError> {
    let path = arg_path(args, "rm: (rm \"путь\")")?;
    if px::unlink(cap_fs(), &path) != 0 {
        return Err(EvalError::new("rm не удался (нет файла? каталог не пуст?)"));
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

/// `(mv старый новый)` — переименовать файл (оба пути — относительно cwd).
fn sh_mv(args: &[Value]) -> Result<Value, EvalError> {
    match (args.first(), args.get(1)) {
        (Some(Value::Str(o)), Some(Value::Str(n))) => {
            let old = resolve(o.as_bytes());
            let new = resolve(n.as_bytes());
            if px::rename(cap_fs(), &old, &new) != 0 {
                return Err(EvalError::new("mv не удался (нет файла?)"));
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
    let ip = match parse_ipv4(ipstr.as_bytes()) {
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
        sys::write(alloc::format!("ответ от {}: {} мкс\n", ipstr, rtt).as_bytes());
        Ok(Value::nil())
    } else {
        Err(EvalError::new("ping: нет ответа"))
    }
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
        match st {
            p::ST_ERR => "не удалось (адрес отверг соединение?)",
            p::ST_TIMEOUT => "не дождались ответа",
            p::ST_EOF => "соединение закрыто другой стороной",
            _ => "негодный запрос (хэндл?)",
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
    let ip = match parse_ipv4(host.as_bytes()) {
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
    if sys::net_cli::tcp_close(net_ep("tcp-close")?, h as u8) {
        Ok(Value::nil())
    } else {
        Err(EvalError::new("tcp-close: негодный хэндл"))
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

/// `(poweroff)` — выключить машину (Веха 101). Нужно право `power` из конфига: выключение —
/// одностороннее действие над всей системой, и оно названо правом, а не считается общедоступным.
fn sh_poweroff(_args: &[Value]) -> Result<Value, EvalError> {
    sys::write("выключаю машину…\n".as_bytes());
    if let Some(pc) = sys::cap_named("POWER") {
        sys::power_off(pc);
    }
    Err(EvalError::new("poweroff: нет права `power` в конфиге поколения"))
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
        sys::write(alloc::format!("поколение выбрано, перезагрузи QEMU: {}\n", name).as_bytes());
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

/// `(gens)` — показать поколения системы.
fn sh_gens(_args: &[Value]) -> Result<Value, EvalError> {
    run_gens();
    Ok(Value::nil())
}

/// `(init-config)` — посеять `/etc/system/*.vv` (модульный конфиг для правки → `rebuild`).
fn sh_init_config(_args: &[Value]) -> Result<Value, EvalError> {
    run_init_config();
    Ok(Value::nil())
}

/// Разобрать IPv4 «A.B.C.D» в 4 байта (для `ping`). `None` — не разобрать.
fn parse_ipv4(s: &[u8]) -> Option<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut idx = 0usize;
    let mut val: u32 = 0;
    let mut digits = 0;
    for &b in s {
        if b == b'.' {
            if digits == 0 || idx >= 3 {
                return None;
            }
            octets[idx] = val as u8;
            idx += 1;
            val = 0;
            digits = 0;
        } else if b.is_ascii_digit() {
            val = val * 10 + (b - b'0') as u32;
            if val > 255 {
                return None;
            }
            digits += 1;
        } else {
            return None;
        }
    }
    if idx != 3 || digits == 0 {
        return None;
    }
    octets[3] = val as u8;
    Some(octets)
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
                                pos += 1;
                                sys::write(b"\x1b[C");
                            }
                        }
                        b'D' => {
                            if pos > 0 {
                                pos -= 1;
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
                                line.copy_within(pos + 1..llen, pos);
                                llen -= 1;
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
                            line.copy_within(pos..llen, pos - 1);
                            pos -= 1;
                            llen -= 1;
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
                                sys::write(&[c]); // добавление в конец — просто эхо
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

/// Перерисовать строку ввода целиком: в начало, приглашение, содержимое, стереть хвост, вернуть курсор.
fn redraw(prompt: &[u8], line: &[u8], llen: usize, pos: usize) {
    sys::write(b"\r");
    sys::write(prompt);
    sys::write(&line[..llen]);
    sys::write(b"\x1b[K"); // стереть до конца строки
    if pos < llen {
        csi_num(llen - pos, b'D'); // курсор влево на (llen-pos) колонок
    }
}

/// Записать управляющую последовательность `ESC[<n><fin>` (например, сдвиг курсора).
fn csi_num(n: usize, fin: u8) {
    if n == 0 {
        return;
    }
    let mut buf = [0u8; 16];
    let mut i = 0;
    buf[i] = 0x1b;
    i += 1;
    buf[i] = b'[';
    i += 1;
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
        buf[i] = tmp[t];
        i += 1;
    }
    buf[i] = fin;
    i += 1;
    sys::write(&buf[..i]);
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
            sys::write("vvsh: не удалось прочитать файл: ".as_bytes());
            sys::write(path);
            sys::write(b"\n");
            return Err(1);
        }
    };
    match String::from_utf8(src) {
        Ok(t) => Ok(t),
        Err(_) => {
            sys::write("vvsh: файл не UTF-8\n".as_bytes());
            Err(1)
        }
    }
}

fn fail(msg: &str) -> ! {
    sys::write("vvsh: ошибка: ".as_bytes());
    sys::write(msg.as_bytes());
    sys::write(b"\n");
    sys::exit(1);
}

/// Прочитать файл posixfs целиком в `Vec<u8>`. `None` — файла нет или это каталог (`stat` до
/// `open`, чтобы опечатка в пути не плодила пустышку — у posixfs `open` создаёт файл).
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

const NET_VV: &str = "# net.vv — сеть: true (вкл) или false (выкл)\ntrue\n";

const SERVICES_VV: &str = "# services.vv — базовые сервисы (файлы)\n\
[service(\"posixfs\", \"store:rw\")]\n";

const NETWORKING_VV: &str = "# networking.vv — сетевой сервис\n\
[service(\"net-srv\", \"dev:net:rw\")]\n";

/// Веха 100 — терминал настраивается ТУТ ЖЕ, обычным модулем конфигурации. Модуль решает и
/// «кто шелл» (пиксельный `term` или текстовый `vsh`), и схему управления: одна вещь — одно
/// место. Записи `terminal`/`bind` ядру не адресованы, их читает сам `term`.
const TERMINAL_VV: &str = "# terminal.vv — чем встречает система: экран, шелл, клавиши.\n\
#\n\
# mode = \"vsh\"  — текстовый шелл в консоли ядра (работает всегда, в том числе на riscv);\n\
# mode = \"term\" — терминал на настоящих глифах во весь экран (нужен пиксельный экран: x86+GRUB);\n\
# mode = \"wm\"   — ОКОННЫЙ РЕЖИМ: композитор, окна, мышь (Вехи 117-119).\n\
#\n\
# Если выбранный режим не поднимется, init через пять секунд запустит спасательный vsh —\n\
# система не превращается в кирпич из-за одной строки конфига.\n\
mode = \"term\"\n\
net = import(\"net.vv\")\n\
\n\
# Программы, которые оконный режим открывает на старте (для mode = \"wm\").\n\
apps = [\"term\"]\n\
\n\
# Схема управления ТЕРМИНАЛОМ: bind(РЕЖИМ, КЛАВИША, ДЕЙСТВИЕ). Пустой список = схема по\n\
# умолчанию, зашитая в term; хоть один bind — схема задаётся ЦЕЛИКОМ отсюда.\n\
#   режимы:   normal · pane\n\
#   клавиши:  C-a (Ctrl+A) · буква · | · - · Left Right Up Down Enter Tab Esc Space PageUp PageDown\n\
#   действия: mode-pane mode-normal literal-prefix split-v split-h next-pane close quit\n\
#             reload · scroll-up scroll-down scroll-top scroll-bottom · go-left go-right go-up go-down\n\
keys = [\n\
\x20 bind(\"normal\", \"C-a\", \"mode-pane\"),\n\
\x20 bind(\"normal\", \"S-PageUp\", \"scroll-up\"),\n\
\x20 bind(\"normal\", \"S-PageDown\", \"scroll-down\"),\n\
\x20 bind(\"pane\", \"C-a\", \"literal-prefix\"),\n\
\x20 bind(\"pane\", \"|\", \"split-v\"),\n\
\x20 bind(\"pane\", \"-\", \"split-h\"),\n\
\x20 bind(\"pane\", \"o\", \"next-pane\"),\n\
\x20 bind(\"pane\", \"x\", \"close\"),\n\
\x20 bind(\"pane\", \"q\", \"quit\"),\n\
\x20 bind(\"pane\", \"r\", \"reload\"),\n\
\x20 bind(\"pane\", \"h\", \"go-left\"),\n\
\x20 bind(\"pane\", \"j\", \"go-down\"),\n\
\x20 bind(\"pane\", \"k\", \"go-up\"),\n\
\x20 bind(\"pane\", \"l\", \"go-right\"),\n\
\x20 bind(\"pane\", \"PageUp\", \"scroll-up\"),\n\
\x20 bind(\"pane\", \"PageDown\", \"scroll-down\"),\n\
]\n\
\n\
# Схема управления ОКНАМИ (mode = \"wm\"): умолчания как в niri.\n\
# Модель — скроллируемый тайлинг: окна живут в КОЛОНКАХ на ленте, экран по ней ездит.\n\
wm_keys = [\n\
  bind(\"wm\", \"Super+Return\", \"spawn-term\"),\n\
  bind(\"wm\", \"Super+Q\", \"close-window\"),\n\
  bind(\"wm\", \"Super+H\", \"focus-column-left\"),\n\
  bind(\"wm\", \"Super+L\", \"focus-column-right\"),\n\
  bind(\"wm\", \"Super+Up\", \"focus-window-up\"),\n\
  bind(\"wm\", \"Super+Down\", \"focus-window-down\"),\n\
  bind(\"wm\", \"Super+Shift+H\", \"move-column-left\"),\n\
  bind(\"wm\", \"Super+Shift+L\", \"move-column-right\"),\n\
  bind(\"wm\", \"Super+Shift+Up\", \"move-window-up\"),\n\
  bind(\"wm\", \"Super+Shift+Down\", \"move-window-down\"),\n\
  bind(\"wm\", \"Super+BracketLeft\", \"move-to-column-left\"),\n\
  bind(\"wm\", \"Super+BracketRight\", \"move-to-column-right\"),\n\
  bind(\"wm\", \"Super+R\", \"width-next\"),\n\
  bind(\"wm\", \"Super+Equal\", \"width-plus\"),\n\
  bind(\"wm\", \"Super+Minus\", \"width-minus\"),\n\
  bind(\"wm\", \"Super+F\", \"maximize-column\"),\n\
  bind(\"wm\", \"Super+Tab\", \"focus-next\"),\n\
  bind(\"wm\", \"Super+Shift+Q\", \"quit\"),\n\
]\n\
\n\
netcap = if net { \"endpoint:net-srv\" } else { [] }\n\
\n\
if mode == \"wm\" {\n\
\x20 append(\n\
\x20   [shell(\"wm\", \"endpoint:posixfs\", \"store:rwx\", netcap, \"mmio:fb\", \"power\", \"env\",\n\
\x20          map(|a| \"arg:\" + a, apps))],\n\
\x20   wm_keys,\n\
\x20 )\n\
} else {\n\
\x20 if mode == \"term\" {\n\
\x20   append(\n\
\x20     [shell(\"term\", \"endpoint:posixfs\", \"store:rwx\", netcap, \"mmio:fb\", \"power\", \"env\")],\n\
\x20     [terminal(\"font-size\", 18),\n\
\x20      terminal(\"shell\", \"bin/vvsh\"),\n\
\x20      terminal(\"shell-args\", \"repl\")],\n\
\x20     keys,\n\
\x20   )\n\
\x20 } else {\n\
\x20   [shell(\"vsh\", \"endpoint:posixfs\", \"store:rwx\", netcap, \"power\", \"env\")]\n\
\x20 }\n\
}\n";

/// Веха 112 — пакеты объявляются здесь же, обычным модулем. Имя пакета в списке значит «система
/// обязана его иметь»: `rebuild` соберёт его в поколение профиля, а откат системы уберёт вместе
/// с поколением. `pkg install` при этом никуда не девается — это по-прежнему способ поставить
/// что-то разово, не объявляя.
const PACKAGES_VV: &str = "# packages.vv — пакеты, которые система обязана иметь, и канал, откуда\n\
# они берутся.\n\
#\n\
# Имена — как в nixpkgs; что есть в канале, покажет `pkg search <строка>`. Резолв имён требует\n\
# индекса канала: один раз сделай `pkg update`. Пустой список — ни одного пакета.\n\
#\n\
# Правка → `rebuild`: пакет скачается и появится в PATH. Откат системы уберёт его обратно.\n\
want = []\n\
\n\
# Канал — место, откуда берутся имена и версии. Сменил канал → `pkg update` (иначе имена будут\n\
# резолвиться по старому индексу, и `pkg` об этом скажет).\n\
source = \"https://channels.nixos.org/nixos-unstable\"\n\
\n\
append(\n\
\x20 [channel(source)],\n\
\x20 if null?(want) { [] } else { [packages(want)] },\n\
)\n";

const DEFAULT_VV: &str = "# default.vv — верхний модуль конфигурации VOID (vvsh, ADR 0006).\n\
# Собери систему из модулей: сеть — net.vv (true/false), терминал и его клавиши — terminal.vv,\n\
# пакеты — packages.vv.\n\
# Затем: run vvsh rebuild\n\
net = import(\"net.vv\")\n\
system(\n\
\x20 import(\"services.vv\"),\n\
\x20 if net { import(\"networking.vv\") } else { [] },\n\
\x20 import(\"terminal.vv\"),\n\
\x20 import(\"packages.vv\"),\n\
)\n";
