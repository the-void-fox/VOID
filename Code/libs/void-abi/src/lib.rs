//! void-abi — общие типы границы ядро/userspace и зачатки объектной модели.
//!
//! `#![no_std]`, без зависимостей: крейт используется и ядром, и (в будущем)
//! userspace-серверами, поэтому здесь живут только определения, общие для обеих
//! сторон границы. Содержательно наполняется на Вехах 6–8 (см. роадмап в Obsidian).
#![no_std]

/// Версия ABI. Растёт при несовместимых изменениях границы ядро/userspace.
/// v1 — Веха 21: IPC несёт capability (CALL: a6=право, возврат a1; RECV: a3; REPLY: a3),
/// новые право `EXEC` и syscall `CAP_DERIVE`.
/// v2 — Веха 30 (контракт запуска): `SYS_EXEC` несёт argv (a3/a4), новые `SYS_ARGS` (18:
/// argv/env процесса) и `SYS_STARTCAP` (19: таблица стартовых прав — «preopen'ы»);
/// env и стартовые права наследуются детям при exec; персоналия: seek (7) и rename (8).
pub const VERSION: u32 = 2;

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

// ─── тесты (Веха 214.9) ──────────────────────────────────────────────────────
//
// Здесь живут ДВЕ вещи, на которых стоит вся модель прав: аттенуация (право можно только сузить)
// и поколение слота (отзыв). Обе — чистая арифметика над числами, то есть проверяемы на хосте
// без ядра, QEMU и железа. До Вехи 214.9 их не проверял никто: у ядра не было ни одного теста,
// а эти функции — его фундамент.
//
// Тесты пишутся про то, что ЕСТЬ, а не про то, как хотелось бы. Там, где поведение удивляет
// (`ALL` — не «все права»), тест это фиксирует и объясняет: сюрприз, записанный в тест, перестаёт
// быть ловушкой.
#[cfg(test)]
mod tests {
    use super::*;

    // ── права: аттенуация ────────────────────────────────────────────────────

    #[test]
    fn пересечение_только_сужает() {
        let rw = Rights::READ.union(Rights::WRITE);
        // Пересечение с чем угодно не может дать бит, которого не было слева.
        for other in [Rights::NONE, Rights::READ, Rights::GRANT, Rights::ALL] {
            assert!(rw.intersect(other).is_subset_of(rw));
        }
        // И наоборот: объединением расширить МОЖНО — поэтому передача права пользуется
        // пересечением, а не объединением.
        assert!(!rw.union(Rights::GRANT).is_subset_of(rw));
    }

    #[test]
    fn пересечение_коммутативно_и_идемпотентно() {
        let a = Rights::READ.union(Rights::GRANT);
        let b = Rights::WRITE.union(Rights::GRANT);
        assert_eq!(a.intersect(b), b.intersect(a));
        assert_eq!(a.intersect(a), a);
    }

    #[test]
    fn пустое_право_подмножество_любого() {
        for r in [Rights::NONE, Rights::READ, Rights::ALL, Rights::EXEC] {
            assert!(Rights::NONE.is_subset_of(r));
            assert_eq!(r.intersect(Rights::NONE), Rights::NONE);
            assert!(r.contains(Rights::NONE));
        }
    }

    #[test]
    fn contains_и_is_subset_of_смотрят_в_разные_стороны() {
        let rw = Rights::READ.union(Rights::WRITE);
        assert!(rw.contains(Rights::READ));
        assert!(Rights::READ.is_subset_of(rw));
        assert!(!Rights::READ.contains(rw));
        assert!(!rw.is_subset_of(Rights::READ));
    }

    /// `ALL` — НЕ «все права», а только права на значение/ячейку: `READ|WRITE|GRANT`.
    ///
    /// `SEND` (вызвать эндпоинт) и `EXEC` (запустить программу) в него не входят, и это замысел:
    /// владелец объекта не получает право звать чужие серверы просто потому, что он владелец.
    /// Имя при этом обманчиво, поэтому здесь тест, а не надежда на внимательность.
    #[test]
    fn all_это_права_на_значение_а_не_все_подряд() {
        assert!(Rights::ALL.contains(Rights::READ));
        assert!(Rights::ALL.contains(Rights::WRITE));
        assert!(Rights::ALL.contains(Rights::GRANT));
        assert!(!Rights::ALL.contains(Rights::SEND));
        assert!(!Rights::ALL.contains(Rights::EXEC));
    }

    #[test]
    fn биты_прав_не_пересекаются() {
        let all = [
            Rights::READ, Rights::WRITE, Rights::GRANT, Rights::SEND, Rights::EXEC,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_eq!(a.intersect(*b), Rights::NONE, "два права делят один бит");
            }
        }
    }

    // ── дескриптор: слот и поколение ─────────────────────────────────────────

    #[test]
    fn слот_и_поколение_достаются_обратно() {
        for (slot, gen) in [(0, 0), (1, 7), (0xffff_ffff, 0), (0, 0xffff_ffff), (42, 1)] {
            let c = Cap::new(slot, gen);
            assert_eq!(c.slot(), slot);
            assert_eq!(c.generation(), gen);
        }
    }

    #[test]
    fn сырые_биты_пересекают_границу_без_потерь() {
        let c = Cap::new(0x1234_5678, 0x9abc_def0);
        assert_eq!(Cap::from_bits(c.bits()), c);
    }

    /// Отзыв: слот тот же, поколение выросло — значит СТАРЫЙ дескриптор больше не тот.
    /// Именно на этом держится механизм отзыва, и проверять его надо здесь, а не гадать.
    #[test]
    fn рост_поколения_обесценивает_прежний_дескриптор() {
        let было = Cap::new(5, 1);
        let стало = Cap::new(5, 2);
        assert_ne!(было, стало);
        assert_eq!(было.slot(), стало.slot());
        assert!(стало.generation() > было.generation());
    }

    #[test]
    fn разные_слоты_разные_дескрипторы() {
        assert_ne!(Cap::new(1, 0), Cap::new(2, 0));
    }

    // ── контент-адрес ────────────────────────────────────────────────────────

    #[test]
    fn один_и_тот_же_вход_даёт_один_адрес() {
        assert_eq!(ContentId::hash(b"void"), ContentId::hash(b"void"));
        assert_ne!(ContentId::hash(b"void"), ContentId::hash(b"voip"));
        // Пустой вход — тоже вход, а не ошибка: у пустого значения есть адрес.
        assert_ne!(ContentId::hash(b""), ContentId::hash(b"\0"));
    }

    /// Сверка с ЭТАЛОНОМ BLAKE3, а не с самим собой: тест «хэш равен хэшу» прошёл бы и на
    /// сломанной реализации. Вектор — официальный для пустого входа.
    #[test]
    fn адрес_пустого_значения_совпадает_с_эталоном_blake3() {
        let ожидание = [
            0xaf, 0x13, 0x49, 0xb9, 0xf5, 0xf9, 0xa1, 0xa6, 0xa0, 0x40, 0x4d, 0xea, 0x36, 0xdc,
            0xc9, 0x49, 0x9b, 0xcb, 0x25, 0xc9, 0xad, 0xc1, 0x12, 0xb7, 0xcc, 0x9a, 0x93, 0xca,
            0xe4, 0x1f, 0x32, 0x62,
        ];
        assert_eq!(ContentId::hash(b"").0, ожидание);
    }
}
