//! Формат store: что он обещает и что обязан выдержать (Веха 214.9).
//!
//! На этом крейте стоит вся персистентность VOID: объекты, корни, поколения, откат. Его читают
//! ДВОЕ — ядро на устройстве и хостовая утилита `void-store-import`, — и расхождение между ними
//! значило бы, что образ, собранный на хосте, не читается на машине.
//!
//! До Вехи 214.9 тестов здесь было два, и оба про один случай (`interleave.rs` — чередование
//! записи и чтения). Остальное проверялось запуском системы: формат ломается редко, но ломается
//! МОЛЧА, и находится это через день, в середине распаковки пакета.
//!
//! Носитель здесь заведомо честный (обычная память), поэтому всякое расхождение означает ровно
//! одно: store записал или запомнил не то.

use void_store::{scratch_sector, BlockIo, Store, TAIL_RESERVED, SECTOR};

/// Идеальный носитель: что записали, то и прочли. Ошибок не выдумывает.
struct Ram {
    sectors: Vec<[u8; SECTOR]>,
    /// Веха 214.9 — в какие секторы вообще писали. Нужен, чтобы доказать, что хвост не трогают.
    touched: Vec<u64>,
}

impl Ram {
    fn new(sectors: usize) -> Self {
        Ram { sectors: vec![[0u8; SECTOR]; sectors], touched: Vec::new() }
    }
}

impl BlockIo for Ram {
    fn read(&mut self, sector: u64, buf: &mut [u8; SECTOR]) -> bool {
        match self.sectors.get(sector as usize) {
            Some(s) => {
                *buf = *s;
                true
            }
            None => false,
        }
    }
    fn write(&mut self, sector: u64, buf: &[u8; SECTOR]) -> bool {
        match self.sectors.get_mut(sector as usize) {
            Some(s) => {
                *s = *buf;
                self.touched.push(sector);
                true
            }
            None => false,
        }
    }
    fn capacity(&mut self) -> u64 {
        self.sectors.len() as u64
    }
}

/// Содержимое, которое видно глазом в дампе, если что-то пойдёт не так.
fn bytes(tag: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| tag.wrapping_add(i as u8)).collect()
}

// ─── обещание первое: записанное переживает перезагрузку ─────────────────────

#[test]
fn объект_и_корень_переживают_перезагрузку() {
    let mut io = Ram::new(4096);
    let id = {
        let mut s = Store::new();
        let id = s.put(&bytes(1, 5000)); // заведомо больше сектора
        s.set_root("система/тест", id);
        assert!(s.commit(&mut io), "коммит не прошёл");
        id
    };

    // «Перезагрузка»: новый Store, тот же носитель.
    let mut s = Store::new();
    assert!(s.load(&mut io), "загрузка не прошла");
    assert_eq!(s.root("система/тест"), Some(id), "корень не нашёлся после загрузки");
    let вернулось = s.with(&mut io, &id, |b| b.map(Vec::from));
    assert_eq!(вернулось.as_deref(), Some(&bytes(1, 5000)[..]), "содержимое не сошлось");
}

#[test]
fn одинаковые_байты_кладутся_один_раз() {
    let mut s = Store::new();
    let a = s.put(b"void");
    let было = s.len();
    let b = s.put(b"void");
    assert_eq!(a, b, "один и тот же вход дал разные адреса");
    assert_eq!(s.len(), было, "дедупликация не сработала: объект лёг вторым");
}

// ─── обещание второе: корни ──────────────────────────────────────────────────

#[test]
fn корень_переставляется_и_удаляется() {
    let mut s = Store::new();
    let a = s.put("первое".as_bytes());
    let b = s.put("второе".as_bytes());

    s.set_root("current", a);
    assert_eq!(s.root("current"), Some(a));

    // Переустановка ячейки — это и есть «запись» в мире неизменяемых значений.
    s.set_root("current", b);
    assert_eq!(s.root("current"), Some(b));

    assert!(s.del_root("current"), "удаление существующего корня вернуло false");
    assert_eq!(s.root("current"), None);
    assert!(!s.del_root("current"), "удаление НЕсуществующего корня вернуло true");
}

// ─── обещание третье: сборка мусора считает достижимость, а не возраст ───────

#[test]
fn сборка_мусора_щадит_достижимое_и_убирает_прочее() {
    let mut io = Ram::new(4096);
    let mut s = Store::new();

    let лист = s.put("лист: на него ссылается узел".as_bytes());
    let узел = s.put_node("узел".as_bytes(), &[лист]);
    let сирота = s.put("сирота: на неё не ссылается никто".as_bytes());
    s.set_root("корень", узел);
    assert!(s.commit(&mut io));

    let (было, стало) = s.gc(&mut io);
    assert!(было > стало, "сборка не убрала ничего (было {было}, стало {стало})");

    // Достижимое живо — И САМО, И ПО ССЫЛКЕ. Второе важнее: ссылка из узла — единственное,
    // что держит лист, и именно её легко потерять при правке обхода.
    assert!(s.with(&mut io, &узел, |b| b.is_some()), "узел собран");
    assert!(s.with(&mut io, &лист, |b| b.is_some()), "ЛИСТ СОБРАН по ссылке");
    // Недостижимое — ушло.
    assert!(!s.with(&mut io, &сирота, |b| b.is_some()), "сирота пережила сборку");
}

#[test]
fn снятый_корень_перестаёт_держать_своё_дерево() {
    let mut io = Ram::new(4096);
    let mut s = Store::new();
    let лист = s.put("держится только корнем".as_bytes());
    let узел = s.put_node("узел".as_bytes(), &[лист]);
    s.set_root("временный", узел);
    assert!(s.commit(&mut io));
    s.gc(&mut io);
    assert!(s.with(&mut io, &лист, |b| b.is_some()), "лист собран при живом корне");

    s.del_root("временный");
    s.gc(&mut io);
    assert!(
        !s.with(&mut io, &лист, |b| b.is_some()),
        "лист пережил снятие последнего корня, который его держал"
    );
}

// ─── обещание четвёртое: носителем владеет store и знает его границы ─────────

/// Хвост носителя store не занимает НИКОГДА — и это не вкус, а цена одного дня разбирательств:
/// демо писало в «заведомо свободный» сектор, store дорос до него, и кадр молча затёрло.
#[test]
fn хвост_носителя_не_трогают() {
    let mut io = Ram::new(600);
    let ёмкость = io.capacity();
    let mut s = Store::new();
    // Пишем, пока принимает, — именно так и наступают на границу.
    for i in 0..400u32 {
        s.put(&bytes(i as u8, 1200));
    }
    let конец = s.put("конец".as_bytes());
    s.set_root("много", конец);
    s.commit(&mut io);

    let запретные = ёмкость - TAIL_RESERVED;
    let нарушение = io.touched.iter().copied().find(|&sec| sec >= запретные);
    assert_eq!(нарушение, None, "store написал в зарезервированный хвост носителя");
}

#[test]
fn место_для_опытов_лежит_в_хвосте_и_внутри_носителя() {
    // Маленький носитель — места для опытов нет вовсе, и это честный ответ, а не адрес наугад.
    assert_eq!(scratch_sector(4), None);
    for ёмкость in [1024u64, 65536, 1 << 20] {
        let s = scratch_sector(ёмкость).expect("на таком носителе место должно найтись");
        assert!(s < ёмкость, "адрес за пределами носителя");
        assert!(s >= ёмкость - TAIL_RESERVED, "адрес не в зарезервированном хвосте");
    }
}

#[test]
fn кончившееся_место_замечено_а_не_пропущено() {
    // Носитель нарочно мал: область объектов начинается за индексом, и уже пара кадров
    // упрётся в край.
    let mut io = Ram::new(64);
    let mut s = Store::new();
    for i in 0..40u32 {
        s.put(&bytes(i as u8, 4000));
    }
    s.commit(&mut io);
    assert!(s.out_of_space() > 0, "store не заметил, что носитель кончился");
}

// ─── обещание пятое: поколение растёт, и по нему видно историю ───────────────

#[test]
fn поколение_растёт_с_каждым_коммитом() {
    let mut io = Ram::new(4096);
    let mut s = Store::new();
    let было = s.generation();
    s.put("раз".as_bytes());
    assert!(s.commit(&mut io));
    let после_первого = s.generation();
    assert!(после_первого > было, "поколение не выросло после коммита");

    s.put("два".as_bytes());
    assert!(s.commit(&mut io));
    assert!(s.generation() > после_первого, "поколение не выросло после второго коммита");
}

#[test]
fn на_чистом_носителе_загрузка_честно_говорит_нет() {
    let mut io = Ram::new(4096);
    let mut s = Store::new();
    assert!(!s.load(&mut io), "пустой носитель принят за store");
    assert_eq!(s.len(), 0);
    assert_eq!(s.roots().count(), 0);
}
