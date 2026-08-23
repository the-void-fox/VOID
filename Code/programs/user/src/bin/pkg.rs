//! `pkg` — пакеты из бинарного кэша nixpkgs (Вехи 106–107, Фаза 8, [[0009-ondevice-packages]]).
//!
//! ```text
//! pkg update           скачать индекс имён канала (иначе имена не работают)
//! pkg search <строка>  какие имена в индексе на неё похожи
//! pkg fetch <путь|имя> скачать ЗАМЫКАНИЕ пути (сам путь и все зависимости)
//! pkg install <п|имя>  то же + новое поколение профиля с этим пакетом
//! pkg remove <имя>     новое поколение профиля без пакета
//! pkg sync             собрать то, что объявил конфиг системы (`packages …`)
//! pkg list             что стоит в активном поколении профиля
//! pkg gens             поколения профиля (активное — *)
//! pkg rollback         вернуть предыдущее поколение
//! ```
//!
//! ## Почему проверка — суть, а не украшение
//!
//! Всё, что мы берём из кэша, приходит с чужой машины. TLS (Веха 95) отвечает лишь за то, что
//! байты не подменили ПО ДОРОГЕ, — он ничего не говорит о том, кто их туда положил. Настоящая
//! гарантия nix другая и куда сильнее: **подпись ключом кэша по отпечатку пути** и
//! **content-address** — сверка sha256 распакованного NAR с тем, что обещано в narinfo. Без этих
//! двух проверок «скачать пакет» означало бы «исполнить то, что прислал незнакомец», а VOID
//! запускает программы по content-id именно затем, чтобы такого не было.
//!
//! Порядок важен: подпись проверяется ДО загрузки архива (незачем тянуть мегабайты, если
//! метаданным нельзя верить), NarHash — после распаковки. Метаданные, взятые из СВОЕГО кэша
//! (корень `pkg/narinfo/<хэш>`), проверяются наравне со свежескачанными: откуда пришли байты —
//! не аргумент.
//!
//! ## Профиль — это поколения, а не каталог
//!
//! Профиль (Веха 107) хранится ровно теми же средствами, что поколения системы
//! ([[declarative-init]]): `pkg/profile/<профиль>/gen<N>` — **узел** store, чьё значение
//! перечисляет пути, а исходящие ссылки держат их содержимое (значит, GC их не тронет);
//! `pkg/profile/<профиль>/current` — корень-указатель, чьё значение = имя активного поколения.
//! Отсюда всё остальное следует само: установка и удаление — это НОВОЕ поколение (пересборка
//! замыкания из списка верхнего уровня), а откат — смена одного указателя, без единой загрузки.
//!
//! ## Имя — это подсказка, а не полномочие (Веха 111)
//!
//! Путь store адресуется хэшем, и по-другому быть не может: имя `hello` ничего не удостоверяет.
//! Но набирать хэш руками — не работа для человека, поэтому появился ИНДЕКС КАНАЛА
//! (`store-paths.xz` с channels.nixos.org): текстовый список всех путей, которые канал собрал.
//!
//! Индекс **не подписан**, и это не оплошность, а следствие устройства: он ничего не решает.
//! Всё, что он может, — назвать хэш; дальше начинается обычная дорога с подписью кэша и
//! NarHash ([[pkg-fetch]]). Единственная выдумка, доступная подменённому индексу, — подсунуть
//! на имя `hello` хэш другого пакета, и **та проверяется отдельно**: имя пути, которое вернул
//! подписанный narinfo, обязано совпасть с тем, что обещал индекс, — и совпасть ДО загрузки
//! содержимого. Итого подменённый индекс способен лишь дать вам не тот `hello` из nixpkgs, а
//! не чужой код: содержимое всё равно подписано ключом кэша.
//!
//! Второе следствие того же: **имя не однозначно**. В одном канале живут 63 пути `stdenv-linux`
//! и два разных `hello-2.12.3`. Значение имён знает вычисление nixpkgs (Фаза 9), а индекс знает
//! только строки, — поэтому выбор делается объявленным правилом (свой выход, старшая версия,
//! дальше порядок индекса) и **говорится вслух**, вместе с числом отвергнутых кандидатов.
//!
//! ## Пакеты объявляются, а не устанавливаются (Веха 112)
//!
//! `pkg install` — это действие, и как всякое действие оно забывается: через полгода никто не
//! скажет, почему в системе стоит `jq`. Поэтому у пакетов есть второй, главный путь — **строка
//! `packages …` в конфиге**. Её читает `pkg sync`: резолвит имена, тянет замыкание и кладёт
//! поколение профиля `system` — под именем поколения СИСТЕМЫ, а не своим.
//!
//! Отсюда откат: у системного профиля нет указателя «активное поколение», его роль играет
//! `system/current`. Откатили систему — вместе с ней откатились и пакеты, потому что это одно
//! решение, а не два согласованных ([[declarative-init]], модуль [`profile`]).
//!
//! ## Почему отдельная программа
//!
//! Тот же довод, что у `httpsc` (Веха 95): чужого кода много (криптография, два распаковщика), а
//! полномочий ему нужно мало. `pkg` получает store и сеть — и всё; posixfs, консоли пользователя
//! и прочего у него нет. Скачиванием занимается `httpsc`, которому `pkg` передаёт URL и имя
//! корня, — TLS остаётся в своём процессе.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use base64ct::{Base64, Encoding};
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};

use void_user as sys;

// Общий с `vvsh` код — подключён ПО ПУТИ (почему так — в шапках самих файлов).
#[path = "../archive.rs"]
mod archive;
#[path = "../roots.rs"]
mod roots;
// Формат дерева пакета в store — общий крейт: его читают и posixfs, и персоналия Linux.
use void_tree as tree;
// Профиль (поколения и их содержимое) — общий с шеллом: ему он нужен для PATH (Веха 109).
#[allow(dead_code)] // читательская половина (PATH) нужна шеллу, `pkg` пишет
#[path = "../profile.rs"]
mod profile;
// Правила именования nix (имя/версия/выход и сравнение версий). Отдельным файлом ради хостовой
// проверки против настоящего nix — см. его шапку.
#[path = "../nixname.rs"]
mod nixname;
use nixname::{parts, HASH_LEN};

// Куча (Веха 108): от размера пакета она БОЛЬШЕ НЕ ЗАВИСИТ — байты идут потоком, и целиком в
// памяти не живёт ни архив, ни NAR, ни файл. Определяет её теперь окно распаковщика: у zstd оно
// до 8 МиБ, и меньше этого ставить нельзя (замер: с 4 МиБ падает ещё до первого файла). Арена
// ленивая (`SYS_MAP`), неиспользованные страницы не стоят ничего.
#[global_allocator]
static ALLOC: sys::heap::Heap<{ 16 * 1024 * 1024 }> = sys::heap::Heap::new();

/// Кэш по умолчанию. Хост зашит намеренно: пока нет конфига подстановщиков, «откуда берём» —
/// решение системы, а не аргумент командной строки.
const CACHE: &str = "https://cache.nixos.org";

/// Каталог store в путях nix. Участвует в отпечатке, которым подписан путь, поэтому это не
/// косметика: другой каталог — другая подпись.
const STORE_DIR: &str = "/nix/store";

/// Доверенный ключ кэша: имя и открытая часть (base64), как в `trusted-public-keys` nix.
const KEY_NAME: &str = "cache.nixos.org-1";
const KEY_B64: &str = "6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=";

/// Алфавит nix-base32 — свой, не RFC: выброшены `e`, `o`, `u`, `t`.
const NIX32: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";

/// Имя профиля и формат поколения живут в общем модуле [`profile`].
use profile::{GEN_MAGIC, PROFILE};

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let argv = sys::argv::Argv::take();
    let mut it = argv.rest();
    let (cmd, rest, third) = (it.next(), it.next(), it.next());

    let code = match (cmd, rest) {
        (Some(b"update"), _) => done(cmd_update()),
        (Some(b"search"), Some(what)) => done(cmd_search(what)),
        (Some(b"fetch"), Some(what)) => done(cmd_fetch(what)),
        (Some(b"install"), Some(what)) => done(cmd_install(what)),
        (Some(b"remove"), Some(what)) => done(cmd_remove(what)),
        (Some(b"sync"), _) => done(cmd_sync()),
        (Some(b"tree"), Some(what)) => done(cmd_tree(what, third.unwrap_or(b""))),
        (Some(b"cat"), Some(what)) => match third {
            Some(sub) => done(cmd_cat(what, sub)),
            None => {
                sys::write("pkg cat <путь> <подпуть внутри пакета>\n".as_bytes());
                2
            }
        },
        (Some(b"list"), _) => done(cmd_list()),
        (Some(b"gens"), _) => done(cmd_gens()),
        (Some(b"rollback"), _) => done(cmd_rollback()),
        (Some(b"gc"), what) => done(cmd_gc(what == Some(b"all"))),
        _ => {
            usage();
            2
        }
    };
    sys::exit(code);
}

/// Ошибку печатаем здесь, одним местом: у каждой команды один и тот же выход.
fn done(r: Result<(), String>) -> usize {
    match r {
        Ok(()) => 0,
        Err(e) => {
            sys::write(format!("pkg: {}\n", e).as_bytes());
            1
        }
    }
}

fn usage() {
    sys::write("pkg — пакеты из бинарного кэша nixpkgs\n".as_bytes());
    sys::write("  update                        скачать индекс имён канала\n".as_bytes());
    sys::write("  search <строка>               какие имена в индексе на неё похожи\n".as_bytes());
    sys::write("  fetch <хэш|путь|имя>          скачать замыкание (путь и все зависимости)\n".as_bytes());
    sys::write("  install <хэш|путь|имя>        скачать и внести в профиль (новое поколение)\n".as_bytes());
    sys::write("  remove <имя|хэш>              убрать из профиля (новое поколение)\n".as_bytes());
    sys::write("  sync                          собрать пакеты, объявленные конфигом системы\n".as_bytes());
    sys::write("  tree <путь> [подпуть]         что лежит внутри распакованного пакета\n".as_bytes());
    sys::write("  cat <путь> <подпуть>          содержимое файла из пакета\n".as_bytes());
    sys::write("  list                          что в активном поколении профиля\n".as_bytes());
    sys::write("  gens                          поколения профиля (активное — *)\n".as_bytes());
    sys::write("  rollback                      вернуть предыдущее поколение\n".as_bytes());
    sys::write("  gc [all]                      убрать лишние пути (`all` — и старые поколения)\n".as_bytes());
}

// ── narinfo: разбор, отпечаток, подпись ─────────────────────────────────────────────────────

/// Разобранный narinfo. Хранятся только поля, которые нам нужны; неизвестные молча пропускаются
/// (формат расширяемый, и ругаться на новое поле значило бы ломаться от чужого обновления).
struct NarInfo {
    store_path: String,
    url: String,
    nar_hash: String,
    nar_size: usize,
    references: Vec<String>,
    sigs: Vec<String>,
}

impl NarInfo {
    /// Имя пути без каталога store: `<хэш>-<имя>`.
    fn base(&self) -> &str {
        self.store_path.rsplit('/').next().unwrap_or(&self.store_path)
    }
}

fn parse_narinfo(text: &str) -> Result<NarInfo, String> {
    let mut store_path = String::new();
    let mut url = String::new();
    let mut nar_hash = String::new();
    let mut nar_size = 0usize;
    let mut references = Vec::new();
    let mut sigs = Vec::new();

    for line in text.lines() {
        let Some((key, val)) = line.split_once(": ") else { continue };
        match key {
            "StorePath" => store_path = val.to_string(),
            "URL" => url = val.to_string(),
            "NarHash" => nar_hash = val.to_string(),
            "NarSize" => nar_size = val.parse::<usize>().map_err(|_| "NarSize не число")?,
            "References" => {
                references = val.split_whitespace().map(|s| s.to_string()).collect()
            }
            "Sig" => sigs.push(val.to_string()),
            _ => {}
        }
    }

    if store_path.is_empty() || url.is_empty() || nar_hash.is_empty() {
        return Err(String::from("narinfo без StorePath/URL/NarHash"));
    }
    Ok(NarInfo { store_path, url, nar_hash, nar_size, references, sigs })
}

/// Отпечаток пути — ровно та строка, которую подписывает кэш (nix, `ValidPathInfo::fingerprint`):
/// `1;<путь>;<NarHash>;<NarSize>;<ссылки через запятую полными путями>`.
///
/// Собирать её надо БАЙТ В БАЙТ, иначе подпись не сойдётся, и отличить «подделка» от «мы неверно
/// склеили строку» будет нечем. Поэтому ссылки разворачиваются в полные пути, а NarHash берётся
/// как есть — в том виде, в каком стоит в narinfo.
fn fingerprint(ni: &NarInfo) -> String {
    let mut refs = String::new();
    for (i, r) in ni.references.iter().enumerate() {
        if i > 0 {
            refs.push(',');
        }
        refs.push_str(STORE_DIR);
        refs.push('/');
        refs.push_str(r);
    }
    format!("1;{};{};{};{}", ni.store_path, ni.nar_hash, ni.nar_size, refs)
}

/// Проверить подпись кэша. Нет подписи доверенным ключом — отказ, а не предупреждение:
/// «скачали, но не проверили» здесь ничем не лучше «скачали что попало».
fn check_signature(ni: &NarInfo) -> Result<(), String> {
    let key_bytes = Base64::decode_vec(KEY_B64).map_err(|_| "ключ кэша не base64")?;
    let key_arr: [u8; 32] = key_bytes.as_slice().try_into().map_err(|_| "ключ кэша не 32 байта")?;
    let key = VerifyingKey::from_bytes(&key_arr).map_err(|_| "ключ кэша неверный")?;

    let fp = fingerprint(ni);
    for s in &ni.sigs {
        let Some((name, b64)) = s.split_once(':') else { continue };
        if name != KEY_NAME {
            continue; // подпись чужим ключом — не наша забота, но и не доверие
        }
        let raw = Base64::decode_vec(b64).map_err(|_| "подпись не base64")?;
        let arr: [u8; 64] = raw.as_slice().try_into().map_err(|_| "подпись не 64 байта")?;
        let sig = Signature::from_bytes(&arr);
        return key
            .verify_strict(fp.as_bytes(), &sig)
            .map_err(|_| String::from("ПОДПИСЬ НЕ СОШЛАСЬ — путь отвергнут"));
    }
    Err(format!("нет подписи ключом {}", KEY_NAME))
}

/// sha256 → строка вида `sha256:<nix-base32>` — в таком виде хэш стоит в narinfo, и сравнивать
/// удобнее в нём же: кодирование однозначно, а разбор чужой строки — лишний источник ошибок.
fn nix_hash32(digest: &[u8; 32]) -> String {
    let len = (32 * 8 - 1) / 5 + 1; // 52 символа
    let mut s = String::with_capacity(7 + len);
    s.push_str("sha256:");
    for n in (0..len).rev() {
        let b = n * 5;
        let i = b / 8;
        let j = b % 8;
        let mut c = digest[i] >> j;
        if j > 0 && i + 1 < 32 {
            c |= digest[i + 1] << (8 - j);
        }
        s.push(NIX32[(c & 0x1f) as usize] as char);
    }
    s
}

// ── загрузка одного пути ────────────────────────────────────────────────────────────────────

/// Скачать URL в корень store через `httpsc` (TLS живёт в своём процессе, см. шапку).
/// Сколько раз пробовать загрузку, прежде чем признать её несостоявшейся.
const DOWNLOAD_TRIES: usize = 3;

/// Подождать. С Вехи 114 это настоящий сон ядра, а не круг из `yield`.
fn pause_ns(ns: u64) {
    sys::sleep_ns(ns);
}

fn download(url: &str, root: &str) -> Result<[u8; 32], String> {
    let mut argv = Vec::new();
    argv.extend_from_slice(url.as_bytes());
    argv.push(0);
    argv.extend_from_slice(root.as_bytes());
    // `-q`: об успехе пусть молчит. Замыкание — это десяток загрузок подряд, и отчёт о каждой
    // («скачано N байт, кусков K, корень …») хоронил бы отчёт о самом пакете. Ошибки он
    // печатает по-прежнему.
    argv.push(0);
    argv.extend_from_slice(b"-q");
    // Загрузка ПОВТОРЯЕТСЯ (Веха 113). Сеть отказывает не только насовсем: при десятке загрузок
    // подряд (замыкание пакета, перебор кандидатов пробником) очередное соединение с кэшем
    // срывается — замечено дважды, и оба раза следующая попытка проходила. Сдаваться с первого
    // раза значит ронять `rebuild` из-за одного пакета, который лежит на месте.
    //
    // Пауза между попытками растёт, и ждём мы не бездельем, а `yield_now`: ядру всё равно есть
    // что делать (сетевой сервер как раз доводит свои дела), а таймера-усыпителя у программы нет.
    let mut attempt = 0;
    loop {
        if sys::exec_args(sys::start_cap(1), b"httpsc", &argv) == 0 {
            break;
        }
        attempt += 1;
        if attempt >= DOWNLOAD_TRIES {
            return Err(format!("не скачалось ({} попытки): {}", DOWNLOAD_TRIES, url));
        }
        sys::write(format!("  сеть отказала — попытка {} из {}\n", attempt + 1, DOWNLOAD_TRIES).as_bytes());
        pause_ns(attempt as u64 * 2_000_000_000);
    }
    let mut id = [0u8; 32];
    if sys::obj_get_root(sys::start_cap(1), root.as_bytes(), &mut id) != 32 {
        return Err(String::from("корень не появился после загрузки"));
    }
    Ok(id)
}

/// Размер куска, которым содержимое файла уезжает в store. Тот же, что у загрузчика: одинаковые
/// байты в двух пакетах должны давать одинаковые объекты, иначе дедуп не сработает.
const FILE_CHUNK: usize = sys::http::CHUNK;

/// Положить значение объектом, вернуть его content-id.
fn put(scap: usize, data: &[u8]) -> Result<[u8; 32], String> {
    let mut id = [0u8; 32];
    if sys::obj_put(scap, data, &mut id) != 0 {
        return Err(String::from("объект не влез в store"));
    }
    Ok(id)
}

/// Положить узел (значение + исходящие ссылки).
fn put_node(scap: usize, data: &[u8], kids: &[[u8; 32]]) -> Result<[u8; 32], String> {
    let mut id = [0u8; 32];
    if sys::obj_put_node(scap, data, kids, &mut id) != 0 {
        return Err(String::from("узел не влез в store"));
    }
    Ok(id)
}

/// Каталог, который сейчас собирается: его индекс и ссылки на уже уложенное содержимое.
struct DirFrame {
    name: String,
    /// Тело индекса (заголовок допишется, когда станет известно число записей) и счётчик.
    body: Vec<u8>,
    count: u32,
    kids: Vec<[u8; 32]>,
}

/// Строитель дерева пакета: события разбора NAR → объекты store ([`tree`]).
///
/// Ничего не копит сверх одного куска файла и индексов открытых каталогов, поэтому размер пакета
/// перестаёт упираться в кучу процесса — ради этого потоковый разбор и затевался.
struct Builder {
    scap: usize,
    stack: Vec<DirFrame>,
    /// Текущий файл: имя, флаг исполняемости, накопитель куска и уже уложенные куски.
    fname: String,
    fexec: bool,
    fbuf: Vec<u8>,
    fkids: Vec<[u8; 32]>,
    fsize: u64,
    /// Корень дерева: (тип, id, размер) — заполняется, когда закрылся самый внешний узел.
    root: Option<(u8, [u8; 32], u64)>,
}

impl Builder {
    fn new(scap: usize) -> Self {
        Builder {
            scap,
            stack: Vec::new(),
            fname: String::new(),
            fexec: false,
            fbuf: Vec::with_capacity(FILE_CHUNK),
            fkids: Vec::new(),
            fsize: 0,
            root: None,
        }
    }

    /// Приписать готовый узел к родителю — или объявить его корнем, если родителя нет.
    fn attach(&mut self, ty: u8, name: &str, size: u64, id: [u8; 32]) {
        match self.stack.last_mut() {
            Some(top) => {
                tree::push(&mut top.body, ty, name.as_bytes(), size);
                top.count += 1;
                top.kids.push(id);
            }
            None => self.root = Some((ty, id, size)),
        }
    }

    fn on(&mut self, e: void_nar::Event<'_>) -> Result<(), void_nar::NarError> {
        use void_nar::Event;
        let r = match e {
            Event::Dir { path } => {
                self.stack.push(DirFrame {
                    name: leaf(path).to_string(),
                    body: Vec::new(),
                    count: 0,
                    kids: Vec::new(),
                });
                Ok(())
            }
            Event::DirEnd { .. } => (|| {
                let f = self.stack.pop().ok_or_else(|| String::from("каталог закрылся дважды"))?;
                let mut data = Vec::with_capacity(tree::HEAD + f.body.len());
                data.extend_from_slice(&tree::head(f.count));
                data.extend_from_slice(&f.body);
                let id = put_node(self.scap, &data, &f.kids)?;
                self.attach(tree::K_DIR, &f.name, f.count as u64, id);
                Ok(())
            })(),
            Event::Symlink { path, target } => (|| {
                let id = put(self.scap, target.as_bytes())?;
                self.attach(tree::K_LINK, leaf(path), target.len() as u64, id);
                Ok(())
            })(),
            Event::FileStart { path, size, exec } => {
                self.fname = leaf(path).to_string();
                self.fexec = exec;
                self.fsize = size;
                self.fbuf.clear();
                self.fkids.clear();
                Ok(())
            }
            Event::FileData { data } => (|| {
                self.fbuf.extend_from_slice(data);
                while self.fbuf.len() >= FILE_CHUNK {
                    let id = put(self.scap, &self.fbuf[..FILE_CHUNK])?;
                    self.fkids.push(id);
                    self.fbuf.drain(..FILE_CHUNK);
                }
                Ok(())
            })(),
            Event::FileEnd => (|| {
                let mut ty = tree::K_FILE;
                if self.fexec {
                    ty |= tree::F_EXEC;
                }
                // Мелкий файл — обычный объект: у блоба есть постоянная цена в манифесте и
                // лишнем объекте, а таких файлов в пакете тысячи.
                let id = if self.fkids.is_empty() {
                    put(self.scap, &self.fbuf)?
                } else {
                    if !self.fbuf.is_empty() {
                        let id = put(self.scap, &self.fbuf)?;
                        self.fkids.push(id);
                    }
                    ty |= tree::F_BLOB;
                    let man = sys::http::blob_manifest(self.fsize as usize, self.fkids.len());
                    put_node(self.scap, &man, &self.fkids)?
                };
                self.fbuf.clear();
                self.fkids.clear();
                let (ty, name, size) = (ty, core::mem::take(&mut self.fname), self.fsize);
                self.attach(ty, &name, size, id);
                Ok(())
            })(),
        };
        r.map_err(void_nar::NarError)
    }
}

/// Последний компонент пути внутри архива (у корня — пусто).
fn leaf(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Скачанный архив → дерево в store. Возвращает (корень дерева, размер NAR, его sha256).
///
/// Байты идут одним потоком: блоб из store → распаковщик → разбор NAR → объекты store. Нигде
/// целиком не собирается ни архив, ни NAR, ни файл — только текущий кусок. Хэш считается по
/// дороге, потому что второго прохода по этим байтам не будет.
fn unpack_tree(scap: usize, dl_id: &[u8; 32], base: &str) -> Result<([u8; 32], usize, String), String> {
    let mut b = Builder::new(scap);
    let mut parser = void_nar::Parser::new();
    let mut hasher = Sha256::new();
    let mut nbytes = 0usize;

    archive::stream(scap, dl_id, &mut |chunk: &[u8]| {
        hasher.update(chunk);
        nbytes += chunk.len();
        parser.push(chunk, &mut |e| b.on(e)).map_err(|e| e.0)
    })?;
    parser.finish().map_err(|e| e.0)?;

    let (ty, id, size) = b.root.ok_or("в архиве нет корневого узла")?;
    // Корень пакета — индекс из ОДНОЙ записи с именем пути store. Так у пакета есть имя и тип
    // ровно там же, где у любой другой записи, а каталог `/nix/store` потом собирается склейкой
    // таких записей — без особого случая для верхнего уровня.
    let mut data = Vec::from(tree::head(1));
    tree::push(&mut data, ty, base.as_bytes(), size);
    let root = put_node(scap, &data, &[id])?;

    let digest: [u8; 32] = hasher.finalize().into();
    Ok((root, nbytes, nix_hash32(&digest)))
}

/// Метаданные пути: из своего кэша (`pkg/narinfo/<хэш>`), а если их там нет — из сети.
///
/// Подпись проверяется В ОБОИХ случаях. Кэш — наш собственный store, но «байты уже лежали у нас»
/// не является доводом в пользу их подлинности: проверка стоит миллисекунды, а исключение из неё
/// стоило бы всей гарантии.
fn narinfo(scap: usize, hash: &str) -> Result<NarInfo, String> {
    let root = format!("pkg/narinfo/{}", hash);
    let id = match root_id(scap, &root) {
        Some(id) => id,
        None => download(&format!("{}/{}.narinfo", CACHE, hash), &root)?,
    };
    let bytes = archive::unpacked(scap, &id, false)?;
    let text = core::str::from_utf8(&bytes).map_err(|_| "narinfo не UTF-8")?;
    let ni = parse_narinfo(text)?;

    // Тот ли это путь, который мы просили. Подпись удостоверяет ПУТЬ ИЗ САМОГО narinfo — без
    // этой сверки кэш мог бы на запрос про A вернуть честно подписанный B, и все проверки ниже
    // прошли бы успешно, просто не про то.
    if ni.base().len() < HASH_LEN || &ni.base()[..HASH_LEN] != hash {
        return Err(format!("кэш ответил про другой путь: просили {}, отдали {}", hash, ni.base()));
    }
    check_signature(&ni)?;
    Ok(ni)
}

/// Сколько времени заняли сеть и распаковка одного пути (секунды). Печатается рядом с путём:
/// на устройстве полезно видеть, во что упирается установка, — в канал или в свой же store.
struct Timing {
    net: u64,
    unpack: u64,
}

/// Один путь: метаданные → (если содержимого ещё нет) архив → распаковка → сверка NarHash → store.
/// `Some(...)` во втором поле — содержимое скачано сейчас, `None` — уже лежало.
fn realize_one(scap: usize, hash: &str) -> Result<(NarInfo, [u8; 32], Option<Timing>), String> {
    let ni = narinfo(scap, hash)?;

    let tree_root = format!("pkg/tree/{}", hash);
    if let Some(id) = root_id(scap, &tree_root) {
        return Ok((ni, id, None));
    }

    let dl_root = format!("pkg/dl/{}", hash);
    let t0 = sys::monotonic_ns();
    let dl_id = download(&format!("{}/{}", CACHE, ni.url), &dl_root)?;
    let t1 = sys::monotonic_ns();

    // Распаковка сразу в дерево store (Веха 108). Объекты при этом уже написаны, но НИ ОДИН
    // корень на них не указывает: пока хэш не сошёлся, дерева для системы не существует, а
    // недостижимые объекты подберёт GC. Порядок «проверить, потом показать» сохранён — просто
    // проверка теперь идёт по дороге, а не по второй копии в памяти.
    let (tree_id, size, got) = unpack_tree(scap, &dl_id, ni.base())?;
    if size != ni.nar_size {
        return Err(format!("размер NAR не сошёлся: обещано {}, получено {}", ni.nar_size, size));
    }
    if got != ni.nar_hash {
        return Err(format!("NarHash не сошёлся: обещано {}, получено {}", ni.nar_hash, got));
    }

    if sys::obj_set_root(scap, tree_root.as_bytes(), &tree_id) != 0 {
        return Err(String::from("корень дерева не завёлся"));
    }
    // Скачанный сжатый архив больше не нужен: он лишь транспорт, а хранить транспорт рядом с
    // содержимым значит платить за него местом дважды.
    sys::obj_del_root(scap, dl_root.as_bytes());
    let t2 = sys::monotonic_ns();
    let timing = Timing { net: (t1 - t0) / 1_000_000_000, unpack: (t2 - t1) / 1_000_000_000 };
    Ok((ni, tree_id, Some(timing)))
}

// ── чтение дерева пакета ────────────────────────────────────────────────────────────────────

/// Одна запись каталога в собственной памяти — итератор `tree` отдаёт ломти чужого буфера,
/// а буфер здесь живёт до конца вызова.
#[derive(Clone)]
struct Ent {
    ty: u8,
    name: String,
    size: u64,
}

/// Прочитать узел-каталог: его записи и content-id детей (по порядку записей).
fn read_dir(scap: usize, id: &[u8; 32]) -> Result<(Vec<Ent>, Vec<[u8; 32]>), String> {
    let data = archive::read_object(scap, id, 8 * 1024 * 1024)?;
    let it = tree::iter(&data).ok_or("это не каталог пакета")?;
    let recs: Vec<Ent> = it
        .map(|r| Ent {
            ty: r.ty,
            name: String::from_utf8_lossy(r.name).into_owned(),
            size: r.size,
        })
        .collect();
    let mut kids = vec![[0u8; 32]; recs.len()];
    if !recs.is_empty() && sys::obj_children(scap, id, &mut kids) != recs.len() {
        return Err(String::from("число ссылок каталога не сошлось с индексом"));
    }
    Ok((recs, kids))
}

/// Найти узел по пути внутри пакета: `(имя пакета, запись, id)`. Пустой путь — сам пакет.
fn resolve(scap: usize, hash: &str, sub: &str) -> Result<(String, Ent, [u8; 32]), String> {
    let root = root_id(scap, &format!("pkg/tree/{}", hash))
        .ok_or("пакета нет в store — сначала `pkg fetch`")?;
    // Корень — индекс из одной записи: сам путь store.
    let (recs, kids) = read_dir(scap, &root)?;
    let (mut rec, mut id) = (
        recs.into_iter().next().ok_or("пустой корень пакета")?,
        *kids.first().ok_or("пустой корень пакета")?,
    );
    let base = rec.name.clone();

    for part in sub.split('/').filter(|p| !p.is_empty() && *p != ".") {
        if !rec.is_dir() {
            return Err(format!("{} — не каталог", rec.name));
        }
        let (recs, kids) = read_dir(scap, &id)?;
        let i = recs
            .iter()
            .position(|r| r.name == part)
            .ok_or_else(|| format!("нет такого пути: {}", part))?;
        rec = recs[i].clone();
        id = kids[i];
    }
    Ok((base, rec, id))
}

/// Выдать содержимое файла кусками (`sink`) — целиком в память он не собирается.
fn read_file<F: FnMut(&[u8])>(
    scap: usize,
    rec: &Ent,
    id: &[u8; 32],
    mut sink: F,
) -> Result<(), String> {
    if !rec.is_blob() {
        sink(&archive::read_object(scap, id, 8 * 1024 * 1024)?);
        return Ok(());
    }
    let mut head = [0u8; 512];
    let n = sys::obj_get(scap, id, &mut head);
    let (_, nchunks, csize) =
        sys::http::blob_info(&head[..n.min(head.len())]).ok_or("испорченный манифест файла")?;
    let mut kids = vec![[0u8; 32]; nchunks];
    if sys::obj_children(scap, id, &mut kids) != nchunks {
        return Err(String::from("список кусков файла не сошёлся"));
    }
    let mut buf = vec![0u8; csize];
    for k in &kids {
        let n = sys::obj_get(scap, k, &mut buf);
        if n == 0 || n > buf.len() {
            return Err(String::from("кусок файла не читается"));
        }
        sink(&buf[..n]);
    }
    Ok(())
}

// ── индекс имён канала (Веха 111) ───────────────────────────────────────────────────────────

/// Индекс путей канала: текст по строке на путь, `/nix/store/<хэш>-<имя>`. Публикуется Hydra
/// рядом с самим каналом и перечисляет ровно то, что канал собрал, — то есть то, что лежит в
/// кэше и до чего мы вообще можем дотянуться.
///
/// Канал по умолчанию. «Откуда система берёт софт» — решение СИСТЕМЫ, а не аргумент команды,
/// поэтому командной строкой его сменить нельзя; с Вехи 113 его называет конфиг — строкой
/// `channel <url>` рядом с `packages` ([`channel`]).
const CHANNEL: &str = "https://channels.nixos.org/nixos-unstable";

/// Файл индекса внутри канала — часть его раскладки, а не настройка: канал, у которого этого
/// файла нет, каналом для нас не является.
const INDEX_FILE: &str = "store-paths.xz";

/// Корень, под которым индекс живёт. Блобом, а не объектом: распакованного текста 15 МиБ, и
/// целиком в памяти он не бывает — ни при записи, ни при поиске.
const INDEX_ROOT: &str = "pkg/index/paths";

/// Канал, ИЗ КОТОРОГО скачан лежащий индекс. Хранится рядом с ним, потому что индекс без этого
/// знания — просто список строк неизвестного происхождения: сменив канал в конфиге, человек
/// обязан узнать, что имена всё ещё резолвятся по старому.
const INDEX_CHANNEL_ROOT: &str = "pkg/index/channel";

/// Канал этой системы: что сказал конфиг активного поколения, иначе [`CHANNEL`].
fn channel(scap: usize) -> String {
    config_word(scap, "channel").unwrap_or_else(|| String::from(CHANNEL))
}

/// Первое слово строки `<вид> …` из конфига активного поколения системы.
///
/// Читать конфиг умеет и `vvsh`, но нести сюда его вычислитель незачем: поколение — уже
/// НОРМАЛИЗОВАННЫЙ текст, строки в нём разобраны, и `pkg` берёт свои две (`channel`,
/// `packages`) тем же способом, каким ядро берёт свои.
fn config_word(scap: usize, kind: &str) -> Option<String> {
    let gen = profile::system_current(scap)?;
    let text = system_config(scap, &gen).ok()?;
    for line in text.lines() {
        let mut it = line.split_whitespace();
        if it.next() == Some(kind) {
            if let Some(w) = it.next() {
                return Some(w.to_string());
            }
        }
    }
    None
}

/// Копилка кусков: распакованные байты уходят в store порциями [`FILE_CHUNK`], а в памяти
/// живёт ровно одна порция. Тот же приём, что у распаковки пакета ([`Builder`]).
struct ChunkWriter {
    scap: usize,
    buf: Vec<u8>,
    kids: Vec<[u8; 32]>,
    total: usize,
    lines: usize,
    told: usize,
}

impl ChunkWriter {
    fn new(scap: usize) -> Self {
        ChunkWriter { scap, buf: Vec::with_capacity(FILE_CHUNK), kids: Vec::new(), total: 0, lines: 0, told: 0 }
    }

    fn put(&mut self, mut data: &[u8]) -> Result<(), String> {
        self.total += data.len();
        self.lines += data.iter().filter(|&&b| b == b'\n').count();

        // Порция от распаковщика бывает В МЕГАБАЙТАХ (xz отдаёт всё, что вышло за словарь), и
        // складывать её целиком к себе значило бы вернуть ту самую память, ради экономии которой
        // распаковка и делалась потоковой. Поэтому полные куски берутся ПРЯМО ИЗ ПОРЦИИ, а у нас
        // задерживается только хвост короче куска.
        if !self.buf.is_empty() {
            let take = (FILE_CHUNK - self.buf.len()).min(data.len());
            self.buf.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buf.len() == FILE_CHUNK {
                let id = put(self.scap, &self.buf)?;
                self.kids.push(id);
                self.buf.clear();
            }
        }
        while data.len() >= FILE_CHUNK {
            let id = put(self.scap, &data[..FILE_CHUNK])?;
            self.kids.push(id);
            data = &data[FILE_CHUNK..];
        }
        self.buf.extend_from_slice(data);
        // Распаковка индекса небыстрая, а молчащая программа неотличима от повисшей.
        let mib = self.total / (4 * 1024 * 1024);
        if mib != self.told {
            self.told = mib;
            sys::write(format!("  … {} МиБ\n", mib * 4).as_bytes());
        }
        Ok(())
    }

    /// Дописать остаток и связать куски манифестом блоба.
    fn finish(mut self) -> Result<([u8; 32], usize, usize), String> {
        if !self.buf.is_empty() {
            let id = put(self.scap, &self.buf)?;
            self.kids.push(id);
        }
        let man = sys::http::blob_manifest(self.total, self.kids.len());
        let id = put_node(self.scap, &man, &self.kids)?;
        Ok((id, self.total, self.lines))
    }
}

/// Пройти по индексу, отдавая строки по одной. Читается кусками блоба, склейка разрезанных на
/// границе строк — через `carry`; целиком индекс в памяти не собирается никогда.
fn index_scan<F: FnMut(&str)>(scap: usize, mut f: F) -> Result<usize, String> {
    let id = root_id(scap, INDEX_ROOT).ok_or("индекса имён нет — сначала `pkg update`")?;
    warn_stale_channel(scap);
    let mut head = [0u8; 512];
    let n = sys::obj_get(scap, &id, &mut head);
    let (_, nchunks, csize) = sys::http::blob_info(&head[..n.min(head.len())])
        .ok_or("индекс имён не похож на блоб — перекачайте его `pkg update`")?;
    let mut kids = vec![[0u8; 32]; nchunks];
    if sys::obj_children(scap, &id, &mut kids) != nchunks {
        return Err(String::from("список кусков индекса не сошёлся"));
    }

    let mut buf = vec![0u8; csize];
    let mut carry: Vec<u8> = Vec::new();
    let mut lines = 0usize;
    for k in &kids {
        let n = sys::obj_get(scap, k, &mut buf);
        if n == 0 || n > buf.len() {
            return Err(String::from("кусок индекса не читается"));
        }
        let mut rest = &buf[..n];
        while let Some(p) = rest.iter().position(|&b| b == b'\n') {
            let (line, tail) = rest.split_at(p);
            rest = &tail[1..];
            if carry.is_empty() {
                if let Ok(s) = core::str::from_utf8(line) {
                    f(s);
                    lines += 1;
                }
            } else {
                carry.extend_from_slice(line);
                if let Ok(s) = core::str::from_utf8(&carry) {
                    f(s);
                    lines += 1;
                }
                carry.clear();
            }
        }
        carry.extend_from_slice(rest);
    }
    if !carry.is_empty() {
        if let Ok(s) = core::str::from_utf8(&carry) {
            f(s);
            lines += 1;
        }
    }
    Ok(lines)
}

/// Сказать вслух, если лежащий индекс скачан НЕ ИЗ ТОГО канала, который назвал конфиг.
///
/// Не ошибка: старый индекс — рабочий список имён, по нему всё установится и проверится
/// подписью. Но это уже не тот софт, который система объявила, и знать об этом человек обязан
/// раньше, чем удивится версии.
fn warn_stale_channel(scap: usize) {
    let want = channel(scap);
    let have = root_id(scap, INDEX_CHANNEL_ROOT)
        .and_then(|id| archive::read_object(scap, &id, 64 * 1024).ok())
        .and_then(|b| String::from_utf8(b).ok());
    match have {
        Some(h) if h.trim() == want => {}
        Some(h) => sys::write(
            format!("  индекс скачан из {} — конфиг называет {} (`pkg update`)\n", h.trim(), want)
                .as_bytes(),
        ),
        // Индекс от Вехи 111: канал тогда не записывался. Молчим — сказать нечего, а гадать
        // «наверняка тот самый» значило бы выдумать факт.
        None => {}
    }
}

/// Сколько кандидатов на одно имя мы готовы держать в памяти. `stdenv-linux` в канале
/// встречается 63 раза, так что запас нужен, но не безграничный.
const CANDS_MAX: usize = 512;

/// Все пути канала с данным именем пакета — в порядке предпочтения ([`nixname::cmp_pref`]).
fn candidates(scap: usize, query: &str) -> Result<Vec<String>, String> {
    Ok(candidates_multi(scap, &[query])?.remove(0))
}

/// То же для НЕСКОЛЬКИХ имён сразу — за один проход по индексу.
///
/// Проход стоит 15 МиБ чтения из store, и он же — вся цена резолва имени. Поэтому конфиг с тремя
/// пакетами (Веха 112) не должен стоить трёх проходов: имена независимы, а индекс один.
fn candidates_multi(scap: usize, queries: &[&str]) -> Result<Vec<Vec<String>>, String> {
    let mut out: Vec<Vec<String>> = vec![Vec::new(); queries.len()];
    let mut over: Option<usize> = None;
    index_scan(scap, |line| {
        let Some(base) = line.strip_prefix("/nix/store/") else { return };
        if base.as_bytes().get(HASH_LEN) != Some(&b'-') {
            return;
        }
        let pname = parts(base).0;
        for (i, q) in queries.iter().enumerate() {
            if pname != *q {
                continue;
            }
            if out[i].len() < CANDS_MAX {
                out[i].push(base.to_string());
            } else {
                over = Some(i);
            }
        }
    })?;

    if let Some(i) = over {
        return Err(format!(
            "у имени {} больше {} путей — назовите путь хэшем",
            queries[i], CANDS_MAX
        ));
    }
    // Устойчивая сортировка: равные кандидаты остаются в порядке индекса.
    for cands in out.iter_mut() {
        cands.sort_by(|a, b| nixname::cmp_pref(a, b));
    }
    Ok(out)
}

/// Найти путь по имени пакета.
///
/// Порядок отбора: сперва **своя архитектура** (по якорю, см. [`probe_arch`]) — это не
/// предпочтение, а условие пригодности; затем **свой выход важнее чужого** (`hello-2.12.3`
/// вместо `hello-2.12.3-doc`), **старшая версия важнее младшей** (сравнением nix), при полном
/// равенстве — **первый в индексе** (он отсортирован по хэшу, значит выбор воспроизводим, а не
/// «какой попался»). Сколько кандидатов отвергнуто — печатается, а не замалчивается.
fn resolve_name(scap: usize, query: &str) -> Result<Pick, String> {
    pick_from(scap, query, candidates(scap, query)?)
}

/// Выбор из уже собранных кандидатов — отдельно от их поиска, потому что искать их можно
/// по-разному (одно имя или пачку за один проход), а правило выбора обязано быть одним.
fn pick_from(scap: usize, query: &str, cands: Vec<String>) -> Result<Pick, String> {
    if cands.is_empty() {
        return Err(format!(
            "в индексе канала нет пакета {} — посмотрите `pkg search {}`",
            query, query
        ));
    }

    // Якоря может не быть: индекс скачан другой архитектурой (диск у них общий), или он от
    // Вехи 111, когда корень был один на всех. Тогда проверяем СЕЙЧАС — это дешевле, чем
    // выглядит: пробник обычно уже лежит в store, а платить за чужую ошибку молчаливым
    // «архитектура не проверена» неправильно.
    let anchor = ensure_anchor(scap)?;
    if anchor.as_deref() == Some(NO_ARCH) {
        return Err(format!(
            "канал не собирает под нашу архитектуру — имена не работают, ставьте по хэшу ({} путей у имени {})",
            cands.len(), query
        ));
    }

    // Отбор по архитектуре. Кандидаты перебираются в порядке предпочтения и берётся ПЕРВЫЙ
    // свой — поэтому в обычном случае это одна-две загрузки narinfo (2–3 КиБ), а не десяток.
    if let Some(libc) = &anchor {
        if cands.len() > 1 {
            for c in &cands {
                let ni = narinfo(scap, hash_of(c))?;
                if ni.references.iter().any(|r| r == libc) {
                    return Ok(Pick { base: c.clone(), total: cands.len(), arch: Arch::Ours });
                }
            }
            // Ни один не сослался на нашу libc. Так бывает у пакетов без libc вообще
            // (данные, статические сборки) — и тогда архитектура ни при чём.
            return Ok(Pick { base: cands[0].clone(), total: cands.len(), arch: Arch::NoLibc });
        }
    }

    let arch = if anchor.is_some() { Arch::Ours } else { Arch::Unknown };
    Ok(Pick { base: cands[0].clone(), total: cands.len(), arch })
}

/// Что известно про архитектуру выбранного пути.
enum Arch {
    /// Он сослался на нашу libc (или кандидат был один).
    Ours,
    /// Никто из кандидатов не ссылается на libc вообще — архитектура ни при чём.
    NoLibc,
    /// Якоря нет: `pkg update` не проверял архитектуру.
    Unknown,
}

/// Что выбрано по имени и из чего.
struct Pick {
    /// `<хэш>-<имя>` выбранного пути.
    base: String,
    /// Сколько всего путей у этого имени пакета было в канале.
    total: usize,
    arch: Arch,
}

// ── архитектура: якорь вместо гадания ───────────────────────────────────────────────────────

/// Имя своей архитектуры — часть имени корня якоря (см. [`libc_root`]).
#[cfg(target_arch = "riscv64")]
const ARCH: &str = "riscv64";
#[cfg(target_arch = "x86_64")]
const ARCH: &str = "x86_64";

/// Корень, где лежит «наша libc» — путь, по ссылке на который узнаётся своя сборка.
///
/// Имя корня **арх-именовано** (Веха 113), и это не аккуратность. Диск у архитектур один
/// (программы разведены как `bin/<arch>/*`), а якорь собирает `pkg update` на конкретной машине:
/// с общим корнем riscv-система читала якорь, снятый под x86, и печатала «первый своей
/// архитектуры», сверившись с чужой libc. Установку это не ломало (подпись и NarHash на месте,
/// чужой ELF просто не стартовал), но сообщение врало — а этого достаточно.
fn libc_root() -> String {
    format!("pkg/index/libc/{}", ARCH)
}

/// Значение якоря, когда своей сборки в канале НЕТ (например, riscv64 в nixos-unstable).
/// Отдельное значение, а не отсутствие корня: «проверяли и не нашли» и «не проверяли» — разные
/// вещи, и вторая не должна выглядеть как первая.
const NO_ARCH: &str = "-";

/// Пробник, которым система узнаёт свою сборку: GNU hello — самая маленькая настоящая программа
/// канала (294 КиБ NAR, 77 КиБ сжато) и при этом обычный динамический ELF со ссылкой на libc.
/// Ровно то, что нужно: дёшево скачать, есть что посмотреть внутри, есть за что зацепиться.
const PROBE: &str = "hello";
const PROBE_BIN: &str = "bin/hello";

/// `e_machine` нашей архитектуры (ELF spec): признак, который **нельзя подделать именем пути**.
#[cfg(target_arch = "riscv64")]
const OUR_MACHINE: u16 = 0xF3;
#[cfg(target_arch = "x86_64")]
const OUR_MACHINE: u16 = 0x3E;

/// Прочитать якорь: путь нашей libc, [`NO_ARCH`] или `None` (не проверяли).
fn read_anchor(scap: usize) -> Option<String> {
    let id = root_id(scap, &libc_root())?;
    let bytes = archive::read_object(scap, &id, 64 * 1024).ok()?;
    core::str::from_utf8(&bytes).ok().map(|s| s.trim().to_string())
}

/// Якорь СВОЕЙ архитектуры: прочитать, а если его нет — выяснить пробником и записать.
fn ensure_anchor(scap: usize) -> Result<Option<String>, String> {
    if let Some(a) = read_anchor(scap) {
        return Ok(Some(a));
    }
    sys::write(format!("якорь архитектуры {} не снят — проверяю пробником {}:\n", ARCH, PROBE).as_bytes());
    let anchor = probe_arch(scap)?;
    write_anchor(scap, &anchor)?;
    Ok(Some(anchor.unwrap_or_else(|| String::from(NO_ARCH))))
}

/// Записать якорь (или [`NO_ARCH`]) и сказать, что вышло.
fn write_anchor(scap: usize, anchor: &Option<String>) -> Result<(), String> {
    let value = anchor.clone().unwrap_or_else(|| String::from(NO_ARCH));
    let aid = put(scap, value.as_bytes())?;
    if sys::obj_set_root(scap, libc_root().as_bytes(), &aid) != 0 {
        return Err(String::from("корень якоря не завёлся"));
    }
    // Якорь Вехи 111 был один на все архитектуры. Снимаем его, а не оставляем «на всякий»:
    // корень, которому нельзя верить, хуже отсутствующего — по нему однажды кто-нибудь ответит.
    sys::obj_del_root(scap, b"pkg/index/libc");
    match anchor {
        Some(libc) => sys::write(format!("  своя libc: {}\n", libc).as_bytes()),
        None => sys::write(
            "  своей сборки в канале НЕТ — имена работать не будут, только хэши\n".as_bytes(),
        ),
    }
    Ok(())
}

/// `e_machine` из ELF-заголовка файла внутри пакета — или `None`, если это не 64-битный ELF.
fn elf_machine(scap: usize, hash: &str, sub: &str) -> Result<Option<u16>, String> {
    let (_, rec, id) = resolve(scap, hash, sub)?;
    if rec.is_dir() || rec.is_link() {
        return Ok(None);
    }
    let mut head = [0u8; 20];
    let mut got = 0usize;
    read_file(scap, &rec, &id, |part| {
        let n = part.len().min(head.len() - got);
        head[got..got + n].copy_from_slice(&part[..n]);
        got += n;
    })?;
    if got < 20 || &head[..4] != b"\x7fELF" || head[4] != 2 {
        return Ok(None);
    }
    Ok(Some(u16::from_le_bytes([head[18], head[19]])))
}

/// Узнать, какая сборка в канале — наша, и запомнить её libc.
///
/// Зачем это вообще нужно. Индекс канала перечисляет пути ВСЕХ архитектур вперемешку
/// (`nixos-unstable` — это x86_64 и aarch64), а имя пути про архитектуру не говорит ничего:
/// `hello-2.12.3` там ровно два, и различить их по строке нельзя. Проверено: на запрос `hello`
/// правило «свой выход, старшая версия, порядок индекса» выбирает `4z8ys…` — сборку под
/// aarch64, тогда как канал под x86_64 собрал `lxra5…`.
///
/// Врать про архитектуру нельзя, а спросить — некого: `.drv`, где стоит `system`, бинарный кэш
/// не хранит (404). Зато архитектуру видно в самом файле — в `e_machine`. Поэтому она узнаётся
/// ОДИН РАЗ, содержимым: качается пробник (77 КиБ), смотрится его ELF-заголовок, и запоминается
/// не «архитектура», а ссылка победителя на libc — по ней дальше отличается своя сборка любого
/// пакета, уже без единой загрузки содержимого.
fn probe_arch(scap: usize) -> Result<Option<String>, String> {
    let cands = candidates(scap, PROBE)?;
    if cands.is_empty() {
        return Err(format!("в индексе нет пробника {} — архитектуру не проверить", PROBE));
    }

    for c in &cands {
        let hash = hash_of(c);
        let (ni, _, _) = realize_one(scap, hash)?;
        // Внутри может не оказаться `bin/hello` вовсе: у имени `hello` в канале есть выходы
        // `-doc`/`-man`, и это не ошибка — просто не пробник. Раньше такой кандидат ронял всю
        // проверку архитектуры; на своей арх до него не доходили (свой находился вторым), а на
        // чужой — доходили сразу.
        let (machine, why) = match elf_machine(scap, hash, PROBE_BIN) {
            Ok(Some(m)) => (Some(m), format!("{:#x}", m)),
            Ok(None) => (None, String::from("не ELF")),
            Err(_) => (None, format!("нет {}", PROBE_BIN)),
        };
        if machine == Some(OUR_MACHINE) {
            // Якорь — единственная ссылка пробника, кроме него самого.
            let libc = ni.references.iter().find(|r| hash_of(r) != hash).cloned();
            sys::write(format!("  {} — наша сборка (e_machine {:#x})\n", c, OUR_MACHINE).as_bytes());
            return Ok(libc);
        }
        // Чужую сборку не оставляем занимать место: корень снят — GC подберёт.
        sys::obj_del_root(scap, format!("pkg/tree/{}", hash).as_bytes());
        sys::write(format!("  {} — не наша сборка ({})\n", c, why).as_bytes());
    }
    Ok(None)
}

/// Куда указывает аргумент: `(хэш пути, чего мы ждём от кэша)`.
///
/// Хэш узнаётся по виду, всё прочее считается именем и ищется в индексе. Разбор по виду, а не
/// по флагу, потому что человеку не должно быть важно, что он скопировал; спутать имя с хэшем
/// можно лишь имея имя из 32 знаков без `e`, `o`, `u` и `t`.
fn target(scap: usize, arg: &[u8]) -> Result<(String, Option<String>), String> {
    if let Ok(h) = path_hash(arg) {
        return Ok((h.to_string(), None));
    }
    let q = core::str::from_utf8(arg).map_err(|_| "аргумент не UTF-8")?;
    let pick = resolve_name(scap, q)?;
    report_pick(q, &pick);
    let hash = hash_of(&pick.base).to_string();
    Ok((hash, Some(pick.base)))
}

/// Сказать вслух, что имя во что превратилось и из скольких кандидатов. Печатается всегда:
/// выбор по имени — это решение системы за человека, и молчать о нём нельзя.
fn report_pick(query: &str, pick: &Pick) {
    sys::write(format!("{} → {}/{}\n", query, STORE_DIR, pick.base).as_bytes());
    if pick.total > 1 {
        let how = match pick.arch {
            Arch::Ours => "первый своей архитектуры",
            Arch::NoLibc => "ни один не ссылается на libc — архитектура ни при чём",
            Arch::Unknown => "архитектура НЕ ПРОВЕРЕНА: нет якоря, сделайте `pkg update`",
        };
        sys::write(format!("  (кандидатов в канале: {}; {})\n", pick.total, how).as_bytes());
    }
}

/// Спросить у кэша метаданные пути и убедиться, что индекс не соврал. Возвращает имя пути,
/// **как его называет подписанный narinfo**, — дальше по коду верить надо этому имени.
///
/// Проверка стоит до единого байта содержимого: если индекс подменён, узнать об этом надо
/// раньше, чем начнётся загрузка, а не после неё.
fn confirm(scap: usize, hash: &str, expect: Option<&str>) -> Result<String, String> {
    let ni = narinfo(scap, hash)?;
    if let Some(exp) = expect {
        if ni.base() != exp {
            return Err(format!(
                "индекс обещал {}, а подписанный кэш зовёт этот путь {} — индекс не от этого кэша",
                exp,
                ni.base()
            ));
        }
    }
    Ok(ni.base().to_string())
}

// ── замыкание ───────────────────────────────────────────────────────────────────────────────

/// Один путь замыкания: как он называется, где его содержимое и просил ли его человек сам.
struct Entry {
    base: String,
    id: [u8; 32],
    size: usize,
    top: bool,
}

/// Замыкание: сами пути плюс то, что стоит сказать человеку.
struct Closure {
    paths: Vec<Entry>,
    fetched: usize,
    bytes: usize,
}

/// Хэш из имени пути (`<хэш>-<имя>` или просто хэш).
fn hash_of(base: &str) -> &str {
    &base[..HASH_LEN.min(base.len())]
}

/// Скачать замыкание списка путей: сами пути и всё, на что они ссылаются, — транзитивно.
///
/// Обход в ширину с множеством уже виденного. `References` пути включает и его самого — это не
/// особый случай, множество и так не даёт зайти дважды. Пути, чьё содержимое уже в store,
/// не скачиваются повторно, но их метаданные всё равно читаются: **без ссылок нельзя идти
/// дальше**, а значит, кэш narinfo — не оптимизация, а часть механики.
fn realize(scap: usize, tops: &[String]) -> Result<Closure, String> {
    let mut queue: Vec<String> = tops.iter().map(|t| hash_of(t).to_string()).collect();
    let mut seen: Vec<String> = Vec::new();
    let mut paths: Vec<Entry> = Vec::new();
    let mut fetched = 0usize;
    let mut bytes = 0usize;

    let mut i = 0;
    while i < queue.len() {
        let hash = queue[i].clone();
        i += 1;
        if seen.iter().any(|h| *h == hash) {
            continue;
        }
        seen.push(hash.clone());

        let (ni, id, fresh) = realize_one(scap, &hash)?;
        let base = ni.base().to_string();
        let tail = match &fresh {
            Some(t) => format!("{} Б  (сеть {} с, распаковка {} с)", ni.nar_size, t.net, t.unpack),
            None => String::from("уже есть"),
        };
        sys::write(
            format!("  {} {}  {}\n", if fresh.is_some() { "↓" } else { "·" }, base, tail).as_bytes(),
        );
        if fresh.is_some() {
            fetched += 1;
        }
        bytes += ni.nar_size;
        paths.push(Entry { base, id, size: ni.nar_size, top: false });

        for r in &ni.references {
            let rh = hash_of(r).to_string();
            if !seen.iter().any(|h| *h == rh) {
                queue.push(rh);
            }
        }
    }

    // Верхний уровень помечается ПОСЛЕ обхода: путь из списка может встретиться раньше как
    // зависимость другого, и тогда пометка «по порядку очереди» досталась бы не ему.
    for e in paths.iter_mut() {
        if tops.iter().any(|t| hash_of(t) == hash_of(&e.base)) {
            e.top = true;
        }
    }
    Ok(Closure { paths, fetched, bytes })
}

/// Из аргумента вытащить 32-символьный хэш пути: принимаем и голый хэш, и полный путь store,
/// и просто имя каталога — человеку не должно быть важно, что он скопировал.
fn path_hash(arg: &[u8]) -> Result<&str, String> {
    let s = core::str::from_utf8(arg).map_err(|_| "аргумент не UTF-8")?;
    let base = s.rsplit('/').next().unwrap_or(s);
    if base.len() < HASH_LEN {
        return Err(String::from("не похоже на путь store (нужен хэш из 32 символов)"));
    }
    let hash = &base[..HASH_LEN];
    if !hash.bytes().all(|b| NIX32.contains(&b)) {
        return Err(String::from("в хэше символы не из алфавита nix-base32"));
    }
    Ok(hash)
}

// ── профиль ─────────────────────────────────────────────────────────────────────────────────

use profile::{current as read_current, gen_root, root_id, set_current};

/// Номера существующих поколений профиля, по возрастанию.
fn gen_numbers(scap: usize) -> Result<Vec<u32>, String> {
    let text = roots::text(scap).ok_or("список корней store не прочитать целиком")?;
    Ok(roots::gen_numbers(&text, format!("pkg/profile/{}/gen", PROFILE).as_bytes()))
}

/// Значение поколения: строки `top|dep <путь> <размер>` под заголовком [`GEN_MAGIC`].
///
/// Порядок строк совпадает с порядком исходящих ссылок узла — i-я строка описывает i-го ребёнка.
/// Ссылки нужны не для чтения (пути мы находим по корням `pkg/nar/*`), а для ДОСТИЖИМОСТИ: пока
/// поколение живо, GC store не тронет ни один путь его замыкания.
fn gen_text(paths: &[Entry]) -> String {
    let mut s = String::from(GEN_MAGIC);
    s.push('\n');
    for e in paths {
        s.push_str(if e.top { "top " } else { "dep " });
        s.push_str(&e.base);
        s.push(' ');
        s.push_str(&format!("{}\n", e.size));
    }
    s
}

use profile::read as read_gen;

/// Уложить замыкание узлом store: значение — текст поколения, дети — содержимое путей.
fn gen_node(scap: usize, c: &Closure) -> Result<[u8; 32], String> {
    let text = gen_text(&c.paths);
    let kids: Vec<[u8; 32]> = c.paths.iter().map(|e| e.id).collect();
    let mut id = [0u8; 32];
    if sys::obj_put_node(scap, text.as_bytes(), &kids, &mut id) != 0 {
        return Err(String::from("поколение профиля не влезло в store"));
    }
    Ok(id)
}

/// Записать поколение под ЗАДАННЫМ корнем, без всякого указателя. `false` — содержимое там уже
/// ровно это (узел содержательно адресуем, сравнение точное).
///
/// Так живёт системный профиль (Веха 112): его поколение называет конфиг, а активное выбирает
/// `system/current` — своего указателя ему не нужно.
fn write_gen_at(scap: usize, root: &str, c: &Closure) -> Result<bool, String> {
    let id = gen_node(scap, c)?;
    if root_id(scap, root) == Some(id) {
        return Ok(false);
    }
    if sys::obj_set_root(scap, root.as_bytes(), &id) != 0 {
        return Err(String::from("корень поколения не завёлся"));
    }
    Ok(true)
}

/// Записать новое поколение и сделать его активным. `false` во втором поле — содержимое совпало с
/// активным поколением, нового не завели (та же защита от пустых поколений, что у `rebuild`).
fn write_gen(scap: usize, c: &Closure) -> Result<(u32, bool), String> {
    let id = gen_node(scap, c)?;

    if let Some(cur) = read_current(scap) {
        if root_id(scap, &gen_root(cur)) == Some(id) {
            return Ok((cur, false)); // содержимое то же — узел содержательно адресуем, сравнение точное
        }
    }

    let nums = gen_numbers(scap)?;
    let n = nums.last().copied().unwrap_or(0) + 1;
    if sys::obj_set_root(scap, gen_root(n).as_bytes(), &id) != 0 {
        return Err(String::from("корень поколения не завёлся"));
    }
    set_current(scap, n)?;
    Ok((n, true))
}

/// Пути верхнего уровня активного поколения (пусто, если профиля ещё нет).
fn current_tops(scap: usize) -> Result<Vec<String>, String> {
    let Some(cur) = read_current(scap) else { return Ok(Vec::new()) };
    Ok(read_gen(scap, cur)?.into_iter().filter(|i| i.top).map(|i| i.base).collect())
}

/// «1 путь», «2 пути», «5 путей». Мелочь, но эти строки читает человек, а не грепалка.
fn paths_word(n: usize) -> &'static str {
    let (ones, tens) = (n % 10, n % 100);
    if ones == 1 && tens != 11 {
        "путь"
    } else if (2..=4).contains(&ones) && !(12..=14).contains(&tens) {
        "пути"
    } else {
        "путей"
    }
}

impl Ent {
    fn is_dir(&self) -> bool {
        tree::is_dir(self.ty)
    }
    fn is_link(&self) -> bool {
        tree::is_link(self.ty)
    }
    fn is_exec(&self) -> bool {
        tree::is_exec(self.ty)
    }
    fn is_blob(&self) -> bool {
        tree::is_blob(self.ty)
    }
}

/// «1 запись», «2 записи», «5 записей».
fn recs_word(n: usize) -> &'static str {
    let (ones, tens) = (n % 10, n % 100);
    if ones == 1 && tens != 11 {
        "запись"
    } else if (2..=4).contains(&ones) && !(12..=14).contains(&tens) {
        "записи"
    } else {
        "записей"
    }
}

/// Совпадает ли путь с тем, что назвал человек: полное имя, хэш или просто имя пакета.
fn matches(base: &str, what: &str) -> bool {
    base == what
        || hash_of(base) == what
        || base.get(HASH_LEN + 1..).is_some_and(|name| name == what)
}

// ── команды ─────────────────────────────────────────────────────────────────────────────────

/// `pkg update` — забрать индекс имён канала.
///
/// Скачанное сжатое уезжает сразу в распаковку и в куски store, минуя память: 5,4 МиБ архива
/// разворачиваются в 14,7 МиБ текста, и «сначала в буфер, потом разберём» здесь не помещается
/// ни на одной из наших машин.
fn cmd_update() -> Result<(), String> {
    let scap = sys::start_cap(1);
    let dl_root = "pkg/dl/index";
    let chan = channel(scap);
    let url = format!("{}/{}", chan.trim_end_matches('/'), INDEX_FILE);

    sys::write(format!("индекс канала: {}\n", url).as_bytes());
    let t0 = sys::monotonic_ns();
    let dl_id = download(&url, dl_root)?;
    let t1 = sys::monotonic_ns();

    let (id, total, lines) = {
        let mut w = ChunkWriter::new(scap);
        archive::stream(scap, &dl_id, &mut |chunk: &[u8]| w.put(chunk))?;
        w.finish()?
    };
    let t2 = sys::monotonic_ns();

    if sys::obj_set_root(scap, INDEX_ROOT.as_bytes(), &id) != 0 {
        return Err(String::from("корень индекса не завёлся"));
    }
    // Откуда он — записывается ВМЕСТЕ с ним: иначе после смены канала в конфиге система резолвит
    // имена по старому индексу и молчит об этом.
    let cid = put(scap, chan.as_bytes())?;
    if sys::obj_set_root(scap, INDEX_CHANNEL_ROOT.as_bytes(), &cid) != 0 {
        return Err(String::from("корень канала не завёлся"));
    }
    // Сжатый архив — транспорт, и хранить его рядом с распакованным незачем.
    sys::obj_del_root(scap, dl_root.as_bytes());

    sys::write(
        format!(
            "индекс: {} {}, {} Б (сеть {} с, распаковка {} с)\n",
            lines,
            paths_word(lines),
            total,
            (t1 - t0) / 1_000_000_000,
            (t2 - t1) / 1_000_000_000
        )
        .as_bytes(),
    );

    // Индекс перечисляет пути всех архитектур вперемешку, а имя про архитектуру молчит. Пока
    // не выяснено, какая сборка наша, имена бесполезны — поэтому проверка идёт здесь же, а не
    // откладывается до первой установки (см. [`probe_arch`]).
    sys::write(format!("архитектура {} (пробник {}):\n", ARCH, PROBE).as_bytes());
    let anchor = probe_arch(scap)?;
    write_anchor(scap, &anchor)
}

/// Сколько имён печатает `pkg search`, прежде чем сдаться: на `python` их тысячи, а экран один.
const SEARCH_MAX: usize = 40;

/// `pkg search <строка>` — какие имена индекса на неё похожи.
///
/// Печатается хэш, а не только имя: строку из вывода должно быть можно скормить `pkg install`
/// как есть — в том числе когда выбор по имени не тот, который нужен человеку.
fn cmd_search(what: &[u8]) -> Result<(), String> {
    let scap = sys::start_cap(1);
    let q = core::str::from_utf8(what).map_err(|_| "аргумент не UTF-8")?;

    // (имя, хэш первого пути, сколько путей) — по одной строке на ИМЯ, а не на путь: в канале
    // 214 тысяч путей на 104 тысячи имён, и повторы имени человеку ничего не говорят.
    let mut hits: Vec<(String, String, usize)> = Vec::new();
    let mut total = 0usize;
    index_scan(scap, |line| {
        let Some(base) = line.strip_prefix("/nix/store/") else { return };
        let Some(name) = base.get(HASH_LEN + 1..) else { return };
        if !name.contains(q) {
            return;
        }
        total += 1;
        match hits.iter_mut().find(|(n, _, _)| n == name) {
            Some(h) => h.2 += 1,
            None => {
                if hits.len() < SEARCH_MAX * 8 {
                    hits.push((name.to_string(), hash_of(base).to_string(), 1));
                }
            }
        }
    })?;

    if hits.is_empty() {
        sys::write(format!("в индексе канала ничего похожего на {}\n", q).as_bytes());
        return Ok(());
    }
    hits.sort_unstable();

    sys::write(
        format!("{}: {} {} на {} имён\n", q, total, paths_word(total), hits.len()).as_bytes(),
    );
    for (name, hash, n) in hits.iter().take(SEARCH_MAX) {
        let more = if *n > 1 { format!("  (+{})", n - 1) } else { String::new() };
        sys::write(format!("  {}  {}{}\n", hash, name, more).as_bytes());
    }
    if hits.len() > SEARCH_MAX {
        sys::write(format!("  … и ещё {} имён\n", hits.len() - SEARCH_MAX).as_bytes());
    }
    Ok(())
}

fn cmd_fetch(what: &[u8]) -> Result<(), String> {
    let scap = sys::start_cap(1);
    let (hash, expect) = target(scap, what)?;
    confirm(scap, &hash, expect.as_deref())?;
    let c = realize(scap, &[hash])?;
    report(&c);
    Ok(())
}

fn report(c: &Closure) {
    sys::write(
        format!(
            "замыкание: {} {}, {} Б (скачано сейчас: {})\n",
            c.paths.len(),
            paths_word(c.paths.len()),
            c.bytes,
            c.fetched
        )
        .as_bytes(),
    );
}

fn cmd_install(what: &[u8]) -> Result<(), String> {
    let scap = sys::start_cap(1);
    let (hash, expect) = target(scap, what)?;
    // Имя пути берём у подписанного narinfo, а не у индекса и не у аргумента: дальше по нему
    // решается, что из профиля вытеснить, — а такое решение нельзя принимать по чужому слову.
    let base = confirm(scap, &hash, expect.as_deref())?;
    let pname = parts(&base).0.to_string();

    // Поколение — это всегда замыкание СПИСКА ВЕРХНЕГО УРОВНЯ, а не «прошлое поколение плюс
    // новое». Отсюда симметрия: установка и удаление — одна и та же пересборка, разница лишь в
    // том, что делает со списком.
    let mut tops = current_tops(scap)?;
    // Пакет с тем же ИМЕНЕМ вытесняется, а не встаёт рядом (Веха 111): иначе `pkg install hello`
    // после смены канала оставил бы в PATH два `hello` разных версий, и какой из них запустится,
    // зависело бы от порядка перебора. Ровно так же поступает `nix-env`.
    let mut replaced: Option<String> = None;
    tops.retain(|t| {
        if hash_of(t) == hash || parts(t).0 != pname {
            return true;
        }
        replaced = Some(t.clone());
        false
    });
    if !tops.iter().any(|t| hash_of(t) == hash) {
        tops.push(base);
    }
    if let Some(old) = &replaced {
        sys::write(format!("вытеснен {}\n", old).as_bytes());
    }

    let c = realize(scap, &tops)?;
    report(&c);
    let (n, fresh) = write_gen(scap, &c)?;
    if fresh {
        sys::write(format!("профиль {}: поколение gen{}\n", PROFILE, n).as_bytes());
    } else {
        sys::write(format!("профиль {}: без изменений (gen{})\n", PROFILE, n).as_bytes());
    }
    Ok(())
}

fn cmd_remove(what: &[u8]) -> Result<(), String> {
    let scap = sys::start_cap(1);
    let what = core::str::from_utf8(what).map_err(|_| "аргумент не UTF-8")?;

    let tops = current_tops(scap)?;
    let kept: Vec<String> = tops.iter().filter(|t| !matches(t, what)).cloned().collect();
    if kept.len() == tops.len() {
        return Err(format!("в профиле нет пакета {}", what));
    }

    // Ни одной загрузки: всё, что остаётся, уже лежит в store — пересборка только пересчитывает
    // замыкание. Пути, выпавшие из него, остаются в store до сборки мусора: поколения, которые на
    // них ссылаются, ещё живы, и откат обязан их найти.
    let c = realize(scap, &kept)?;
    let (n, fresh) = write_gen(scap, &c)?;
    if fresh {
        sys::write(
            format!(
                "профиль {}: поколение gen{} ({} {})\n",
                PROFILE, n, c.paths.len(), paths_word(c.paths.len())
            )
            .as_bytes(),
        );
    } else {
        sys::write(format!("профиль {}: без изменений (gen{})\n", PROFILE, n).as_bytes());
    }
    Ok(())
}

// ── декларативные пакеты: конфиг → системный профиль (Веха 112) ─────────────────────────────

/// Нормализованный конфиг поколения системы (значение корня `system/gen<N>`).
fn system_config(scap: usize, gen: &str) -> Result<String, String> {
    let root = format!("system/{}", gen);
    let id = root_id(scap, &root).ok_or_else(|| format!("поколения {} нет в store", root))?;
    let bytes = archive::read_object(scap, &id, 4 * 1024 * 1024)?;
    String::from_utf8(bytes).map_err(|_| String::from("конфиг поколения не UTF-8"))
}

/// Имена, объявленные конфигом: строки `packages имя…`, в порядке появления, без повторов.
///
/// Записей `packages` в конфиге может быть сколько угодно — их приносят разные модули, ровно как
/// `service`. Объявленное множество — их объединение.
fn declared_packages(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in void_conf::of(text, "packages").flat_map(|e| e.words()) {
        if !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
    }
    out
}

/// Объявленные имена → пути store, подтверждённые подписанным narinfo.
///
/// Индекс читается ОДИН раз на весь список: проход по нему — вся цена резолва, а имена друг о
/// друге ничего не знают.
fn resolve_declared(scap: usize, names: &[String]) -> Result<Vec<String>, String> {
    // Хэш искать в индексе незачем — он уже адрес. Разбор тот же, что у [`target`].
    let queries: Vec<&str> = names
        .iter()
        .filter(|n| path_hash(n.as_bytes()).is_err())
        .map(|s| s.as_str())
        .collect();
    let mut found = if queries.is_empty() {
        Vec::new()
    } else {
        candidates_multi(scap, &queries)?
    };

    let mut tops = Vec::new();
    for name in names {
        let (hash, expect) = match path_hash(name.as_bytes()) {
            Ok(h) => (h.to_string(), None),
            Err(_) => {
                let i = queries
                    .iter()
                    .position(|q| *q == name.as_str())
                    .ok_or_else(|| format!("имя {} потерялось при резолве", name))?;
                let pick = pick_from(scap, name, core::mem::take(&mut found[i]))?;
                report_pick(name, &pick);
                (hash_of(&pick.base).to_string(), Some(pick.base))
            }
        };
        tops.push(confirm(scap, &hash, expect.as_deref())?);
    }
    Ok(tops)
}

/// `pkg sync` — привести системный профиль в соответствие с активным поколением конфига.
///
/// Зовётся из `rebuild` (Веха 112) и вручную. Идемпотентна: если объявленное уже собрано для
/// этого поколения, не качается и не пишется ничего.
fn cmd_sync() -> Result<(), String> {
    let scap = sys::start_cap(1);
    let gen = profile::system_current(scap)
        .ok_or("система ещё не собрана — сначала `rebuild`")?;
    let names = declared_packages(&system_config(scap, &gen)?);
    let root = profile::system_gen_root(&gen);

    // «Не объявлено ни одного» — тоже ответ, и он ЗАПИСЫВАЕТСЯ пустым поколением. Иначе
    // «конфиг не просил пакетов» выглядело бы точно как «sync ещё не отрабатывал», а это разные
    // вещи: во втором случае система не выполнила обещание и должна об этом сказать.
    if names.is_empty() {
        let empty = Closure { paths: Vec::new(), fetched: 0, bytes: 0 };
        write_gen_at(scap, &root, &empty)?;
        sys::write(format!("пакеты {}: конфиг не объявляет ни одного\n", gen).as_bytes());
        return Ok(());
    }

    sys::write(format!("пакеты {}: объявлено {}\n", gen, names.len()).as_bytes());
    let tops = resolve_declared(scap, &names)?;

    // Сверка ДО пересборки: `rebuild` зовёт `sync` каждый раз, и обычный случай — «всё уже так».
    // Замыкание — функция от списка верхнего уровня, поэтому совпадения списков достаточно.
    if let Ok(items) = profile::read_root(scap, &root) {
        let have: Vec<&String> = items.iter().filter(|i| i.top).map(|i| &i.base).collect();
        if have.len() == tops.len() && tops.iter().all(|t| have.iter().any(|h| *h == t)) {
            sys::write(format!("пакеты {}: без изменений\n", gen).as_bytes());
            return Ok(());
        }
    }

    let c = realize(scap, &tops)?;
    report(&c);
    write_gen_at(scap, &root, &c)?;
    sys::write(
        format!("пакеты {}: собрано, {} {}\n", gen, c.paths.len(), paths_word(c.paths.len()))
            .as_bytes(),
    );
    Ok(())
}

/// Корни поколений системного профиля (`pkg/profile/system/*`).
fn system_gen_roots(scap: usize) -> Result<Vec<String>, String> {
    let text = roots::text(scap).ok_or("список корней store не прочитать целиком")?;
    let prefix = format!("pkg/profile/{}/", profile::SYSTEM);
    Ok(roots::suffixes(&text, prefix.as_bytes())
        .into_iter()
        .filter_map(|s| core::str::from_utf8(s).ok())
        .map(|s| format!("{}{}", prefix, s))
        .collect())
}

fn cmd_list() -> Result<(), String> {
    let scap = sys::start_cap(1);
    list_system(scap);

    let Some(cur) = read_current(scap) else {
        sys::write(format!("профиль {} пуст — `pkg install <путь>`\n", PROFILE).as_bytes());
        return Ok(());
    };
    let paths = read_gen(scap, cur)?;
    show_gen(&format!("профиль {} — поколение gen{}", PROFILE, cur), &paths);
    Ok(())
}

/// Что объявляет конфиг активного поколения системы. Отдельная секция, потому что это ДРУГАЯ
/// вещь: не «что я поставил», а «что система обещает иметь».
fn list_system(scap: usize) {
    let Some(gen) = profile::system_current(scap) else { return };
    match profile::read_root(scap, &profile::system_gen_root(&gen)) {
        Ok(items) if items.is_empty() => {
            sys::write(format!("профиль {} ({}) — конфиг не объявляет пакетов\n", profile::SYSTEM, gen).as_bytes());
        }
        Ok(items) => show_gen(&format!("профиль {} — поколение системы {}", profile::SYSTEM, gen), &items),
        // Корня нет — про это поколение `sync` не отрабатывал. Хорошо это или плохо, знает
        // только конфиг: если он ничего не объявлял, всё в порядке; если объявлял — система не
        // выполнила собственное обещание, и молчать об этом нельзя.
        Err(_) => {
            let asked = system_config(scap, &gen).map(|t| declared_packages(&t)).unwrap_or_default();
            let msg = if asked.is_empty() {
                format!("профиль {} ({}) — конфиг не объявляет пакетов\n", profile::SYSTEM, gen)
            } else {
                format!(
                    "профиль {} ({}) НЕ СОБРАН: конфиг просит {} — `pkg sync`\n",
                    profile::SYSTEM,
                    gen,
                    asked.join(", ")
                )
            };
            sys::write(msg.as_bytes());
        }
    }
}

/// Печать одного поколения: верхний уровень, зависимости, итог.
fn show_gen(title: &str, paths: &[profile::Item]) {
    let total: usize = paths.iter().map(|i| i.size).sum();
    sys::write(format!("{}\n", title).as_bytes());
    for i in paths.iter().filter(|i| i.top) {
        sys::write(format!("  {}\n", i.base).as_bytes());
    }
    let deps: Vec<&String> = paths.iter().filter(|i| !i.top).map(|i| &i.base).collect();
    if !deps.is_empty() {
        sys::write(format!("  зависимости ({}):\n", deps.len()).as_bytes());
        for d in deps {
            sys::write(format!("    {}\n", d).as_bytes());
        }
    }
    sys::write(format!("  всего {} {}, {} Б\n", paths.len(), paths_word(paths.len()), total).as_bytes());
}

/// `pkg tree <путь> [подпуть]` — что лежит внутри распакованного пакета.
fn cmd_tree(what: &[u8], sub: &[u8]) -> Result<(), String> {
    let scap = sys::start_cap(1);
    let hash = path_hash(what)?;
    let sub = core::str::from_utf8(sub).map_err(|_| "подпуть не UTF-8")?;
    let (base, rec, id) = resolve(scap, hash, sub)?;

    if !rec.is_dir() {
        let what = if rec.is_link() { "симлинк" } else { "файл" };
        sys::write(format!("{} — {}, {} Б\n", rec.name, what, rec.size).as_bytes());
        if rec.is_link() {
            let target = archive::read_object(scap, &id, 64 * 1024)?;
            sys::write(format!("  → {}\n", String::from_utf8_lossy(&target)).as_bytes());
        }
        return Ok(());
    }

    let (recs, _) = read_dir(scap, &id)?;
    let sep = if sub.is_empty() { "" } else { "/" };
    sys::write(
        format!(
            "{}/{}{}{} — {} {}\n",
            STORE_DIR, base, sep, sub, recs.len(), recs_word(recs.len())
        )
        .as_bytes(),
    );
    for r in &recs {
        // Пометка вида слева — как в `ls -F`, только явным столбцом: тип у нас лежит в записи
        // каталога, а не угадывается по имени.
        let mark = if r.is_dir() {
            "кат "
        } else if r.is_link() {
            "лнк "
        } else if r.is_exec() {
            "исп "
        } else {
            "фйл "
        };
        sys::write(format!("  {}{:>10}  {}\n", mark, r.size, r.name).as_bytes());
    }
    Ok(())
}

/// `pkg cat <путь> <подпуть>` — содержимое файла из пакета.
fn cmd_cat(what: &[u8], sub: &[u8]) -> Result<(), String> {
    let scap = sys::start_cap(1);
    let hash = path_hash(what)?;
    let sub = core::str::from_utf8(sub).map_err(|_| "подпуть не UTF-8")?;
    let (_, rec, id) = resolve(scap, hash, sub)?;
    if rec.is_dir() {
        return Err(format!("{} — каталог", rec.name));
    }
    read_file(scap, &rec, &id, |part| sys::write(part))
}

fn cmd_gens() -> Result<(), String> {
    let scap = sys::start_cap(1);
    let nums = gen_numbers(scap)?;
    let cur = read_current(scap);
    sys::write(format!("поколения профиля {} (активно — *):\n", PROFILE).as_bytes());
    if nums.is_empty() {
        sys::write("  (пусто — `pkg install <путь>`)\n".as_bytes());
    }
    for n in nums {
        let count = read_gen(scap, n).map(|p| p.len()).unwrap_or(0);
        sys::write(
            format!(
                "  gen{}{}  {} {}\n",
                n,
                if cur == Some(n) { " *" } else { "  " },
                count,
                paths_word(count)
            )
            .as_bytes(),
        );
    }

    // Системный профиль (Веха 112): поколения называет СИСТЕМА, активное выбирает не `pkg`, а
    // `system/current`. Поэтому и звёздочка тут стоит по другому основанию.
    let active = profile::system_current(scap);
    let mut roots = system_gen_roots(scap)?;
    roots.sort();
    if !roots.is_empty() {
        sys::write(
            format!("поколения профиля {} (активно — то, что выбрала система):\n", profile::SYSTEM)
                .as_bytes(),
        );
        for r in roots {
            let name = r.rsplit('/').next().unwrap_or("").to_string();
            let count = profile::read_root(scap, &r).map(|p| p.len()).unwrap_or(0);
            sys::write(
                format!(
                    "  {}{}  {} {}\n",
                    name,
                    if active.as_deref() == Some(name.as_str()) { " *" } else { "  " },
                    count,
                    paths_word(count)
                )
                .as_bytes(),
            );
        }
    }
    Ok(())
}

/// `pkg gc` — снять корни путей, которых нет НИ В ОДНОМ поколении профиля, и собрать мусор.
///
/// Модель та же, что у nix: корни — только у профилей, всё прочее рано или поздно уходит.
/// Поколения при этом неприкосновенны: пока живо старое поколение, живы и его пути — иначе
/// откат перестал бы работать, а он и есть главное свойство профиля.
fn cmd_gc(drop_old: bool) -> Result<(), String> {
    let scap = sys::start_cap(1);

    // `all` — снести все поколения, кроме активного. Отдельной командой и по явной просьбе:
    // после этого ОТКАТЫВАТЬСЯ НЕКУДА, а откат — главное свойство профиля.
    if drop_old {
        let active = read_current(scap);
        for n in gen_numbers(scap)? {
            if Some(n) == active {
                continue;
            }
            sys::obj_del_root(scap, gen_root(n).as_bytes());
            sys::write(format!("  поколение gen{} снято\n", n).as_bytes());
        }
        // То же для системного профиля: остаётся собранное для АКТИВНОГО поколения системы.
        // Значит, откат системы на старое поколение после `gc all` вернёт её пакеты не сразу —
        // понадобится `pkg sync` (и сеть, если содержимое успело уйти). Это и есть цена `all`.
        let sys_active = profile::system_current(scap).map(|g| profile::system_gen_root(&g));
        for r in system_gen_roots(scap)? {
            if Some(&r) == sys_active.as_ref() {
                continue;
            }
            sys::obj_del_root(scap, r.as_bytes());
            sys::write(format!("  поколение {} снято\n", r).as_bytes());
        }
    }

    // Что держат ВСЕ оставшиеся поколения (не только активное).
    fn hold(items: Vec<profile::Item>, keep: &mut Vec<String>) {
        for item in items {
            let h = hash_of(&item.base).to_string();
            if !keep.contains(&h) {
                keep.push(h);
            }
        }
    }
    let mut keep: Vec<String> = Vec::new();
    for n in gen_numbers(scap)? {
        hold(read_gen(scap, n).unwrap_or_default(), &mut keep);
    }
    // Объявленное конфигом держится наравне с поставленным руками — иначе `gc` сносил бы то,
    // что система обещает иметь, и первый же откат системы упёрся бы в пустоту.
    for r in system_gen_roots(scap)? {
        hold(profile::read_root(scap, &r).unwrap_or_default(), &mut keep);
    }

    // Что лежит в сторе.
    let text = roots::text(scap).ok_or("список корней store не прочитать целиком")?;
    let mut dropped = 0usize;
    for suffix in roots::suffixes(&text, b"pkg/tree/") {
        let Ok(hash) = core::str::from_utf8(suffix) else { continue };
        if hash.len() != HASH_LEN || keep.iter().any(|k| k == hash) {
            continue;
        }
        sys::obj_del_root(scap, format!("pkg/tree/{}", hash).as_bytes());
        // Метаданные пути больше не нужны: без дерева они ничего не описывают.
        sys::obj_del_root(scap, format!("pkg/narinfo/{}", hash).as_bytes());
        sys::write(format!("  снят {}\n", hash).as_bytes());
        dropped += 1;
    }

    // Снятый корень сам по себе места не возвращает — по графу должен кто-то пройти.
    let collected = sys::obj_gc(scap);
    if collected == usize::MAX {
        return Err(String::from("сборка мусора отклонена (нужен store WRITE)"));
    }
    sys::write(
        format!(
            "снято путей: {}, собрано объектов: {} (в поколениях осталось {})\n",
            dropped,
            collected,
            keep.len()
        )
        .as_bytes(),
    );
    Ok(())
}

fn cmd_rollback() -> Result<(), String> {
    let scap = sys::start_cap(1);
    let cur = read_current(scap).ok_or("профиля ещё нет — откатывать нечего")?;
    let nums = gen_numbers(scap)?;
    let prev = nums.iter().rev().find(|&&n| n < cur).copied().ok_or_else(|| {
        format!("gen{} — самое старое поколение профиля {}", cur, PROFILE)
    })?;

    // Откат — смена ОДНОГО указателя: содержимое обоих поколений и так лежит в store, а его
    // достижимость держат сами узлы поколений. Ни одной загрузки, ни одной распаковки.
    set_current(scap, prev)?;
    let count = read_gen(scap, prev).map(|p| p.len()).unwrap_or(0);
    sys::write(
        format!(
            "профиль {}: gen{} → gen{} ({} {})\n",
            PROFILE, cur, prev, count, paths_word(count)
        )
        .as_bytes(),
    );
    Ok(())
}
