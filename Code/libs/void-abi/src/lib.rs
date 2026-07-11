//! void-abi — общие типы границы ядро/userspace и зачатки объектной модели.
//!
//! `#![no_std]`, без зависимостей: крейт используется и ядром, и (в будущем)
//! userspace-серверами, поэтому здесь живут только определения, общие для обеих
//! сторон границы. Содержательно наполняется на Вехах 6–8 (см. роадмап в Obsidian).
#![no_std]

/// Версия ABI. Растёт при несовместимых изменениях границы ядро/userspace.
/// v1 — Веха 21: IPC несёт capability (CALL: a6=право, возврат a1; RECV: a3; REPLY: a3),
/// новые право `EXEC` и syscall `CAP_DERIVE`.
pub const VERSION: u32 = 1;

/// Контент-адрес неизменяемого значения в объектном store — хэш его содержимого.
/// Основа контент-адресации из [[0002-persistent-content-addressed-capability-core]].
///
/// `Ord` нужен, чтобы класть `ContentId` в отсортированные структуры (BTreeMap хранилища).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(transparent)]
pub struct ContentId(pub [u8; 32]);

impl ContentId {
    /// Вычислить контент-адрес байтов — **BLAKE3** (256 бит). Детерминированно: одинаковые
    /// байты → один адрес. Криптографический: адрес можно использовать как границу доверия
    /// (проверка целостности, дедуп недоверенных данных), а вероятность коллизии —
    /// криптографически пренебрежима. no_std-портируемая реализация крейта `blake3`.
    pub fn hash(bytes: &[u8]) -> ContentId {
        ContentId(*blake3::hash(bytes).as_bytes())
    }
}

/// Права, которые несёт capability (битовая маска). Вторая половина модели
/// KeyKOS/EROS из [[0002-persistent-content-addressed-capability-core]].
///
/// - `READ`  — прочитать значение (или текущую цель ячейки); для устройства — читать сектора;
/// - `WRITE` — переустановить ячейку-корень на новое значение (значения неизменяемы,
///   поэтому «запись» — это мутация *ячейки*, а не байтов);
/// - `GRANT` — передать capability дальше (иначе право «залипает» у обладателя);
/// - `SEND`  — отправить сообщение эндпоинту (право вызвать сервер по IPC).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(transparent)]
pub struct Rights(pub u32);

impl Rights {
    pub const NONE: Rights = Rights(0);
    pub const READ: Rights = Rights(1 << 0);
    pub const WRITE: Rights = Rights(1 << 1);
    pub const GRANT: Rights = Rights(1 << 2);
    /// Право отправить сообщение эндпоинту (вызвать сервер по IPC) — см. `kernel/src/proc.rs`.
    pub const SEND: Rights = Rights(1 << 3);
    /// Право ЗАПУСТИТЬ программу из store по имени корня (`SYS_EXEC`, Веха 20). Отдельно от
    /// `READ`/`WRITE`: обладатель может исполнять программы, не имея права читать или менять
    /// объекты, — аттенуация «только запуск» (как исполняемый-но-не-читаемый файл, только честно).
    pub const EXEC: Rights = Rights(1 << 4);
    /// Полный набор прав на значение/ячейку — то, что получает владелец при mint.
    pub const ALL: Rights = Rights(0b111);

    /// Содержит ли `self` все биты из `other`.
    pub const fn contains(self, other: Rights) -> bool {
        self.0 & other.0 == other.0
    }
    /// Пересечение прав — основа аттенуации (сужения) при передаче.
    pub const fn intersect(self, other: Rights) -> Rights {
        Rights(self.0 & other.0)
    }
    /// Объединение прав.
    pub const fn union(self, other: Rights) -> Rights {
        Rights(self.0 | other.0)
    }
    /// Является ли `self` подмножеством `other` (нельзя расширить право сверх исходного).
    pub const fn is_subset_of(self, other: Rights) -> bool {
        self.0 & other.0 == self.0
    }
}

/// Capability — непод­делываемая ссылка на объект с набором прав.
///
/// Для обладателя это **непрозрачный дескриптор**: слот в c-space домена + поколение
/// этого слота. Само по себе число ничего не даёт — ядро проверяет его по таблице прав
/// домена перед каждым доступом (см. `kernel/src/cap.rs`). Поколение делает возможным
/// отзыв: при освобождении слота оно растёт, и старые дескрипторы становятся устаревшими.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(transparent)]
pub struct Cap(u64);

impl Cap {
    /// Собрать дескриптор из номера слота и поколения.
    pub const fn new(slot: u32, generation: u32) -> Cap {
        Cap(((slot as u64) << 32) | generation as u64)
    }
    /// Восстановить дескриптор из сырых битов (как он пересекает границу ядро/userspace:
    /// процесс держит `Cap` как непрозрачное число в регистре и предъявляет его в syscall).
    pub const fn from_bits(bits: u64) -> Cap {
        Cap(bits)
    }
    /// Номер слота в c-space.
    pub const fn slot(self) -> u32 {
        (self.0 >> 32) as u32
    }
    /// Поколение слота на момент выдачи.
    pub const fn generation(self) -> u32 {
        self.0 as u32
    }
    /// Сырые биты (то, что пересекало бы границу ядро/userspace).
    pub const fn bits(self) -> u64 {
        self.0
    }
}
