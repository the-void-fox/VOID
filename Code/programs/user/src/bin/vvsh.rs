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

/// `repl` (S2a) — интерактивный Lisp-REPL. Окружение ЖИВЁТ между строками (`(define x 5)` → потом
/// `(+ x 10)` → 15). Пока чисто-вычислительный (без эффектов): доказывает, что язык работает
/// интерактивно на VOID. Запуск из vsh: `run vvsh repl`; выход — `(exit)`/`exit`/EOF → назад в vsh
/// (vsh остаётся внешним спасательным шеллом — если vvsh упадёт, он ловит обратно).
fn cmd_repl() -> ! {
    sys::write(
        "vvsh REPL — Lisp VOID (ADR 0006). (exit) или Ctrl-D — назад в vsh.\n".as_bytes(),
    );
    let loader = vvsh_core::NoLoader;
    let interp = vvsh_core::Interp::new(&loader);
    let env = vvsh_core::root_env(); // ПЕРСИСТЕНТНОЕ окружение сессии
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
        let text = match core::str::from_utf8(src) {
            Ok(t) => t,
            Err(_) => {
                sys::write("ошибка: ввод не UTF-8\n".as_bytes());
                continue;
            }
        };
        match vvsh_core::read_all(text) {
            Ok(forms) => {
                for f in &forms {
                    match interp.eval(f, &env) {
                        Ok(v) => print_value(&v),
                        Err(e) => {
                            sys::write("ошибка: ".as_bytes());
                            sys::write(e.0.as_bytes());
                            sys::write(b"\n");
                        }
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
    sys::write("vvsh: выход из REPL — vsh продолжает\n".as_bytes());
    sys::exit(0);
}

/// Печать значения-результата (каноничная форма Value).
fn print_value(v: &vvsh_core::Value) {
    let s = alloc::format!("{}\n", v);
    sys::write(s.as_bytes());
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
