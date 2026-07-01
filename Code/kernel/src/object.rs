//! Объектная модель + персистентность на диске (Вехи 6, 7.2 и доводка).
//!
//! Два кирпича из [[0002-persistent-content-addressed-capability-core]]:
//! - **Неизменяемые значения**, адресуемые по хэшу содержимого ([`ContentId`]); дедупликация.
//! - **Изменяемый корень** — указатель на значение; мутация = новое значение (история версий).
//!
//! Персистентность (Веха 7.2, см. [[persistence-and-volatility]]): RAM — это КЭШ, диск — истина.
//! Раскладка диска (секторы по 512 Б):
//! ```text
//!   0            суперблок (1 сектор = атомарная запись = точка коммита)
//!   1 .. 1+16    индекс A  ┐ чередуются: суперблок указывает на активный
//!   17 .. 33     индекс B  ┘
//!   33 ..        область объектов (дописывается, никогда не перезаписывается)
//! ```
//! Крах-устойчивость: объекты неизменяемы и только дописываются; индекс пишется в НЕактивный
//! регион; суперблок (одна атомарная запись сектора) переключает активный индекс и корни.
//!
//! Доводка (после Вехи 9):
//! - **Ленивая загрузка** ([[persistent-store]]): `load` читает только ИНДЕКС; байты объекта
//!   подтягиваются с диска при первом обращении в [`with`] (промах кэша). Так «RAM = кэш»
//!   становится правдой, а не фигурой речи — вся база не обязана влезать в RAM.
//! - **Множественные корни**: карта корней сериализуется в отдельный контент-адресуемый
//!   объект (roots-blob), его адрес лежит в суперблоке. Персистентен любой именованный корень,
//!   не только «system».

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use void_abi::ContentId;

use crate::sync::SpinLock;
use crate::virtio_blk::{self, SECTOR_SIZE as SECTOR};

// ─── раскладка диска ─────────────────────────────────────────────────────────
const MAGIC: u64 = 0x0003_5346_4449_4F56; // "VOIDFS\x03\x00" (v3: контент-адреса на BLAKE3)
const SB_SECTOR: u64 = 0;
const IDX_SECTORS: u64 = 16;
const IDX_A: u64 = 1;
const IDX_B: u64 = IDX_A + IDX_SECTORS;
const OBJ_START: u64 = IDX_B + IDX_SECTORS; // первый сектор области объектов

const ENTRY_SIZE: usize = 64; // делит 512 → 8 записей в секторе, без «нахлёста»
const ENTRIES_PER_SECTOR: usize = SECTOR / ENTRY_SIZE;

// смещения полей суперблока
const SB_MAGIC: usize = 0;
const SB_GENERATION: usize = 8;
const SB_ACTIVE: usize = 16;
const SB_NEXT_FREE: usize = 20;
const SB_COUNT: usize = 24;
const SB_ROOTS_PRESENT: usize = 28;
const SB_ROOTS_ID: usize = 32; // [u8; 32] — контент-адрес roots-blob

/// Объект: (кэшированные) байты и/или местоположение на диске.
/// Инвариант: хотя бы одно из полей `Some` (иначе объект «ниоткуда»).
struct Object {
    /// Байты значения в RAM. `None` — ещё не подгружены с диска (промах кэша).
    bytes: Option<Vec<u8>>,
    /// (сектор, длина) на диске. `None` — только в RAM, ещё не зафиксирован коммитом.
    disk: Option<(u32, u32)>,
}

struct Store {
    objects: BTreeMap<ContentId, Object>,
    roots: BTreeMap<String, ContentId>,
    next_free: u32,    // следующий свободный сектор области объектов
    generation: u64,   // номер коммита (растёт)
    active_index: u32, // какой индекс-регион активен (0=A, 1=B)
}

impl Store {
    const fn new() -> Self {
        Self {
            objects: BTreeMap::new(),
            roots: BTreeMap::new(),
            next_free: OBJ_START as u32,
            generation: 0,
            active_index: 0,
        }
    }
}

static STORE: SpinLock<Store> = SpinLock::new(Store::new());

// ─── объектная модель ────────────────────────────────────────────────────────

/// Положить значение, получить контент-адрес. Идемпотентно (дедуп по хэшу).
pub fn put(bytes: &[u8]) -> ContentId {
    let id = ContentId::hash(bytes);
    STORE.lock().objects.entry(id).or_insert_with(|| Object {
        bytes: Some(bytes.to_vec()),
        disk: None,
    });
    id
}

/// Прочитать значение по адресу под замком. При промахе кэша (байты не в RAM) —
/// подтянуть их с диска (ленивая загрузка) и закэшировать.
pub fn with<R>(id: &ContentId, f: impl FnOnce(Option<&[u8]>) -> R) -> R {
    let mut store = STORE.lock();
    // Нужна ли подгрузка: объект есть, байтов нет, но известно место на диске.
    let fetch = match store.objects.get(id) {
        Some(o) => o.bytes.is_none().then(|| o.disk).flatten(),
        None => None,
    };
    if let Some((sector, len)) = fetch {
        let bytes = read_object(sector, len as usize); // читает диск (замок BLK), STORE→BLK
        store.objects.get_mut(id).unwrap().bytes = Some(bytes);
    }
    f(store.objects.get(id).and_then(|o| o.bytes.as_deref()))
}

/// Число объектов (известно из индекса даже для неподгруженных).
pub fn len() -> usize {
    STORE.lock().objects.len()
}

/// Номер последнего коммита.
pub fn generation() -> u64 {
    STORE.lock().generation
}

/// Установить/переключить корень `name` (любое имя, персистентен).
pub fn set_root(name: &str, id: ContentId) {
    STORE.lock().roots.insert(String::from(name), id);
}

/// На какое значение указывает корень `name`.
pub fn root(name: &str) -> Option<ContentId> {
    STORE.lock().roots.get(name).copied()
}

// ─── персистентность ─────────────────────────────────────────────────────────

/// Загрузить состояние с диска. `true` — были данные; `false` — чистый диск.
/// ЛЕНИВО: читает только индекс и корни; байты объектов подтянет [`with`] по обращению.
pub fn load() -> bool {
    let mut sb = [0u8; SECTOR];
    if !virtio_blk::read(SB_SECTOR, &mut sb) || get_u64(&sb, SB_MAGIC) != MAGIC {
        return false; // диска нет или он пуст/чужой/старого формата
    }

    let generation = get_u64(&sb, SB_GENERATION);
    let active = get_u32(&sb, SB_ACTIVE);
    let next_free = get_u32(&sb, SB_NEXT_FREE);
    let count = get_u32(&sb, SB_COUNT) as usize;
    let roots_present = get_u32(&sb, SB_ROOTS_PRESENT) != 0;

    // Считать записи индекса из активного региона (8 записей на сектор).
    let idx_start = if active == 0 { IDX_A } else { IDX_B };
    let mut entries: Vec<(ContentId, u32, u32)> = Vec::with_capacity(count);
    let sectors = count.div_ceil(ENTRIES_PER_SECTOR);
    for si in 0..sectors {
        let mut buf = [0u8; SECTOR];
        virtio_blk::read(idx_start + si as u64, &mut buf);
        for e in 0..ENTRIES_PER_SECTOR {
            let gi = si * ENTRIES_PER_SECTOR + e;
            if gi >= count {
                break;
            }
            let off = e * ENTRY_SIZE;
            let mut id = [0u8; 32];
            id.copy_from_slice(&buf[off..off + 32]);
            entries.push((ContentId(id), get_u32(&buf, off + 32), get_u32(&buf, off + 36)));
        }
    }

    let mut store = STORE.lock();
    store.objects.clear();
    store.roots.clear();
    // Только МЕТАданные: место на диске, байты не читаем (ленивая загрузка).
    for (id, sector, len) in entries {
        store.objects.insert(id, Object { bytes: None, disk: Some((sector, len)) });
    }
    // Корни: подтянуть roots-blob по адресу из суперблока и разобрать.
    if roots_present {
        let mut rid = [0u8; 32];
        rid.copy_from_slice(&sb[SB_ROOTS_ID..SB_ROOTS_ID + 32]);
        let rid = ContentId(rid);
        if let Some((sector, len)) = store.objects.get(&rid).and_then(|o| o.disk) {
            let blob = read_object(sector, len as usize);
            store.roots = deserialize_roots(&blob);
            store.objects.get_mut(&rid).unwrap().bytes = Some(blob); // закэшировать
        }
    }
    store.next_free = next_free;
    store.generation = generation;
    store.active_index = active;
    true
}

/// Зафиксировать текущее состояние на диск (checkpoint). Крах-устойчиво: новые объекты
/// дописываются, индекс — в неактивный регион, суперблок пишется последним (точка коммита).
pub fn commit() {
    let mut store = STORE.lock();

    // 0) Сериализовать корни в контент-адресуемый объект (roots-blob) и учесть его.
    let roots_blob = serialize_roots(&store.roots);
    let roots_id = ContentId::hash(&roots_blob);
    store.objects.entry(roots_id).or_insert_with(|| Object {
        bytes: Some(roots_blob),
        disk: None,
    });

    // 1) Дописать объекты, которых ещё нет на диске (append-only, они неизменяемы).
    let to_write: Vec<ContentId> = store
        .objects
        .iter()
        .filter(|(_, o)| o.disk.is_none())
        .map(|(id, _)| *id)
        .collect();
    for id in to_write {
        // У неперсистентного объекта байты всегда в RAM (put/roots-blob) — можно клонировать.
        let bytes = store.objects[&id].bytes.clone().expect("объект без байтов и без диска");
        let sector = store.next_free;
        write_object(sector, &bytes);
        store.next_free += bytes.len().div_ceil(SECTOR) as u32;
        store.objects.get_mut(&id).unwrap().disk = Some((sector, bytes.len() as u32));
    }

    // 2) Записать индекс в НЕактивный регион (у всех объектов теперь есть место на диске).
    let new_active = store.active_index ^ 1;
    let idx_start = if new_active == 0 { IDX_A } else { IDX_B };
    let entries: Vec<(ContentId, u32, u32)> = store
        .objects
        .iter()
        .map(|(id, o)| {
            let (sector, len) = o.disk.unwrap();
            (*id, sector, len)
        })
        .collect();
    let count = entries.len();
    let sectors = count.div_ceil(ENTRIES_PER_SECTOR);
    for si in 0..sectors {
        let mut buf = [0u8; SECTOR];
        for e in 0..ENTRIES_PER_SECTOR {
            let gi = si * ENTRIES_PER_SECTOR + e;
            if gi >= count {
                break;
            }
            let (id, sector, len) = entries[gi];
            let off = e * ENTRY_SIZE;
            buf[off..off + 32].copy_from_slice(&id.0);
            put_u32(&mut buf, off + 32, sector);
            put_u32(&mut buf, off + 36, len);
        }
        virtio_blk::write(idx_start + si as u64, &buf);
    }

    // 3) Суперблок — последним. Одна запись сектора = атомарная точка коммита.
    let generation = store.generation + 1;
    let mut sb = [0u8; SECTOR];
    put_u64(&mut sb, SB_MAGIC, MAGIC);
    put_u64(&mut sb, SB_GENERATION, generation);
    put_u32(&mut sb, SB_ACTIVE, new_active);
    put_u32(&mut sb, SB_NEXT_FREE, store.next_free);
    put_u32(&mut sb, SB_COUNT, count as u32);
    put_u32(&mut sb, SB_ROOTS_PRESENT, 1);
    sb[SB_ROOTS_ID..SB_ROOTS_ID + 32].copy_from_slice(&roots_id.0);
    virtio_blk::write(SB_SECTOR, &sb);

    store.generation = generation;
    store.active_index = new_active;
}

// ─── (де)сериализация корней ──────────────────────────────────────────────────
// Формат roots-blob: count(u32) | [ name_len(u32) | name | ContentId(32) ] * count

fn serialize_roots(roots: &BTreeMap<String, ContentId>) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&(roots.len() as u32).to_le_bytes());
    for (name, id) in roots {
        buf.extend_from_slice(&(name.len() as u32).to_le_bytes());
        buf.extend_from_slice(name.as_bytes());
        buf.extend_from_slice(&id.0);
    }
    buf
}

fn deserialize_roots(bytes: &[u8]) -> BTreeMap<String, ContentId> {
    let mut roots = BTreeMap::new();
    if bytes.len() < 4 {
        return roots;
    }
    let count = get_u32(bytes, 0);
    let mut off = 4;
    for _ in 0..count {
        if off + 4 > bytes.len() {
            break;
        }
        let nl = get_u32(bytes, off) as usize;
        off += 4;
        if off + nl + 32 > bytes.len() {
            break;
        }
        let name = String::from_utf8_lossy(&bytes[off..off + nl]).into_owned();
        off += nl;
        let mut id = [0u8; 32];
        id.copy_from_slice(&bytes[off..off + 32]);
        off += 32;
        roots.insert(name, ContentId(id));
    }
    roots
}

// ─── помощники дисковой (де)сериализации ──────────────────────────────────────

/// Прочитать объект длиной `len` байт, начиная с сектора `start`.
fn read_object(start: u32, len: usize) -> Vec<u8> {
    let sectors = len.div_ceil(SECTOR);
    let mut out = Vec::with_capacity(sectors * SECTOR);
    for i in 0..sectors {
        let mut buf = [0u8; SECTOR];
        virtio_blk::read(start as u64 + i as u64, &mut buf);
        out.extend_from_slice(&buf);
    }
    out.truncate(len);
    out
}

/// Записать байты объекта, начиная с сектора `start` (последний сектор дополняется нулями).
fn write_object(start: u32, bytes: &[u8]) {
    let sectors = bytes.len().div_ceil(SECTOR);
    for i in 0..sectors {
        let mut buf = [0u8; SECTOR];
        let off = i * SECTOR;
        let end = (off + SECTOR).min(bytes.len());
        buf[..end - off].copy_from_slice(&bytes[off..end]);
        virtio_blk::write(start as u64 + i as u64, &buf);
    }
}

fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
fn get_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}
fn get_u64(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}
