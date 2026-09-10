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
/// Веха 187 — префикс корня-ПЕЧАТИ: сборка, доведённая до конца, объявляет свой путь готовым, и
/// с этой минуты он только читается. Ставит печать `nixb`, снаружи ядра.
const BUILT_ROOT: &str = "pkg/built/";
/// Длина хэша пути nix.
const HASH_LEN: usize = 32;

/// Откуда узел. Веха 181 — различать обязательно: у дерева пакета и у иерархии `posixfs`
/// РАЗНЫЕ форматы индекса каталога, и прочитать один другим значит увидеть мусор вместо имён.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Src {
    /// Дерево распакованного пакета под `/nix/store` — формат [`void_tree`]. Только чтение.
    Tree,
    /// Иерархия `posixfs` — формат [`void_fs`]. С этой вехи ещё и записывается.
    Hier,
}

/// Найденный узел: что читать и чем оно является.
#[derive(Clone, Copy)]
pub struct Meta {
    pub id: ContentId,
    pub size: u64,
    /// Тип записи дерева пакета ([`void_tree`]); у файлов иерархии posixfs — обычный файл.
    pub ty: u8,
    pub src: Src,
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

/// Хэш первой компоненты пути под точкой монтирования (`<хэш>-<имя>/…`).
fn path_hash(rel: &[u8]) -> Option<&str> {
    let base = rel.split(|&b| b == b'/').find(|c| !c.is_empty())?;
    if base.len() < HASH_LEN {
        return None;
    }
    core::str::from_utf8(&base[..HASH_LEN]).ok()
}

/// Есть ли корень с таким именем-префиксом и хэшем.
fn has_root(prefix: &str, hash: &str) -> bool {
    let mut name = String::with_capacity(prefix.len() + hash.len());
    name.push_str(prefix);
    name.push_str(hash);
    object::root(&name).is_some()
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
    let mut cur =
        Meta { id: object::root(&root_name)?, size: 0, ty: void_tree::K_DIR, src: Src::Tree };
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
        cur = Meta { id: *kids.get(i)?, size, ty, src: Src::Tree };
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
    if path == b"/nix" {
        return Some(Meta { id: ContentId([0u8; 32]), size: 0, ty: void_tree::K_DIR, src: Src::Tree });
    }
    if let Some(rel) = under_mount(path) {
        if rel.is_empty() {
            // Сам /nix/store — каталог, но у него нет узла: перечисление живёт в `store_roots`.
            return Some(Meta { id: ContentId([0u8; 32]), size: 0, ty: void_tree::K_DIR, src: Src::Tree });
        }
        if let Some(m) = tree_lookup(rel) {
            return Some(m);
        }
        // Веха 187 — распакованного дерева с таким хэшем нет, но путь мог быть СОБРАН здесь:
        // своя деривация кладёт результат по адресу того же вида. Проваливаемся в иерархию.
    }
    // Иерархия posixfs. Веха 181 — теперь и КАТАЛОГИ: без них Linux-процесс не может ни
    // `stat` каталога, ни `getdents`, а сборке нужно и то и другое (`configure` начинает с
    // обхода дерева). Формат индекса — `void_fs`, и различает их `Meta::src`.
    if let Some(m) = hier_dir(path) {
        return Some(m);
    }
    let id = object::root(&hier_root(void_fs::K_FILE, path)?)?;
    let size = object::with(&id, |payload| payload.map(|b| b.len() as u64))?;
    // Большой файл лежит блобом — размер берётся из манифеста, а не из длины узла.
    let (size, ty) = match object::with(&id, |p| {
        p.and_then(|b| void_tree::blob::info(b, 16 * 1024)).map(|(total, _, _)| total)
    }) {
        Some(total) => (total as u64, void_tree::K_FILE | void_tree::F_BLOB),
        None => (size, void_tree::K_FILE),
    };
    Some(Meta { id, size, ty, src: Src::Hier })
}

/// Имя корня иерархии: вид плюс путь. `None` — путь не UTF-8 (в store имена корней — строки).
fn hier_root(kind: u8, path: &[u8]) -> Option<String> {
    let p = core::str::from_utf8(path).ok()?;
    let mut name = String::with_capacity(1 + p.len());
    name.push(kind as char);
    name.push_str(p);
    Some(name)
}

/// Каталог иерархии, если он есть.
fn hier_dir(path: &[u8]) -> Option<Meta> {
    let id = object::root(&hier_root(void_fs::K_DIR, path)?)?;
    Some(Meta { id, size: 0, ty: void_tree::K_DIR, src: Src::Hier })
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
            // Веха 187 — собранное ЗДЕСЬ стоит в списке наравне со скачанным: store один, и
            // делить его на две витрины значило бы объявить свою сборку второсортной. Полное имя
            // лежит в теле печати — там записан сам путь.
            if let Some(hash) = name.strip_prefix(BUILT_ROOT) {
                if hash.len() < HASH_LEN {
                    continue;
                }
                let mut rn = String::with_capacity(BUILT_ROOT.len() + HASH_LEN);
                rn.push_str(BUILT_ROOT);
                rn.push_str(&hash[..HASH_LEN]);
                let Some(id) = object::root(&rn) else { continue };
                let Some(full) = object::with(&id, |p| {
                    p.and_then(|b| core::str::from_utf8(b).ok()).map(String::from)
                }) else {
                    continue;
                };
                let base = match full.rfind('/') {
                    Some(i) => &full[i + 1..],
                    None => full.as_str(),
                };
                let ty = if hier_dir(full.as_bytes()).is_some() {
                    void_tree::K_DIR
                } else {
                    void_tree::K_FILE
                };
                out.push((ty, String::from(base)));
                continue;
            }
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
    // Веха 181 — у иерархии СВОЙ формат индекса (`void_fs`). Прочитать его форматом дерева
    // пакета значит показать мусор вместо имён, поэтому `Meta` и носит `src`.
    if meta.src == Src::Hier {
        object::with(&meta.id, |payload| {
            let Some(b) = payload else { return };
            for e in void_fs::entries(b, b.len()) {
                if let Ok(name) = core::str::from_utf8(e.name) {
                    let ty = if e.is_dir() { void_tree::K_DIR } else { void_tree::K_FILE };
                    out.push((ty, String::from(name)));
                }
            }
        });
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

// ─── запись (Веха 181, ADR 0019) ─────────────────────────────────────────────────────────────
//
// Писать иерархию ядру нужно ровно затем, чтобы на VOID можно было СОБРАТЬ деривацию: builder
// пишет `$out`, а он — обычный Linux-процесс, чей `write` обязан ответить внутри системного
// вызова. Сходить отсюда по IPC к `posixfs` нечем (см. шапку файла), поэтому пишем сами — тем же
// форматом (`void_fs`), одной с ним реализацией.
//
// **Правило, которое надо помнить: один файл — один писатель.** Каталоги от двух писателей не
// страдают (индекс перечитывается на каждой операции), а вот файл, который `posixfs` держит
// открытым и грязным, его `close` перезапишет поверх написанного нами. В песочнице сборки этого
// не случится — там файлы принадлежат Linux-стороне.

/// Лежит ли путь в ГОТОВОМ объекте store — распакованном пакете или запечатанной сборке.
/// Там запись запрещена: объект store неизменяем, в этом весь его смысл.
///
/// Веха 187 — «готовый» перестало значить «под `/nix/store`», и это не послабление, а условие
/// сборки. Деривация обязана писать в СВОЙ настоящий адрес (`$out`): всё, что сборка о нём
/// запомнит — а запоминает она его щедро, в shebang'ах, RPATH и текстах скриптов, — иначе будет
/// указывать в никуда. Поэтому адрес открыт на запись ровно до тех пор, пока сборка его не
/// запечатает; после печати он такой же неизменяемый, как скачанный.
pub fn in_package(path: &[u8]) -> bool {
    let Some(rel) = under_mount(path) else { return false };
    if rel.is_empty() {
        // Сам каталог `/nix/store` — иерархия: в него и складывают собранное.
        return false;
    }
    let Some(hash) = path_hash(rel) else { return true };
    has_root(TREE_ROOT, hash) || has_root(BUILT_ROOT, hash)
}

/// Положить содержимое в store: маленькое одним объектом, большое КУСКАМИ.
///
/// Формат кусков — общий с загрузкой из сети и с `posixfs` (`void_tree::blob`). Свой был бы
/// вторым, и первым же следствием стало бы, что файл, записанный из сборки, не открывается
/// файловым менеджером.
fn put_body(body: &[u8]) -> Option<(ContentId, u8)> {
    const CHUNK: usize = 16 * 1024;
    if body.len() <= CHUNK {
        return object::try_put(body).map(|id| (id, void_tree::K_FILE));
    }
    let n = body.len().div_ceil(CHUNK);
    let mut kids: Vec<ContentId> = Vec::with_capacity(n);
    for k in 0..n {
        let to = ((k + 1) * CHUNK).min(body.len());
        kids.push(object::try_put(&body[k * CHUNK..to])?);
    }
    let manifest = void_tree::blob::manifest(body.len(), n, CHUNK);
    object::try_put_node(&manifest, &kids).map(|id| (id, void_tree::K_FILE | void_tree::F_BLOB))
}

/// Прочитать индекс каталога в буфер; 0 — каталога нет.
fn read_index(path: &[u8], dir: &mut [u8; void_fs::DIR_MAX]) -> usize {
    let Some(name) = hier_root(void_fs::K_DIR, path) else { return 0 };
    let Some(id) = object::root(&name) else { return 0 };
    object::with(&id, |payload| {
        let Some(b) = payload else { return 0 };
        let n = b.len().min(dir.len());
        dir[..n].copy_from_slice(&b[..n]);
        n
    })
}

fn write_index(path: &[u8], dir: &[u8]) -> bool {
    let Some(name) = hier_root(void_fs::K_DIR, path) else { return false };
    let Some(id) = object::try_put(dir) else { return false };
    object::set_root(&name, id);
    true
}

/// Пометить запись `name` каталога `dirp` временем «сейчас».
fn touch(dirp: &[u8], name: &[u8]) {
    let Some(root) = hier_root(void_fs::K_TIME, dirp) else { return };
    let mut t = [0u8; void_fs::MT_MAX];
    let n = match object::root(&root) {
        Some(id) => object::with(&id, |payload| {
            let Some(b) = payload else { return 0 };
            let n = b.len().min(t.len());
            t[..n].copy_from_slice(&b[..n]);
            n
        }),
        None => 0,
    };
    let n = void_fs::mt_set(&mut t, n, name, crate::clock::realtime_ns());
    if let Some(id) = object::try_put(&t[..n]) {
        object::set_root(&root, id);
    }
}

/// Забыть время записи — её больше нет в каталоге.
fn untouch(dirp: &[u8], name: &[u8]) {
    let Some(root) = hier_root(void_fs::K_TIME, dirp) else { return };
    let Some(id) = object::root(&root) else { return };
    let mut t = [0u8; void_fs::MT_MAX];
    let n = object::with(&id, |payload| {
        let Some(b) = payload else { return 0 };
        let n = b.len().min(t.len());
        t[..n].copy_from_slice(&b[..n]);
        n
    });
    if n == 0 {
        return;
    }
    let n = void_fs::mt_remove(&mut t, n, name);
    if let Some(id) = object::try_put(&t[..n]) {
        object::set_root(&root, id);
    }
}

/// Вписать имя в индекс родителя. `false` — родителя нет (создавать его молча мы не станем:
/// `mkdir -p` это работа вызывающего, а не файловой системы).
fn link_into_parent(path: &[u8], is_dir: bool) -> bool {
    let par = void_fs::parent(path);
    let mut dir = [0u8; void_fs::DIR_MAX];
    let mut n = read_index(par, &mut dir);
    if n == 0 && par != b"/" {
        return false;
    }
    n = void_fs::idx_add(&mut dir, n, void_fs::leaf(path), is_dir);
    if !write_index(par, &dir[..n]) {
        return false;
    }
    touch(par, void_fs::leaf(path));
    true
}

fn unlink_from_parent(path: &[u8]) {
    let par = void_fs::parent(path);
    let mut dir = [0u8; void_fs::DIR_MAX];
    let n = read_index(par, &mut dir);
    if n == 0 {
        return;
    }
    let n = void_fs::idx_remove(&mut dir, n, void_fs::leaf(path));
    write_index(par, &dir[..n]);
    untouch(par, void_fs::leaf(path));
}

/// Записать файл целиком. `false` — не влезло в store либо нет родительского каталога.
pub fn write_file(path: &[u8], body: &[u8]) -> bool {
    if in_package(path) {
        return false; // пакет неизменяем — и это не недоделка, а его смысл
    }
    let Some(root) = hier_root(void_fs::K_FILE, path) else { return false };
    let Some((id, _)) = put_body(body) else { return false };
    object::set_root(&root, id);
    link_into_parent(path, false)
}

/// Создать каталог. `false` — уже есть, нет родителя или путь не наш.
pub fn mkdir(path: &[u8]) -> bool {
    if in_package(path) || path == b"/" {
        return false;
    }
    if hier_dir(path).is_some() {
        return false;
    }
    let empty = [0u8; 2];
    if !write_index(path, &empty) {
        return false;
    }
    link_into_parent(path, true)
}

/// Снять файл либо ПУСТОЙ каталог. Рекурсии здесь нет намеренно: `rm -r` разворачивает обход
/// вызывающий, и согласие человека на потерю содержимого — тоже его дело.
pub fn unlink(path: &[u8]) -> bool {
    if in_package(path) || path == b"/" {
        return false;
    }
    if let Some(m) = hier_dir(path) {
        let empty = object::with(&m.id, |p| p.map(|b| void_fs::idx_empty(b, b.len())).unwrap_or(true));
        if !empty {
            return false;
        }
        let Some(d) = hier_root(void_fs::K_DIR, path) else { return false };
        object::del_root(&d);
        if let Some(t) = hier_root(void_fs::K_TIME, path) {
            object::del_root(&t);
        }
        unlink_from_parent(path);
        return true;
    }
    let Some(f) = hier_root(void_fs::K_FILE, path) else { return false };
    if !object::del_root(&f) {
        return false;
    }
    unlink_from_parent(path);
    true
}

/// Переименовать ФАЙЛ. Каталог здесь не переименовывается: путь входит в имя корня каждого
/// потомка, значит это обход поддерева — и он уже написан в `posixfs`. Дублировать его в ядре
/// ради сборки незачем: сборочные скрипты переименовывают файлы, а каталоги переносят по одному.
pub fn rename(old: &[u8], new: &[u8]) -> bool {
    if in_package(old) || in_package(new) {
        return false;
    }
    if hier_dir(old).is_some() {
        return false;
    }
    let (Some(fo), Some(fna)) = (hier_root(void_fs::K_FILE, old), hier_root(void_fs::K_FILE, new))
    else {
        return false;
    };
    let Some(id) = object::root(&fo) else { return false };
    object::set_root(&fna, id);
    object::del_root(&fo);
    unlink_from_parent(old);
    link_into_parent(new, false)
}
