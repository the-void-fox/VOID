//! Объектная модель + персистентность на диске (Вехи 6, 7.2 и доводка).
//!
//! Два кирпича из [[0002-persistent-content-addressed-capability-core]]:
//! - **Неизменяемые значения**, адресуемые по хэшу содержимого ([`ContentId`], BLAKE3); дедуп.
//! - **Изменяемый корень** — указатель на значение; мутация = новое значение (история версий).
//!
//! Объекты образуют **граф**: объект может ссылаться на другие через их `ContentId` (children).
//! На диске/в хэше это единый КАДР: `[ nchildren(u32) | child-id×n | payload ]`. Адрес покрывает
//! и ссылки, и полезную нагрузку (как дерево git ссылается на blob'ы). Наружу [`with`] отдаёт
//! только payload — вызывающему кадр не виден. Ссылки нужны сборщику мусора для трассировки.
//!
//! Персистентность (Веха 7.2, см. [[persistence-and-volatility]]): RAM — это КЭШ, диск — истина.
//! Раскладка диска (секторы по 512 Б):
//! ```text
//!   0            суперблок (1 сектор = атомарная запись = точка коммита)
//!   1 .. 17      индекс A  ┐ чередуются: суперблок указывает на активный
//!   17 .. 33     индекс B  ┘
//!   33 ..        область объектов (кадры; дописывается, GC уплотняет перезаписью)
//! ```
//! Крах-устойчивость: индекс пишется в НЕактивный регион; суперблок (одна атомарная запись
//! сектора) переключает активный индекс и корни. Сбой до суперблока → грузимся со старого.
//!
//! Доводка (после Вехи 9): ленивая загрузка (load читает лишь индекс), множественные
//! персистентные корни (roots-blob), BLAKE3, и **сборка мусора** ([`gc`]) — см. [[persistent-store]].

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;

use void_abi::ContentId;

use crate::sync::SpinLock;
use crate::virtio_blk::{self, SECTOR_SIZE as SECTOR};

// ─── раскладка диска ─────────────────────────────────────────────────────────
const MAGIC: u64 = 0x0004_5346_4449_4F56; // "VOIDFS\x04\x00" (v4: кадры со ссылками + GC)
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

/// Загруженное содержимое объекта: полезная нагрузка + исходящие ссылки.
struct Loaded {
    payload: Vec<u8>,
    children: Vec<ContentId>,
}

/// Объект: (кэшированное) содержимое и/или местоположение кадра на диске.
/// Инвариант: хотя бы одно из полей `Some`.
struct Object {
    /// Содержимое в RAM. `None` — ещё не подгружено с диска (промах кэша).
    data: Option<Loaded>,
    /// (сектор, длина КАДРА) на диске. `None` — только в RAM, ещё не зафиксирован.
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

// ─── кадр объекта: [ nchildren(u32) | child-id×n | payload ] ───────────────────

fn encode(payload: &[u8], children: &[ContentId]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 + children.len() * 32 + payload.len());
    buf.extend_from_slice(&(children.len() as u32).to_le_bytes());
    for c in children {
        buf.extend_from_slice(&c.0);
    }
    buf.extend_from_slice(payload);
    buf
}

fn decode(frame: &[u8]) -> Loaded {
    let n = get_u32(frame, 0) as usize;
    let mut children = Vec::with_capacity(n);
    let mut off = 4;
    for _ in 0..n {
        let mut id = [0u8; 32];
        id.copy_from_slice(&frame[off..off + 32]);
        children.push(ContentId(id));
        off += 32;
    }
    Loaded { payload: frame[off..].to_vec(), children }
}

// ─── объектная модель ────────────────────────────────────────────────────────

/// Положить лист (значение без исходящих ссылок), получить контент-адрес.
pub fn put(bytes: &[u8]) -> ContentId {
    put_node(bytes, &[])
}

/// Положить узел: значение + исходящие ссылки на другие объекты. Адрес покрывает и то, и
/// другое (одинаковый узел с одинаковыми детьми → один адрес). Идемпотентно (дедуп).
pub fn put_node(bytes: &[u8], children: &[ContentId]) -> ContentId {
    let frame = encode(bytes, children);
    let id = ContentId::hash(&frame);
    STORE.lock().objects.entry(id).or_insert_with(|| Object {
        data: Some(Loaded { payload: bytes.to_vec(), children: children.to_vec() }),
        disk: None,
    });
    id
}

/// Прочитать полезную нагрузку по адресу под замком. При промахе кэша — подтянуть кадр с
/// диска (ленивая загрузка), разобрать и закэшировать.
pub fn with<R>(id: &ContentId, f: impl FnOnce(Option<&[u8]>) -> R) -> R {
    let mut store = STORE.lock();
    let ok = ensure_loaded(&mut store, id);
    let payload = if ok {
        store.objects.get(id).and_then(|o| o.data.as_ref()).map(|d| d.payload.as_slice())
    } else {
        None
    };
    f(payload)
}

/// Исходящие ссылки объекта (подгружает при необходимости).
pub fn children(id: &ContentId) -> Vec<ContentId> {
    let mut store = STORE.lock();
    if ensure_loaded(&mut store, id) {
        store.objects.get(id).and_then(|o| o.data.as_ref()).map(|d| d.children.clone()).unwrap_or_default()
    } else {
        Vec::new()
    }
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

/// Подгрузить содержимое объекта в RAM, если оно на диске. Возвращает `true`, если объект
/// существует и данные доступны. Держит замок `STORE`; чтение диска → замок `BLK` (STORE→BLK).
fn ensure_loaded(store: &mut Store, id: &ContentId) -> bool {
    match store.objects.get(id) {
        None => false,
        Some(o) if o.data.is_some() => true,
        Some(o) => {
            let (sector, len) = o.disk.expect("объект без данных и без диска");
            let frame = read_object(sector, len as usize);
            store.objects.get_mut(id).unwrap().data = Some(decode(&frame));
            true
        }
    }
}

// ─── сборка мусора (mark-sweep + уплотнение) ──────────────────────────────────

/// Собрать мусор: оставить только объекты, достижимые (по ссылкам) от корней; остальные —
/// старые версии и осиротевшие `put` — удалить. Уплотнение произойдёт ближайшим [`commit`]
/// (у выживших сбрасывается место на диске → перезапишутся подряд). Возвращает (оставлено,
/// собрано).
pub fn gc() -> (usize, usize) {
    let mut store = STORE.lock();

    // Mark: обход в глубину от корней. Попутно подгружаем объекты (нужны их ссылки).
    let mut reachable: BTreeSet<ContentId> = BTreeSet::new();
    let mut stack: Vec<ContentId> = store.roots.values().copied().collect();
    while let Some(id) = stack.pop() {
        if !reachable.insert(id) {
            continue;
        }
        if ensure_loaded(&mut store, &id) {
            if let Some(d) = store.objects.get(&id).and_then(|o| o.data.as_ref()) {
                for c in &d.children {
                    stack.push(*c);
                }
            }
        }
    }

    // Sweep: удалить недостижимые.
    let before = store.objects.len();
    let dead: Vec<ContentId> = store
        .objects
        .keys()
        .filter(|id| !reachable.contains(id))
        .copied()
        .collect();
    for id in dead {
        store.objects.remove(&id);
    }

    // Подготовить уплотнение: выжившие уже загружены (обошли их при mark) → сбросить их
    // место на диске, чтобы commit переписал всё подряд с начала области объектов.
    for o in store.objects.values_mut() {
        o.disk = None;
    }
    store.next_free = OBJ_START as u32;

    let kept = store.objects.len();
    (kept, before - kept)
}

// ─── персистентность ─────────────────────────────────────────────────────────

/// Загрузить состояние с диска. `true` — были данные; `false` — чистый диск.
/// ЛЕНИВО: читает только индекс и корни; содержимое объектов подтянет [`with`]/[`gc`].
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
    // Только МЕТАданные: место кадра на диске, содержимое не читаем (ленивая загрузка).
    for (id, sector, len) in entries {
        store.objects.insert(id, Object { data: None, disk: Some((sector, len)) });
    }
    // Корни: подтянуть roots-blob по адресу из суперблока и разобрать.
    if roots_present {
        let mut rid = [0u8; 32];
        rid.copy_from_slice(&sb[SB_ROOTS_ID..SB_ROOTS_ID + 32]);
        let rid = ContentId(rid);
        if ensure_loaded(&mut store, &rid) {
            if let Some(d) = store.objects.get(&rid).and_then(|o| o.data.as_ref()) {
                store.roots = deserialize_roots(&d.payload);
            }
        }
    }
    store.next_free = next_free;
    store.generation = generation;
    store.active_index = active;
    true
}

/// Зафиксировать состояние на диск (checkpoint). Крах-устойчиво: новые кадры дописываются,
/// индекс — в неактивный регион, суперблок пишется последним (точка коммита).
pub fn commit() {
    let mut store = STORE.lock();

    // 0) Сериализовать корни в контент-адресуемый объект (roots-blob) и учесть его.
    let roots_blob = serialize_roots(&store.roots);
    let roots_id = ContentId::hash(&encode(&roots_blob, &[]));
    store.objects.entry(roots_id).or_insert_with(|| Object {
        data: Some(Loaded { payload: roots_blob, children: Vec::new() }),
        disk: None,
    });

    // 1) Дописать кадры объектов, которых ещё нет на диске (append-only / уплотнение после GC).
    let to_write: Vec<ContentId> = store
        .objects
        .iter()
        .filter(|(_, o)| o.disk.is_none())
        .map(|(id, _)| *id)
        .collect();
    for id in to_write {
        let d = store.objects[&id].data.as_ref().expect("объект без данных и без диска");
        let frame = encode(&d.payload, &d.children);
        let sector = store.next_free;
        write_object(sector, &frame);
        store.next_free += frame.len().div_ceil(SECTOR) as u32;
        store.objects.get_mut(&id).unwrap().disk = Some((sector, frame.len() as u32));
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
// Формат roots-blob (payload): count(u32) | [ name_len(u32) | name | ContentId(32) ] * count

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

/// Прочитать кадр длиной `len` байт, начиная с сектора `start`.
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

/// Записать кадр, начиная с сектора `start` (последний сектор дополняется нулями).
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
