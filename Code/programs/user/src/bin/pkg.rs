//! `pkg` — пакеты из бинарного кэша nixpkgs (Вехи 106–107, Фаза 8, [[0009-ondevice-packages]]).
//!
//! ```text
//! pkg fetch <путь>     скачать ЗАМЫКАНИЕ пути (сам путь и все зависимости)
//! pkg install <путь>   то же + новое поколение профиля с этим пакетом
//! pkg remove <имя>     новое поколение профиля без пакета
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

// Куча: распакованный NAR целиком плюс окно словаря распаковщика. Арена ленивая (`SYS_MAP`),
// неиспользованные страницы не стоят ничего.
#[global_allocator]
static ALLOC: sys::heap::Heap<{ 32 * 1024 * 1024 }> = sys::heap::Heap::new();

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

/// Длина хэша пути store в символах.
const HASH_LEN: usize = 32;

/// Имя профиля. Профиль пока один; множественность — это ровно другой префикс корня, поэтому
/// имя вынесено в константу, а не размазано по коду.
const PROFILE: &str = "default";

/// Заголовок значения поколения профиля — версия формата, чтобы будущий разбор мог отличить
/// своё от чужого, а не гадать по первой строке.
const GEN_MAGIC: &str = "void-profile 1";

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let mut argbuf = [0u8; 512];
    let n = sys::args(&mut argbuf);
    let mut it = argbuf[..n].split(|&b| b == 0).filter(|s| !s.is_empty()).skip(1);
    let (cmd, rest) = (it.next(), it.next());

    let code = match (cmd, rest) {
        (Some(b"fetch"), Some(what)) => done(cmd_fetch(what)),
        (Some(b"install"), Some(what)) => done(cmd_install(what)),
        (Some(b"remove"), Some(what)) => done(cmd_remove(what)),
        (Some(b"list"), _) => done(cmd_list()),
        (Some(b"gens"), _) => done(cmd_gens()),
        (Some(b"rollback"), _) => done(cmd_rollback()),
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
    sys::write("  fetch <хэш|/nix/store/путь>   скачать замыкание (путь и все зависимости)\n".as_bytes());
    sys::write("  install <хэш|путь>            скачать и внести в профиль (новое поколение)\n".as_bytes());
    sys::write("  remove <имя|хэш>              убрать из профиля (новое поколение)\n".as_bytes());
    sys::write("  list                          что в активном поколении профиля\n".as_bytes());
    sys::write("  gens                          поколения профиля (активное — *)\n".as_bytes());
    sys::write("  rollback                      вернуть предыдущее поколение\n".as_bytes());
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
    if sys::exec_args(sys::start_cap(1), b"httpsc", &argv) != 0 {
        return Err(format!("не скачалось: {}", url));
    }
    let mut id = [0u8; 32];
    if sys::obj_get_root(sys::start_cap(1), root.as_bytes(), &mut id) != 32 {
        return Err(String::from("корень не появился после загрузки"));
    }
    Ok(id)
}

/// Content-id именованного корня, если он есть.
fn root_id(scap: usize, name: &str) -> Option<[u8; 32]> {
    let mut id = [0u8; 32];
    if sys::obj_get_root(scap, name.as_bytes(), &mut id) == 32 {
        Some(id)
    } else {
        None
    }
}

/// Положить NAR в store **блобом**: куски по `http::CHUNK` плюс узел-манифест, ровно тем же
/// форматом, каким пишет загрузчик ([`sys::http`]).
///
/// Одним объектом нельзя: NAR настоящего пакета — десятки мегабайт, а объект целиком живёт в
/// куче ЯДРА. Заодно достаётся дедуп кусков между пакетами: одинаковые страницы данных в двух
/// путях становятся одним объектом.
fn put_nar(scap: usize, data: &[u8]) -> Result<[u8; 32], String> {
    let mut kids: Vec<[u8; 32]> = Vec::new();
    for part in data.chunks(sys::http::CHUNK) {
        let mut id = [0u8; 32];
        if sys::obj_put(scap, part, &mut id) != 0 {
            return Err(String::from("кусок NAR не влез в store"));
        }
        kids.push(id);
    }
    let man = sys::http::blob_manifest(data.len(), kids.len());
    let mut id = [0u8; 32];
    if sys::obj_put_node(scap, &man, &kids, &mut id) != 0 {
        return Err(String::from("узел NAR не влез в store"));
    }
    Ok(id)
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

/// Один путь: метаданные → (если содержимого ещё нет) архив → распаковка → сверка NarHash → store.
/// `true` во втором поле — содержимое скачано сейчас, `false` — уже лежало.
fn realize_one(scap: usize, hash: &str) -> Result<(NarInfo, [u8; 32], bool), String> {
    let ni = narinfo(scap, hash)?;

    let nar_root = format!("pkg/nar/{}", hash);
    if let Some(id) = root_id(scap, &nar_root) {
        return Ok((ni, id, false));
    }

    let dl_root = format!("pkg/dl/{}", hash);
    let dl_id = download(&format!("{}/{}", CACHE, ni.url), &dl_root)?;

    let nar = archive::unpacked(scap, &dl_id, false)?;
    if nar.len() != ni.nar_size {
        return Err(format!(
            "размер NAR не сошёлся: обещано {}, получено {}",
            ni.nar_size,
            nar.len()
        ));
    }
    let digest: [u8; 32] = Sha256::digest(&nar).into();
    let got = nix_hash32(&digest);
    if got != ni.nar_hash {
        return Err(format!("NarHash не сошёлся: обещано {}, получено {}", ni.nar_hash, got));
    }

    // Готовый NAR — в store. Скачанный сжатый архив больше не нужен: он лишь транспорт, а
    // хранить транспорт рядом с содержимым значит платить за него местом дважды.
    let id = put_nar(scap, &nar)?;
    if sys::obj_set_root(scap, nar_root.as_bytes(), &id) != 0 {
        return Err(String::from("корень NAR не завёлся"));
    }
    sys::obj_del_root(scap, dl_root.as_bytes());
    Ok((ni, id, true))
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
        sys::write(
            format!(
                "  {} {}  {}\n",
                if fresh { "↓" } else { "·" },
                base,
                if fresh { format!("{} Б", ni.nar_size) } else { String::from("уже есть") }
            )
            .as_bytes(),
        );
        if fresh {
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

fn gen_root(n: u32) -> String {
    format!("pkg/profile/{}/gen{}", PROFILE, n)
}

fn current_root() -> String {
    format!("pkg/profile/{}/current", PROFILE)
}

/// Номер активного поколения профиля. `None` — профиля ещё нет.
fn read_current(scap: usize) -> Option<u32> {
    let id = root_id(scap, &current_root())?;
    let mut buf = [0u8; 64];
    let n = sys::obj_get(scap, &id, &mut buf);
    if n == 0 || n > buf.len() {
        return None;
    }
    let s = core::str::from_utf8(&buf[..n]).ok()?;
    roots::number(s.trim().strip_prefix("gen")?.as_bytes())
}

/// Сделать поколение активным: значение корня-указателя — ИМЯ поколения, а не его адрес.
/// Так же устроен `system/current` ([[declarative-init]]): указатель на имя переживает то, что
/// содержимое поколения переехало, и читается человеком в `roots`.
fn set_current(scap: usize, n: u32) -> Result<(), String> {
    let name = format!("gen{}", n);
    let mut id = [0u8; 32];
    if sys::obj_put(scap, name.as_bytes(), &mut id) != 0 {
        return Err(String::from("имя поколения не влезло в store"));
    }
    if sys::obj_set_root(scap, current_root().as_bytes(), &id) != 0 {
        return Err(String::from("указатель профиля не переключился"));
    }
    Ok(())
}

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

/// Разобрать значение поколения: `(верхний уровень, имя пути, размер)`.
fn parse_gen(text: &str) -> Result<Vec<(bool, String, usize)>, String> {
    let mut lines = text.lines();
    if lines.next() != Some(GEN_MAGIC) {
        return Err(String::from("поколение профиля незнакомого формата"));
    }
    let mut out = Vec::new();
    for line in lines {
        let mut it = line.split_whitespace();
        let (Some(kind), Some(base)) = (it.next(), it.next()) else { continue };
        let size = it.next().and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
        out.push((kind == "top", base.to_string(), size));
    }
    Ok(out)
}

/// Прочитать поколение по номеру.
fn read_gen(scap: usize, n: u32) -> Result<Vec<(bool, String, usize)>, String> {
    let id = root_id(scap, &gen_root(n)).ok_or_else(|| format!("нет поколения gen{}", n))?;
    let bytes = archive::read_object(scap, &id, 1024 * 1024)?;
    let text = core::str::from_utf8(&bytes).map_err(|_| "поколение профиля не UTF-8")?;
    parse_gen(text)
}

/// Записать новое поколение и сделать его активным. `false` во втором поле — содержимое совпало с
/// активным поколением, нового не завели (та же защита от пустых поколений, что у `rebuild`).
fn write_gen(scap: usize, c: &Closure) -> Result<(u32, bool), String> {
    let text = gen_text(&c.paths);
    let kids: Vec<[u8; 32]> = c.paths.iter().map(|e| e.id).collect();
    let mut id = [0u8; 32];
    if sys::obj_put_node(scap, text.as_bytes(), &kids, &mut id) != 0 {
        return Err(String::from("поколение профиля не влезло в store"));
    }

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
    Ok(read_gen(scap, cur)?.into_iter().filter(|(top, _, _)| *top).map(|(_, b, _)| b).collect())
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

/// Совпадает ли путь с тем, что назвал человек: полное имя, хэш или просто имя пакета.
fn matches(base: &str, what: &str) -> bool {
    base == what
        || hash_of(base) == what
        || base.get(HASH_LEN + 1..).is_some_and(|name| name == what)
}

// ── команды ─────────────────────────────────────────────────────────────────────────────────

fn cmd_fetch(what: &[u8]) -> Result<(), String> {
    let scap = sys::start_cap(1);
    let hash = path_hash(what)?.to_string();
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
    let hash = path_hash(what)?.to_string();

    // Поколение — это всегда замыкание СПИСКА ВЕРХНЕГО УРОВНЯ, а не «прошлое поколение плюс
    // новое». Отсюда симметрия: установка и удаление — одна и та же пересборка, разница лишь в
    // том, что делает со списком.
    let mut tops = current_tops(scap)?;
    if !tops.iter().any(|t| hash_of(t) == hash) {
        tops.push(hash);
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

fn cmd_list() -> Result<(), String> {
    let scap = sys::start_cap(1);
    let Some(cur) = read_current(scap) else {
        sys::write(format!("профиль {} пуст — `pkg install <путь>`\n", PROFILE).as_bytes());
        return Ok(());
    };
    let paths = read_gen(scap, cur)?;
    let total: usize = paths.iter().map(|(_, _, s)| *s).sum();
    sys::write(format!("профиль {} — поколение gen{}\n", PROFILE, cur).as_bytes());
    for (top, base, _) in paths.iter().filter(|(t, _, _)| *t) {
        let _ = top;
        sys::write(format!("  {}\n", base).as_bytes());
    }
    let deps: Vec<&String> = paths.iter().filter(|(t, _, _)| !*t).map(|(_, b, _)| b).collect();
    if !deps.is_empty() {
        sys::write(format!("  зависимости ({}):\n", deps.len()).as_bytes());
        for d in deps {
            sys::write(format!("    {}\n", d).as_bytes());
        }
    }
    sys::write(format!("  всего {} {}, {} Б\n", paths.len(), paths_word(paths.len()), total).as_bytes());
    Ok(())
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
