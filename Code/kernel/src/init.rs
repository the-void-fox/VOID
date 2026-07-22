//! Веха 40 — декларативный init: система поднимается по КОНФИГУ из store, а не по сценарию,
//! зашитому в `kmain`. «configuration.nix родными средствами»: какие серверы запускать, какие
//! права им дать, что сделать shell'ом — описано текстом-объектом; ядро его читает и исполняет.
//!
//! Модель — ровно как у NixOS ([[0004-void-pkg]]): язык Nix (если нужен) вычисляется на ХОСТЕ и
//! кладёт готовый конфиг в store через мост; работающая система Nix НЕ исполняет — читает
//! уже-вычисленный результат. Поколения — история корня, откат — смена корня (`system/current`):
//! `switch NAME` в vsh + перезагрузка = загрузка в другую конфигурацию, и назад.
//!
//! Формат конфига (текст, редактируемый из vsh И генерируемый из `nix/system.nix`):
//! ```text
//! # комментарий
//! service posixfs store:rw          # сервер + права a0
//! service net-srv dev:net:rw
//! shell   vsh endpoint:posixfs store:xw endpoint:net-srv env
//! ```
//! Токены прав: `store:RWX`, `dev:net:RW`, `dev:block:RW`, `endpoint:ИМЯ[:S]` (по умолчанию SEND),
//! `env` (передать ARCH/SYSTEM). Буквы прав: `r`=READ `w`=WRITE `x`=EXEC `s`=SEND `g`=GRANT.
//! Права по порядку → `a0`, `a1`, и все → таблица стартовых capability (как контракт Вехи 30).

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use void_abi::Rights;

use crate::{arch, cap, object, println, proc};

/// Корень-указатель активного поколения: его содержимое — ИМЯ поколения (`system/<имя>`).
const CURRENT_ROOT: &str = "system/current";

/// Конфиг по умолчанию (поколение `gen1`) — полный: файлы, сеть, интерактивный shell.
/// Совпадает с тем, что до Вехи 40 было зашито в `shell_session`.
const DEFAULT_GEN1: &str = "\
# VOID — поколение по умолчанию (полное: файлы + сеть)
service posixfs store:rw
service net-srv dev:net:rw
shell vsh endpoint:posixfs store:xw endpoint:net-srv env
";

/// Второе поколение (`gen2`) — минимальное, БЕЗ сети: витрина отката. Тот же shell, но без
/// net-srv и без сетевого эндпоинта → `ping` в vsh честно говорит «сети нет».
const DEFAULT_GEN2: &str = "\
# VOID — минимальное поколение (без сети)
service posixfs store:rw
shell vsh endpoint:posixfs store:xw env
";

/// Прочитать текстовый объект по корню-имени. `None` — корня нет или это не UTF-8.
fn read_text(root: &str) -> Option<String> {
    let id = object::root(root)?;
    object::with(&id, |b| b.and_then(|x| core::str::from_utf8(x).ok()).map(String::from))
}

/// Записать текст объектом и привязать к корню (сев конфига/поколения).
fn write_text(root: &str, text: &str) {
    let id = object::put(text.as_bytes());
    object::set_root(root, id);
}

/// Разобрать буквенный набор прав (`rwxsg`) в [`Rights`]. Неизвестные буквы игнорируются.
fn parse_rights(s: &str) -> Rights {
    let mut r = Rights::NONE;
    for c in s.chars() {
        r = r.union(match c {
            'r' => Rights::READ,
            'w' => Rights::WRITE,
            'x' => Rights::EXEC,
            's' => Rights::SEND,
            'g' => Rights::GRANT,
            _ => Rights::NONE,
        });
    }
    r
}

/// Запустить программу из store по имени (арх-корень → content-id → ELF → процесс). Личность
/// процесса (`pname`) — имя из конфига, «утёкшее» в `'static` (устойчиво по СОДЕРЖИМОМУ: домен
/// `.cspace` переиспользуется по имени). `None` — программы нет в store или ELF негоден.
fn spawn(name: &str) -> Option<usize> {
    let root = crate::prog_root(name);
    let id = object::root(&root)?;
    let bytes = object::with(&id, |b| b.map(<[u8]>::to_vec))?;
    let pname: &'static str = Box::leak(name.to_string().into_boxed_str());
    match proc::spawn_elf(pname, &bytes, 0) {
        Ok(pid) => Some(pid),
        Err(e) => {
            println!("  [init] '{}': негодный ELF: {:?}", name, e);
            None
        }
    }
}

/// Сминтить capability по токену конфига в домен процесса `pid`. `services` — уже поднятые
/// серверы (имя → pid) для разрешения `endpoint:ИМЯ`. Возвращает дескриптор или `None`
/// (`env` — не capability; неизвестный/несогласованный токен — предупреждение).
fn mint_cap(pid: usize, token: &str, services: &[(String, usize)]) -> Option<usize> {
    let dom = proc::domain(pid);
    if let Some(r) = token.strip_prefix("store:") {
        Some(cap::mint(dom, cap::Target::Store, parse_rights(r)).bits() as usize)
    } else if let Some(r) = token.strip_prefix("dev:net:") {
        Some(cap::mint(dom, cap::Target::Device(cap::Device::Net), parse_rights(r)).bits() as usize)
    } else if let Some(r) = token.strip_prefix("dev:block:") {
        Some(cap::mint(dom, cap::Target::Device(cap::Device::Block), parse_rights(r)).bits() as usize)
    } else if let Some(rest) = token.strip_prefix("endpoint:") {
        // endpoint:ИМЯ  или  endpoint:ИМЯ:права (по умолчанию SEND).
        let (svc, rights) = match rest.split_once(':') {
            Some((s, r)) => (s, parse_rights(r)),
            None => (rest, Rights::SEND),
        };
        match services.iter().find(|(n, _)| n == svc) {
            Some((_, spid)) => {
                Some(cap::mint(dom, cap::Target::Endpoint(*spid), rights).bits() as usize)
            }
            None => {
                println!("  [init] endpoint:{} — нет такого сервера (пропуск)", svc);
                None
            }
        }
    } else {
        None
    }
}

/// Разобрать и исполнить конфиг: поднять каждую запись (`service`/`shell`), сминтить её права,
/// разложить их как `a0`/`a1`/стартовые (контракт Вехи 30). Первый два права дублируются в
/// регистры запуска — как раньше делал `shell_session` руками.
fn apply(config: &str) {
    let mut services: Vec<(String, usize)> = Vec::new(); // имя → pid (для endpoint:)
    let env = alloc::format!("ARCH={}\0SYSTEM=void\0", arch::ARCH_NAME);

    for line in config.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut tok = line.split_whitespace();
        let kind = tok.next().unwrap_or("");
        if kind != "service" && kind != "shell" {
            println!("  [init] неизвестная директива '{}' (пропуск)", kind);
            continue;
        }
        let Some(name) = tok.next() else { continue };
        let Some(pid) = spawn(name) else { continue };

        // Права по порядку: собрать дескрипторы, разложить в a0/a1 + стартовую таблицу.
        let mut caps: Vec<usize> = Vec::new();
        let mut want_env = false;
        for t in tok {
            if t == "env" {
                want_env = true;
            } else if let Some(bits) = mint_cap(pid, t, &services) {
                caps.push(bits);
            }
        }
        if let Some(&a0) = caps.first() {
            proc::set_arg(pid, a0);
        }
        if let Some(&a1) = caps.get(1) {
            proc::set_arg2(pid, a1);
        }
        for &c in &caps {
            proc::push_start_cap(pid, c);
        }
        if want_env {
            proc::set_env(pid, env.as_bytes());
        }
        println!(
            "  [init] {} P{} '{}' — прав {}{}",
            kind, pid, name, caps.len(),
            if want_env { " +env" } else { "" },
        );
        if kind == "service" {
            services.push((name.to_string(), pid));
        }
    }
}

/// Веха 40 — точка входа декларативной загрузки (заменяет зашитый `shell_session`). Читает
/// активное поколение из `system/current` (сеет два поколения по умолчанию на чистом диске),
/// исполняет его конфиг и отдаёт управление планировщику до выхода shell'а.
pub fn boot() {
    // Чистый диск: посеять поколения по умолчанию и выбрать полное.
    if object::root("system/gen1").is_none() {
        write_text("system/gen1", DEFAULT_GEN1);
        write_text("system/gen2", DEFAULT_GEN2);
        write_text(CURRENT_ROOT, "gen1");
        println!("  [init] чистый диск — посеяны поколения gen1 (полное) и gen2 (без сети)");
    }

    // Активное поколение: system/current → имя → system/<имя> → текст конфига.
    let gen = read_text(CURRENT_ROOT).unwrap_or_else(|| "gen1".to_string());
    let gen = gen.trim().to_string();
    let config = match read_text(&alloc::format!("system/{}", gen)) {
        Some(c) => c,
        None => {
            println!("  [init] поколение '{}' не найдено — беру gen1", gen);
            read_text("system/gen1").unwrap_or_else(|| DEFAULT_GEN1.to_string())
        }
    };
    println!("  [init] поколение '{}' — поднимаю систему по конфигу:", gen);
    apply(&config);

    proc::run();
    println!("  [init] сессия '{}' завершена (shell вышел) — обратно в ядро", gen);
}
