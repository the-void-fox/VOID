//! Файлы для персоналии Linux (Веха 108.3): чтение **прямо из объектного store**.
//!
//! ## Почему не через файловый сервер
//!
//! Файлы в VOID показывает `posixfs` — userspace-сервер, и это правильно. Но персоналия Linux
//! ([[linux-abi]]) живёт В ЯДРЕ (тонкий транслятор syscall'ов, путь FreeBSD linuxulator / gVisor),
//! и `openat` чужого бинаря обязан ответить ВНУТРИ syscall'а. Сходить оттуда по IPC к серверу
//! нечем: IPC у нас — механизм для процессов, у ядра нет ни своего домена, ни состояния «жду
//! ответа»; заводить его ради `open` значило бы построить полсистемы асинхронных syscall'ов.
//!
//! Зато у ядра есть store — он его собственный. А **и дерево пакета, и иерархия posixfs суть
//! просто корни store**: `pkg/tree/<хэш>` и `f<путь>`. Поэтому чтение здесь — не вторая файловая
//! система, а тот же граф объектов, прочитанный с другой стороны, и формат дерева берётся из
//! общего крейта `void_tree` — того же, которым его пишет `pkg` и показывает `posixfs`.
//!
//! **Только чтение.** Запись потребовала бы согласия с `posixfs` о его кэше открытых файлов, а
//! пакет и вовсе неизменяем. Оговорка честная: файл, который posixfs держит открытым и грязным,
//! мы увидим в том виде, в каком он лёг в store последним `close`.

use alloc::string::String;
use alloc::vec::Vec;

use void_abi::ContentId;

use crate::object;

/// Точка монтирования пакетов — та же, что у `posixfs`.
const MOUNT: &[u8] = b"/nix/store";
/// Префикс корня, под которым `pkg` держит распакованные деревья.
const TREE_ROOT: &str = "pkg/tree/";
/// Длина хэша пути nix.
const HASH_LEN: usize = 32;

/// Найденный узел: что читать и чем оно является.
#[derive(Clone, Copy)]
pub struct Meta {
    pub id: ContentId,
    pub size: u64,
    /// Тип записи дерева пакета ([`void_tree`]); у файлов иерархии posixfs — обычный файл.
    pub ty: u8,
}

/// Путь относительно точки монтирования (пустой ломоть — сам `/nix/store`).
fn under_mount(path: &[u8]) -> Option<&[u8]> {
    if path == MOUNT {
        return Some(b"");
    }
    let rest = path.strip_prefix(MOUNT)?;
    if rest.first() == Some(&b'/') {
        Some(&rest[1..])
    } else {
        None
    }
}

/// Спуститься по пути внутри дерева пакета.
fn tree_lookup(rel: &[u8]) -> Option<Meta> {
    let mut parts = rel.split(|&b| b == b'/').filter(|c| !c.is_empty());
    let base = parts.next()?;
    if base.len() < HASH_LEN {
        return None;
    }
    let hash = core::str::from_utf8(&base[..HASH_LEN]).ok()?;
    let mut root_name = String::with_capacity(TREE_ROOT.len() + HASH_LEN);
    root_name.push_str(TREE_ROOT);
    root_name.push_str(hash);
    let mut cur = Meta { id: object::root(&root_name)?, size: 0, ty: void_tree::K_DIR };
    let mut name: &[u8] = base;

    loop {
        // Имя сверяется целиком (а не по хэшу): `/nix/store/<хэш>-что-угодно` не должен открывать
        // чужой пакет.
        let found = object::with(&cur.id, |payload| {
            let buf = payload?;
            let (i, r) = void_tree::find(buf, name)?;
            Some((i, r.ty, r.size))
        })?;
        let (i, ty, size) = found;
        let kids = object::children(&cur.id);
        cur = Meta { id: *kids.get(i)?, size, ty };
        match parts.next() {
            Some(next) => {
                if !void_tree::is_dir(cur.ty) {
                    return None; // спуск сквозь файл или симлинк
                }
                name = next;
            }
            None => return Some(cur),
        }
    }
}

/// Найти путь, **разыменовывая символические ссылки** — в том числе промежуточные (Веха 108.4).
///
/// Нужно это `ld.so`: в пакетах nixpkgs половина имён библиотек — ссылки (`libc.so.6` рядом с
/// `libc.so.6.x`, `lib64 → lib`), и без разыменования загрузчик спотыкается на первой же.
/// Потолок в 8 переходов — против петель, которые чужой пакет может завести и случайно.
pub fn lookup(path: &[u8]) -> Option<Meta> {
    let mut work: Vec<u8> = path.to_vec();
    let mut hops = 0usize;
    'restart: loop {
        let n = work.split(|&b| b == b'/').filter(|c| !c.is_empty()).count();
        for i in 1..=n {
            // Префикс из первых `i` компонент — ищем ссылку как можно раньше.
            let mut prefix: Vec<u8> = Vec::new();
            for c in work.split(|&b| b == b'/').filter(|c| !c.is_empty()).take(i) {
                prefix.push(b'/');
                prefix.extend_from_slice(c);
            }
            let m = lookup_nofollow(&prefix)?;
            if void_tree::is_link(m.ty) {
                hops += 1;
                if hops > 8 {
                    return None;
                }
                let target = readlink(&m)?;
                let mut next: Vec<u8> = Vec::new();
                if target.first() == Some(&b'/') {
                    next.extend_from_slice(&target);
                } else {
                    // Относительная цель — от каталога самой ссылки.
                    let cut = prefix.iter().rposition(|&b| b == b'/').unwrap_or(0);
                    next.extend_from_slice(&prefix[..cut]);
                    next.push(b'/');
                    next.extend_from_slice(&target);
                }
                for c in work.split(|&b| b == b'/').filter(|c| !c.is_empty()).skip(i) {
                    next.push(b'/');
                    next.extend_from_slice(c);
                }
                work = next;
                continue 'restart;
            }
            if i == n {
                return Some(m);
            }
        }
        return lookup_nofollow(&work); // путь без компонент — корень
    }
}

/// Прочитать файл целиком (ядру это нужно ровно для одного: загрузить образ `ld.so`).
pub fn read_all(meta: &Meta) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(meta.size as usize);
    let mut off = 0u64;
    let mut buf = alloc::vec![0u8; 64 * 1024];
    while off < meta.size {
        let n = read_at(meta, off, &mut buf);
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        off += n as u64;
    }
    (out.len() as u64 == meta.size).then_some(out)
}

/// Найти путь БЕЗ разыменования ссылок: сперва дерево пакета, затем обычный файл иерархии
/// `posixfs` (корень `f<путь>`).
pub fn lookup_nofollow(path: &[u8]) -> Option<Meta> {
    // Родители точки монтирования синтетические: своего `/nix` в персоналии нет, но разбор пути
    // обязан пройти сквозь него — иначе спотыкается сам поиск `ld.so`, чей путь начинается
    // именно с него. (Ровно так же их показывает posixfs.)
    if path == b"/" || path == b"/nix" {
        return Some(Meta { id: ContentId([0u8; 32]), size: 0, ty: void_tree::K_DIR });
    }
    if let Some(rel) = under_mount(path) {
        if rel.is_empty() {
            // Сам /nix/store — каталог, но у него нет узла: перечисление живёт в `store_roots`.
            return Some(Meta { id: ContentId([0u8; 32]), size: 0, ty: void_tree::K_DIR });
        }
        return tree_lookup(rel);
    }
    // Иерархия posixfs: файл = объект под корнем `f<путь>` (каталоги её здесь не читаем — их
    // индекс другого формата, а Linux-программе они пока и не нужны).
    let p = core::str::from_utf8(path).ok()?;
    let mut name = String::with_capacity(1 + p.len());
    name.push('f');
    name.push_str(p);
    let id = object::root(&name)?;
    let size = object::with(&id, |payload| payload.map(|b| b.len() as u64))?;
    Some(Meta { id, size, ty: void_tree::K_FILE })
}

/// Прочитать не больше `out.len()` байт файла с позиции `off`. Возвращает сколько прочитано;
/// 0 — конец файла (или это не файл).
///
/// Крупный файл лежит блобом (манифест + куски), и чтение идёт ПО КУСКАМ: за один вызов отдаётся
/// не больше остатка текущего куска. Короткое чтение законно — так же ведёт себя `read` на трубе,
/// и вызывающий дочитает следующим вызовом.
pub fn read_at(meta: &Meta, off: u64, out: &mut [u8]) -> usize {
    if void_tree::is_dir(meta.ty) || out.is_empty() {
        return 0;
    }
    if !void_tree::is_blob(meta.ty) {
        return object::with(&meta.id, |payload| {
            let Some(b) = payload else { return 0 };
            let off = off as usize;
            if off >= b.len() {
                return 0;
            }
            let n = (b.len() - off).min(out.len());
            out[..n].copy_from_slice(&b[off..off + n]);
            n
        });
    }
    // Блоб: манифест хранит общий размер, число кусков и размер куска.
    let Some((total, nchunks, csize)) = object::with(&meta.id, |payload| {
        let b = payload?;
        void_tree::blob::info(b, 16 * 1024)
    }) else {
        return 0;
    };
    if off >= total as u64 || csize == 0 {
        return 0;
    }
    let ci = (off as usize) / csize;
    if ci >= nchunks {
        return 0;
    }
    let kids = object::children(&meta.id);
    let Some(cid) = kids.get(ci) else { return 0 };
    let within = (off as usize) % csize;
    object::with(cid, |payload| {
        let Some(b) = payload else { return 0 };
        if within >= b.len() {
            return 0;
        }
        let n = (b.len() - within).min(out.len());
        out[..n].copy_from_slice(&b[within..within + n]);
        n
    })
}

/// Записи каталога: `(тип, имя)`. Для `/nix/store` — список пакетов (корни `pkg/tree/*`).
pub fn dir_entries(path: &[u8], meta: &Meta) -> Vec<(u8, String)> {
    let mut out = Vec::new();
    if under_mount(path) == Some(b"") {
        for line in object::list_roots_text().lines() {
            // Строка списка: 12 hex короткого id + два пробела + имя корня.
            let Some(name) = line.get(14..) else { continue };
            let Some(hash) = name.strip_prefix(TREE_ROOT) else { continue };
            if hash.len() < HASH_LEN {
                continue;
            }
            let mut rn = String::with_capacity(TREE_ROOT.len() + HASH_LEN);
            rn.push_str(TREE_ROOT);
            rn.push_str(&hash[..HASH_LEN]);
            let Some(id) = object::root(&rn) else { continue };
            // Полное имя пакета лежит в его корневом индексе — там оно с версией.
            if let Some(e) = object::with(&id, |payload| {
                let b = payload?;
                let r = void_tree::iter(b)?.next()?;
                Some((r.ty, String::from(core::str::from_utf8(r.name).ok()?)))
            }) {
                out.push(e);
            }
        }
        return out;
    }
    if !void_tree::is_dir(meta.ty) {
        return out;
    }
    object::with(&meta.id, |payload| {
        let Some(b) = payload else { return };
        let Some(it) = void_tree::iter(b) else { return };
        for r in it {
            if let Ok(name) = core::str::from_utf8(r.name) {
                out.push((r.ty, String::from(name)));
            }
        }
    });
    out
}

/// Номер инода файла — первые 8 байт его content-id. Уникальность даётся содержимым: два файла
/// с одинаковыми байтами и правда один объект, и `ld.so`, посчитав их одним, будет прав.
pub fn ino(meta: &Meta) -> u64 {
    u64::from_le_bytes(meta.id.0[..8].try_into().unwrap_or([0; 8]))
}

/// Цель символической ссылки.
pub fn readlink(meta: &Meta) -> Option<Vec<u8>> {
    if !void_tree::is_link(meta.ty) {
        return None;
    }
    object::with(&meta.id, |payload| payload.map(|b| b.to_vec()))
}
