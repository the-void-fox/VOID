//! Объектный store ядра — фасад над крейтом `void-store` (Веха 29) + политика
//! коммитов (Веха 33, [[commit-policy]]).
//!
//! Формат диска и вся логика (put/get, корни, GC, дельта-индекс) живут в `libs/void-store` —
//! одна реализация на ядро и хост-утилиты (`void-store-import`). Здесь остаётся ядерное:
//! замок ([`SpinLock`]), носитель — virtio-blk через [`Disk`], и **group commit**:
//! `set_root`/`del_root` диск не трогают, копится счётчик грязных операций, а фиксацию
//! делает [`maybe_commit`] — по порогу операций или по периоду (его зовёт планировщик
//! при каждом возобновлении процесса; вытеснение таймером гарантирует регулярность).
//! Точки жёсткого синка: конец загрузки и конец vsh-сессии (прямой [`commit`]).
//!
//! Порядок замков прежний: `with`/`gc` держат STORE, чтение диска берёт замок BLK
//! внутри virtio_blk (STORE→BLK).

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use alloc::vec::Vec;

use void_abi::ContentId;
use void_store::{BlockIo, Store, SECTOR};

use crate::sync::SpinLock;
use crate::{ahci, println, timer, virtio_blk};

/// Веха 47 — какой носитель активен: AHCI (реальный SATA) или virtio-blk (QEMU). Выбор
/// делает загрузка ([`use_ahci`]): на железе поднялся AHCI — сектора идут через него,
/// иначе — через virtio-blk. Формат сектора одинаков (512 Б), поэтому store не различает.
static AHCI_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Переключить носитель store на AHCI (зовёт `kmain`, когда `ahci::init()` удался).
pub fn use_ahci() {
    AHCI_ACTIVE.store(true, Ordering::Relaxed);
}

/// Веха 48 — «заморозка»: после установки на диск ([`crate::install`]) раскладка диска сменилась,
/// и кэш работающего store'а НЕ должен больше туда писать (иначе group-commit затрёт свежий
/// образ). Ставит установщик; [`Disk::write`] и коммиты становятся no-op — состояние живёт в RAM
/// до перезагрузки с диска.
static FROZEN: AtomicBool = AtomicBool::new(false);

/// Заморозить запись на диск (см. [`FROZEN`]). Необратимо в пределах сессии — дальше ребут.
pub fn freeze() {
    FROZEN.store(true, Ordering::Relaxed);
}

/// Носитель ядра: сектор store = сектор блочного устройства (размеры совпадают по построению).
struct Disk;

impl BlockIo for Disk {
    fn read(&mut self, sector: u64, buf: &mut [u8; SECTOR]) -> bool {
        if AHCI_ACTIVE.load(Ordering::Relaxed) {
            ahci::read(sector, buf)
        } else {
            virtio_blk::read(sector, buf)
        }
    }
    fn write(&mut self, sector: u64, buf: &[u8; SECTOR]) -> bool {
        if FROZEN.load(Ordering::Relaxed) {
            return true; // Веха 48: после установки диск заморожен — коммиты «успешны», но без записи
        }
        if AHCI_ACTIVE.load(Ordering::Relaxed) {
            ahci::write(sector, buf)
        } else {
            virtio_blk::write(sector, buf)
        }
    }
    /// Веха 89 — ёмкость носителя store в секторах: раздел p2 на реальном диске (AHCI сам держит
    /// смещение раздела) либо весь virtio-диск. 0 — устройства нет, проверок не будет.
    fn capacity(&mut self) -> u64 {
        if AHCI_ACTIVE.load(Ordering::Relaxed) {
            ahci::capacity_sectors()
        } else {
            virtio_blk::capacity_sectors()
        }
    }
}

static STORE: SpinLock<Store> = SpinLock::new(Store::new());

/// Положить лист (значение без исходящих ссылок), получить контент-адрес.
pub fn put(bytes: &[u8]) -> ContentId {
    STORE.lock().put(bytes)
}

/// Положить узел: значение + исходящие ссылки. Идемпотентно (дедуп).
pub fn put_node(bytes: &[u8], children: &[ContentId]) -> ContentId {
    STORE.lock().put_node(bytes, children)
}

/// Веха 89 — самоизлечение: починить объект этих байтов, если его кадр на диске повреждён.
/// `true` — была порча и она устранена (кадр перезапишется ближайшим коммитом).
pub fn repair(bytes: &[u8]) -> bool {
    with_store(|s| s.repair(&mut Disk, bytes, &[]))
}

/// Прочитать полезную нагрузку по адресу (ленивая подгрузка с диска при промахе кэша).
pub fn with<R>(id: &ContentId, f: impl FnOnce(Option<&[u8]>) -> R) -> R {
    with_store(|s| s.with(&mut Disk, id, f))
}

/// Исходящие ссылки объекта (подгружает при необходимости).
pub fn children(id: &ContentId) -> Vec<ContentId> {
    with_store(|s| s.children(&mut Disk, id))
}

/// Число объектов (известно из индекса даже для неподгруженных).
pub fn len() -> usize {
    STORE.lock().len()
}

/// Номер последнего коммита.
pub fn generation() -> u64 {
    STORE.lock().generation()
}

/// Установить/переключить корень `name` (любое имя, персистентен).
pub fn set_root(name: &str, id: ContentId) {
    STORE.lock().set_root(name, id);
}

/// На какое значение указывает корень `name`.
pub fn root(name: &str) -> Option<ContentId> {
    STORE.lock().root(name)
}

/// Снять корень `name`. Возвращает `true`, если корень существовал.
pub fn del_root(name: &str) -> bool {
    STORE.lock().del_root(name)
}

/// Текстовый список корней store (для `SYS_OBJ_LIST_ROOTS` / vsh `roots`): «короткий content-id
/// (первые 6 байт hex) + два пробела + имя», по строке на корень (порядок BTreeMap — по имени).
/// Показывает СЫРЫЕ корни store (`bin/*`, `system/*`, `.dir`, `.cspace`, …) — как `void-store-import ls`.
pub fn list_roots_text() -> alloc::string::String {
    use core::fmt::Write;
    let store = STORE.lock();
    let mut out = alloc::string::String::new();
    for (name, id) in store.roots() {
        for b in &id.0[..6] {
            let _ = write!(out, "{:02x}", b);
        }
        let _ = write!(out, "  {}\n", name);
    }
    out
}

/// Собрать мусор (mark-sweep от корней; жертвы — надгробиями до уплотнения).
/// (оставлено, собрано).
pub fn gc() -> (usize, usize) {
    with_store(|s| s.gc(&mut Disk))
}

/// Уплотнить область объектов (двухфазно, крах-устойчиво) — зовётся по порогу мусора.
pub fn compact() {
    with_store(|s| s.compact(&mut Disk))
}

/// Мусора в области объектов, байт (для порога уплотнения).
pub fn garbage_bytes() -> u64 {
    STORE.lock().garbage_bytes()
}

/// Занято областью объектов, байт (включая мусор).
pub fn area_bytes() -> u64 {
    STORE.lock().area_bytes()
}

/// Всего байт записано на носитель за сессию (честная статистика для замеров).
pub fn bytes_written() -> u64 {
    STORE.lock().bytes_written()
}

/// Загрузить состояние с диска. `true` — были данные; `false` — чистый диск.
pub fn load() -> bool {
    with_store(|s| s.load(&mut Disk))
}

/// Зафиксировать состояние на диск (крах-устойчиво; пустой коммит — no-op).
pub fn commit() {
    with_store(|s| {
        if !s.commit(&mut Disk) {
            reclaim_and_retry(s);
        }
    })
}

/// Веха 89 — носитель полон: попробовать освободить место и повторить коммит ОДИН раз.
/// Мусор в области объектов (старые версии, кадры удалённых GC) — обычно и есть причина, а
/// [`Store::compact`] именно для этого и существует; просто сдаться, не попытавшись, значило бы
/// терять данные при живом диске. Не помогло — считаем честно и говорим вслух: дальше это
/// решение владельца (расширить раздел), а не ядра.
fn reclaim_and_retry(s: &mut Store) -> bool {
    static REPORTED: AtomicU64 = AtomicU64::new(0);
    let before = s.out_of_space();
    if before == REPORTED.load(Ordering::Relaxed) {
        return false; // нового «полно» не случилось — чинить нечего
    }
    // `compact` сам умеет отступить, если фаза 1 не влезает (иначе он переписал бы кадры,
    // на которые ещё указывает активный суперблок) — звать его на полном диске безопасно.
    s.compact(&mut Disk);
    let ok = s.commit(&mut Disk);
    REPORTED.store(s.out_of_space(), Ordering::Relaxed);
    if ok {
        println!("  [store] носитель был полон — уплотнение освободило место, коммит прошёл");
        return true;
    }
    let left = s.sectors_left(&mut Disk).unwrap_or(0);
    println!(
        "  [store] НОСИТЕЛЬ ПОЛОН: коммит отменён (свободно {} секторов) — на диске прежнее \
         консистентное поколение, состояние живёт в RAM",
        left,
    );
    false
}

/// Веха 89 — выполнить операцию над store и СРАЗУ сказать, если носитель соврал. Отчёт обязан
/// стоять здесь, а не только в коммите: расхождение хэша ловится при ЧТЕНИИ, и без этого система
/// падала бы с «объект недоступен», не сказав, что диск испорчен.
fn with_store<R>(f: impl FnOnce(&mut Store) -> R) -> R {
    let mut s = STORE.lock();
    let r = f(&mut s);
    let (bad_r, bad_w) = (s.corrupt_reads(), s.failed_writes());
    drop(s); // печатать под замком store нельзя: println берёт консоль
    report_integrity(bad_r, bad_w);
    r
}

/// Веха 89 — сказать вслух, если носитель соврал. Целостность, о которой молчат, бесполезна:
/// расхождение хэша или отказ записи означают, что диск портит данные, и узнать об этом надо
/// в момент события, а не когда система не поднимется. Печатаем ОДИН раз на каждое новое
/// расхождение (счётчики монотонные), чтобы не залить консоль на сыплющемся диске.
fn report_integrity(corrupt_reads: u64, failed_writes: u64) {
    static SEEN_R: AtomicU64 = AtomicU64::new(0);
    static SEEN_W: AtomicU64 = AtomicU64::new(0);
    if corrupt_reads > SEEN_R.swap(corrupt_reads, Ordering::Relaxed) {
        println!(
            "  [store] ЦЕЛОСТНОСТЬ: кадр с диска не сошёлся со своим content-id ({} раз) — \
             объект считается недоступным, мусор в store не попал",
            corrupt_reads,
        );
    }
    if failed_writes > SEEN_W.swap(failed_writes, Ordering::Relaxed) {
        println!(
            "  [store] ЗАПИСЬ ОТКАЗАНА ({} раз) — коммит не состоялся, на диске прежнее \
             консистентное поколение",
            failed_writes,
        );
    }
}

// ─── group commit (Веха 33) ──────────────────────────────────────────────────

/// Порог: столько грязных операций коммитятся немедленно, не дожидаясь периода.
const COMMIT_OPS_MAX: u32 = 64;
/// Период фиксации в тиках таймера (тик ~10 мс → ~2 с). KeyKOS жил минутами,
/// ext4-журнал — 5 с; нам для демо важно видеть группировку глазами.
const COMMIT_PERIOD_TICKS: u64 = 200;

/// Тик первой незафиксированной операции (0 — грязных нет).
static FIRST_DIRTY_TICK: AtomicU64 = AtomicU64::new(0);

/// Немедленный синк, если есть несинхронизированное: зовётся при уходе системы в
/// простой (все процессы блокированы, [`crate::proc`] идёт спать до ввода) — под
/// нагрузкой пачки собирает [`maybe_commit`], а простой — естественная точка
/// фиксации хвоста (flush-on-idle, как у журналируемых ФС): окно потерь при
/// простое схлопывается в ноль.
pub fn commit_if_dirty() {
    let mut s = STORE.lock();
    let dirty = s.dirty_ops();
    if dirty == 0 {
        return;
    }
    let ok = s.commit(&mut Disk);
    let ok = ok || reclaim_and_retry(&mut s);
    let generation = s.generation();
    let (bad_r, bad_w) = (s.corrupt_reads(), s.failed_writes());
    drop(s);
    report_integrity(bad_r, bad_w);
    if !ok {
        return; // коммит не состоялся — операции остаются грязными до следующей попытки
    }
    FIRST_DIRTY_TICK.store(0, Ordering::Relaxed);
    println!(
        "  [store] синк при простое: {} операций одним коммитом → поколение {}",
        dirty,
        generation,
    );
}

/// Политика group commit: зовётся планировщиком при каждом возобновлении процесса.
/// Дёшево, пока чисто (одна проверка счётчика под замком); фиксирует пачку операций
/// одним коммитом по порогу [`COMMIT_OPS_MAX`] или периоду [`COMMIT_PERIOD_TICKS`].
pub fn maybe_commit() {
    let mut s = STORE.lock();
    let dirty = s.dirty_ops();
    if dirty == 0 {
        FIRST_DIRTY_TICK.store(0, Ordering::Relaxed);
        return;
    }
    let now = timer::ticks();
    let first = FIRST_DIRTY_TICK.load(Ordering::Relaxed);
    if first == 0 {
        FIRST_DIRTY_TICK.store(now.max(1), Ordering::Relaxed);
        return;
    }
    if dirty >= COMMIT_OPS_MAX || now.saturating_sub(first) >= COMMIT_PERIOD_TICKS {
        let ok = s.commit(&mut Disk);
        if !ok && !reclaim_and_retry(&mut s) {
            return; // носитель полон — операции ждут следующей попытки
        }
        let generation = s.generation();
        drop(s);
        FIRST_DIRTY_TICK.store(0, Ordering::Relaxed);
        // Лог групповой фиксации намеренно тихий не бывает: это и есть демо политики.
        // Бенчи store не гоняют commit в замерах — шум им не мешает.
        println!(
            "  [store] group commit: {} операций одним коммитом → поколение {}",
            dirty,
            generation,
        );
    }
}
