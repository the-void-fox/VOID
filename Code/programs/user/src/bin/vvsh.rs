//! vvsh — конфиг/язык VOID (ADR 0006, [[vvsh-config-layout]], [[vvsh-lang]]).
//!
//! Подкоманды (запуск через vsh: `run vvsh <под> …`; права наследуются от vsh, как install.rs —
//! start-cap 0 = posixfs-endpoint, 1 = store):
//!   `eval FILE`   — прочитать `.vv`, вычислить НА VOID, напечатать нормализованный конфиг (M1a/b).
//!   `init-config` — посеять модульный конфиг `/etc/system/*.vv` (правишь его → `rebuild`) (M1c).
//!   `rebuild`     — вычислить `/etc/system/default.vv` → КОММИТ нового поколения `system/gen<N>`,
//!                   двинуть `system/current` (активно после ребута) (M1c).
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
use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

use void_user as sys;
use void_user::posix as px;
use vvsh_core::{Env, EvalError, Value};

// ── глобальный аллокатор: bump + СВОБОДНЫЙ СПИСОК поверх ленивой кучи процесса ──
//
// Веха 89. Раньше здесь был чистый bump с пустым `dealloc`: 4-МиБ арена на весь сеанс, память
// не возвращалась НИКОГДА. Для основного шелла системы, который обязан жить долго, это значит
// «через N команд аллокация вернёт null» — Lisp-REPL порождает временные значения на каждое
// выражение. Теперь освобождённые блоки идут в адресно-упорядоченный список со СЛИЯНИЕМ
// соседей (та же схема, что у кучи ядра): память переиспользуется, фрагментация ограничена.
struct Heap;
const ARENA: usize = 4 * 1024 * 1024;
/// Узел свободного списка живёт ВНУТРИ свободного блока: [size][next]. Отсюда минимальный
/// размер блока и минимальное выравнивание.
const NODE: usize = 2 * core::mem::size_of::<usize>();
static BASE: AtomicUsize = AtomicUsize::new(0);
static NEXT: AtomicUsize = AtomicUsize::new(0);
static END: AtomicUsize = AtomicUsize::new(0);
/// Голова списка свободных блоков (адрес; 0 — список пуст). Список отсортирован по адресу —
/// это и даёт дешёвое слияние соседей.
static FREE: AtomicUsize = AtomicUsize::new(0);

#[inline]
unsafe fn nsize(p: usize) -> usize {
    *(p as *const usize)
}
#[inline]
unsafe fn nnext(p: usize) -> usize {
    *((p + core::mem::size_of::<usize>()) as *const usize)
}
#[inline]
unsafe fn nset(p: usize, size: usize, next: usize) {
    *(p as *mut usize) = size;
    *((p + core::mem::size_of::<usize>()) as *mut usize) = next;
}

/// Запрос → (размер, выравнивание), нормализованные под узел списка.
fn norm(layout: Layout) -> (usize, usize) {
    let align = layout.align().max(core::mem::align_of::<usize>());
    let size = ((layout.size() + align - 1) & !(align - 1)).max(NODE);
    (size, align)
}

unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if BASE.load(Ordering::Relaxed) == 0 {
            let base = sys::heap_map(ARENA);
            if base == 0 || base == usize::MAX {
                return core::ptr::null_mut();
            }
            BASE.store(base, Ordering::Relaxed);
            NEXT.store(base, Ordering::Relaxed);
            END.store(base + ARENA, Ordering::Relaxed);
        }
        let (size, align) = norm(layout);

        // 1) First-fit по свободному списку. Хвост блока возвращаем в список, если в нём
        //    помещается узел; иначе отдаём блок целиком (остаток был бы потерян навсегда).
        let (mut prev, mut cur) = (0usize, FREE.load(Ordering::Relaxed));
        while cur != 0 {
            let bsize = nsize(cur);
            let start = (cur + align - 1) & !(align - 1);
            let head_gap = start - cur; // «хвостик» перед выравненным началом
            if (head_gap == 0 || head_gap >= NODE) && start + size <= cur + bsize {
                let next = nnext(cur);
                let rest = (cur + bsize) - (start + size);
                // вынуть блок из списка
                if prev == 0 {
                    FREE.store(next, Ordering::Relaxed);
                } else {
                    nset(prev, nsize(prev), next);
                }
                if head_gap >= NODE {
                    dealloc_raw(cur, head_gap); // «хвостик» слева — обратно в список
                }
                if rest >= NODE {
                    dealloc_raw(start + size, rest); // остаток справа — тоже
                }
                return start as *mut u8;
            }
            prev = cur;
            cur = nnext(cur);
        }

        // 2) Свободного блока нет — отрезать от нетронутой части арены.
        let aligned = (NEXT.load(Ordering::Relaxed) + align - 1) & !(align - 1);
        let new_next = aligned + size;
        if new_next > END.load(Ordering::Relaxed) {
            return core::ptr::null_mut();
        }
        NEXT.store(new_next, Ordering::Relaxed);
        aligned as *mut u8
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let (size, _) = norm(layout);
        dealloc_raw(ptr as usize, size);
    }
}

/// Вернуть блок `[addr, addr+size)` в список: вставка по адресу + слияние с соседями слева и
/// справа. Без слияния длинный сеанс шелла раскрошил бы арену в пыль.
unsafe fn dealloc_raw(addr: usize, size: usize) {
    let (mut prev, mut cur) = (0usize, FREE.load(Ordering::Relaxed));
    while cur != 0 && cur < addr {
        prev = cur;
        cur = nnext(cur);
    }
    let mut size = size;
    let mut next = cur;
    // слияние с правым соседом
    if cur != 0 && addr + size == cur {
        size += nsize(cur);
        next = nnext(cur);
    }
    // слияние с левым соседом
    if prev != 0 && prev + nsize(prev) == addr {
        nset(prev, nsize(prev) + size, next);
        return;
    }
    nset(addr, size, next);
    if prev == 0 {
        FREE.store(addr, Ordering::Relaxed);
    } else {
        nset(prev, nsize(prev), addr);
    }
}

#[global_allocator]
static ALLOC: Heap = Heap;

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

// ── программа ───────────────────────────────────────────────────────────────
#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
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
    let ep = sys::start_cap(0);
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
    let ep = sys::start_cap(0);
    px::mkdir(ep, b"/etc"); // идемпотентно: если есть — MAX, игнорируем
    px::mkdir(ep, b"/etc/system");
    px::echo_to(ep, b"/etc/system/net.vv", NET_VV.as_bytes());
    px::echo_to(ep, b"/etc/system/services.vv", SERVICES_VV.as_bytes());
    px::echo_to(ep, b"/etc/system/networking.vv", NETWORKING_VV.as_bytes());
    px::echo_to(ep, DEFAULT_PATH, DEFAULT_VV.as_bytes());
    sys::write(
        "vvsh: посеян модульный конфиг /etc/system/*.vv. Правь net.vv (#t/#f) и `rebuild`.\n"
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
    let ep = sys::start_cap(0);
    let scap = sys::start_cap(1);
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
                return;
            }
        }
    }

    // Новое поколение gen<N> (N = max существующих + 1) + активировать (current).
    let name = alloc::format!("gen{}", next_gen_number(scap));
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
}

/// `gens` (подкоманда) — перечислить поколения и выйти. Логика — в [`run_gens`].
fn cmd_gens() -> ! {
    run_gens();
    sys::exit(0);
}

/// Перечислить поколения `system/gen*` и пометить активное (`*`). Печатает; ВОЗВРАЩАЕТСЯ.
fn run_gens() {
    let scap = sys::start_cap(1);
    let cur = read_current_name(scap);

    let mut buf = [0u8; 16384];
    let n = sys::obj_list_roots(scap, &mut buf);
    let mut nums: Vec<u32> = Vec::new();
    for line in buf[..n].split(|&b| b == b'\n') {
        if line.len() <= 14 {
            continue; // "hex(12)  имя": имя с 14-го байта
        }
        if let Some(rest) = line[14..].strip_prefix(b"system/gen") {
            if let Some(k) = parse_u32(rest) {
                nums.push(k);
            }
        }
    }
    nums.sort_unstable();
    nums.dedup();

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
    sys::write(" — Lisp-шелл VOID (ADR 0006). `(...)` — выражение, иначе команда. `help` — команды, `(exit)` — назад в vsh.\n".as_bytes());
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
        if src == b"exit" || src == b"(exit)" || src == b"quit" || src == b"(quit)" {
            break;
        }
        if src[0] == b'(' || src[0] == b'\'' {
            expr_line(&interp, &env, src); // Lisp-выражение
        } else {
            command_line(&interp, &env, src); // команда (голые слова)
        }
    }
    sys::write("vvsh: выход из REPL — vsh продолжает\n".as_bytes());
    sys::exit(0);
}

/// Строка-ВЫРАЖЕНИЕ (`(...)`): распарсить, вычислить каждую форму, напечатать непустой результат.
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
fn spawn_program(name: &[u8], arg_words: &[&[u8]]) {
    let mut blob = alloc::vec::Vec::new();
    for w in arg_words {
        blob.extend_from_slice(w);
        blob.push(0);
    }
    let code = px::spawn_args(sys::start_cap(1), name, &blob);
    if code == usize::MAX {
        sys::write("vvsh: команда не найдена: ".as_bytes());
        sys::write(name);
        sys::write(b"\n");
    } else if code != 0 {
        sys::write(alloc::format!("[код {}]\n", code).as_bytes());
    }
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
        ("rm", sh_rm),
        ("tail", sh_tail),
        ("mv", sh_mv),
        ("ping", sh_ping),
        ("resolve", sh_resolve), // Веха 92 — DNS
        ("thaw", sh_thaw),
        ("switch", sh_switch),
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
    let ep = sys::start_cap(0);
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
    let mut buf = [0u8; 4096];
    let n = px::readdir(ep, &path, &mut buf);
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
    let ep = sys::start_cap(0);
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
    sys::write(" — Lisp-шелл. Строка с `(` — выражение, иначе команда.\n".as_bytes());
    help_row(b"ls [DIR]", "список файлов (каталог или текущий)");
    help_row(b"cat FILE", "показать содержимое файла");
    help_row(b"tail FILE", "последние ~32 байта файла");
    help_row(b"cd [DIR]", "сменить каталог (.. вверх, без арг — в корень)");
    help_row(b"pwd", "текущий каталог");
    help_row(b"mkdir DIR", "создать каталог");
    help_row(b"rm PATH", "удалить файл (или пустой каталог)");
    help_row(b"mv OLD NEW", "переименовать файл");
    help_row(b"echo TEXT", "напечатать ($x — переменная; TEXT > FILE — запись)");
    help_row(b"grep SUB L", "фильтр строк списка (для конвейеров)");
    help_row(b"run NAME", "запустить программу из store (или просто NAME)");
    help_row(b"thaw NAME", "разморозить процесс из образа");
    help_row(b"ping IP", "ICMP-пинг адреса A.B.C.D");
    help_row(b"resolve NAME", "DNS: имя → адрес (возвращает строку)");
    help_row(b"roots", "сырые корни store (bin/*, system/*, …)");
    help_row(b"init-config", "посеять /etc/system/*.vv");
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
    let ep = sys::start_cap(0);
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
        px::echo_to(sys::start_cap(0), &path, text.as_bytes());
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
    let code = px::spawn_args(sys::start_cap(1), name.as_bytes(), &blob);
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
    let mut buf = [0u8; 16384];
    let n = sys::obj_list_roots(sys::start_cap(1), &mut buf);
    if n == 0 {
        sys::write("нет корней (или нет прав на store)\n".as_bytes());
    } else {
        sys::write(&buf[..n]);
    }
    Ok(Value::nil())
}

/// `(mkdir путь)` — создать каталог (относительно cwd).
fn sh_mkdir(args: &[Value]) -> Result<Value, EvalError> {
    let path = arg_path(args, "mkdir: (mkdir \"путь\")")?;
    if px::mkdir(sys::start_cap(0), &path) != 0 {
        return Err(EvalError::new("mkdir не удался (уже есть? нет родителя?)"));
    }
    Ok(Value::nil())
}

/// `(rm путь)` — удалить файл или пустой каталог (относительно cwd).
fn sh_rm(args: &[Value]) -> Result<Value, EvalError> {
    let path = arg_path(args, "rm: (rm \"путь\")")?;
    if px::unlink(sys::start_cap(0), &path) != 0 {
        return Err(EvalError::new("rm не удался (нет файла? каталог не пуст?)"));
    }
    Ok(Value::nil())
}

/// `(tail путь)` — последние ~32 байта файла (витрина lseek SEEK_END).
fn sh_tail(args: &[Value]) -> Result<Value, EvalError> {
    let ep = sys::start_cap(0);
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
            if px::rename(sys::start_cap(0), &old, &new) != 0 {
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
    let netep = sys::start_cap(2);
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
    let netep = sys::start_cap(2);
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

/// `(thaw "имя")` — разморозить процесс из образа `proc/<arch>/имя` (start-cap 1 несёт EXEC).
fn sh_thaw(args: &[Value]) -> Result<Value, EvalError> {
    let name = match args.first() {
        Some(Value::Str(s)) => s.clone(),
        _ => return Err(EvalError::new("thaw: (thaw \"имя\")")),
    };
    let code = sys::restore(sys::start_cap(1), name.as_bytes());
    if code == usize::MAX {
        return Err(EvalError::new("thaw не удался (нет образа?)"));
    }
    Ok(Value::Int(code as i64))
}

/// `(switch "gen")` — выбрать поколение системы (запись в корень `system/current`; после ребута).
fn sh_switch(args: &[Value]) -> Result<Value, EvalError> {
    let name = match args.first() {
        Some(Value::Str(s)) => s.clone(),
        _ => return Err(EvalError::new("switch: (switch \"gen\")")),
    };
    let scap = sys::start_cap(1);
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
    let ep = sys::start_cap(0);
    let scap = sys::start_cap(1);
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
const LINE_CAP: usize = 256;

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
fn next_gen_number(scap: usize) -> u32 {
    let mut buf = [0u8; 16384];
    let n = sys::obj_list_roots(scap, &mut buf);
    let mut max = 0u32;
    for line in buf[..n].split(|&b| b == b'\n') {
        if line.len() <= 14 {
            continue;
        }
        if let Some(rest) = line[14..].strip_prefix(b"system/gen") {
            if let Some(k) = parse_u32(rest) {
                if k > max {
                    max = k;
                }
            }
        }
    }
    max + 1
}

fn parse_u32(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() {
        return None;
    }
    let mut n: u32 = 0;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add((b - b'0') as u32)?;
    }
    Some(n)
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

const NET_VV: &str = ";; net.vv — сеть: #t (вкл) или #f (выкл)\n#t\n";

const SERVICES_VV: &str = ";; services.vv — базовые сервисы (файлы)\n\
(list (service \"posixfs\" \"store:rw\"))\n";

const NETWORKING_VV: &str = ";; networking.vv — сетевой сервис\n\
(list (service \"net-srv\" \"dev:net:rw\"))\n";

const DEFAULT_VV: &str = ";; default.vv — верхний модуль конфигурации VOID (vvsh, ADR 0006).\n\
;; Собери систему из модулей. Тумблер сети — в net.vv (#t/#f). Затем: run vvsh rebuild\n\
(define net (import \"net.vv\"))\n\
(system\n\
\x20 (append\n\
\x20   (import \"services.vv\")\n\
\x20   (if net (import \"networking.vv\") (list))\n\
\x20   (list (shell \"vsh\"\n\
\x20                \"endpoint:posixfs\" \"store:rwx\"\n\
\x20                (if net \"endpoint:net-srv\" (list))\n\
\x20                \"env\"))))\n";
