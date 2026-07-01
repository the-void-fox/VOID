//! Объектная модель — начало стержня VOID (Веха 6).
//!
//! Два кирпича из [[0002-persistent-content-addressed-capability-core]]:
//! - **Неизменяемые значения**, адресуемые по хэшу содержимого ([`ContentId`]). Кладём байты —
//!   получаем их контент-адрес. Одинаковые байты → один адрес и одна копия (дедупликация).
//! - **Изменяемые корни (ячейки)** — именованные указатели на значения. Мутация = переключить
//!   корень на *новое* неизменяемое значение; старое остаётся в хранилище (история версий).
//!
//! Пока всё живёт в RAM (куча Вехи 4) и волатильно. На Вехе 7 это же хранилище станет
//! долговечным на диске (см. [[persistence-and-volatility]]): контент-адресация делает такой
//! перенос дешёвым и безопасным.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use void_abi::ContentId;

use crate::sync::SpinLock;

/// Контент-адресуемое хранилище неизменяемых значений + именованные корни.
struct Store {
    /// ContentId → байты значения. Значения неизменяемы; ключ = хэш содержимого.
    objects: BTreeMap<ContentId, Vec<u8>>,
    /// Имя корня → на какое значение он сейчас указывает.
    roots: BTreeMap<&'static str, ContentId>,
}

impl Store {
    const fn new() -> Self {
        Self {
            objects: BTreeMap::new(),
            roots: BTreeMap::new(),
        }
    }
}

static STORE: SpinLock<Store> = SpinLock::new(Store::new());

/// Положить значение в хранилище и получить его контент-адрес.
/// Идемпотентно: одинаковые байты дают один и тот же адрес и хранятся один раз.
pub fn put(bytes: &[u8]) -> ContentId {
    let id = ContentId::hash(bytes);
    let mut store = STORE.lock();
    // Вставляем копию только если такого содержимого ещё нет (дедупликация).
    store.objects.entry(id).or_insert_with(|| bytes.to_vec());
    id
}

/// Прочитать значение по адресу под замком: `f` получает `Some(bytes)` или `None`.
/// (Замыкание, потому что байты живут внутри хранилища и нельзя отдавать ссылку наружу.)
pub fn with<R>(id: &ContentId, f: impl FnOnce(Option<&[u8]>) -> R) -> R {
    let store = STORE.lock();
    f(store.objects.get(id).map(|v| v.as_slice()))
}

/// Сколько уникальных значений сейчас в хранилище.
pub fn len() -> usize {
    STORE.lock().objects.len()
}

/// Установить (или переключить) корень `name` на значение `id`.
pub fn set_root(name: &'static str, id: ContentId) {
    STORE.lock().roots.insert(name, id);
}

/// На какое значение указывает корень `name`.
pub fn root(name: &str) -> Option<ContentId> {
    STORE.lock().roots.get(name).copied()
}
