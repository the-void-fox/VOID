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
use core::sync::atomic::{AtomicUsize, Ordering};

use void_user as sys;
use void_user::posix as px;
use vvsh_core::{Env, EvalError, Value};

// ── глобальный аллокатор: bump поверх ленивой кучи процесса (heap_map) ──────────
struct Bump;
const ARENA: usize = 4 * 1024 * 1024;
static BASE: AtomicUsize = AtomicUsize::new(0);
static NEXT: AtomicUsize = AtomicUsize::new(0);
static END: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Bump {
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
        let align = layout.align();
        let aligned = (NEXT.load(Ordering::Relaxed) + align - 1) & !(align - 1);
        let new_next = aligned + layout.size();
        if new_next > END.load(Ordering::Relaxed) {
            return core::ptr::null_mut();
        }
        NEXT.store(new_next, Ordering::Relaxed);
        aligned as *mut u8
    }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[global_allocator]
static ALLOC: Bump = Bump;

const DEFAULT_PATH: &[u8] = b"/etc/system/default.vv";
const CURRENT_ROOT: &[u8] = b"system/current";

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

/// `init-config` — посеять модульный конфиг в `/etc/system/` (posixfs, start-cap 0).
fn cmd_init_config() -> ! {
    let ep = sys::start_cap(0);
    px::mkdir(ep, b"/etc"); // идемпотентно: если есть — MAX, игнорируем
    px::mkdir(ep, b"/etc/system");
    px::echo_to(ep, b"/etc/system/net.vv", NET_VV.as_bytes());
    px::echo_to(ep, b"/etc/system/services.vv", SERVICES_VV.as_bytes());
    px::echo_to(ep, b"/etc/system/networking.vv", NETWORKING_VV.as_bytes());
    px::echo_to(ep, DEFAULT_PATH, DEFAULT_VV.as_bytes());
    sys::write(
        "vvsh: посеян модульный конфиг /etc/system/*.vv. Правь net.vv (#t/#f) и `run vvsh rebuild`.\n"
            .as_bytes(),
    );
    sys::exit(0);
}

/// `rebuild` — вычислить `/etc/system/default.vv` → коммит нового поколения → двинуть `current`.
fn cmd_rebuild() -> ! {
    let ep = sys::start_cap(0);
    let scap = sys::start_cap(1);
    let text = match read_config_text(ep, DEFAULT_PATH) {
        Ok(t) => t,
        Err(_) => {
            sys::write(
                "vvsh: нет /etc/system/default.vv — сначала `run vvsh init-config`\n".as_bytes(),
            );
            sys::exit(1);
        }
    };
    let loader = FsLoader { ep, base: dirname(DEFAULT_PATH) };
    let norm = match vvsh_core::build_config_with(&text, &loader) {
        Ok(out) => out,
        Err(e) => fail(&e),
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
                sys::exit(0);
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
    sys::exit(0);
}

/// `gens` — перечислить поколения `system/gen*` и пометить активное (`*`).
fn cmd_gens() -> ! {
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
        sys::write("  (нет собранных поколений — `run vvsh rebuild`)\n".as_bytes());
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
    sys::exit(0);
}

/// `repl` (S2a/S2b) — интерактивный шелл-REPL. Окружение ЖИВЁТ между строками (`(define x 5)` →
/// потом `(* x x)` → 25). Гибридный синтаксис: строка с `(`/`'` — Lisp-выражение (eval + печать
/// результата); иначе — КОМАНДА (голые слова, `ls /etc` ≡ `(ls "/etc")`; несвязанное имя → спавн
/// программы, как PATH). Запуск из vsh: `run vvsh repl`; выход — `(exit)`/`exit`/Ctrl-D → назад в
/// vsh (внешний спасательный шелл — если vvsh упадёт, он ловит обратно).
fn cmd_repl() -> ! {
    sys::write(
        "vvsh REPL — Lisp VOID (ADR 0006). `(...)` — Lisp; иначе команда. (exit) — назад в vsh.\n"
            .as_bytes(),
    );
    let loader = vvsh_core::NoLoader;
    let interp = vvsh_core::Interp::new(&loader);
    let env = shell_env(); // ПЕРСИСТЕНТНОЕ окружение сессии (чистые builtins + команды-эффекты)
    let mut line = [0u8; 512];
    loop {
        sys::write("vvsh> ".as_bytes());
        let len = match read_line(&mut line) {
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
                    Ok(v) => {
                        if !is_nil(&v) {
                            print_value(&v); // () (обычно результат команд) не печатаем
                        }
                    }
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
        Some(v) if is_callable(&v) => match build_command_form(&words) {
            Ok(form) => {
                if let Err(e) = interp.eval(&form, env) {
                    print_err(&e);
                }
            }
            Err(m) => {
                sys::write("vvsh: ".as_bytes());
                sys::write(m.as_bytes());
                sys::write(b"\n");
            }
        },
        Some(v) => {
            if words.len() == 1 {
                print_value(&v); // инспекция переменной
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
fn build_command_form(words: &[&[u8]]) -> Result<Value, alloc::string::String> {
    let head = core::str::from_utf8(words[0]).map_err(|_| str_owned("имя команды не UTF-8"))?;
    let mut items = alloc::vec::Vec::with_capacity(words.len());
    items.push(Value::sym(head));
    for w in &words[1..] {
        let s = core::str::from_utf8(w).map_err(|_| str_owned("аргумент не UTF-8"))?;
        items.push(Value::str(s));
    }
    Ok(Value::list(items))
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

fn is_nil(v: &Value) -> bool {
    matches!(v, Value::List(items) if items.is_empty())
}

fn str_owned(s: &str) -> alloc::string::String {
    alloc::string::String::from(s)
}

fn print_err(e: &EvalError) {
    sys::write("ошибка: ".as_bytes());
    sys::write(e.0.as_bytes());
    sys::write(b"\n");
}

/// Печать значения-результата (каноничная форма Value).
fn print_value(v: &Value) {
    sys::write(alloc::format!("{}\n", v).as_bytes());
}

// ── команды-эффекты шелла (S2b): builtins в бинаре, дёргают синкаллы напрямую ──
// vvsh-core остаётся ЧИСТЫМ (конфиг использует `root_env`, эти команды — только в REPL). Caps
// приходят из `start_cap` (ambient процессу), вывод — `sys::write`; Host-trait не нужен.

/// Окружение шелл-сессии: чистые builtins vvsh-core + команды-эффекты (`ls`/`cat`/`echo`/`run`).
fn shell_env() -> Env {
    let env = vvsh_core::root_env();
    let cmds: &[(&'static str, fn(&[Value]) -> Result<Value, EvalError>)] =
        &[("ls", sh_ls), ("cat", sh_cat), ("echo", sh_echo), ("run", sh_run)];
    for (name, f) in cmds {
        env.define(alloc::rc::Rc::from(*name), Value::Builtin(name, *f));
    }
    env
}

/// `(ls [путь])` — список файлов каталога (по умолчанию `/`).
fn sh_ls(args: &[Value]) -> Result<Value, EvalError> {
    let ep = sys::start_cap(0);
    let path: &[u8] = match args.first() {
        None => b"/",
        Some(Value::Str(s)) => s.as_bytes(),
        Some(other) => {
            return Err(EvalError::new(alloc::format!(
                "ls: путь — строка, дано {}",
                other.type_name()
            )))
        }
    };
    let mut buf = [0u8; 4096];
    let n = px::readdir(ep, path, &mut buf);
    if n > 0 {
        sys::write(&buf[..n]);
        if buf[n - 1] != b'\n' {
            sys::write(b"\n");
        }
    }
    Ok(Value::nil())
}

/// `(cat путь)` — вывести содержимое файла.
fn sh_cat(args: &[Value]) -> Result<Value, EvalError> {
    let ep = sys::start_cap(0);
    let path = match args.first() {
        Some(Value::Str(s)) => s.as_bytes(),
        _ => return Err(EvalError::new("cat: нужен путь-строка")),
    };
    match read_file(ep, path) {
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

/// `(echo арг…)` — напечатать аргументы через пробел (строки — как есть, прочее — каноничной формой).
fn sh_echo(args: &[Value]) -> Result<Value, EvalError> {
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

/// Прочитать строку с консоли: эхо набранного + backspace (`\x7f`/`\x08`), конец — `\r`/`\n`.
/// `None` — EOF (пустой ввод при закрытом stdin). Минимальный редактор; стрелки/история — позже.
fn read_line(line: &mut [u8]) -> Option<usize> {
    let mut len = 0usize;
    loop {
        let mut b = [0u8; 1];
        if sys::read_stdin(&mut b) == 0 {
            return if len == 0 { None } else { Some(len) };
        }
        match b[0] {
            b'\r' | b'\n' => {
                sys::write(b"\r\n");
                return Some(len);
            }
            0x7f | 0x08 => {
                if len > 0 {
                    len -= 1;
                    sys::write(b"\x08 \x08"); // стереть символ на терминале
                }
            }
            c => {
                if len < line.len() {
                    line[len] = c;
                    len += 1;
                    sys::write(&b[..1]); // эхо
                }
            }
        }
    }
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
