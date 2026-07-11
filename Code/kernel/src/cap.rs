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
//! Веха 21 — c-space стал **персистентным**: [`persist`] сериализует все домены в store под
//! спец-корень `.cspace` (каждая передача права — чекпойнт), [`load`] поднимает их при загрузке,
//! а [`create_domain`] переиспользует восстановленный домен по имени — так процесс новой
//! загрузки находит права, выданные ему в прошлой. Это закрывает третью треть тезиса
//! [[0002-persistent-content-addressed-capability-core|ADR 0002]]: «capability требуют
//! персистентности (иначе теряются при перезагрузке)». Граница персистентного: цели
//! Store/Device/Value/Root долговечны; Endpoint/Reply указывают на процессы, которые пока
//! эфемерны, — эти права умирают с загрузкой (слот остаётся, с поколением: отзыв переживает).

use alloc::boxed::Box;
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

/// Создать домен защиты, вернуть его id. Если домен с таким именем уже есть (восстановлен
/// из `.cspace` — Веха 21.3), ПЕРЕИСПОЛЬЗОВАТЬ его: имя домена и есть персистентная личность,
/// по ней процесс новой загрузки находит права, выданные ему в прошлой.
pub fn create_domain(name: &'static str) -> DomainId {
    let mut cs = CSPACE.lock();
    if let Some(i) = cs.domains.iter().position(|d| d.name == name) {
        return i;
    }
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

/// Аттенуация СВОЕЙ копии (Веха 21.1): новый дескриптор в том же домене с правами
/// `права ∩ mask`. `GRANT` не требуется — урезать то, чем владеешь, безопасно всегда;
/// `GRANT` контролирует передачу ДРУГИМ (см. [`grant`]). Вместе они дают паттерн
/// «сервер-раздатчик»: derive урезает, grant-в-сообщении передаёт ([[ipc-cap-transfer]]).
pub fn derive(dom: DomainId, cap: Cap, mask: Rights) -> Result<Cap, CapError> {
    let mut cs = CSPACE.lock();
    let (target, rights) = {
        let e = resolve(&cs, dom, cap)?;
        (e.target.clone(), e.rights.intersect(mask))
    };
    Ok(alloc_slot(&mut cs.domains[dom], Entry { target, rights }))
}

// ─── Веха 21.3: персистентный c-space ────────────────────────────────────────

/// Спец-корень store, под которым лежит сериализованный c-space.
const CSPACE_ROOT: &str = ".cspace";

/// Найти в домене живой cap на STORE (для восстановления: процесс эфемерен и не помнит
/// своих дескрипторов из прошлой загрузки — ядро находит выживший в его домене и отдаёт
/// при spawn'е). Возвращает дескриптор и его права.
pub fn find_store_cap(dom: DomainId) -> Option<(Cap, Rights)> {
    let cs = CSPACE.lock();
    let d = cs.domains.get(dom)?;
    for (i, s) in d.slots.iter().enumerate() {
        if let Some(e) = &s.entry {
            if matches!(e.target, Target::Store) {
                return Some((Cap::new(i as u32, s.generation), e.rights));
            }
        }
    }
    None
}

/// Сериализовать ВЕСЬ c-space и зафиксировать в store под `.cspace` (+ commit — атомарный
/// чекпойнт, как у корней [[persistent-store]]). Формат: домены (имя, слоты), слот =
/// (поколение, вид цели, права, полезная нагрузка цели). Endpoint/Reply — эфемерные
/// (см. шапку модуля): пишутся как пустые слоты, поколение сохраняется (отзыв переживает
/// перезагрузку, само право — нет).
pub fn persist() {
    let bytes = {
        let cs = CSPACE.lock();
        let mut b: Vec<u8> = Vec::new();
        b.extend_from_slice(&(cs.domains.len() as u32).to_le_bytes());
        for d in &cs.domains {
            b.push(d.name.len() as u8);
            b.extend_from_slice(d.name.as_bytes());
            b.extend_from_slice(&(d.slots.len() as u32).to_le_bytes());
            for s in &d.slots {
                b.extend_from_slice(&s.generation.to_le_bytes());
                let kind: u8 = match &s.entry {
                    Some(e) => match &e.target {
                        Target::Store => 1,
                        Target::Device(Device::Block) => 2,
                        Target::Value(_) => 3,
                        Target::Root(_) => 4,
                        Target::Endpoint(_) | Target::Reply(_) => 0, // эфемерные — не переживают
                    },
                    None => 0,
                };
                b.push(kind);
                if kind != 0 {
                    let e = s.entry.as_ref().unwrap();
                    b.extend_from_slice(&e.rights.0.to_le_bytes());
                    match &e.target {
                        Target::Value(id) => b.extend_from_slice(&id.0),
                        Target::Root(name) => {
                            b.push(name.len() as u8);
                            b.extend_from_slice(name.as_bytes());
                        }
                        _ => {}
                    }
                }
            }
        }
        b
    }; // замок c-space отпущен ДО обращений к store (у него свои замки)
    let id = object::put(&bytes);
    object::set_root(CSPACE_ROOT, id);
    object::commit();
}

/// Восстановить c-space из `.cspace`. Возвращает число доменов (0 — корня нет, чистый старт).
/// Дескрипторы (слот+поколение) восстанавливаются В ТОЧНОСТИ — право, выданное в прошлой
/// загрузке, валидно в новой без повторной выдачи. Звать после `object::load` и до первых
/// [`create_domain`].
pub fn load() -> usize {
    let Some(id) = object::root(CSPACE_ROOT) else { return 0 };
    let bytes: Option<Vec<u8>> = object::with(&id, |x| x.map(Vec::from));
    let Some(b) = bytes else { return 0 };

    fn u32_at(b: &[u8], off: &mut usize) -> u32 {
        let v = u32::from_le_bytes(b[*off..*off + 4].try_into().unwrap());
        *off += 4;
        v
    }
    // Имена живут дольше любых структур — утекают в 'static (домены живут вечно и так).
    fn str_at(b: &[u8], off: &mut usize, len: usize) -> &'static str {
        let s = core::str::from_utf8(&b[*off..*off + len]).unwrap_or("?");
        *off += len;
        Box::leak(String::from(s).into_boxed_str())
    }

    let mut cs = CSPACE.lock();
    cs.domains.clear();
    let mut off = 0usize;
    let ndom = u32_at(&b, &mut off) as usize;
    for _ in 0..ndom {
        let nlen = b[off] as usize;
        off += 1;
        let name = str_at(&b, &mut off, nlen);
        let nslots = u32_at(&b, &mut off) as usize;
        let mut slots = Vec::with_capacity(nslots);
        for _ in 0..nslots {
            let generation = u32_at(&b, &mut off);
            let kind = b[off];
            off += 1;
            let entry = if kind == 0 {
                None
            } else {
                let rights = Rights(u32_at(&b, &mut off));
                let target = match kind {
                    1 => Target::Store,
                    2 => Target::Device(Device::Block),
                    3 => {
                        let mut idb = [0u8; 32];
                        idb.copy_from_slice(&b[off..off + 32]);
                        off += 32;
                        Target::Value(ContentId(idb))
                    }
                    _ => {
                        let l = b[off] as usize;
                        off += 1;
                        Target::Root(str_at(&b, &mut off, l))
                    }
                };
                Some(Entry { target, rights })
            };
            slots.push(Slot { generation, entry });
        }
        cs.domains.push(Domain { name, slots });
    }
    cs.domains.len()
}

/// «rwgsx»-строка прав для вывода (read · write · grant · send · exec).
pub fn rights_str(r: Rights) -> String {
    let mut s = String::new();
    s.push(if r.contains(Rights::READ) { 'r' } else { '-' });
    s.push(if r.contains(Rights::WRITE) { 'w' } else { '-' });
    s.push(if r.contains(Rights::GRANT) { 'g' } else { '-' });
    s.push(if r.contains(Rights::SEND) { 's' } else { '-' });
    s.push(if r.contains(Rights::EXEC) { 'x' } else { '-' });
    s
}
