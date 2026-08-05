//! void-store — объектная модель + персистентность на диске (Вехи 6, 7.2, вынос — Веха 29,
//! политика коммитов — Веха 33).
//!
//! Два кирпича из [[0002-persistent-content-addressed-capability-core]]:
//! - **Неизменяемые значения**, адресуемые по хэшу содержимого ([`ContentId`], BLAKE3); дедуп.
//! - **Изменяемый корень** — указатель на значение; мутация = новое значение (история версий).
//!
//! Объекты образуют **граф**: объект может ссылаться на другие через их `ContentId` (children).
//! На диске/в хэше это единый КАДР: `[ nchildren(u32) | child-id×n | payload ]`. Адрес покрывает
//! и ссылки, и полезную нагрузку (как дерево git ссылается на blob'ы). Наружу [`Store::with`]
//! отдаёт только payload — вызывающему кадр не виден. Ссылки нужны сборщику мусора.
//!
//! Персистентность: RAM — это КЭШ, диск — истина. Раскладка диска v2 (секторы по 512 Б):
//! ```text
//!   0            суперблок (1 сектор = атомарная запись = точка коммита)
//!   1 .. 33     (зарезервировано: A/B-регионы формата v1; v2 их не использует)
//!   33 ..        область объектов: кадры объектов И индекс-кадры, append-only
//! ```
//! **Индекс — дельта-цепочка в области объектов** (Веха 33, [[commit-policy]]): суперблок
//! указывает (сектор, длина) ПОСЛЕДНЕГО индекс-кадра; каждый кадр несёт ссылку на предыдущий,
//! новые записи и надгробия (id, удалённые GC). Загрузка реплеит цепочку от старейшего к
//! новейшему; уплотнение ([`Store::compact`]) пишет полную базу (prev = 0) и обнуляет мусор.
//! Итог: коммит пишет O(изменений), а не O(store) — «убийца SSD» обезврежен. A/B-регионы
//! больше не нужны: атомарность даёт один сектор суперблока (пишется последним, после данных).
//!
//! **Group commit**: сами [`Store::set_root`]/[`Store::del_root`] диск не трогают — копится
//! [`Store::dirty_ops`]; политику (порог/период/точки синка) держит владелец (ядро — в
//! планировщике, хост-утилита — явный `commit` в конце операции).
//!
//! Крах-устойчивость: всё новое дописывается в свободные секторы, суперблок — одна атомарная
//! запись — переключает состояние целиком. Сбой до суперблока → грузимся со старого, окно
//! несинхронизированных операций теряется (честная цена group commit, как у ext4-журнала).
//!
//! **Веха 29:** логика вынесена из ядра в этот `no_std`-крейт. Носитель абстрагирован
//! трейтом [`BlockIo`]: ядро подставляет virtio-blk, хост-утилиты (`void-store-import`) —
//! файл-образ. Один формат — одна реализация. Ядро оборачивает [`Store`] в свой замок; сама
//! библиотека синхронизации не делает. Образы v1 (MAGIC v4) читаются и мигрируют первым
//! коммитом (кадры объектов не двигаются — переезжает только индекс).

#![no_std]

extern crate alloc;

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;

use void_abi::ContentId;

/// Размер сектора носителя. Весь формат считает в таких единицах.
pub const SECTOR: usize = 512;

/// Носитель store — минимальный контракт блочного устройства.
/// `false` из `read`/`write` — устройства нет или операция не удалась.
pub trait BlockIo {
    fn read(&mut self, sector: u64, buf: &mut [u8; SECTOR]) -> bool;
    fn write(&mut self, sector: u64, buf: &[u8; SECTOR]) -> bool;
    /// Веха 89 — ёмкость носителя в секторах. `0` — неизвестна, тогда store не проверяет границу
    /// (поведение до Вехи 89: писать, пока носитель принимает).
    ///
    /// До этого трейт размер не сообщал вовсе, а `next_free` рос без оглядки: диск кончался
    /// МОЛЧА — запись за краем либо отвергалась драйвером, либо (на образе-файле) уходила в
    /// никуда. Для системы, которая собирается тащить в store мегабайтные пакеты из сети, это
    /// первый отказ, который случится на практике.
    fn capacity(&mut self) -> u64 {
        0
    }
}

// ─── раскладка диска ─────────────────────────────────────────────────────────
const MAGIC_V1: u64 = 0x0004_5346_4449_4F56; // "VOIDFS\x04\x00" — формат с A/B-индексом
const MAGIC: u64 = 0x0005_5346_4449_4F56; // "VOIDFS\x05\x00" — v2: индекс дельта-цепочкой
const SB_SECTOR: u64 = 0;
const IDX_SECTORS: u64 = 16;
const IDX_A: u64 = 1;
const IDX_B: u64 = IDX_A + IDX_SECTORS;
/// Область объектов начинается там же, где в v1 (миграция не двигает кадры);
/// секторы 1..33 в v2 просто зарезервированы.
const OBJ_START: u64 = IDX_B + IDX_SECTORS;

/// v1: 64 Б на запись A/B-индекса (8 в секторе). Нужен только загрузчику v1.
const ENTRY_SIZE_V1: usize = 64;
const ENTRIES_PER_SECTOR_V1: usize = SECTOR / ENTRY_SIZE_V1;

/// Запись индекс-кадра v2: id(32) + сектор(4) + длина(4).
const ENTRY_SIZE: usize = 40;
/// «Сектор» надгробия: запись с ним означает «объект id удалён GC».
const TOMBSTONE: u32 = u32::MAX;
/// Длиннее этой цепочка дельт не растёт — следующий коммит пишет полную базу
/// (иначе загрузка после тысяч коммитов читала бы тысячи кадров).
const CHAIN_MAX: u32 = 64;

// смещения полей суперблока v2
const SB_MAGIC: usize = 0;
const SB_GENERATION: usize = 8;
const SB_NEXT_FREE: usize = 16;
const SB_IDX_SECTOR: usize = 20; // последний индекс-кадр (0 — индекса нет, store пуст)
const SB_IDX_LEN: usize = 24;
const SB_GARBAGE: usize = 32; // u64: мусорных байт в области объектов (для порога уплотнения)
const SB_ROOTS_PRESENT: usize = 40;
const SB_ROOTS_ID: usize = 44; // [u8; 32] — контент-адрес roots-blob

// смещения полей суперблока v1 (только для миграции)
const SB1_GENERATION: usize = 8;
const SB1_ACTIVE: usize = 16;
const SB1_NEXT_FREE: usize = 20;
const SB1_COUNT: usize = 24;
const SB1_ROOTS_PRESENT: usize = 28;
const SB1_ROOTS_ID: usize = 32;

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

/// Персистентный контент-адресуемый store. Владелец решает, как его защищать
/// (ядро — SpinLock) и через какой [`BlockIo`] говорить с носителем — и КОГДА
/// коммитить ([`Store::dirty_ops`] + [[commit-policy]]).
pub struct Store {
    objects: BTreeMap<ContentId, Object>,
    roots: BTreeMap<String, ContentId>,
    next_free: u32,  // следующий свободный сектор области объектов
    generation: u64, // номер коммита (растёт)
    /// Последний записанный индекс-кадр (сектор, длина); (0,0) — ещё не было.
    index_tail: (u32, u32),
    /// Длина текущей цепочки дельт (для форса базы по [`CHAIN_MAX`]).
    chain_len: u32,
    /// Мусор в области объектов, байт (кадры, чьи объекты удалены GC).
    garbage: u64,
    /// Надгробия, ожидающие фиксации: (id, длина кадра погибшего).
    pending_dead: Vec<(ContentId, u32)>,
    /// Операций над корнями с последнего коммита (политика group commit — у владельца).
    dirty_ops: u32,
    /// roots-blob последнего коммита — чтобы no-op commit не писал ничего.
    last_roots_id: Option<ContentId>,
    /// Следующий коммит обязан писать полную базу индекса (миграция v1 / после compact).
    need_base: bool,
    /// Всего байт записано на носитель за сессию (статистика для честных замеров).
    bytes_written: u64,
    /// Веха 89 — сколько раз кадр с диска НЕ сошёлся со своим content-id (или носитель не отдал
    /// сектор). Ненулевое значение = носитель врёт или портит; объект при этом не подменяется
    /// мусором, а считается недоступным ([`Store::ensure_loaded`]).
    corrupt_reads: u64,
    /// Веха 89 — сколько записей носитель не принял. Ненулевое значение означает, что коммит
    /// НЕ состоялся целиком, и на диске осталось прежнее консистентное состояние.
    failed_writes: u64,
    /// Веха 89 — сколько коммитов отменено из-за нехватки места на носителе.
    out_of_space: u64,
}

impl Store {
    pub const fn new() -> Self {
        Self {
            objects: BTreeMap::new(),
            roots: BTreeMap::new(),
            next_free: OBJ_START as u32,
            generation: 0,
            index_tail: (0, 0),
            chain_len: 0,
            garbage: 0,
            pending_dead: Vec::new(),
            dirty_ops: 0,
            last_roots_id: None,
            need_base: true, // первый коммит пустого/нового store — база
            bytes_written: 0,
            corrupt_reads: 0,
            failed_writes: 0,
            out_of_space: 0,
        }
    }

    /// Веха 89 — сколько коммитов отменено из-за нехватки места (носитель полон).
    /// Ненулевое значение — сигнал владельцу: уплотнять ([`Store::compact`]) или расширяться.
    pub fn out_of_space(&self) -> u64 {
        self.out_of_space
    }

    /// Веха 89 — сколько секторов носителя ещё свободно под область объектов.
    /// `None` — ёмкость неизвестна ([`BlockIo::capacity`] вернул 0).
    pub fn sectors_left(&self, io: &mut impl BlockIo) -> Option<u64> {
        let cap = io.capacity();
        (cap != 0).then(|| cap.saturating_sub(self.next_free as u64))
    }

    /// Хватит ли места под `sectors` секторов, начиная с `next_free`.
    fn fits(&self, io: &mut impl BlockIo, sectors: u64) -> bool {
        let cap = io.capacity();
        cap == 0 || self.next_free as u64 + sectors <= cap
    }

    /// Веха 89 — сколько кадров не сошлись со своим content-id (или не прочитались).
    /// Ноль — носитель отдавал ровно то, что у него просили.
    pub fn corrupt_reads(&self) -> u64 {
        self.corrupt_reads
    }

    /// Веха 89 — сколько записей носитель отверг за сессию.
    pub fn failed_writes(&self) -> u64 {
        self.failed_writes
    }

    // ─── объектная модель ────────────────────────────────────────────────────

    /// Положить лист (значение без исходящих ссылок), получить контент-адрес.
    pub fn put(&mut self, bytes: &[u8]) -> ContentId {
        self.put_node(bytes, &[])
    }

    /// Положить узел: значение + исходящие ссылки на другие объекты. Адрес покрывает и то, и
    /// другое (одинаковый узел с одинаковыми детьми → один адрес). Идемпотентно (дедуп).
    /// Веха 104 — то же, но **без паники при нехватке памяти**: `None` вместо аварии.
    ///
    /// Куча ядра конечна, а размеры в пакетной фазе приходят из сети и из чужих архивов, то есть
    /// не под нашим контролем. Аллокатор Rust на нехватку ПАНИКУЕТ — то есть один оверсайзный
    /// объект убивал бы систему целиком вместо честного «не влезло». Резервируем заранее через
    /// `try_reserve`, и большие куски (а это ровно тот случай, что опасен) отказывают мягко.
    ///
    /// **Оговорка:** полной устойчивости к нехватке это не даёт — вставка в `BTreeMap` внутри
    /// по-прежнему может паниковать. Но её узлы малы, а падали мы на полезной нагрузке.
    pub fn try_put_node(&mut self, bytes: &[u8], children: &[ContentId]) -> Option<ContentId> {
        let frame = try_encode(bytes, children)?;
        let id = ContentId::hash(&frame);
        drop(frame); // кадр нужен был только ради адреса — держать его в памяти незачем
        if self.objects.contains_key(&id) {
            return Some(id); // дедуп: ничего не выделяем вовсе
        }
        let mut payload: Vec<u8> = Vec::new();
        payload.try_reserve_exact(bytes.len()).ok()?;
        payload.extend_from_slice(bytes);
        let mut kids: Vec<ContentId> = Vec::new();
        kids.try_reserve_exact(children.len()).ok()?;
        kids.extend_from_slice(children);
        self.objects.insert(id, Object { data: Some(Loaded { payload, children: kids }), disk: None });
        self.dirty_ops += 1;
        Some(id)
    }

    pub fn put_node(&mut self, bytes: &[u8], children: &[ContentId]) -> ContentId {
        let frame = encode(bytes, children);
        let id = ContentId::hash(&frame);
        let mut fresh = false;
        self.objects.entry(id).or_insert_with(|| {
            fresh = true;
            Object {
                data: Some(Loaded { payload: bytes.to_vec(), children: children.to_vec() }),
                disk: None,
            }
        });
        // Веха 101 — НОВЫЙ объект тоже «грязная операция». Раньше счётчик считал только смену
        // корней, поэтому программа, льющая объекты подряд (распаковка пакета, загрузка по
        // сети), не давала политике group commit ни одного повода сработать: несинхронизированное
        // копилось в куче ядра, пока та не кончалась — а кончалась она ПАНИКОЙ, не отказом.
        // Замер (`store-probe`): 8 МиБ пачкой проходили, 16 МиБ роняли машину.
        // Дедуп не считаем: повторный put ничего нового на диск не добавляет.
        if fresh {
            self.dirty_ops += 1;
        }
        id
    }

    /// Прочитать полезную нагрузку по адресу. При промахе кэша — подтянуть кадр с
    /// диска (ленивая загрузка), разобрать и закэшировать.
    pub fn with<R>(
        &mut self,
        io: &mut impl BlockIo,
        id: &ContentId,
        f: impl FnOnce(Option<&[u8]>) -> R,
    ) -> R {
        let payload = if self.ensure_loaded(io, id) {
            self.objects.get(id).and_then(|o| o.data.as_ref()).map(|d| d.payload.as_slice())
        } else {
            None
        };
        f(payload)
    }

    /// Исходящие ссылки объекта (подгружает при необходимости).
    pub fn children(&mut self, io: &mut impl BlockIo, id: &ContentId) -> Vec<ContentId> {
        if self.ensure_loaded(io, id) {
            self.objects.get(id).and_then(|o| o.data.as_ref()).map(|d| d.children.clone()).unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    /// Число объектов (известно из индекса даже для неподгруженных).
    pub fn len(&self) -> usize {
        self.objects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    /// Номер последнего коммита.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Установить/переключить корень `name` (любое имя; персистентен после коммита).
    pub fn set_root(&mut self, name: &str, id: ContentId) {
        self.roots.insert(String::from(name), id);
        self.dirty_ops += 1;
    }

    /// На какое значение указывает корень `name`.
    pub fn root(&self, name: &str) -> Option<ContentId> {
        self.roots.get(name).copied()
    }

    /// Снять корень `name`. Возвращает `true`, если корень существовал. Объект, на который он
    /// указывал, становится недостижимым и уйдёт ближайшим [`Store::gc`] (если больше ни на что
    /// не сослан) — привязку можно не только создать, но и отвязать.
    pub fn del_root(&mut self, name: &str) -> bool {
        let was = self.roots.remove(name).is_some();
        if was {
            self.dirty_ops += 1;
        }
        was
    }

    /// Перечислить корни (имя → адрес). Ядро наружу это не отдаёт (userspace ведёт свой
    /// индекс имён), а хост-утилитам нужно: `void-store-import ls`.
    pub fn roots(&self) -> impl Iterator<Item = (&str, &ContentId)> {
        self.roots.iter().map(|(n, id)| (n.as_str(), id))
    }

    // ─── политика коммитов: что видит владелец ───────────────────────────────

    /// Сколько операций над корнями накопилось с последнего коммита.
    /// 0 — на диске всё актуально (с точностью до RAM-кэша объектов без корней).
    pub fn dirty_ops(&self) -> u32 {
        self.dirty_ops
    }

    /// Мусор в области объектов, байт (жертвы GC, ещё не уплотнены).
    pub fn garbage_bytes(&self) -> u64 {
        self.garbage
    }

    /// Занято областью объектов, байт (включая мусор).
    pub fn area_bytes(&self) -> u64 {
        (self.next_free as u64 - OBJ_START) * SECTOR as u64
    }

    /// Всего байт записано на носитель за сессию (кадры + индекс + суперблоки).
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// Подгрузить содержимое объекта в RAM, если оно на диске. Возвращает `true`, если объект
    /// существует и данные доступны.
    /// Веха 89 — **СВЕРКА ХЭША**. Тезис VOID: «адрес объекта = хэш его содержимого». До этой
    /// вехи он держался на честном слове носителя: кадр читался с диска и принимался как есть,
    /// поэтому битый сектор молча становился «объектом», а сбой чтения — объектом из нулей.
    /// Теперь прочитанный кадр хэшируется и сверяется с `id`; не сошлось (или носитель не отдал
    /// сектор) — объект считается НЕДОСТУПНЫМ (`false`), а не подменяется мусором. Счётчик
    /// расхождений виден в [`Store::corrupt_reads`].
    ///
    /// Цена — один BLAKE3 по кадру на ПЕРВУЮ загрузку объекта (дальше он в кэше). Это ровно то,
    /// что store и так считает при `put`, так что порядок величины известен.
    fn ensure_loaded(&mut self, io: &mut impl BlockIo, id: &ContentId) -> bool {
        match self.objects.get(id) {
            None => false,
            Some(o) if o.data.is_some() => true,
            Some(_) => {
                match self.load_frame(io, id) {
                    Some(frame) => {
                        self.objects.get_mut(id).unwrap().data = Some(decode(&frame));
                        true
                    }
                    None => {
                        self.corrupt_reads += 1;
                        false
                    }
                }
            }
        }
    }

    /// Прочитать кадр объекта с диска и проверить его хэш. `None` — носитель не отдал сектор
    /// ЛИБО содержимое не сходится с content-id. Учёт расхождений — на вызывающем: одну и ту же
    /// порчу могут увидеть и [`Store::ensure_loaded`], и [`Store::repair`], а событие это одно.
    fn load_frame(&self, io: &mut impl BlockIo, id: &ContentId) -> Option<Vec<u8>> {
        let (sector, len) = self.objects.get(id)?.disk?;
        let frame = read_object(io, sector, len as usize)?;
        (ContentId::hash(&frame) == *id).then_some(frame)
    }

    /// Веха 89 — **САМОИЗЛЕЧЕНИЕ**: если объект с таким содержимым известен store'у, но его кадр
    /// на диске не читается или не сходится со своим content-id — заменить содержимое заведомо
    /// верными байтами. `true` — объект был повреждён и починен.
    ///
    /// Работает ровно там, где содержимое можно взять из НАДЁЖНОГО источника: программы-семена
    /// живут в образе ядра, поэтому их кадры восстановимы полностью. Пользовательские данные
    /// восстановить неоткуда — их порча только обнаруживается ([`Store::corrupt_reads`]).
    ///
    /// Старый кадр не переписывается на месте (append-only): он становится учтённым мусором,
    /// а свежий допишется ближайшим коммитом — та же механика, что у обычного `put`.
    pub fn repair(&mut self, io: &mut impl BlockIo, bytes: &[u8], children: &[ContentId]) -> bool {
        let frame = encode(bytes, children);
        let id = ContentId::hash(&frame);
        if !self.objects.contains_key(&id) {
            return false; // объекта нет вовсе — это не порча, а обычный промах (см. `put_node`)
        }
        // Кадр уже в кэше или читается корректно — чинить нечего. Проверяем через `load_frame`,
        // а не `ensure_loaded`: расхождение здесь — та же порча, которую вызывающий уже учёл.
        let cached = self.objects.get(&id).is_some_and(|o| o.data.is_some());
        if cached || self.load_frame(io, &id).is_some() {
            return false;
        }
        let orphan = self.objects.get(&id).and_then(|o| o.disk).map(|(_, len)| len);
        self.objects.insert(
            id,
            Object {
                data: Some(Loaded { payload: bytes.to_vec(), children: children.to_vec() }),
                disk: None,
            },
        );
        if let Some(len) = orphan {
            self.garbage += len as u64; // осиротевший кадр — под нож ближайшему `compact`
        }
        true
    }

    // ─── сборка мусора и уплотнение (разделены — Веха 33) ────────────────────

    /// Собрать мусор: оставить только объекты, достижимые (по ссылкам) от корней; остальные —
    /// старые версии и осиротевшие `put` — удалить из RAM и пометить НАДГРОБИЯМИ (кадры на
    /// диске не трогаются — они становятся учтённым мусором до [`Store::compact`] по порогу).
    /// Возвращает (оставлено, собрано).
    pub fn gc(&mut self, io: &mut impl BlockIo) -> (usize, usize) {
        // Mark: обход в глубину от корней. Попутно подгружаем объекты (нужны их ссылки).
        // Актуальный roots-blob — тоже корень обхода: сам он от корней не достижим
        // (он их НОСИТЕЛЬ), а надгробие ему разрушило бы состояние при загрузке.
        let mut reachable: BTreeSet<ContentId> = BTreeSet::new();
        let mut stack: Vec<ContentId> = self.roots.values().copied().collect();
        if let Some(rid) = self.last_roots_id {
            stack.push(rid);
        }
        while let Some(id) = stack.pop() {
            if !reachable.insert(id) {
                continue;
            }
            // Веха 108: обход НЕ ДОЛЖЕН тянуть в память весь достижимый store. От объекта нужны
            // только его ссылки; сам кадр, если он уже на диске, тут же отпускается обратно —
            // как это делает коммит с Вехи 104. Замер, который это вскрыл: первый настоящий
            // пакет (glibc — 4291 объект, 37 МБ) при куче ядра 16 МиБ ронял систему НА ЗАГРУЗКЕ,
            // и падал при этом не тот, кто виноват, а первый попросивший память.
            let was_loaded = self.objects.get(&id).is_some_and(|o| o.data.is_some());
            if self.ensure_loaded(io, &id) {
                let kids = self
                    .objects
                    .get(&id)
                    .and_then(|o| o.data.as_ref())
                    .map(|d| d.children.clone())
                    .unwrap_or_default();
                if !was_loaded {
                    if let Some(o) = self.objects.get_mut(&id) {
                        if o.disk.is_some() {
                            o.data = None; // кадр на диске — вторая копия в куче не нужна
                        }
                    }
                }
                for c in kids {
                    stack.push(c);
                }
            }
        }

        // Sweep: удалить недостижимые из RAM; зафиксированным — надгробие и счёт мусора.
        let before = self.objects.len();
        let dead: Vec<ContentId> = self
            .objects
            .keys()
            .filter(|id| !reachable.contains(id))
            .copied()
            .collect();
        for id in dead {
            if let Some(o) = self.objects.remove(&id) {
                if let Some((_, len)) = o.disk {
                    // Мусор меряем секторами: кадр занимает их целиком.
                    self.garbage += (len as u64).div_ceil(SECTOR as u64) * SECTOR as u64;
                    self.pending_dead.push((id, len));
                    self.dirty_ops += 1; // надгробия должны доехать до диска
                }
            }
        }

        let kept = self.objects.len();
        (kept, before - kept)
    }

    /// Уплотнение: переписать живые объекты подряд с начала области и зафиксировать полную
    /// базу индекса. Дорого (O(живого) × 2) — потому и вызывается ПО ПОРОГУ мусора
    /// ([[commit-policy]]), а не на каждый boot.
    ///
    /// Крах-устойчиво в ДВЕ фазы: сначала живые копируются В КОНЕЦ области (пишем только в
    /// свободные сектора; суперблок фазы 1 — атомарное переключение на копии), затем — в
    /// начало (эти сектора после фазы 1 никем не адресуются; суперблок фазы 2 завершает).
    /// Обрыв в любой точке оставляет консистентное состояние: до суперблока фазы — старое,
    /// после — новое. Наивная перезапись начала «на месте» ломала бы старый индекс.
    pub fn compact(&mut self, io: &mut impl BlockIo) {
        // ВНИМАНИЕ (Веха 108): здесь всё живое обязано оказаться в RAM — фазы объявляют кадры
        // «не на диске», а коммит пишет только то, чей payload у него в руках. На сторе крупнее
        // кучи ядра это упрётся так же, как упирался обход gc, только позже: уплотнение зовут по
        // порогу мусора, а не на каждой загрузке. Настоящее лечение — научить коммит писать
        // объект, подгружая его прямо перед записью и отпуская сразу после; записано в
        // known-gaps, чтобы это не всплыло опять как «непонятная нехватка памяти».
        let ids: Vec<ContentId> = self.objects.keys().copied().collect();
        for id in &ids {
            self.ensure_loaded(io, id);
        }

        // Веха 89 — снимок «где что лежит». Обе фазы сперва объявляют объекты «не на диске»
        // и только потом пишут; если запись не состоится, без отката store решил бы, что на
        // диске пусто, хотя данные там.
        let snapshot: Vec<(ContentId, Option<(u32, u32)>)> =
            self.objects.iter().map(|(id, o)| (*id, o.disk)).collect();
        let (saved_next_free, saved_garbage) = (self.next_free, self.garbage);
        let saved_dead = self.pending_dead.clone();

        // Фаза 1: копии живых в конец области + база-индекс + суперблок.
        for o in self.objects.values_mut() {
            o.disk = None;
        }
        self.pending_dead.clear(); // база пишет только живых — надгробия не нужны
        self.need_base = true;
        if !self.commit(io) {
            // Веха 89 — места не хватило даже на копию. **Фазу 2 запускать НЕЛЬЗЯ**: она пишет
            // с начала области, а активный суперблок всё ещё указывает на исходные кадры именно
            // там — переписать их значит потерять данные. Откатываем и уходим ни с чем: диск
            // остался ровно таким, каким был.
            self.restore(&snapshot, saved_next_free, saved_garbage, saved_dead);
            return;
        }

        // Фаза 2: то же самое, но с начала области — бывшие сектора живых теперь мусор,
        // на них не указывает ни активный индекс, ни суперблок (его переставила фаза 1).
        let snapshot2: Vec<(ContentId, Option<(u32, u32)>)> =
            self.objects.iter().map(|(id, o)| (*id, o.disk)).collect();
        let (nf2, g2) = (self.next_free, self.garbage);
        for o in self.objects.values_mut() {
            o.disk = None;
        }
        self.next_free = OBJ_START as u32;
        self.garbage = 0;
        self.need_base = true;
        if !self.commit(io) {
            // Диск консистентен состоянием фазы 1 — возвращаем учёт к нему же.
            self.restore(&snapshot2, nf2, g2, Vec::new());
        }
    }

    /// Веха 89 — вернуть учёт размещения кадров к снятому снимку (см. [`Store::compact`]).
    fn restore(
        &mut self,
        snapshot: &[(ContentId, Option<(u32, u32)>)],
        next_free: u32,
        garbage: u64,
        dead: Vec<(ContentId, u32)>,
    ) {
        for (id, disk) in snapshot {
            if let Some(o) = self.objects.get_mut(id) {
                o.disk = *disk;
            }
        }
        self.next_free = next_free;
        self.garbage = garbage;
        self.pending_dead = dead;
        self.need_base = true; // раскладка не та, что на диске — следующий коммит пишет базу
    }

    // ─── персистентность ─────────────────────────────────────────────────────

    /// Загрузить состояние с диска. `true` — были данные; `false` — чистый диск.
    /// ЛЕНИВО: читает только индекс-цепочку и корни; содержимое объектов подтянет
    /// [`Store::with`]/[`Store::gc`]. Образ v1 читается тоже — первый коммит мигрирует.
    pub fn load(&mut self, io: &mut impl BlockIo) -> bool {
        let mut sb = [0u8; SECTOR];
        if !io.read(SB_SECTOR, &mut sb) {
            return false;
        }
        match get_u64(&sb, SB_MAGIC) {
            MAGIC => self.load_v2(io, &sb),
            MAGIC_V1 => self.load_v1(io, &sb),
            _ => false, // диска нет или он пуст/чужой/неизвестного формата
        }
    }

    fn load_v2(&mut self, io: &mut impl BlockIo, sb: &[u8; SECTOR]) -> bool {
        self.objects.clear();
        self.roots.clear();

        // Собрать цепочку индекс-кадров от новейшего к базе...
        let mut frames: Vec<Vec<u8>> = Vec::new();
        let mut cur = (get_u32(sb, SB_IDX_SECTOR), get_u32(sb, SB_IDX_LEN));
        while cur.0 != 0 {
            let Some(frame) = read_object(io, cur.0, cur.1 as usize) else {
                self.corrupt_reads += 1;
                break; // носитель не отдал кадр — дальше цепочку не читаем
            };
            if frame.len() < 12 {
                break; // повреждённый кадр — дальше цепочку не читаем
            }
            let prev = (get_u32(&frame, 0), get_u32(&frame, 4));
            frames.push(frame);
            cur = prev;
        }
        // ...и реплеить ОТ СТАРЕЙШЕГО к новейшему: добавления и надгробия в верном порядке.
        for frame in frames.iter().rev() {
            let n = get_u32(frame, 8) as usize;
            for i in 0..n {
                let off = 12 + i * ENTRY_SIZE;
                if off + ENTRY_SIZE > frame.len() {
                    break;
                }
                let mut id = [0u8; 32];
                id.copy_from_slice(&frame[off..off + 32]);
                let id = ContentId(id);
                let sector = get_u32(frame, off + 32);
                let len = get_u32(frame, off + 36);
                if sector == TOMBSTONE {
                    self.objects.remove(&id);
                } else {
                    self.objects.insert(id, Object { data: None, disk: Some((sector, len)) });
                }
            }
        }
        self.chain_len = frames.len() as u32;
        self.index_tail = (get_u32(sb, SB_IDX_SECTOR), get_u32(sb, SB_IDX_LEN));

        // Корни: подтянуть roots-blob по адресу из суперблока и разобрать.
        if get_u32(sb, SB_ROOTS_PRESENT) != 0 {
            let mut rid = [0u8; 32];
            rid.copy_from_slice(&sb[SB_ROOTS_ID..SB_ROOTS_ID + 32]);
            let rid = ContentId(rid);
            if self.ensure_loaded(io, &rid) {
                if let Some(d) = self.objects.get(&rid).and_then(|o| o.data.as_ref()) {
                    self.roots = deserialize_roots(&d.payload);
                }
            }
            self.last_roots_id = Some(rid);
        }
        self.next_free = get_u32(sb, SB_NEXT_FREE);
        self.generation = get_u64(sb, SB_GENERATION);
        self.garbage = get_u64(sb, SB_GARBAGE);
        self.pending_dead.clear();
        self.dirty_ops = 0;
        self.need_base = false;
        true
    }

    /// Чтение образа v1 (A/B-индекс). Кадры объектов совместимы — мигрирует только
    /// индекс: первый же коммит запишет базу v2 и суперблок с новым MAGIC.
    fn load_v1(&mut self, io: &mut impl BlockIo, sb: &[u8; SECTOR]) -> bool {
        let generation = get_u64(sb, SB1_GENERATION);
        let active = get_u32(sb, SB1_ACTIVE);
        let next_free = get_u32(sb, SB1_NEXT_FREE);
        let count = get_u32(sb, SB1_COUNT) as usize;
        let roots_present = get_u32(sb, SB1_ROOTS_PRESENT) != 0;

        let idx_start = if active == 0 { IDX_A } else { IDX_B };
        let mut entries: Vec<(ContentId, u32, u32)> = Vec::with_capacity(count);
        let sectors = count.div_ceil(ENTRIES_PER_SECTOR_V1);
        for si in 0..sectors {
            let mut buf = [0u8; SECTOR];
            if !io.read(idx_start + si as u64, &mut buf) {
                self.corrupt_reads += 1;
                break; // индекс v1 не дочитался — мигрируем тем, что успели разобрать
            }
            for e in 0..ENTRIES_PER_SECTOR_V1 {
                let gi = si * ENTRIES_PER_SECTOR_V1 + e;
                if gi >= count {
                    break;
                }
                let off = e * ENTRY_SIZE_V1;
                let mut id = [0u8; 32];
                id.copy_from_slice(&buf[off..off + 32]);
                entries.push((ContentId(id), get_u32(&buf, off + 32), get_u32(&buf, off + 36)));
            }
        }

        self.objects.clear();
        self.roots.clear();
        for (id, sector, len) in entries {
            self.objects.insert(id, Object { data: None, disk: Some((sector, len)) });
        }
        if roots_present {
            let mut rid = [0u8; 32];
            rid.copy_from_slice(&sb[SB1_ROOTS_ID..SB1_ROOTS_ID + 32]);
            let rid = ContentId(rid);
            if self.ensure_loaded(io, &rid) {
                if let Some(d) = self.objects.get(&rid).and_then(|o| o.data.as_ref()) {
                    self.roots = deserialize_roots(&d.payload);
                }
            }
            self.last_roots_id = Some(rid);
        }
        self.next_free = next_free;
        self.generation = generation;
        self.index_tail = (0, 0);
        self.chain_len = 0;
        self.garbage = 0; // v1 не считал мусор; после миграции счёт честный с нуля
        self.pending_dead.clear();
        self.dirty_ops = 0;
        self.need_base = true; // миграция: первый коммит пишет базу v2
        true
    }

    /// Зафиксировать состояние на диск (checkpoint). Крах-устойчиво: кадры и индекс
    /// дописываются в свободные секторы, суперблок пишется последним (точка коммита).
    /// Если фиксировать нечего (нет новых объектов/надгробий, корни не менялись) — no-op.
    ///
    /// Веха 89 — возвращает `false`, если коммит НЕ состоялся: носитель полон или отверг
    /// запись. В этом случае на диске осталось прежнее консистентное поколение целиком.
    pub fn commit(&mut self, io: &mut impl BlockIo) -> bool {
        // 0) Сериализовать корни в контент-адресуемый объект (roots-blob) и учесть его.
        let roots_blob = serialize_roots(&self.roots);
        let roots_id = ContentId::hash(&encode(&roots_blob, &[]));
        let force_base = self.need_base;
        if !force_base
            && self.last_roots_id == Some(roots_id)
            && self.pending_dead.is_empty()
            && self.objects.values().all(|o| o.disk.is_some())
        {
            self.dirty_ops = 0; // идемпотентные set_root в то же значение и т.п.
            return true;
        }
        self.objects.entry(roots_id).or_insert_with(|| Object {
            data: Some(Loaded { payload: roots_blob, children: Vec::new() }),
            disk: None,
        });

        // 1) Дописать кадры объектов, которых ещё нет на диске (append-only). Их записи —
        //    содержимое будущей дельты.
        let to_write: Vec<ContentId> = self
            .objects
            .iter()
            .filter(|(_, o)| o.disk.is_none())
            .map(|(id, _)| *id)
            .collect();
        let mut delta: Vec<(ContentId, u32, u32)> = Vec::with_capacity(to_write.len());
        // Веха 89 — влезет ли коммит целиком? Считаем ДО первой записи: наполовину записанный
        // коммит — это осиротевшие кадры и потраченные секторы там, где места и так нет.
        // Индекс-кадр ещё не построен, поэтому кладём на него запас: заголовок (8 Б) + счётчик
        // (4 Б) + запись на каждый объект store (база в худшем случае перечисляет все).
        let need_objects: u64 = to_write
            .iter()
            .map(|id| {
                let d = self.objects[id].data.as_ref().expect("объект без данных и без диска");
                (encode(&d.payload, &d.children).len().div_ceil(SECTOR)) as u64
            })
            .sum();
        let need_index =
            (12 + (self.objects.len() + self.pending_dead.len()) * ENTRY_SIZE).div_ceil(SECTOR) as u64;
        if !self.fits(io, need_objects + need_index) {
            self.out_of_space += 1;
            return false;
        }
        for id in to_write {
            let d = self.objects[&id].data.as_ref().expect("объект без данных и без диска");
            let frame = encode(&d.payload, &d.children);
            let sector = self.next_free;
            // Веха 89: носитель отказал — коммит НЕ состоялся. Выходим до записи суперблока,
            // значит на диске остаётся прежнее консистентное состояние, а объекты остаются
            // «не на диске» и уедут следующим коммитом. Отметки `disk` тут не ставим вовсе —
            // иначе недописанный объект считался бы зафиксированным и потерялся.
            if !write_object(io, sector, &frame) {
                self.failed_writes += 1;
                return false;
            }
            self.bytes_written += frame.len().div_ceil(SECTOR) as u64 * SECTOR as u64;
            self.next_free += frame.len().div_ceil(SECTOR) as u32;
            delta.push((id, sector, frame.len() as u32));
        }
        for (id, sector, len) in &delta {
            self.objects.get_mut(id).unwrap().disk = Some((*sector, *len));
        }

        // 2) Индекс-кадр: дельта (новые записи + надгробия) со ссылкой на предыдущий, либо
        //    полная база (prev = 0) — при миграции/уплотнении/слишком длинной цепочке.
        let base = force_base || self.chain_len >= CHAIN_MAX;
        let mut frame: Vec<u8> = Vec::new();
        let (prev_sector, prev_len) = if base { (0, 0) } else { self.index_tail };
        frame.extend_from_slice(&prev_sector.to_le_bytes());
        frame.extend_from_slice(&prev_len.to_le_bytes());
        let entries: Vec<(ContentId, u32, u32)> = if base {
            self.objects
                .iter()
                .map(|(id, o)| {
                    let (s, l) = o.disk.unwrap();
                    (*id, s, l)
                })
                .collect()
        } else {
            // Надгробия ПЕРЕД добавлениями: если один id и умер, и возродился в этом
            // же коммите (переиспользованное содержимое), replay при загрузке должен
            // закончиться «жив» — remove, потом insert.
            let mut v: Vec<(ContentId, u32, u32)> =
                self.pending_dead.iter().map(|(id, len)| (*id, TOMBSTONE, *len)).collect();
            v.extend(delta.iter().copied());
            v
        };
        frame.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for (id, sector, len) in &entries {
            frame.extend_from_slice(&id.0);
            frame.extend_from_slice(&sector.to_le_bytes());
            frame.extend_from_slice(&len.to_le_bytes());
        }
        let idx_sector = self.next_free;
        if !write_object(io, idx_sector, &frame) {
            // Индекс не лёг — откатить отметки «кадр на диске»: без записи в индексе следующий
            // коммит обязан записать эти объекты заново, иначе они потеряются. Сами кадры
            // останутся в области объектов мусором — его подберёт `compact`.
            self.failed_writes += 1;
            self.rollback_delta(&delta);
            return false;
        }
        self.bytes_written += frame.len().div_ceil(SECTOR) as u64 * SECTOR as u64;
        self.next_free += frame.len().div_ceil(SECTOR) as u32;

        // 3) Суперблок — последним. Одна запись сектора = атомарная точка коммита.
        let generation = self.generation + 1;
        let mut sb = [0u8; SECTOR];
        put_u64(&mut sb, SB_MAGIC, MAGIC);
        put_u64(&mut sb, SB_GENERATION, generation);
        put_u32(&mut sb, SB_NEXT_FREE, self.next_free);
        put_u32(&mut sb, SB_IDX_SECTOR, idx_sector);
        put_u32(&mut sb, SB_IDX_LEN, frame.len() as u32);
        put_u64(&mut sb, SB_GARBAGE, self.garbage);
        put_u32(&mut sb, SB_ROOTS_PRESENT, 1);
        sb[SB_ROOTS_ID..SB_ROOTS_ID + 32].copy_from_slice(&roots_id.0);
        // Точка коммита. Не легла — на диске осталось ПРЕЖНЕЕ поколение целиком, поэтому
        // откатываем ровно то же, что и при сбое индекса.
        if !io.write(SB_SECTOR, &sb) {
            self.failed_writes += 1;
            self.rollback_delta(&delta);
            return false;
        }
        self.bytes_written += SECTOR as u64;

        self.generation = generation;
        self.index_tail = (idx_sector, frame.len() as u32);
        self.chain_len = if base { 1 } else { self.chain_len + 1 };
        self.last_roots_id = Some(roots_id);
        self.pending_dead.clear();
        self.dirty_ops = 0;
        self.need_base = false;
        self.evict_committed();
        true
    }

    /// Веха 104 — **отпустить из RAM то, что уже лежит на диске.** До этого store держал
    /// содержимое ВСЕХ объектов сессии: коммит их персистил, но кэш не трогал, и потолок системы
    /// оказывался равен куче ядра (16 МиБ) — причём упирались в него ПАНИКОЙ, а не отказом.
    /// Замер Вехи 101 (`store-probe`): 8 МиБ пачкой проходили, 16 МиБ роняли машину; пакетам с
    /// их NAR в десятки мегабайт этого не хватило бы заведомо.
    ///
    /// Забрать обратно умеет [`Store::ensure_loaded`] — ленивая подгрузка с диска была написана
    /// давно и ровно для этого случая. Здесь остаётся отпустить: объект без `disk` (ещё не
    /// зафиксирован) не трогаем никогда — иначе потеряли бы единственную его копию.
    fn evict_committed(&mut self) {
        for o in self.objects.values_mut() {
            if o.disk.is_some() {
                o.data = None;
            }
        }
    }

    /// Веха 89 — снять отметки «кадр на диске» с объектов незавершённого коммита: их кадры
    /// записаны, но в индекс не попали, значит для формата их нет. Следующий коммит запишет
    /// заново (кадры-сироты станут мусором и уйдут при `compact`).
    fn rollback_delta(&mut self, delta: &[(ContentId, u32, u32)]) {
        for (id, _, _) in delta {
            if let Some(o) = self.objects.get_mut(id) {
                o.disk = None;
            }
        }
    }
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

// ─── кадр объекта: [ nchildren(u32) | child-id×n | payload ] ───────────────────

/// Веха 104 — тот же кадр, но **без паники при нехватке памяти**: `None` вместо аварии.
/// Нужен пути из userspace: кадр — самая крупная из выделяемых здесь вещей (полезная нагрузка
/// целиком), и падать на нём всей системой из-за чужого размера недопустимо.
fn try_encode(payload: &[u8], children: &[ContentId]) -> Option<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    buf.try_reserve_exact(4 + children.len() * 32 + payload.len()).ok()?;
    buf.extend_from_slice(&(children.len() as u32).to_le_bytes());
    for c in children {
        buf.extend_from_slice(&c.0);
    }
    buf.extend_from_slice(payload);
    Some(buf)
}

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
/// Веха 89: `None` — носитель не отдал хотя бы один сектор. Раньше результат `io.read`
/// игнорировался, и сбойное чтение молча превращалось в «объект» из мусора или нулей.
fn read_object(io: &mut impl BlockIo, start: u32, len: usize) -> Option<Vec<u8>> {
    let sectors = len.div_ceil(SECTOR);
    let mut out = Vec::with_capacity(sectors * SECTOR);
    for i in 0..sectors {
        let mut buf = [0u8; SECTOR];
        if !io.read(start as u64 + i as u64, &mut buf) {
            return None;
        }
        out.extend_from_slice(&buf);
    }
    out.truncate(len);
    Some(out)
}

/// Записать кадр, начиная с сектора `start` (последний сектор дополняется нулями).
/// Веха 89: `false` — носитель не принял хотя бы один сектор (раньше терялось молча).
#[must_use]
fn write_object(io: &mut impl BlockIo, start: u32, bytes: &[u8]) -> bool {
    let sectors = bytes.len().div_ceil(SECTOR);
    for i in 0..sectors {
        let mut buf = [0u8; SECTOR];
        let off = i * SECTOR;
        let end = (off + SECTOR).min(bytes.len());
        buf[..end - off].copy_from_slice(&bytes[off..end]);
        if !io.write(start as u64 + i as u64, &buf) {
            return false;
        }
    }
    true
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
