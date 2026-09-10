//! `nixb` — ПЕСОЧНИЦА СБОРКИ (Веха 187, ADR 0019 шаг 4): выполнить деривацию на самом VOID.
//!
//! ```text
//! nixb <путь к .drv>
//!   ├─ разобрать задание (void_drv)
//!   ├─ приготовить место: /build чистый, /nix/store существует
//!   ├─ окружение = детерминированные умолчания + окружение деривации (оно главнее)
//!   ├─ SYS_EXEC сборщика — ЖДЁМ кода выхода
//!   ├─ проверить, что $out появился
//!   └─ ЗАПЕЧАТАТЬ путь: с этой минуты он только читается
//! ```
//!
//! ## Песочница здесь — не забор, а ОТСУТСТВИЕ ДВЕРИ
//!
//! Nix городит вокруг сборки пространства имён, чтобы отнять у неё сеть, чужие файлы и имена
//! пользователей. В VOID отнимать нечего: **процесс личности Linux не может позвать ни одного
//! системного вызова VOID**. Его трапы разбирает транслятор `linux_syscall`, а там есть
//! `read`/`write`/`openat` и нет ни `SYS_CALL`, ни `SYS_SPAWN`, ни единого способа тронуть
//! capability. Сети у сборки нет не потому, что мы её запретили, а потому что `socket` в
//! личности не существует — как не существует и способа его туда протащить.
//!
//! Что действительно приходится делать руками — это **детерминизм**: одинаковые часы, одинаковый
//! `HOME`, одинаковый временный каталог. Тут VOID ничем не лучше других, и умолчания взяты у
//! nix буквально (`/homeless-shelter`, `PATH=/path-not-set`), чтобы сборка, которая на них
//! наткнётся, сломалась ТАК ЖЕ, как сломалась бы там.
//!
//! ## Почему `$out` открыт на запись
//!
//! Деривация пишет в свой НАСТОЯЩИЙ адрес в store, а не в подменный каталог. Иначе всё, что
//! сборка о себе запомнит — shebang'и, RPATH, пути внутри скриптов, — указывало бы в никуда.
//! Ровно поэтому ядро (Веха 187) считает «в пакете» не «под `/nix/store`», а «внутри готового
//! объекта»: путь открыт на запись, пока не запечатан, и неизменяем после. Печать ставит эта
//! программа — последним действием, когда сборка уже удалась.
//!
//! ## Граф сборок (Веха 190)
//!
//! Задание может зависеть от других заданий, и тогда они собираются ПЕРВЫМИ — обходом в глубину,
//! от листьев к корню. Порядок здесь не выбор, а следствие: сборщик зависимого пишет пути своих
//! зависимостей в аргументы, и к моменту запуска они обязаны существовать.
//!
//! Планировщика и параллельности нет: обход строго последовательный. Уже запечатанное не
//! пересобирается — путь адресует содержимое, и второй раз собирать то же самое незачем.
//!
//! ## Чего он НЕ делает
//!
//! Не проверяет хэши фиксированных выходов (нечем: сеть сюда не ходит). Не ловит вывод сборки —
//! он идёт в журнал ядра, как и всякий вывод чужого процесса без stdio.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use void_user as sys;
use void_user::posix as px;

#[global_allocator]
static ALLOC: sys::heap::Heap<{ 4 * 1024 * 1024 }> = sys::heap::Heap::new();

/// Каталог сборки. Один на систему и намеренно: две сборки разом — это уже планировщик.
const BUILD_DIR: &str = "/build";
/// Каталог store в путях nix.
const STORE_DIR: &str = "/nix/store";
/// Префикс корня-печати: он же читается ядром (`lxfs::BUILT_ROOT`).
const BUILT_ROOT: &str = "pkg/built/";
/// Длина хэша пути nix.
const HASH_LEN: usize = 32;

fn say(s: &str) {
    sys::write(s.as_bytes());
}

/// Система, которую мы умеем собирать. Это не «мы Linux», а честное описание сборщика: он
/// линуксовый бинарь и исполняется личностью Linux.
const fn system() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x86_64-linux"
    } else {
        "riscv64-linux"
    }
}

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let fs = sys::cap_named("POSIXFS").unwrap_or_else(|| sys::start_cap(0));
    let store = sys::cap_named("STORE").unwrap_or_else(|| sys::start_cap(1));

    let argv = sys::argv::Argv::take();
    let mut words = argv.rest();
    let Some(path) = words.next() else {
        say("nixb: nixb <путь к .drv>\n");
        sys::exit(2);
    };
    let path = path.to_vec();

    match build(fs, store, &path) {
        Ok(out) => {
            say(&format!("{}\n", out));
            sys::exit(0);
        }
        Err(e) => {
            say(&format!("nixb: {}\n", e));
            sys::exit(1);
        }
    }
}

/// Потолок глубины графа. Циклов в нём быть не может — путь задания зависит от его текста, —
/// но чужой `.drv` мы читаем как данные, и упереться в стек из-за испорченного файла нельзя.
const MAX_DEPTH: usize = 32;

fn build(fs: usize, store: usize, drv_path: &[u8]) -> Result<String, String> {
    build_rec(fs, store, drv_path, 0)
}

fn build_rec(fs: usize, store: usize, drv_path: &[u8], depth: usize) -> Result<String, String> {
    if depth > MAX_DEPTH {
        return Err(format!("граф сборок глубже {} — похоже на испорченное задание", MAX_DEPTH));
    }
    let text = read_file(fs, drv_path).ok_or_else(|| {
        format!("деривации нет: {}", String::from_utf8_lossy(drv_path))
    })?;
    let d = void_drv::parse(&text).map_err(|e| e.to_string())?;

    // ── что мы отказываемся делать, и сразу ────────────────────────────────────
    if d.system != system() {
        return Err(format!("деривация для '{}', а мы '{}'", d.system, system()));
    }
    // Зависимости — ПЕРВЫМИ. Обход в глубину: к запуску нашего сборщика их пути обязаны
    // существовать, потому что он ими и пользуется.
    for (input, _) in &d.input_drvs {
        build_rec(fs, store, input.as_bytes(), depth + 1)?;
    }
    for src in &d.input_srcs {
        if px::stat(fs, src.as_bytes()).is_none() {
            return Err(format!("исходника нет в store: {}", src));
        }
    }
    let out = d.output("out").ok_or("у деривации нет выхода 'out'")?.to_string();
    if !out.starts_with(STORE_DIR) || out.len() < STORE_DIR.len() + 1 + HASH_LEN {
        return Err(format!("выход не похож на путь store: {}", out));
    }
    let hash = &out[STORE_DIR.len() + 1..STORE_DIR.len() + 1 + HASH_LEN];
    // Уже собрано — это не ошибка, а весь смысл: путь адресует содержимое, и пересобирать его
    // незачем. Отвечаем тем же путём, что ответили бы после сборки.
    if sealed(store, hash) {
        return Ok(out);
    }
    if px::stat(fs, out.as_bytes()).is_some() {
        return Err(format!("путь занят, но не запечатан — оборванная сборка: {}", out));
    }

    // ── место ──────────────────────────────────────────────────────────────────
    px::mkdir(fs, b"/nix");
    px::mkdir(fs, STORE_DIR.as_bytes());
    px::unlink_all(fs, BUILD_DIR.as_bytes());
    if px::mkdir(fs, BUILD_DIR.as_bytes()) == usize::MAX {
        return Err(format!("не удалось завести {}", BUILD_DIR));
    }

    // ── окружение ──────────────────────────────────────────────────────────────
    // Своё окружение сохраняем и вернём: подменять его — единственный способ задать окружение
    // ребёнку (оно НАСЛЕДУЕТСЯ при запуске), а нам после сборки ещё говорить с терминалом.
    let mut saved = [0u8; 4096];
    let n = sys::env(&mut saved);
    let saved = saved[..n.min(saved.len())].to_vec();

    if !sys::set_env(&build_env(&d, &out)) {
        return Err(String::from("окружение деривации не влезло в потолок ядра"));
    }
    let mut args: Vec<u8> = Vec::new();
    for a in &d.args {
        args.extend_from_slice(a.as_bytes());
        args.push(0);
    }
    let code = sys::exec_args(store, d.builder.as_bytes(), &args);
    sys::set_env(&saved);

    if code == usize::MAX {
        return Err(format!("сборщик не запустился: {}", d.builder));
    }
    if code != 0 {
        return Err(format!("сборка кончилась кодом {}", code));
    }
    let Some((_, size)) = px::stat(fs, out.as_bytes()) else {
        return Err(format!("сборка прошла, но {} не появился", out));
    };

    // ── печать ─────────────────────────────────────────────────────────────────
    // В теле корня — сам путь: печать должна быть читаемой снаружи, иначе она превращается в
    // невидимое состояние, о котором знает одна эта программа.
    let mut id = [0u8; 32];
    // Успех у обоих вызовов — НОЛЬ (`SYS_OBJ_PUT`/`SYS_OBJ_SET_ROOT` возвращают 0/MAX).
    if sys::obj_put(store, out.as_bytes(), &mut id) != 0 {
        return Err(String::from("печать не легла в store"));
    }
    let name = format!("{}{}", BUILT_ROOT, hash);
    if sys::obj_set_root(store, name.as_bytes(), &id) != 0 {
        return Err(String::from("печать не встала корнем — нет права записи в store?"));
    }
    let _ = size;
    px::unlink_all(fs, BUILD_DIR.as_bytes());
    Ok(out)
}

/// Запечатан ли путь с таким хэшем.
fn sealed(store: usize, hash: &str) -> bool {
    let name = format!("{}{}", BUILT_ROOT, hash);
    let mut id = [0u8; 32];
    sys::obj_get_root(store, name.as_bytes(), &mut id) == 32
}

/// Окружение сборки: сперва детерминированные умолчания, затем окружение деривации — оно
/// главнее. Порядок именно такой и у nix: деривация вправе переопределить всё, кроме места,
/// куда её поселили.
fn build_env(d: &void_drv::Drv, out: &str) -> Vec<u8> {
    let mut env: Vec<(String, String)> = Vec::new();
    let mut put = |k: &str, v: &str| {
        let k = k.to_string();
        match env.iter_mut().find(|(ek, _)| *ek == k) {
            Some(slot) => slot.1 = v.to_string(),
            None => env.push((k, v.to_string())),
        }
    };
    // Умолчания nix, взятые буквально. `/homeless-shelter` и `/path-not-set` — не шутка и не
    // заглушка: это адреса, которых заведомо нет, чтобы сборка, полезшая в `$HOME` или в `PATH`,
    // сломалась ЗДЕСЬ и громко, а не собралась по-разному на двух машинах.
    put("PATH", "/path-not-set");
    put("HOME", "/homeless-shelter");
    put("NIX_STORE", STORE_DIR);
    put("NIX_BUILD_TOP", BUILD_DIR);
    put("NIX_BUILD_CORES", "1");
    put("TMPDIR", BUILD_DIR);
    put("TEMPDIR", BUILD_DIR);
    put("TMP", BUILD_DIR);
    put("TEMP", BUILD_DIR);
    put("PWD", BUILD_DIR);
    put("TERM", "dumb");
    put("TZ", "UTC");
    put("SOURCE_DATE_EPOCH", "1");
    for (k, v) in &d.env {
        put(k, v);
    }
    // `out` ставим ПОСЛЕ всего: куда легло, решаем мы, и деривация переспорить это не может.
    put("out", out);

    let mut blob = Vec::new();
    for (k, v) in &env {
        blob.extend_from_slice(k.as_bytes());
        blob.push(b'=');
        blob.extend_from_slice(v.as_bytes());
        blob.push(0);
    }
    blob
}

/// Прочитать файл целиком через файловый сервер.
fn read_file(fs: usize, path: &[u8]) -> Option<Vec<u8>> {
    let (_, size) = px::stat(fs, path)?;
    let fd = px::open(fs, path, 0);
    if fd == usize::MAX {
        return None;
    }
    let mut out = Vec::with_capacity(size);
    let mut chunk = [0u8; 4096];
    loop {
        let n = px::read(fs, fd, &mut chunk);
        if n == 0 || n == usize::MAX {
            break;
        }
        out.extend_from_slice(&chunk[..n]);
    }
    px::close(fs, fd);
    Some(out)
}
