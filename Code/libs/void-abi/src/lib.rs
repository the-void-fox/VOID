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

/// Capability — непод­делываемая ссылка на объект с набором прав.
///
/// Пока непрозрачный токен (индекс в таблице прав процесса). Наполним на Вехе 8.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(transparent)]
pub struct Cap(pub u64);
