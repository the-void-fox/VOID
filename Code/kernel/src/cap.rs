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
    /// Сетевая карта virtio-net (Веха 34): право слать/принимать сырые Ethernet-кадры.
    Net,
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
    /// Веха 51 — окно MMIO устройства (физ. база + длина): право замапить регистры железа в
    /// адресное пространство userspace-драйвера (`SYS_MMIO_MAP`). Эфемерно (минтится на загрузке
    /// после PCI-поиска; не переживает перезагрузку — устройство ищется заново).
    Mmio { base: usize, len: usize },
    /// Веха 51 — право выделять DMA-память (`SYS_DMA_ALLOC`): физически-адресуемые страницы под
    /// кольца/буферы устройства (userspace программирует железо физ-адресами). Без IOMMU это
    /// ДОВЕРЕННОЕ право (DMA куда угодно) — даётся только драйверам. Эфемерно.
    Dma,
    /// Веха 101 — право ВЫКЛЮЧИТЬ машину (`SYS_POWEROFF`). Отдельным правом, а не «может любой»:
    /// выключение — это одностороннее действие над всей системой, ровно то, что модель
    /// capability обязана называть вслух. Выдаётся токеном `power` в конфиге — обычно шеллу.
    /// Эфемерно (минтится на каждой загрузке из конфига, как MMIO/DMA).
    Power,
    /// Веха 129 — **область разделяемой памяти** ([[shm]]): право отобразить у себя ТЕ ЖЕ
    /// физические страницы. Создатель получает право при `SYS_SHM_NEW` и передаёт его по IPC
    /// тому, с кем делится буфером; `READ` — отобразить на чтение, `WRITE` — ещё и на запись.
    ///
    /// Так «этот процесс имеет доступ к буферу того окна» становится проверяемым фактом, а не
    /// соглашением: без права область не отобразить, а право не подделать. Эфемерно — область
    /// живёт в памяти и перезагрузку не переживает.
    Shm(usize),
    /// Веха 52 — прерывание устройства: право ждать IRQ (`SYS_IRQ_WAIT`). `vector` — на который
    /// ядро замаршрутизировало IRQ устройства (IOAPIC → LAPIC). Так userspace-драйвер спит до
    /// прерывания вместо опроса. Эфемерно (маршрутизация ставится на загрузке).
    Irq { vector: u8 },
    /// Веха 153 — **обзор и управление процессами** ([[task-manager]]): `READ` — видеть, ЧТО
    /// запущено и что оно может (перечислить процессы, их права ГРАФОМ, счётчики IPC); `WRITE` —
    /// отзывать чужие права на ходу и щёлкать рубильником сети. Ambient-доступа к списку
    /// процессов у нас нет (в отличие от `/proc`): «видеть запущенное» — само по себе capability,
    /// и по умолчанию его нет ни у кого, кроме диспетчера задач. init минтит его из конфига
    /// поколения (токен `sysview`); эфемерно — не переживает перезагрузку, минтится заново.
    Sysview,
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
    /// Веха 152.3 — слот поднят из `.cspace` ([`load`]) и ещё НЕ усыновлён живым наделением.
    /// Такой слот — власть ПРОШЛОЙ загрузки: пока процесс новой не подтвердил его своим
    /// поколением ([`clamp_persisted`]), он не должен давать власть сверх выданной. Свежий минт
    /// этого боута — `false`: он и есть выданное.
    persisted: bool,
    /// Веха 154 — право остаётся у ЭТОГО процесса, но НЕ наследуется его детьми при spawn'е.
    /// Тому, кто владеет экраном (`mmio:fb`) или гасит машину (`power`), незачем раздавать это
    /// всем окнам, которые он открывает: наследование прав — копиями (`proc.rs`), и без пометки
    /// каждый клиент композитора получал прямой фреймбуфер и выключение впридачу. Не персистится
    /// (восстановление даёт `false`): пометку заново ставит init из конфига поколения каждый боут.
    noinherit: bool,
}

/// Домен защиты: имя + его личное capability-пространство (c-space).
struct Domain {
    /// Имя-личность программы. **Пустое — домен СВОБОДЕН** (тумба): его место в таблице занимает
    /// следующий проситель. Освобождаем именно так, а не удалением из вектора: `DomainId` — это
    /// индекс, и сдвиг вектора переадресовал бы права живых процессов на чужие таблицы.
    name: &'static str,
    slots: Vec<Slot>,
    /// Веха 156 — доменом СЕЙЧАС владеет живой процесс. Пока владеет, его тёзке домен не отдаём.
    live: bool,
    /// Веха 156 — c-space ВТОРОГО живого тёзки: не личность программы, а временная таблица.
    /// Не персистится и умирает вместе с процессом (домен становится тумбой).
    transient: bool,
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
///
/// Для ПРОЦЕССОВ зовётся не это, а [`claim_domain`]: живой тёзка не должен получить чужую
/// таблицу прав. Здесь остаются домены ядра и демо — они не «живут» и тёзок не имеют.
pub fn create_domain(name: &'static str) -> DomainId {
    let mut cs = CSPACE.lock();
    if let Some(i) = cs.domains.iter().position(|d| d.name == name && !d.transient) {
        return i;
    }
    push_domain(&mut cs, name, false, false)
}

/// Завести домен в свободной тумбе (или в конце таблицы) и вернуть его id.
fn push_domain(cs: &mut CSpace, name: &'static str, live: bool, transient: bool) -> DomainId {
    let d = Domain { name, slots: Vec::new(), live, transient };
    match cs.domains.iter().position(|x| x.name.is_empty()) {
        Some(i) => {
            cs.domains[i] = d;
            i
        }
        None => {
            cs.domains.push(d);
            cs.domains.len() - 1
        }
    }
}

/// Веха 156 — **занять домен под ЖИВОЙ процесс** (единственный путь для `create_process_locked`).
///
/// До вехи домен искался по имени и отдавался кому угодно — а значит ДВА РАБОТАЮЩИХ процесса
/// одной программы делили одну таблицу прав. Это видно было глазами в диспетчере задач: у одного
/// диспетчера в графе стояли эндпоинты другого, и «отнять» отбирало право у обоих сразу. Хуже
/// того, право, выданное одному экземпляру по политике (обзор процессов от композитора),
/// оказывалось в таблице второго, которому его не давали.
///
/// Правило: **имя — личность ПРОГРАММЫ, но таблица прав — у ПРОЦЕССА.**
/// - канонический домен имени свободен → занять его (так персистентность Вехи 21.3 работает как
///   работала: процесс новой загрузки находит права, выданные в прошлой);
/// - канонический занят живым тёзкой → завести ВРЕМЕННЫЙ c-space, который умрёт вместе с
///   процессом и не попадёт в `.cspace`: у второго экземпляра нет прошлого, за которое его можно
///   было бы наделить.
pub fn claim_domain(name: &'static str) -> DomainId {
    let mut cs = CSPACE.lock();
    match cs.domains.iter().position(|d| d.name == name && !d.transient) {
        Some(i) if !cs.domains[i].live => {
            cs.domains[i].live = true;
            i
        }
        Some(_) => push_domain(&mut cs, name, true, true),
        None => push_domain(&mut cs, name, true, false),
    }
}

/// Веха 156 — процесс умер: отпустить его домен. Канонический освобождается для следующего
/// тёзки (права остаются — это персистентная личность программы, Веха 21.3), временный
/// становится ТУМБОЙ: его таблица уходит вместе с процессом, которому она принадлежала.
pub fn release_domain(dom: DomainId) {
    let mut cs = CSPACE.lock();
    let Some(d) = cs.domains.get_mut(dom) else { return };
    d.live = false;
    if d.transient {
        d.slots = Vec::new();
        d.name = "";
        d.transient = false;
    }
}

/// Веха 89 — **отозвать все права, указывающие на умерший процесс**: эндпоинты и reply-права
/// эфемерны и живут ровно столько, сколько процесс. Пока слоты процессов не переиспользовались,
/// устаревший cap просто указывал в мертвеца; с переиспользованием (`proc`, та же веха) он стал
/// бы указывать на ЧУЖОЙ, НОВЫЙ процесс — то есть право появлялось бы из ниоткуда, ровно то, что
/// модель обязана исключать.
///
/// Поколение слота растёт, поэтому старый дескриптор честно становится `Stale` — тем же
/// механизмом, что и обычный [`revoke`].
pub fn revoke_process(pid: usize) {
    let mut cs = CSPACE.lock();
    for d in cs.domains.iter_mut() {
        for s in d.slots.iter_mut() {
            let hit = matches!(
                s.entry.as_ref().map(|e| &e.target),
                Some(Target::Endpoint(p) | Target::Reply(p)) if *p == pid
            );
            if hit {
                s.entry = None;
                s.generation = s.generation.wrapping_add(1);
            }
        }
    }
}

/// Веха 101 — есть ли у домена право ВЫКЛЮЧИТЬ машину по этому дескриптору (`Power` + WRITE).
/// Отдельная функция, а не проверка на месте: единственное употребление, зато названо вслух.
pub fn may_power_off(dom: DomainId, cap: Cap) -> bool {
    let cs = CSPACE.lock();
    match resolve(&cs, dom, cap) {
        Ok(e) => matches!(e.target, Target::Power) && e.rights.contains(Rights::WRITE),
        Err(_) => false,
    }
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
            s.persisted = false; // свежий минт — не наследство прошлой загрузки
            s.noinherit = false; // Веха 154 — переиспользованный слот наследуется, пока не помечен
            return Cap::new(i as u32, s.generation);
        }
    }
    dom.slots.push(Slot { generation: 1, entry: Some(entry), persisted: false, noinherit: false });
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

/// Веха 152.2 — **вид и права** дескриптора: read-only интроспекция для `SYS_CAP_INFO`.
///
/// Возвращает `(код вида, права)`. Кода вида хватает, чтобы отличить «дотянулся до store-read,
/// которое и так есть» от «дотянулся до POWER, которого не давали». Побочного эффекта нет — это
/// не «действие правом», а его ОПИСАНИЕ; узнать вид можно только про cap, который уже держишь.
/// Коды видов держит [`info_kind`], чтобы userspace и ядро называли их одинаково.
pub fn info(dom: DomainId, cap: Cap) -> Result<(u8, Rights), CapError> {
    let cs = CSPACE.lock();
    let e = resolve(&cs, dom, cap)?;
    Ok((info_kind(&e.target), e.rights))
}

/// Код вида цели — общий словарь ядра и зонда конфайнмента ([[redteam]]).
pub fn info_kind(t: &Target) -> u8 {
    match t {
        Target::Store => 1,
        Target::Root(_) => 2,
        Target::Value(_) => 3,
        Target::Endpoint(_) => 4,
        Target::Reply(_) => 5,
        Target::Device(Device::Block) => 6,
        Target::Device(Device::Net) => 7,
        Target::Mmio { .. } => 8,
        Target::Dma => 9,
        Target::Power => 10,
        Target::Shm(_) => 11,
        Target::Irq { .. } => 12,
        Target::Sysview => 13,
    }
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
        // Эндпоинт/reply/устройство/store/mmio/dma — не значения: их «читают» через IPC/BLK_READ/etc.
        Target::Endpoint(_) | Target::Reply(_) | Target::Device(_) | Target::Store
        | Target::Mmio { .. } | Target::Dma | Target::Irq { .. } | Target::Power
        | Target::Shm(_) | Target::Sysview => return Err(CapError::WrongKind),
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
            | Target::Store | Target::Mmio { .. } | Target::Dma | Target::Irq { .. }
            | Target::Power | Target::Shm(_) | Target::Sysview => return Err(CapError::WrongKind),
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

/// Веха 51 — разрешить cap на **окно MMIO устройства** и вернуть `(база, длина)` физ-региона
/// (требует `need`, обычно `READ|WRITE`). Так `SYS_MMIO_MAP` даёт userspace-драйверу регистры
/// железа только при наличии права.
pub fn mmio(dom: DomainId, cap: Cap, need: Rights) -> Result<(usize, usize), CapError> {
    let cs = CSPACE.lock();
    let e = resolve(&cs, dom, cap)?;
    if !e.rights.contains(need) {
        return Err(CapError::Denied);
    }
    match e.target {
        Target::Mmio { base, len } => Ok((base, len)),
        _ => Err(CapError::WrongKind),
    }
}

/// Веха 51 — проверить право на **DMA-память** (`SYS_DMA_ALLOC`). Требует `need` (обычно `WRITE`).
pub fn dma(dom: DomainId, cap: Cap, need: Rights) -> Result<(), CapError> {
    let cs = CSPACE.lock();
    let e = resolve(&cs, dom, cap)?;
    if !e.rights.contains(need) {
        return Err(CapError::Denied);
    }
    match e.target {
        Target::Dma => Ok(()),
        _ => Err(CapError::WrongKind),
    }
}

/// Веха 52 — разрешить cap на **прерывание устройства** и вернуть вектор (требует `READ`).
/// Так `SYS_IRQ_WAIT` даёт userspace-драйверу спать до IRQ только при наличии права.
/// Веха 129 — проверить право на ОБЛАСТЬ разделяемой памяти и вернуть её индекс.
pub fn shm(dom: DomainId, cap: Cap, need: Rights) -> Result<usize, CapError> {
    let cs = CSPACE.lock();
    let e = resolve(&cs, dom, cap)?;
    if !e.rights.contains(need) {
        return Err(CapError::Denied);
    }
    match e.target {
        Target::Shm(id) => Ok(id),
        _ => Err(CapError::WrongKind),
    }
}

pub fn irq(dom: DomainId, cap: Cap, need: Rights) -> Result<u8, CapError> {
    let cs = CSPACE.lock();
    let e = resolve(&cs, dom, cap)?;
    if !e.rights.contains(need) {
        return Err(CapError::Denied);
    }
    match e.target {
        Target::Irq { vector } => Ok(vector),
        _ => Err(CapError::WrongKind),
    }
}

/// Веха 153 — проверить право **обзора/управления процессами** (требует `need`: `READ` для
/// перечисления/графа/счётчиков, `WRITE` для отзыва прав и рубильника сети). Без такого cap
/// ядро не выдаёт наружу НИЧЕГО о чужих процессах — «видеть запущенное» само есть право
/// ([[task-manager]]).
pub fn sysview(dom: DomainId, cap: Cap, need: Rights) -> Result<(), CapError> {
    let cs = CSPACE.lock();
    let e = resolve(&cs, dom, cap)?;
    if !e.rights.contains(need) {
        return Err(CapError::Denied);
    }
    match e.target {
        Target::Sysview => Ok(()),
        _ => Err(CapError::WrongKind),
    }
}

/// Веха 153.2 — перечислить ЖИВЫЕ слоты домена для диспетчера задач ([[task-manager]]):
/// `(слот, вид, права, aux)`. `aux` — id СВЯЗАННОГО процесса у `Endpoint`/`Reply` (это и строит
/// ГРАФ «кто чей эндпоинт держит»: слот `Endpoint(Y)` в домене X = ребро «X может позвать Y»),
/// иначе `u16::MAX`. Гейт права Sysview — на вызывающем (в `proc.rs`), сама функция лишь читает.
pub fn list_caps(dom: DomainId) -> Vec<(u16, u8, u32, u16)> {
    let cs = CSPACE.lock();
    let mut out = Vec::new();
    let Some(d) = cs.domains.get(dom) else { return out };
    for (i, s) in d.slots.iter().enumerate() {
        if let Some(e) = &s.entry {
            let aux = match &e.target {
                Target::Endpoint(p) | Target::Reply(p) => *p as u16,
                _ => u16::MAX,
            };
            out.push((i as u16, info_kind(&e.target), e.rights.0 as u32, aux));
        }
    }
    out
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

/// Наделить потомка (Веха 30): скопировать capability из домена `from` в домен `to` —
/// БЕЗ требования `GRANT` и без маски, тем же правом. Это не передача равному ([`grant`]),
/// а наделение СОЗДАВАЕМОГО ребёнка стартовым набором (как preopen'ы WASI): родитель и так
/// может действовать этим правом сам или проксировать каждый вызов через себя — новых
/// полномочий у пары родитель+ребёнок не появляется. Зовёт только ядро из `SYS_EXEC`;
/// syscall'а с такой силой нет.
pub fn endow(from: DomainId, cap: Cap, to: DomainId) -> Result<Cap, CapError> {
    let mut cs = CSPACE.lock();
    let (target, rights) = {
        let e = resolve(&cs, from, cap)?;
        (e.target.clone(), e.rights)
    };
    Ok(alloc_slot(&mut cs.domains[to], Entry { target, rights }))
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

/// Веха 153.4 — отозвать право в СЛОТЕ домена (что бы там ни лежало сейчас) — для диспетчера
/// задач ([[task-manager]]): «отобрать это право у того процесса на ходу». В отличие от [`revoke`]
/// не требует дескриптора с верным поколением: диспетчер держит не сам cap цели, а лишь номер
/// слота (из [`list_caps`]). Бумкает поколение — уже выданные дескрипторы на этот слот протухают,
/// то есть процесс теряет право немедленно (следующий resolve его отвергнет). `false` — слота нет
/// или он пуст. Гейт (право Sysview WRITE у вызывающего) — на стороне `proc.rs`.
pub fn revoke_slot(dom: DomainId, slot: usize) -> bool {
    let mut cs = CSPACE.lock();
    let Some(d) = cs.domains.get_mut(dom) else { return false };
    let Some(s) = d.slots.get_mut(slot) else { return false };
    if s.entry.is_none() {
        return false;
    }
    s.entry = None;
    s.generation = s.generation.wrapping_add(1);
    true
}

/// Веха 154 — пометить право «не наследуемым»: оно остаётся у своего домена, но spawn НЕ
/// скопирует его детям ([`inheritable`]). Ставит init для `mmio:fb!`/`power!` в конфиге поколения.
/// Тихо игнорирует несуществующий/протухший слот.
pub fn set_noinherit(dom: DomainId, cap: Cap) {
    let mut cs = CSPACE.lock();
    let Some(d) = cs.domains.get_mut(dom) else { return };
    let Some(s) = d.slots.get_mut(cap.slot() as usize) else { return };
    if s.generation == cap.generation() && s.entry.is_some() {
        s.noinherit = true;
    }
}

/// Веха 154 — наследуется ли право ребёнку при spawn'е. `false` — либо помечено `set_noinherit`,
/// либо слот пуст/протух (наследовать нечего). Читает spawn перед [`endow`] дочернего домена.
pub fn inheritable(dom: DomainId, cap: Cap) -> bool {
    let cs = CSPACE.lock();
    let Some(d) = cs.domains.get(dom) else { return false };
    let Some(s) = d.slots.get(cap.slot() as usize) else { return false };
    s.generation == cap.generation() && s.entry.is_some() && !s.noinherit
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

/// Та же цель ли (по СОДЕРЖИМОМУ, не по слоту): нужна [`clamp_persisted`], чтобы сверять
/// наследованную власть с выданной по КОНКРЕТНОЙ цели (не «любой root», а «этот root»).
fn target_eq(a: &Target, b: &Target) -> bool {
    match (a, b) {
        (Target::Store, Target::Store) => true,
        (Target::Dma, Target::Dma) => true,
        (Target::Power, Target::Power) => true,
        (Target::Root(x), Target::Root(y)) => x == y,
        (Target::Value(x), Target::Value(y)) => x.0 == y.0,
        (Target::Endpoint(x), Target::Endpoint(y)) => x == y,
        (Target::Reply(x), Target::Reply(y)) => x == y,
        (Target::Device(x), Target::Device(y)) => x == y,
        (Target::Mmio { base: b1, len: l1 }, Target::Mmio { base: b2, len: l2 }) => {
            b1 == b2 && l1 == l2
        }
        (Target::Shm(x), Target::Shm(y)) => x == y,
        (Target::Irq { vector: v1 }, Target::Irq { vector: v2 }) => v1 == v2,
        (Target::Sysview, Target::Sysview) => true,
        _ => false,
    }
}

/// Веха 152.3 — **потолок наделения**: персистентное право не может дать процессу власть
/// СВЕРХ той, что дало ему поколение (конфиг/родитель).
///
/// Находка №1 красной команды ([[redteam]]): c-space привязан к ИМЕНИ, а `.cspace` переживает
/// перезагрузку, — значит привилегированный тёзка (`store:rwg`), персистнувший домен, оставлял
/// урезанному тёзке (`store:rx`) на СЛЕДУЮЩЕЙ загрузке власть, которой тому не давали (перебором
/// дескрипторов, без гранта). Корень: имена не уникальны по привилегии, а наследование таблицы
/// было молчаливым.
///
/// Правка: **авторитетно НАДЕЛЕНИЕ, а не имя.** Зовётся, когда наделение процесса собрано
/// (`start_caps`). Для каждого слота, поднятого из `.cspace` и ещё не усыновлённого:
/// - покрыт наделением (та же цель, права — надмножество) → усыновить (снять флаг: теперь его
///   держит живой процесс, и обычный mint/derive этого боута работает как прежде);
/// - НЕ покрыт → отозвать (освободить слот, бумкнуть поколение). Так `.cspace` может лишь
///   ВОССТАНОВИТЬ власть, которую поколение и так даёт, но никогда её не расширить.
///
/// Власть ВНУТРИ одной загрузки безопасна аттенуацией (derive/grant только сужают), поэтому
/// тёзки-накопители дают лишь DUP (лишние ссылки на ту же власть), не эскалацию — их не трогаем.
pub fn clamp_persisted(dom: DomainId, endowment: &[Cap]) {
    let mut cs = CSPACE.lock();
    // Наделённая власть — (цель, права) каждого честно выданного дескриптора. Клонируем, чтобы
    // отпустить заимствование `cs` перед мутацией слотов ниже.
    let mut allowed: Vec<(Target, Rights)> = Vec::new();
    for &c in endowment {
        if let Ok(e) = resolve(&cs, dom, c) {
            allowed.push((e.target.clone(), e.rights));
        }
    }
    let Some(d) = cs.domains.get_mut(dom) else { return };
    for s in d.slots.iter_mut() {
        if !s.persisted {
            continue;
        }
        match &s.entry {
            None => s.persisted = false,
            Some(e) => {
                let covered = allowed
                    .iter()
                    .any(|(t, r)| target_eq(t, &e.target) && e.rights.0 & !r.0 == 0);
                if covered {
                    s.persisted = false; // усыновлено живым наделением
                } else {
                    s.entry = None;
                    s.generation = s.generation.wrapping_add(1);
                    s.persisted = false;
                }
            }
        }
    }
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
        // Веха 156 — пишем только ЛИЧНОСТИ: тумбы (пустое имя) и временные c-space вторых живых
        // тёзок в `.cspace` не едут. Иначе после перезагрузки в таблице оказались бы два домена
        // с одним именем, и следующий процесс занял бы случайный из них.
        let keep: Vec<&Domain> =
            cs.domains.iter().filter(|d| !d.name.is_empty() && !d.transient).collect();
        b.extend_from_slice(&(keep.len() as u32).to_le_bytes());
        for d in keep {
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
                        Target::Device(Device::Net) => 5,
                        // Эфемерные (не переживают ребут): reply, эндпоинты, MMIO/DMA-права
                        // драйверов (минтятся заново после PCI-поиска), области разделяемой
                        // памяти (живут в RAM и умирают вместе с ней).
                        Target::Endpoint(_) | Target::Reply(_)
                        | Target::Mmio { .. } | Target::Dma | Target::Irq { .. }
                        | Target::Power | Target::Shm(_) | Target::Sysview => 0,
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
    // Веха 33: без прямого commit — фиксацию пачкой сделает group commit
    // ([`object::maybe_commit`]); передача права переживает крах с окном ≤ ~2 с.
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
                    5 => Target::Device(Device::Net),
                    _ => {
                        let l = b[off] as usize;
                        off += 1;
                        Target::Root(str_at(&b, &mut off, l))
                    }
                };
                Some(Entry { target, rights })
            };
            // Веха 152.3 — долговечное право прошлой загрузки: помечаем персистентным, пока
            // живой процесс не подтвердит его своим наделением ([`clamp_persisted`]). Пустой
            // слот усыновлять нечего.
            let persisted = entry.is_some();
            // Веха 154 — пометка «не наследуется» не персистится: её ставит init из конфига
            // каждый боут (mmio/power — свежий минт этого поколения, не наследство из store).
            slots.push(Slot { generation, entry, persisted, noinherit: false });
        }
        // Веха 156 — поднятый домен никем не занят: живым его сделает первый же процесс с этим
        // именем (`claim_domain`), и он же тогда получит права прошлой загрузки.
        cs.domains.push(Domain { name, slots, live: false, transient: false });
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
