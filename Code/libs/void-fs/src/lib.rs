//! Формат **иерархии файлов** VOID в объектном store.
//!
//! ## Что здесь лежит и почему это вообще формат
//!
//! Каталогов как объектов в store нет. Есть три вида ИМЁН корней, и путь входит в само имя:
//!
//! | корень | что в нём |
//! |---|---|
//! | `f<путь>` | содержимое файла (одним объектом либо кусками — [`void_tree::blob`]) |
//! | `d<путь>` | индекс каталога: какие имена в нём лежат и что каждое такое |
//! | `m<путь>` | времена ЗАПИСЕЙ этого каталога |
//!
//! Отсюда всё остальное поведение персоналии: переименовать каталог значит перевесить корень
//! каждого потомка, скопировать — завести второе имя для тех же объектов (мгновенно и без места),
//! а удалить непустой — снять их все.
//!
//! ## Почему отдельный крейт
//!
//! Та же причина, по которой отдельным крейтом стал [`void_tree`], и причина эта — ЯДРО.
//! Персоналия Linux живёт в ядре: `openat` чужого бинаря обязан ответить внутри системного
//! вызова, а сходить оттуда по IPC к файловому серверу нечем. Читать иерархию ядро умеет уже
//! сегодня (`lxfs`), а с [[0019-nix-on-device]] ему нужно в неё ПИСАТЬ — иначе на VOID нельзя
//! собрать ни одной деривации.
//!
//! Значит формат обязан существовать в одном месте. Две реализации разошлись бы на первой же
//! правке, а разойтись им здесь значит **потерять файлы**: индекс, записанный по одному
//! пониманию и прочитанный по другому, — это каталог, в котором содержимое есть, а имён нет.
//!
//! ## Правила, которым подчиняется всё ниже
//!
//! - **Никакой кучи.** У `posixfs` её нет вовсе, у ядра своя. Всё работает в буфере вызывающего,
//!   и все функции возвращают новую длину, а не выделяют.
//! - **Не влезло — не соврали.** Функции, которым не хватило места, возвращают прежнюю длину:
//!   вызывающий видит, что размер не изменился. Молчаливое усечение в этой системе — родовая
//!   болезнь, и здесь его нет по построению.
//! - **Формат не версионируется полем версии.** Версия — это ВИД корня: незнакомый вид система
//!   прежних поколений просто не заметит. Так добавились времена (`m`), не тронув индекс.

#![cfg_attr(not(test), no_std)]

/// Корень СОДЕРЖИМОГО файла.
pub const K_FILE: u8 = b'f';
/// Корень ИНДЕКСА каталога.
pub const K_DIR: u8 = b'd';
/// Корень ВРЕМЁН записей каталога.
pub const K_TIME: u8 = b'm';

/// Тип записи в индексе: обычный файл.
pub const T_FILE: u8 = 0;
/// Тип записи в индексе: каталог.
pub const T_DIR: u8 = 1;

/// Потолок длины пути. Веха 108.2: было 128 — не хватало даже на
/// `/nix/store/<хэш>-<имя>/lib/...`.
pub const PATH_MAX: usize = 512;

/// Потолок индекса каталога. Он же ограничивает число записей в каталоге.
pub const DIR_MAX: usize = 4096;

/// Потолок буфера времён каталога.
///
/// Не на глаз: запись индекса стоит `2 + nlen` байт, запись времён — `9 + nlen`, а индекс не
/// длиннее [`DIR_MAX`]. Худший случай — имена в один байт: записей не больше `(4096 - 2) / 3 =
/// 1364`, времён на них `2 + 1364 * 10 = 13 642` байта. То есть времена помещаются ВСЕГДА, и
/// молчаливой потери времени быть не может по построению.
pub const MT_MAX: usize = 16384;

/// Имя корня — вид плюс путь.
pub const ROOT_MAX: usize = 1 + PATH_MAX;

// ─── пути ───────────────────────────────────────────────────────────────────────────────────

/// Нормализовать запрос в абсолютный путь в `out`, вернуть длину.
///
/// Пусто, `.` и `/` — корень `/`; голое имя — `/<имя>` (файлы std-программ живут в корне, и
/// поэтому старые бинари работают без пересборки); хвостовой `/` (кроме самого корня) убирается.
pub fn normalize(req: &[u8], out: &mut [u8; PATH_MAX]) -> usize {
    let mut n = 0usize;
    if req.is_empty() || req == b"." || req == b"/" {
        out[0] = b'/';
        return 1;
    }
    if req[0] != b'/' {
        out[0] = b'/';
        n = 1;
    }
    for &b in req {
        if n < PATH_MAX {
            out[n] = b;
            n += 1;
        }
    }
    if n > 1 && out[n - 1] == b'/' {
        n -= 1;
    }
    n
}

/// Родитель пути (`/a/b` → `/a`; `/a` → `/`; `/` → `/`).
pub fn parent(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&b| b == b'/') {
        Some(0) | None => b"/",
        Some(i) => &path[..i],
    }
}

/// Листовое имя (`/a/b` → `b`; `/` → пусто).
pub fn leaf(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&b| b == b'/') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

/// Собрать имя корня `<вид><путь>` в `out`, вернуть длину.
pub fn root_name(kind: u8, path: &[u8], out: &mut [u8; ROOT_MAX]) -> usize {
    out[0] = kind;
    let n = path.len().min(ROOT_MAX - 1);
    out[1..1 + n].copy_from_slice(&path[..n]);
    1 + n
}

// ─── индекс каталога: count(u16 LE) | [type(1) | nlen(1) | name]* ───────────────────────────

/// Есть ли `name` в индексе `dir[..len]`; возвращает ТИП записи ([`T_FILE`] / [`T_DIR`]).
pub fn idx_type(dir: &[u8], len: usize, name: &[u8]) -> Option<u8> {
    if len < 2 {
        return None;
    }
    let cnt = u16::from_le_bytes([dir[0], dir[1]]) as usize;
    let mut off = 2usize;
    for _ in 0..cnt {
        if off + 2 > len {
            break;
        }
        let ty = dir[off];
        let nl = dir[off + 1] as usize;
        if off + 2 + nl > len {
            break;
        }
        if &dir[off + 2..off + 2 + nl] == name {
            return Some(ty);
        }
        off += 2 + nl;
    }
    None
}

/// Добавить запись, если её ещё нет. Возвращает новую длину (прежнюю — если не влезло).
pub fn idx_add(dir: &mut [u8], mut len: usize, name: &[u8], is_dir: bool) -> usize {
    if len < 2 {
        dir[0] = 0;
        dir[1] = 0;
        len = 2;
    }
    if idx_type(dir, len, name).is_some() || name.len() > 255 || len + 2 + name.len() > dir.len() {
        return len;
    }
    dir[len] = is_dir as u8;
    dir[len + 1] = name.len() as u8;
    dir[len + 2..len + 2 + name.len()].copy_from_slice(name);
    let cnt = u16::from_le_bytes([dir[0], dir[1]]) + 1;
    dir[0..2].copy_from_slice(&cnt.to_le_bytes());
    len + 2 + name.len()
}

/// Убрать запись (сдвиг хвоста). Возвращает новую длину (не меняется, если записи не было).
pub fn idx_remove(dir: &mut [u8], mut len: usize, name: &[u8]) -> usize {
    if len < 2 {
        return len;
    }
    let cnt = u16::from_le_bytes([dir[0], dir[1]]) as usize;
    let mut off = 2usize;
    for _ in 0..cnt {
        if off + 2 > len {
            break;
        }
        let nl = dir[off + 1] as usize;
        let entry = 2 + nl;
        if off + entry > len {
            break;
        }
        if &dir[off + 2..off + 2 + nl] == name {
            dir.copy_within(off + entry..len, off);
            len -= entry;
            dir[0..2].copy_from_slice(&((cnt - 1) as u16).to_le_bytes());
            return len;
        }
        off += entry;
    }
    len
}

/// Пуст ли каталог.
pub fn idx_empty(dir: &[u8], len: usize) -> bool {
    len < 2 || u16::from_le_bytes([dir[0], dir[1]]) == 0
}

/// Одна запись каталога.
pub struct Entry<'a> {
    pub ty: u8,
    pub name: &'a [u8],
}

impl Entry<'_> {
    pub fn is_dir(&self) -> bool {
        self.ty == T_DIR
    }
}

/// Перебрать записи индекса. Обрыв на полуслове кончает перебор — половину имени не отдаём.
pub fn entries(dir: &[u8], len: usize) -> Entries<'_> {
    let cnt = if len >= 2 { u16::from_le_bytes([dir[0], dir[1]]) as usize } else { 0 };
    Entries { dir: &dir[..len.min(dir.len())], off: 2, left: cnt }
}

pub struct Entries<'a> {
    dir: &'a [u8],
    off: usize,
    left: usize,
}

impl<'a> Iterator for Entries<'a> {
    type Item = Entry<'a>;

    fn next(&mut self) -> Option<Entry<'a>> {
        if self.left == 0 || self.off + 2 > self.dir.len() {
            return None;
        }
        let ty = self.dir[self.off];
        let nl = self.dir[self.off + 1] as usize;
        if self.off + 2 + nl > self.dir.len() {
            self.left = 0;
            return None;
        }
        let name = &self.dir[self.off + 2..self.off + 2 + nl];
        self.off += 2 + nl;
        self.left -= 1;
        Some(Entry { ty, name })
    }
}

// ─── времена: count(u16 LE) | [nlen(1) | name | mtime(u64 LE)]* ─────────────────────────────
//
// Веха 177. Внутри объекта времени быть не может: содержимое адресуется хэшем, и два одинаковых
// файла — это ОДИН объект с двумя именами, а времена у них разные. Значит время живёт снаружи,
// рядом с именем, — то есть в каталоге. Отдельным корнем, а не полем индекса, потому что индекс
// читают и прежние поколения системы (на них откатываются), а незнакомый корень они не заметят.

/// Время записи `name`, если оно известно. Наносекунды Unix; **отсутствие — не ноль, а `None`**.
pub fn mt_find(t: &[u8], len: usize, name: &[u8]) -> Option<u64> {
    if len < 2 {
        return None;
    }
    let cnt = u16::from_le_bytes([t[0], t[1]]) as usize;
    let mut off = 2usize;
    for _ in 0..cnt {
        if off >= len {
            break;
        }
        let nl = t[off] as usize;
        if off + 9 + nl > len {
            break;
        }
        if &t[off + 1..off + 1 + nl] == name {
            let at = off + 1 + nl;
            let mut v = [0u8; 8];
            v.copy_from_slice(&t[at..at + 8]);
            return Some(u64::from_le_bytes(v));
        }
        off += 9 + nl;
    }
    None
}

/// Убрать время записи. Возвращает новую длину.
pub fn mt_remove(t: &mut [u8], mut len: usize, name: &[u8]) -> usize {
    if len < 2 {
        return len;
    }
    let cnt = u16::from_le_bytes([t[0], t[1]]) as usize;
    let mut off = 2usize;
    for _ in 0..cnt {
        if off >= len {
            break;
        }
        let nl = t[off] as usize;
        let entry = 9 + nl;
        if off + entry > len {
            break;
        }
        if &t[off + 1..off + 1 + nl] == name {
            t.copy_within(off + entry..len, off);
            len -= entry;
            t[0..2].copy_from_slice(&((cnt - 1) as u16).to_le_bytes());
            return len;
        }
        off += entry;
    }
    len
}

/// Проставить время записи, заменив прежнее. Возвращает новую длину.
pub fn mt_set(t: &mut [u8], mut len: usize, name: &[u8], when: u64) -> usize {
    if len < 2 {
        t[0] = 0;
        t[1] = 0;
        len = 2;
    }
    len = mt_remove(t, len, name);
    let entry = 9 + name.len();
    if name.len() > 255 || len + entry > t.len() {
        return len;
    }
    t[len] = name.len() as u8;
    t[len + 1..len + 1 + name.len()].copy_from_slice(name);
    t[len + 1 + name.len()..len + entry].copy_from_slice(&when.to_le_bytes());
    let cnt = u16::from_le_bytes([t[0], t[1]]) + 1;
    t[0..2].copy_from_slice(&cnt.to_le_bytes());
    len + entry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_add_find_remove() {
        let mut d = [0u8; DIR_MAX];
        let mut n = 0;
        n = idx_add(&mut d, n, b"a.txt", false);
        n = idx_add(&mut d, n, b"sub", true);
        assert_eq!(idx_type(&d, n, b"a.txt"), Some(T_FILE));
        assert_eq!(idx_type(&d, n, b"sub"), Some(T_DIR));
        assert_eq!(idx_type(&d, n, b"missing"), None);
        // Повторное добавление НЕ дублирует.
        let same = idx_add(&mut d, n, b"a.txt", false);
        assert_eq!(same, n);
        n = idx_remove(&mut d, n, b"a.txt");
        assert_eq!(idx_type(&d, n, b"a.txt"), None);
        assert_eq!(idx_type(&d, n, b"sub"), Some(T_DIR));
        n = idx_remove(&mut d, n, b"sub");
        assert!(idx_empty(&d, n));
    }

    #[test]
    fn entries_walk_matches_lookup() {
        let mut d = [0u8; DIR_MAX];
        let mut n = 0;
        for name in [b"one".as_slice(), b"two".as_slice(), b"three".as_slice()] {
            n = idx_add(&mut d, n, name, false);
        }
        // Перебор обязан отдать РОВНО то же, что и поиск по одному, — иначе `readdir` и `stat`
        // разойдутся во мнении о том, что в каталоге лежит.
        let mut count = 0;
        for e in entries(&d, n) {
            assert_eq!(idx_type(&d, n, e.name), Some(e.ty));
            count += 1;
        }
        assert_eq!(count, 3);
    }

    #[test]
    fn times_survive_replacement() {
        let mut t = [0u8; MT_MAX];
        let mut n = 0;
        n = mt_set(&mut t, n, b"a", 100);
        n = mt_set(&mut t, n, b"b", 200);
        assert_eq!(mt_find(&t, n, b"a"), Some(100));
        // Замена не плодит вторую запись — иначе каталог рос бы на каждой записи файла.
        let before = n;
        n = mt_set(&mut t, n, b"a", 300);
        assert_eq!(n, before);
        assert_eq!(mt_find(&t, n, b"a"), Some(300));
        assert_eq!(mt_find(&t, n, b"b"), Some(200));
        n = mt_remove(&mut t, n, b"a");
        assert_eq!(mt_find(&t, n, b"a"), None);
        assert_eq!(mt_find(&t, n, b"b"), Some(200));
    }

    #[test]
    fn paths() {
        let mut p = [0u8; PATH_MAX];
        assert_eq!(normalize(b"", &mut p), 1);
        assert_eq!(&p[..1], b"/");
        let n = normalize(b"a.txt", &mut p);
        assert_eq!(&p[..n], b"/a.txt");
        let n = normalize(b"/etc/system/", &mut p);
        assert_eq!(&p[..n], b"/etc/system");
        assert_eq!(parent(b"/etc/system"), b"/etc");
        assert_eq!(parent(b"/etc"), b"/");
        assert_eq!(parent(b"/"), b"/");
        assert_eq!(leaf(b"/etc/system"), b"system");
        let mut r = [0u8; ROOT_MAX];
        let n = root_name(K_DIR, b"/etc", &mut r);
        assert_eq!(&r[..n], b"d/etc");
    }
}
