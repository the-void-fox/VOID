//! `pkg` — пакеты из бинарного кэша nixpkgs (Веха 106, Фаза 8, [[0009-ondevice-packages]]).
//!
//! `pkg fetch <хэш|/nix/store/путь>` — скачать narinfo, **проверить его подпись**, скачать архив,
//! распаковать, **сверить NarHash** и положить готовый NAR в store под корень `pkg/nar/<хэш>`.
//!
//! ## Почему проверка — суть вехи, а не украшение
//!
//! Всё, что мы берём из кэша, приходит с чужой машины. TLS (Веха 95) отвечает лишь за то, что
//! байты не подменили ПО ДОРОГЕ, — он ничего не говорит о том, кто их туда положил. Настоящая
//! гарантия nix другая и куда сильнее: **подпись ключом кэша по отпечатку пути** и
//! **content-address** — сверка sha256 распакованного NAR с тем, что обещано в narinfo. Без этих
//! двух проверок «скачать пакет» означало бы «исполнить то, что прислал незнакомец», а VOID
//! запускает программы по content-id именно затем, чтобы такого не было.
//!
//! Порядок важен: подпись проверяется ДО загрузки архива (незачем тянуть мегабайты, если
//! метаданным нельзя верить), NarHash — после распаковки.
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

// Общий с `vvsh` код формата архивов — подключён ПО ПУТИ (почему так — в шапке файла).
#[path = "../archive.rs"]
mod archive;

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

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let mut argbuf = [0u8; 512];
    let n = sys::args(&mut argbuf);
    let mut it = argbuf[..n].split(|&b| b == 0).filter(|s| !s.is_empty()).skip(1);
    let (Some(cmd), rest) = (it.next(), it.next()) else {
        usage();
        sys::exit(2);
    };

    let code = match (cmd, rest) {
        (b"fetch", Some(what)) => match fetch(what) {
            Ok(()) => 0,
            Err(e) => {
                sys::write(format!("pkg: {}\n", e).as_bytes());
                1
            }
        },
        _ => {
            usage();
            2
        }
    };
    sys::exit(code);
}

fn usage() {
    sys::write("pkg fetch <хэш|/nix/store/путь> — скачать путь из кэша с проверкой\n".as_bytes());
}

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

/// Скачать URL в корень store через `httpsc` (TLS живёт в своём процессе, см. шапку).
fn download(url: &str, root: &str) -> Result<[u8; 32], String> {
    let mut argv = Vec::new();
    argv.extend_from_slice(url.as_bytes());
    argv.push(0);
    argv.extend_from_slice(root.as_bytes());
    if sys::exec_args(sys::start_cap(1), b"httpsc", &argv) != 0 {
        return Err(format!("не скачалось: {}", url));
    }
    let mut id = [0u8; 32];
    if sys::obj_get_root(sys::start_cap(1), root.as_bytes(), &mut id) != 32 {
        return Err(String::from("корень не появился после загрузки"));
    }
    Ok(id)
}

/// Из аргумента вытащить 32-символьный хэш пути: принимаем и голый хэш, и полный путь store,
/// и просто имя каталога — человеку не должно быть важно, что он скопировал.
fn path_hash(arg: &[u8]) -> Result<&str, String> {
    let s = core::str::from_utf8(arg).map_err(|_| "аргумент не UTF-8")?;
    let base = s.rsplit('/').next().unwrap_or(s);
    if base.len() < 32 {
        return Err(String::from("не похоже на путь store (нужен хэш из 32 символов)"));
    }
    let hash = &base[..32];
    if !hash.bytes().all(|b| NIX32.contains(&b)) {
        return Err(String::from("в хэше символы не из алфавита nix-base32"));
    }
    Ok(hash)
}

fn fetch(what: &[u8]) -> Result<(), String> {
    let scap = sys::start_cap(1);
    let hash = path_hash(what)?;

    // 1. narinfo — метаданные пути.
    let ni_root = format!("pkg/narinfo/{}", hash);
    let ni_id = download(&format!("{}/{}.narinfo", CACHE, hash), &ni_root)?;
    let ni_bytes = archive::unpacked(scap, &ni_id, false)?;
    let ni_text = core::str::from_utf8(&ni_bytes).map_err(|_| "narinfo не UTF-8")?;
    let ni = parse_narinfo(ni_text)?;
    sys::write(format!("{}\n", ni.store_path).as_bytes());

    // 2. Подпись — ДО загрузки архива.
    check_signature(&ni)?;
    sys::write(format!("  подпись {} — верна\n", KEY_NAME).as_bytes());

    // 3. Архив.
    let dl_root = format!("pkg/dl/{}", hash);
    let dl_id = download(&format!("{}/{}", CACHE, ni.url), &dl_root)?;

    // 4. Распаковка и сверка содержимого.
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
    sys::write(format!("  NarHash {} — сошёлся ({} Б)\n", got, nar.len()).as_bytes());

    // 5. Готовый NAR — в store. Скачанный сжатый архив больше не нужен: он лишь транспорт,
    //    а хранить транспорт рядом с содержимым значит платить за него местом дважды.
    let nar_root = format!("pkg/nar/{}", hash);
    let mut id = [0u8; 32];
    if sys::obj_put(scap, &nar, &mut id) != 0 {
        return Err(String::from("NAR не влез в store"));
    }
    if sys::obj_set_root(scap, nar_root.as_bytes(), &id) != 0 {
        return Err(String::from("корень NAR не завёлся"));
    }
    sys::obj_del_root(scap, dl_root.as_bytes());

    sys::write(format!("  корень {}\n", nar_root).as_bytes());
    if !ni.references.is_empty() {
        sys::write(format!("  зависимостей: {} (замыкание — Веха 107)\n", ni.references.len()).as_bytes());
    }
    Ok(())
}
