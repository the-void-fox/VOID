//! `nixe` — ВЫЧИСЛИТЕЛЬ ЯЗЫКА NIX (Веха 188, ADR 0019 шаг 5).
//!
//! ```text
//! nixe <файл.nix>      — вычислить выражение из файла
//! nixe -e '<выражение>' — вычислить прямо из строки
//! ```
//!
//! Печатает значение так же, как `nix-instantiate --eval --strict`: строго, до дна и в той же
//! записи. Это не украшение — на совпадении печати стоит вся проверка вычислителя (см. шапку
//! `void-nix`): язык чужой, спецификации у него нет, и единственное осмысленное утверждение о
//! правильности звучит «на тех же выражениях мы отвечаем то же, что настоящий nix».
//!
//! ## Отдельная программа, а не команда шелла
//!
//! У `vvsh` свой язык ([[0006-config-language]]), и подмешивать к нему второй значило бы завести
//! в системе два способа сказать одно и то же. Nix здесь — чужой язык для чужой задачи (описание
//! пакетов), и живёт он отдельной программой со своими правами: store и файлы, ничего больше.
//!
//! ## Деривации он строит (Веха 189)
//!
//! `derivation { … }` считает пути store и КЛАДЁТ текст задания в `/nix/store/<хэш>-<имя>.drv`,
//! после чего его собирает `nixb`. Пути совпадают с настоящим nix байт в байт — иначе всё
//! теряет смысл: адрес в store и есть удостоверение того, что собрано то же самое.
//!
//! ## Чего он пока не делает
//!
//! Не умеет `import` — читать файлы из выражения незачем, пока нечего собирать из нескольких.
//! Отсутствующее отсутствует явно, а не отвечает неправдой.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

use void_user as sys;
use void_user::posix as px;

// Куча щедрая: дерево выражения и отложки живут все разом, а выражение nixpkgs — это тысячи
// узлов. Арена ленивая (`SYS_MAP`), неиспользованные страницы не стоят ничего.
#[global_allocator]
static ALLOC: sys::heap::Heap<{ 32 * 1024 * 1024 }> = sys::heap::Heap::new();

fn say(s: &str) {
    sys::write(s.as_bytes());
}

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let fs = sys::cap_named("POSIXFS").unwrap_or_else(|| sys::start_cap(0));

    let argv = sys::argv::Argv::take();
    let mut words = argv.rest();
    let src: String = match words.next() {
        Some(b"-e") => {
            // Остаток строки — само выражение. Собираем ПРОБЕЛАМИ обратно: шелл разбил его на
            // слова, а для nix пробел между лексемами значения не имеет.
            let mut out = String::new();
            for w in words {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(&String::from_utf8_lossy(w));
            }
            if out.is_empty() {
                say("nixe: после -e нужно выражение\n");
                sys::exit(2);
            }
            out
        }
        Some(path) => match read_file(fs, path) {
            Some(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            None => {
                say(&format!("nixe: файла нет: {}\n", String::from_utf8_lossy(path)));
                sys::exit(2);
            }
        },
        None => {
            say("nixe: nixe <файл.nix> | nixe -e '<выражение>'\n");
            sys::exit(2);
        }
    };

    // Текст задания кладём в иерархию под тем самым путём, который вычислили. Каталоги
    // создаём заранее: писать в `/nix/store` может и ядро, и файловый сервер, но индекс
    // родителя должен существовать (Веха 187).
    px::mkdir(fs, b"/nix");
    px::mkdir(fs, b"/nix/store");
    let store: Rc<dyn void_nix::derive::StoreText> = Rc::new(Иерархия { fs });

    match void_nix::eval_with_store(&src, store) {
        Ok(text) => {
            say(&text);
            say("\n");
            sys::exit(0);
        }
        Err(e) => {
            say(&format!("nixe: {}\n", e));
            sys::exit(1);
        }
    }
}

/// Куда уезжает текст задания: обычный файл в иерархии по вычисленному пути.
struct Иерархия {
    fs: usize,
}

impl void_nix::derive::StoreText for Иерархия {
    fn add(&self, path: &str, text: &str) -> Result<(), String> {
        // Уже лежит такой же — не работа, а тождество: путь адресует содержимое.
        if let Some((_, size)) = px::stat(self.fs, path.as_bytes()) {
            if size == text.len() {
                return Ok(());
            }
        }
        if px::echo_to(self.fs, path.as_bytes(), text.as_bytes()) {
            Ok(())
        } else {
            Err(format!("задание не записалось: {}", path))
        }
    }

    fn read(&self, path: &str) -> Option<String> {
        read_file(self.fs, path.as_bytes()).map(|b| String::from_utf8_lossy(&b).into_owned())
    }
}

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
