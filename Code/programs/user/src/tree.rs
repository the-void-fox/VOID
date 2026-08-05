//! Дерево пакета в объектном store: формат каталога-узла (Веха 108).
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
//! posixfs же потолки его собственной задачи (путь 128 байт, файл 128 КиБ, индекс каталога 4 КиБ)
//! — они разумны для конфигов и мешают пакетам. Показывать это дерево файловым API будет он же,
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

use alloc::string::String;
use alloc::vec::Vec;

/// Заголовок индекса — чтобы разбор мог отличить своё от чужого, а не гадать по первым байтам.
pub const MAGIC: &[u8; 4] = b"vt1\0";

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

/// Одна запись каталога.
#[derive(Clone, Debug, PartialEq)]
pub struct Rec {
    pub ty: u8,
    pub name: String,
    pub size: u64,
}

impl Rec {
    pub fn is_dir(&self) -> bool {
        kind(self.ty) == K_DIR
    }
    pub fn is_link(&self) -> bool {
        kind(self.ty) == K_LINK
    }
    pub fn is_exec(&self) -> bool {
        self.ty & F_EXEC != 0
    }
    pub fn is_blob(&self) -> bool {
        self.ty & F_BLOB != 0
    }
}

/// Сборщик индекса каталога.
#[derive(Default)]
pub struct Index {
    buf: Vec<u8>,
    count: u32,
}

impl Index {
    pub fn new() -> Self {
        Index { buf: Vec::new(), count: 0 }
    }

    /// Добавить запись. Имена в NAR отсортированы, и мы сохраняем их порядок как есть: он часть
    /// содержимого (от него зависит content-id), и переупорядочивать его значило бы получать
    /// разные адреса для одного и того же дерева.
    pub fn add(&mut self, ty: u8, name: &str, size: u64) {
        self.buf.push(ty);
        self.buf.push(name.len().min(255) as u8);
        self.buf.extend_from_slice(&name.as_bytes()[..name.len().min(255)]);
        self.buf.extend_from_slice(&size.to_le_bytes());
        self.count += 1;
    }

    pub fn len(&self) -> u32 {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Готовое значение узла-каталога.
    pub fn finish(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.buf.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.count.to_le_bytes());
        out.extend_from_slice(&self.buf);
        out
    }
}

/// Разобрать индекс каталога. `None` — это не индекс (чужое значение либо обрезанное).
pub fn parse(buf: &[u8]) -> Option<Vec<Rec>> {
    if buf.len() < 8 || &buf[..4] != MAGIC {
        return None;
    }
    let count = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    let mut out = Vec::with_capacity(count.min(4096));
    let mut off = 8usize;
    for _ in 0..count {
        if off + 2 > buf.len() {
            return None;
        }
        let ty = buf[off];
        let nlen = buf[off + 1] as usize;
        off += 2;
        if off + nlen + 8 > buf.len() {
            return None;
        }
        let name = core::str::from_utf8(&buf[off..off + nlen]).ok()?;
        off += nlen;
        let mut sz = [0u8; 8];
        sz.copy_from_slice(&buf[off..off + 8]);
        off += 8;
        out.push(Rec { ty, name: String::from(name), size: u64::from_le_bytes(sz) });
    }
    Some(out)
}
