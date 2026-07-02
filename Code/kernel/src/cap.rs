//! Веха 8 — **capability**: непод­делываемые ссылки на объекты с правами.
//!
//! Вторая половина модели KeyKOS/EROS (см. [[0002-persistent-content-addressed-capability-core]]).
//! В едином персистентном пространстве объектов ([[object-model]]) права выражаются не
//! UID/файловыми битами, а *ссылками*: обладание capability на объект И ЕСТЬ право на него.
//! Три свойства, каждое — реальный механизм:
//! - **Неподделываемость**: capability нельзя сфабриковать — только сминтить (при владении)
//!   или получить (grant). Для обладателя это дескриптор `Cap` (слот + поколение) в c-space
//!   его домена; ядро валидирует его по таблице перед КАЖДЫМ доступом. Выдуманный дескриптор
//!   не совпадёт с таблицей → отказ.
//! - **Наименьшая привилегия (аттенуация)**: при передаче права можно только СУЗИТЬ
//!   (пересечение масок, [`Rights::intersect`]), никогда не расширить.
//! - **Отзыв (revocation)**: освобождение слота бумкает его поколение → все ранее выданные
//!   дескрипторы на этот слот становятся устаревшими (generational handle).
//!
//! Пока c-space живёт в RAM (как объектная модель на Вехе 6 до персистентности на 7.2);
//! сделать capability долговечными (сложить c-space в [[persistent-store]]) — отдельный шаг.

use alloc::string::String;
use alloc::vec::Vec;

use void_abi::{Cap, ContentId, Rights};

use crate::object;
use crate::sync::SpinLock;

/// Идентификатор домена защиты (протопроцесс) — индекс в глобальном c-space.
pub type DomainId = usize;

/// Устройство, на которое можно держать capability (пока — только блочный диск).
/// Право `READ` на такой cap — единственный вход к секторам (см. `SYS_BLK_READ` в [[block-driver]]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Device {
    /// Блочное устройство virtio-blk ([[virtio-blk]]).
    Block,
}

/// На что указывает capability.
#[derive(Clone)]
pub enum Target {
    /// Неизменяемое значение по контент-адресу ([[object-model]]).
    Value(ContentId),
    /// Изменяемая именованная ячейка-корень.
    Root(&'static str),
    /// IPC-эндпоинт: право отправить сообщение процессу-серверу (его id). Держать такой cap
    /// с правом `SEND` — единственный способ сделать `CALL` этому серверу ([[ipc]], [[processes]]).
    Endpoint(usize),
    /// **Одноразовый reply-cap** на вызвавшего клиента (его id). Ядро минтит его серверу при
    /// доставке запроса (`RECV`); `REPLY` требует его и по исполнении отзывает. Так нельзя
    /// ответить тому, кто не звал, и нельзя подделать ответ ([[reply-capability]]).
    Reply(usize),
    /// Аппаратное устройство: доступ к железу только по этому cap (право на устройство).
    Device(Device),
    /// Персистентный объектный [[object-model|store]] как ресурс: `READ` — читать значения по
    /// content-id (`OBJ_GET`), `WRITE` — класть новые значения (`OBJ_PUT`). Так доступ к
    /// пространству объектов выдаётся процессу-серверу store, а не зашит в каждый процесс.
    Store,
}

/// Запись в c-space: цель + права на неё.
struct Entry {
    target: Target,
    rights: Rights,
}

/// Слот таблицы прав. `generation` растёт при каждом освобождении → отзыв.
struct Slot {
    generation: u32,
    entry: Option<Entry>,
}

/// Домен защиты: имя + его личное capability-пространство (c-space).
struct Domain {
    name: &'static str,
    slots: Vec<Slot>,
}

/// Все домены системы. Отдельная таблица прав на домен — суть модели.
struct CSpace {
    domains: Vec<Domain>,
}

static CSPACE: SpinLock<CSpace> = SpinLock::new(CSpace { domains: Vec::new() });

/// Ошибки проверки capability.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CapError {
    /// Слот вне таблицы или домен не существует.
    Invalid,
    /// Поколение не совпало (слот отозван/переиспользован) или слот пуст.
    Stale,
    /// У capability нет нужного права.
    Denied,
    /// Операция не подходит цели (например, запись по cap на значение).
    WrongKind,
    /// Цель — значение/корень, которого нет в store.
    Dangling,
}

// ─── домены и минт ─────────────────────────────────────────────────────────

/// Создать домен защиты, вернуть его id.
pub fn create_domain(name: &'static str) -> DomainId {
    let mut cs = CSPACE.lock();
    cs.domains.push(Domain { name, slots: Vec::new() });
    cs.domains.len() - 1
}

/// Имя домена (для вывода/интроспекции).
pub fn domain_name(dom: DomainId) -> &'static str {
    CSPACE.lock().domains[dom].name
}

/// Занять слот под запись: переиспользовать освобождённый, иначе добавить новый.
/// Возвращает дескриптор (слот + текущее поколение слота).
fn alloc_slot(dom: &mut Domain, entry: Entry) -> Cap {
    for (i, s) in dom.slots.iter_mut().enumerate() {
        if s.entry.is_none() {
            s.entry = Some(entry);
            return Cap::new(i as u32, s.generation);
        }
    }
    dom.slots.push(Slot { generation: 1, entry: Some(entry) });
    Cap::new((dom.slots.len() - 1) as u32, 1)
}

/// Сминтить capability — привилегированное создание при владении объектом.
/// (В настоящей системе минт происходит при создании объекта; здесь его делает ядро.)
pub fn mint(dom: DomainId, target: Target, rights: Rights) -> Cap {
    let mut cs = CSPACE.lock();
    alloc_slot(&mut cs.domains[dom], Entry { target, rights })
}

// ─── проверка и доступ ───────────────────────────────────────────────────────

/// Найти живую запись по дескриптору или вернуть ошибку. Сердце неподделываемости:
/// любой доступ проходит здесь, и выдуманный `Cap` не совпадёт с таблицей.
fn resolve<'a>(cs: &'a CSpace, dom: DomainId, cap: Cap) -> Result<&'a Entry, CapError> {
    let d = cs.domains.get(dom).ok_or(CapError::Invalid)?;
    let s = d.slots.get(cap.slot() as usize).ok_or(CapError::Invalid)?;
    if s.generation != cap.generation() {
        return Err(CapError::Stale);
    }
    s.entry.as_ref().ok_or(CapError::Stale)
}

/// Права, которые несёт дескриптор (для интроспекции/вывода).
pub fn rights(dom: DomainId, cap: Cap) -> Result<Rights, CapError> {
    let cs = CSPACE.lock();
    Ok(resolve(&cs, dom, cap)?.rights)
}

/// Прочитать значение по capability (требует `READ`). Возвращает результат `f`.
///
/// Замок c-space держим только на время проверки: цель клонируем и отпускаем замок
/// перед обращением к [`object`] (у него свой замок) — иначе вложенная блокировка.
pub fn read<R>(dom: DomainId, cap: Cap, f: impl FnOnce(&[u8]) -> R) -> Result<R, CapError> {
    let target = {
        let cs = CSPACE.lock();
        let e = resolve(&cs, dom, cap)?;
        if !e.rights.contains(Rights::READ) {
            return Err(CapError::Denied);
        }
        e.target.clone()
    };
    let id = match target {
        Target::Value(id) => id,
        Target::Root(name) => object::root(name).ok_or(CapError::Dangling)?,
        // Эндпоинт/reply/устройство/store — не значения: их «читают» через IPC/BLK_READ/OBJ_GET.
        Target::Endpoint(_) | Target::Reply(_) | Target::Device(_) | Target::Store => {
            return Err(CapError::WrongKind)
        }
    };
    object::with(&id, |b| match b {
        Some(bytes) => Ok(f(bytes)),
        None => Err(CapError::Dangling),
    })
}

/// Переустановить ячейку-корень на новое значение (требует `WRITE` и цель `Root`).
/// Значения неизменяемы — «запись» двигает именно ячейку (история версий сохраняется).
pub fn write_root(dom: DomainId, cap: Cap, new_value: ContentId) -> Result<(), CapError> {
    let name = {
        let cs = CSPACE.lock();
        let e = resolve(&cs, dom, cap)?;
        if !e.rights.contains(Rights::WRITE) {
            return Err(CapError::Denied);
        }
        match e.target {
            Target::Root(name) => name,
            Target::Value(_) | Target::Endpoint(_) | Target::Reply(_) | Target::Device(_)
            | Target::Store => return Err(CapError::WrongKind),
        }
    };
    object::set_root(name, new_value);
    Ok(())
}

/// Разрешить cap на **IPC-эндпоинт** и вернуть id процесса-сервера (требует `SEND`).
/// Сердце защищённого IPC: `SYS_CALL` берёт не сырой pid, а этот дескриптор — сминтить его
/// может только ядро при выдаче права, а `resolve` отвергнет подделку/чужую цель.
pub fn endpoint(dom: DomainId, cap: Cap) -> Result<usize, CapError> {
    let cs = CSPACE.lock();
    let e = resolve(&cs, dom, cap)?;
    if !e.rights.contains(Rights::SEND) {
        return Err(CapError::Denied);
    }
    match e.target {
        Target::Endpoint(owner) => Ok(owner),
        _ => Err(CapError::WrongKind),
    }
}

/// Разрешить **reply-cap** и вернуть id клиента, которому адресован ответ (требует `SEND`).
/// Пара к [`endpoint`], но цель — конкретный вызвавший. `REPLY` затем отзывает cap (одноразовость).
pub fn reply_endpoint(dom: DomainId, cap: Cap) -> Result<usize, CapError> {
    let cs = CSPACE.lock();
    let e = resolve(&cs, dom, cap)?;
    if !e.rights.contains(Rights::SEND) {
        return Err(CapError::Denied);
    }
    match e.target {
        Target::Reply(caller) => Ok(caller),
        _ => Err(CapError::WrongKind),
    }
}

/// Разрешить cap на **устройство** и вернуть его (требует прав `need`, напр. `READ`).
/// Так `SYS_BLK_READ` перестаёт быть открытым для всех: без device-cap доступа к диску нет.
pub fn device(dom: DomainId, cap: Cap, need: Rights) -> Result<Device, CapError> {
    let cs = CSPACE.lock();
    let e = resolve(&cs, dom, cap)?;
    if !e.rights.contains(need) {
        return Err(CapError::Denied);
    }
    match e.target {
        Target::Device(d) => Ok(d),
        _ => Err(CapError::WrongKind),
    }
}

/// Разрешить cap на **объектный store** (требует прав `need`: `READ` для `OBJ_GET`, `WRITE` для
/// `OBJ_PUT`). Без такого cap процесс не может обращаться к пространству объектов напрямую —
/// только через сервер store по IPC.
pub fn store(dom: DomainId, cap: Cap, need: Rights) -> Result<(), CapError> {
    let cs = CSPACE.lock();
    let e = resolve(&cs, dom, cap)?;
    if !e.rights.contains(need) {
        return Err(CapError::Denied);
    }
    match e.target {
        Target::Store => Ok(()),
        _ => Err(CapError::WrongKind),
    }
}

// ─── передача и отзыв ──────────────────────────────────────────────────────

/// Передать capability из домена `from` в домен `to`, сузив права маской `mask`
/// (требует `GRANT` у исходного). Итоговые права = исходные ∩ mask — **аттенуация**:
/// расширить права передачей нельзя. Возвращает новый дескриптор, валидный в `to`.
pub fn grant(from: DomainId, cap: Cap, to: DomainId, mask: Rights) -> Result<Cap, CapError> {
    let mut cs = CSPACE.lock();
    let (target, new_rights) = {
        let e = resolve(&cs, from, cap)?;
        if !e.rights.contains(Rights::GRANT) {
            return Err(CapError::Denied);
        }
        (e.target.clone(), e.rights.intersect(mask))
    };
    Ok(alloc_slot(&mut cs.domains[to], Entry { target, rights: new_rights }))
}

/// Отозвать capability: освободить слот и бумкнуть его поколение. Все ранее выданные
/// дескрипторы на этот слот становятся `Stale` при следующей проверке.
pub fn revoke(dom: DomainId, cap: Cap) -> Result<(), CapError> {
    let mut cs = CSPACE.lock();
    let d = cs.domains.get_mut(dom).ok_or(CapError::Invalid)?;
    let s = d.slots.get_mut(cap.slot() as usize).ok_or(CapError::Invalid)?;
    if s.generation != cap.generation() {
        return Err(CapError::Stale);
    }
    s.entry = None;
    s.generation = s.generation.wrapping_add(1);
    Ok(())
}

/// «rwgs»-строка прав для вывода (read · write · grant · send).
pub fn rights_str(r: Rights) -> String {
    let mut s = String::new();
    s.push(if r.contains(Rights::READ) { 'r' } else { '-' });
    s.push(if r.contains(Rights::WRITE) { 'w' } else { '-' });
    s.push(if r.contains(Rights::GRANT) { 'g' } else { '-' });
    s.push(if r.contains(Rights::SEND) { 's' } else { '-' });
    s
}
