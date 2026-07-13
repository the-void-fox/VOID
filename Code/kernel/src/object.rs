//! Объектный store ядра — фасад над крейтом `void-store` (Веха 29).
//!
//! Формат диска и вся логика (put/get, корни, GC, A/B-коммит) живут в `libs/void-store` —
//! одна реализация на ядро и хост-утилиты (`void-store-import`). Здесь остаётся ядерное:
//! замок ([`SpinLock`]) и носитель — virtio-blk через [`Disk`]. Публичный API модуля
//! не изменился, остальное ядро правок не заметило.
//!
//! Порядок замков прежний: `with`/`gc` держат STORE, чтение диска берёт замок BLK
//! внутри virtio_blk (STORE→BLK).

use alloc::vec::Vec;

use void_abi::ContentId;
use void_store::{BlockIo, Store, SECTOR};

use crate::sync::SpinLock;
use crate::virtio_blk;

/// Носитель ядра: сектор store = сектор virtio-blk (размеры совпадают по построению).
struct Disk;

impl BlockIo for Disk {
    fn read(&mut self, sector: u64, buf: &mut [u8; SECTOR]) -> bool {
        virtio_blk::read(sector, buf)
    }
    fn write(&mut self, sector: u64, buf: &[u8; SECTOR]) -> bool {
        virtio_blk::write(sector, buf)
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

/// Прочитать полезную нагрузку по адресу (ленивая подгрузка с диска при промахе кэша).
pub fn with<R>(id: &ContentId, f: impl FnOnce(Option<&[u8]>) -> R) -> R {
    STORE.lock().with(&mut Disk, id, f)
}

/// Исходящие ссылки объекта (подгружает при необходимости).
pub fn children(id: &ContentId) -> Vec<ContentId> {
    STORE.lock().children(&mut Disk, id)
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

/// Собрать мусор (mark-sweep от корней + подготовка уплотнения). (оставлено, собрано).
pub fn gc() -> (usize, usize) {
    STORE.lock().gc(&mut Disk)
}

/// Загрузить состояние с диска. `true` — были данные; `false` — чистый диск.
pub fn load() -> bool {
    STORE.lock().load(&mut Disk)
}

/// Зафиксировать состояние на диск (крах-устойчивый A/B-коммит).
pub fn commit() {
    STORE.lock().commit(&mut Disk)
}
