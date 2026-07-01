//! void-abi — общие типы границы ядро/userspace и зачатки объектной модели.
//!
//! `#![no_std]`, без зависимостей: крейт используется и ядром, и (в будущем)
//! userspace-серверами, поэтому здесь живут только определения, общие для обеих
//! сторон границы. Содержательно наполняется на Вехах 6–8 (см. роадмап в Obsidian).
#![no_std]

/// Версия ABI. Растёт при несовместимых изменениях границы ядро/userspace.
pub const VERSION: u32 = 0;

/// Контент-адрес неизменяемого значения в объектном store — хэш его содержимого.
/// Основа контент-адресации из [[0002-persistent-content-addressed-capability-core]].
///
/// `Ord` нужен, чтобы класть `ContentId` в отсортированные структуры (BTreeMap хранилища).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(transparent)]
pub struct ContentId(pub [u8; 32]);

impl ContentId {
    /// Вычислить контент-адрес байтов. Детерминированно: одинаковые байты → один адрес.
    ///
    /// ЗАГЛУШКА, НЕ криптографическая: четыре независимых FNV-1a с разными базисами дают
    /// 4×64 = 256 бит. Для учебной объектной модели хватает (детерминизм + низкая вероятность
    /// случайных коллизий). Позже заменим на криптохэш (BLAKE3), когда контент-адрес станет
    /// границей доверия.
    pub fn hash(bytes: &[u8]) -> ContentId {
        // Разные стартовые базисы → четыре разных потока хэширования.
        const BASES: [u64; 4] = [
            0xcbf2_9ce4_8422_2325,
            0x9e37_79b9_7f4a_7c15,
            0xff51_afd7_ed55_8ccd,
            0xc4ce_b9fe_1a85_ec53,
        ];
        const PRIME: u64 = 0x0000_0100_0000_01b3; // FNV-1a 64-bit prime
        let mut out = [0u8; 32];
        for (k, &base) in BASES.iter().enumerate() {
            let mut h = base;
            for &b in bytes {
                h ^= b as u64;
                h = h.wrapping_mul(PRIME);
            }
            out[k * 8..k * 8 + 8].copy_from_slice(&h.to_le_bytes());
        }
        ContentId(out)
    }
}

/// Права, которые несёт capability (битовая маска). Вторая половина модели
/// KeyKOS/EROS из [[0002-persistent-content-addressed-capability-core]].
///
/// - `READ`  — прочитать значение (или текущую цель ячейки);
/// - `WRITE` — переустановить ячейку-корень на новое значение (значения неизменяемы,
///   поэтому «запись» — это мутация *ячейки*, а не байтов);
/// - `GRANT` — передать capability дальше (иначе право «залипает» у обладателя).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(transparent)]
pub struct Rights(pub u32);

impl Rights {
    pub const NONE: Rights = Rights(0);
    pub const READ: Rights = Rights(1 << 0);
    pub const WRITE: Rights = Rights(1 << 1);
    pub const GRANT: Rights = Rights(1 << 2);
    /// Полный набор — то, что получает владелец при mint.
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
