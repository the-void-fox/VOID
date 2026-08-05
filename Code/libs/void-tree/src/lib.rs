#![cfg_attr(not(test), no_std)]

//! Дерево пакета в объектном store: формат каталога-узла (Вехи 108.1–108.3).
//!
//! Распакованный пакет ложится в store **как есть, деревом объектов**, а не файлами в posixfs:
//!
//! - **файл** — объект (или блоб из кусков, если крупный: объект целиком живёт в куче ядра);
//! - **каталог** — узел, чьё ЗНАЧЕНИЕ есть индекс записей (этот модуль), а исходящие ссылки —
//!   содержимое этих записей, по порядку;
//! - **симлинк** — объект, значение которого есть цель ссылки.
//!
//! Почему в store, а не через posixfs, хотя распаковка Вехи 105 шла именно через него: у пакета
//! другие свойства. Он **неизменяем**, он **общий** между поколениями и профилями, и он большой.
//! Content-адресация даёт на этом ровно то, что нужно, — дедуп одинаковых файлов между пакетами,
//! достижимость вместо «списка не удалять», а GC подберёт то, на что перестали ссылаться. У
//! posixfs же потолки его собственной задачи (путь, размер файла, индекс каталога) — они разумны
//! для конфигов и мешают пакетам. Показывает это дерево файловым API он же (Веха 108.2),
//! **читая тот же формат** через этот модуль: раскладка остаётся одной на всех.
//!
//! ## Формат индекса каталога
//!
//! ```text
//! "vt1\0" | count(u32 LE) | { type(1) | nlen(1) | name | size(u64 LE) }*
//! ```
//!
//! `size` — размер файла в байтах (у каталога — число записей, у симлинка — длина цели): чтобы
//! `stat` и `ls` не читали каждый объект ради одного числа. Порядок записей = порядок исходящих
//! ссылок узла: i-я запись описывает i-го ребёнка.
//!
//! **Модуль без `alloc` намеренно.** Его читает `posixfs`, живущий вообще без кучи, поэтому
//! запись идёт в чужой буфер (`Extend<u8>` из `core`), а чтение — итератором по ломтям, без
//! единой копии. Иначе формат пришлось бы разложить на две реализации — ровно то, ради чего его
//! сюда и выносили.

/// Заголовок индекса — чтобы разбор мог отличить своё от чужого, а не гадать по первым байтам.
pub const MAGIC: &[u8; 4] = b"vt1\0";

/// Длина заголовка: магия + счётчик записей.
pub const HEAD: usize = 8;

/// Вид записи (младшие три бита типа).
pub const K_FILE: u8 = 0;
pub const K_DIR: u8 = 1;
pub const K_LINK: u8 = 2;

/// Исполняемый бит (в NAR он у файла, у нас — у записи родителя: так вид и права лежат вместе).
pub const F_EXEC: u8 = 0x08;
/// Содержимое лежит БЛОБОМ (манифест + куски), а не одним объектом.
pub const F_BLOB: u8 = 0x10;

/// Вид записи без флагов.
pub fn kind(ty: u8) -> u8 {
    ty & 0x07
}

pub fn is_dir(ty: u8) -> bool {
    kind(ty) == K_DIR
}

pub fn is_link(ty: u8) -> bool {
    kind(ty) == K_LINK
}

pub fn is_exec(ty: u8) -> bool {
    ty & F_EXEC != 0
}

pub fn is_blob(ty: u8) -> bool {
    ty & F_BLOB != 0
}

/// Заголовок индекса на `count` записей — пишется перед телом, когда оно уже собрано.
pub fn head(count: u32) -> [u8; HEAD] {
    let mut h = [0u8; HEAD];
    h[..4].copy_from_slice(MAGIC);
    h[4..].copy_from_slice(&count.to_le_bytes());
    h
}

/// Дописать запись в тело индекса.
///
/// Имена в NAR отсортированы, и порядок сохраняется как есть: он часть содержимого (от него
/// зависит content-id), и переупорядочивать его значило бы получать разные адреса для одного и
/// того же дерева.
pub fn push<E: Extend<u8>>(out: &mut E, ty: u8, name: &[u8], size: u64) {
    let n = name.len().min(255);
    out.extend([ty, n as u8]);
    out.extend(name[..n].iter().copied());
    out.extend(size.to_le_bytes());
}

/// Одна запись каталога — ломтями исходного буфера, без копий.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rec<'a> {
    pub ty: u8,
    pub name: &'a [u8],
    pub size: u64,
}

impl Rec<'_> {
    pub fn is_dir(&self) -> bool {
        is_dir(self.ty)
    }
    pub fn is_link(&self) -> bool {
        is_link(self.ty)
    }
    pub fn is_exec(&self) -> bool {
        is_exec(self.ty)
    }
    pub fn is_blob(&self) -> bool {
        is_blob(self.ty)
    }
}

/// Обход записей индекса. `None` — это не индекс каталога (чужое значение либо обрезанное).
pub fn iter(buf: &[u8]) -> Option<Iter<'_>> {
    if buf.len() < HEAD || &buf[..4] != MAGIC {
        return None;
    }
    let count = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    Some(Iter { buf, off: HEAD, left: count })
}

/// Сколько записей объявлено в индексе (без обхода).
pub fn count(buf: &[u8]) -> Option<u32> {
    iter(buf).map(|i| i.left)
}

pub struct Iter<'a> {
    buf: &'a [u8],
    off: usize,
    left: u32,
}

impl<'a> Iterator for Iter<'a> {
    type Item = Rec<'a>;

    fn next(&mut self) -> Option<Rec<'a>> {
        if self.left == 0 {
            return None;
        }
        let b = self.buf;
        if self.off + 2 > b.len() {
            self.left = 0;
            return None;
        }
        let ty = b[self.off];
        let nlen = b[self.off + 1] as usize;
        let name_at = self.off + 2;
        if name_at + nlen + 8 > b.len() {
            self.left = 0; // индекс обрезан — молча отдавать половину нельзя
            return None;
        }
        let name = &b[name_at..name_at + nlen];
        let mut sz = [0u8; 8];
        sz.copy_from_slice(&b[name_at + nlen..name_at + nlen + 8]);
        self.off = name_at + nlen + 8;
        self.left -= 1;
        Some(Rec { ty, name, size: u64::from_le_bytes(sz) })
    }
}

/// Найти запись по имени: `(порядковый номер, запись)`. Номер — это и номер исходящей ссылки узла.
pub fn find<'a>(buf: &'a [u8], name: &[u8]) -> Option<(usize, Rec<'a>)> {
    iter(buf)?.enumerate().find(|(_, r)| r.name == name)
}

/// Формат БЛОБА — содержимого, которое не влезло одним объектом: узел-манифест плюс куски
/// обычными объектами.
///
/// Живёт здесь, рядом с деревом, потому что читателей у него ровно те же трое и по той же
/// причине: так им кладёт скачанное загрузчик (Веха 94), так же `pkg` кладёт крупные файлы
/// пакета, и так же их читают `posixfs` и персоналия Linux в ядре. Разойтись двум копиям этой
/// раскладки — значит однажды прочитать чужие байты как свои.
pub mod blob {
    /// Опознавательный знак манифеста.
    pub const MAGIC: &[u8; 9] = b"VOIDBLOB1";

    /// Собрать манифест: magic, общая длина, число кусков, размер куска.
    pub fn manifest(total: usize, chunks: usize, chunk: usize) -> [u8; MAGIC.len() + 16] {
        let mut m = [0u8; MAGIC.len() + 16];
        let mut p = 0;
        m[p..p + MAGIC.len()].copy_from_slice(MAGIC);
        p += MAGIC.len();
        m[p..p + 8].copy_from_slice(&(total as u64).to_le_bytes());
        p += 8;
        m[p..p + 4].copy_from_slice(&(chunks as u32).to_le_bytes());
        p += 4;
        // Размер куска — в манифесте, а не в коде читателя: иначе смена константы в будущем
        // сделала бы уже лежащие в сторе блобы нечитаемыми.
        m[p..p + 4].copy_from_slice(&(chunk as u32).to_le_bytes());
        m
    }

    /// Разобрать манифест: `(общая длина, число кусков, размер куска)`. `None` — это не блоб.
    ///
    /// `default_chunk` подставляется блобам ПЕРВОГО вида (Веха 94): там на месте размера куска
    /// стояла длина первого куска, что для полного куска с ним совпадает.
    pub fn info(manifest: &[u8], default_chunk: usize) -> Option<(usize, usize, usize)> {
        if manifest.len() < MAGIC.len() + 12 || &manifest[..MAGIC.len()] != MAGIC {
            return None;
        }
        let total = u64::from_le_bytes(manifest[MAGIC.len()..MAGIC.len() + 8].try_into().ok()?);
        let n = u32::from_le_bytes(manifest[MAGIC.len() + 8..MAGIC.len() + 12].try_into().ok()?);
        let chunk = manifest
            .get(MAGIC.len() + 12..MAGIC.len() + 16)
            .and_then(|s| s.try_into().ok())
            .map_or(default_chunk, |b| u32::from_le_bytes(b) as usize);
        Some((total as usize, n as usize, if chunk == 0 { default_chunk } else { chunk }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate alloc;
    use alloc::vec::Vec;

    #[test]
    fn roundtrip() {
        let mut body: Vec<u8> = Vec::new();
        push(&mut body, K_DIR, b"lib", 48);
        push(&mut body, K_LINK, b"lib64", 3);
        push(&mut body, K_FILE | F_EXEC | F_BLOB, b"libc.so.6", 2413096);
        let mut idx = Vec::from(head(3));
        idx.extend_from_slice(&body);

        let recs: Vec<Rec> = iter(&idx).expect("это индекс").collect();
        assert_eq!(recs.len(), 3);
        assert!(recs[0].is_dir() && recs[0].size == 48);
        assert!(recs[1].is_link() && recs[1].name == b"lib64");
        assert!(recs[2].is_exec() && recs[2].is_blob() && recs[2].size == 2413096);

        let (i, r) = find(&idx, b"libc.so.6").expect("найдено");
        assert_eq!(i, 2);
        assert_eq!(r.size, 2413096);
        assert!(find(&idx, "нет такого".as_bytes()).is_none());
    }

    /// Чужое значение индексом не притворяется, а обрезанное не отдаёт половину.
    #[test]
    fn foreign_and_truncated() {
        assert!(iter(b"nix-archive-1").is_none());
        let mut idx = Vec::from(head(2));
        push(&mut idx, K_FILE, "есть".as_bytes(), 1);
        push(&mut idx, K_FILE, "обрежется".as_bytes(), 2);
        idx.truncate(idx.len() - 4);
        let recs: Vec<Rec> = iter(&idx).unwrap().collect();
        assert_eq!(recs.len(), 1, "вторая запись обрезана — её не должно быть");
    }
}
