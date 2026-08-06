//! Чередование записи и чтения на носителе, с которого объекты УЖЕ выгружены из RAM.
//!
//! Веха 111. Ровно эта смесь ломалась на устройстве: `pkg update` пишет в store куски
//! распакованного индекса и одновременно читает куски скачанного архива, которые давно
//! легли на диск и были отпущены из памяти. Store отвечал «кадр с диска не сошёлся со своим
//! content-id» — то есть терял связь «объект → место на диске».
//!
//! Тест ставит вопрос ребром: виновата логика store или носитель? Носитель здесь ЗАВЕДОМО
//! честный (обычная память), поэтому расхождение хэша тут может означать только одно — что
//! store сам записал или запомнил не то.

use void_store::{BlockIo, Store, SECTOR};

/// Идеальный носитель: что записали, то и прочли. Ошибок не выдумывает.
struct Ram {
    sectors: Vec<[u8; SECTOR]>,
}

impl Ram {
    fn new(sectors: usize) -> Self {
        Ram { sectors: vec![[0u8; SECTOR]; sectors] }
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
                true
            }
            None => false,
        }
    }
    fn capacity(&mut self) -> u64 {
        self.sectors.len() as u64
    }
}

/// Кусок с предсказуемым содержимым — чтобы сверять не только хэш, но и байты.
fn chunk(n: usize, len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    let mut x = n as u64 * 2_654_435_761 + 1;
    while v.len() < len {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.truncate(len);
    v
}

/// Сперва «скачивание»: много кусков подряд с коммитами, как их кладёт `httpsc`.
/// Потом «распаковка»: читаем те куски по порядку и на каждый прочитанный пишем новый —
/// именно так `pkg update` и работает.
#[test]
fn read_old_chunks_while_writing_new() {
    const CHUNK: usize = 16 * 1024;
    const N: usize = 200;

    let mut io = Ram::new(200_000); // ~97 МиБ — с запасом
    let mut s = Store::new();

    // Фаза 1 — «загрузка»: куски + корень, коммит каждые 16 штук (у ядра group commit похож).
    let mut ids = Vec::new();
    for i in 0..N {
        ids.push(s.put(&chunk(i, CHUNK)));
        if i % 16 == 15 {
            assert!(s.commit(&mut io), "коммит фазы загрузки {}", i);
        }
    }
    let blob = s.put_node(b"blob", &ids);
    s.set_root("dl", blob);
    assert!(s.commit(&mut io), "коммит корня");

    // Фаза 2 — «распаковка»: читаем старое, пишем новое, и так по кругу.
    let mut out = Vec::new();
    for i in 0..N {
        let got = s.with(&mut io, &ids[i], |p| p.map(Vec::from)).expect("кусок загрузки читается");
        assert_eq!(got, chunk(i, CHUNK), "кусок {} прочитался не тем", i);
        out.push(s.put(&chunk(1000 + i, CHUNK)));
        if i % 16 == 15 {
            assert!(s.commit(&mut io), "коммит фазы распаковки {}", i);
        }
    }
    let idx = s.put_node(b"index", &out);
    s.set_root("index", idx);
    assert!(s.commit(&mut io), "коммит индекса");

    assert_eq!(s.corrupt_reads(), 0, "store не сошёлся сам с собой на честном носителе");

    // И всё это должно пережить перезагрузку.
    let mut s2 = Store::new();
    assert!(s2.load(&mut io), "store грузится");
    for i in 0..N {
        assert_eq!(s2.with(&mut io, &ids[i], |p| p.map(Vec::from)).as_deref(), Some(chunk(i, CHUNK).as_slice()));
        assert_eq!(s2.with(&mut io, &out[i], |p| p.map(Vec::from)).as_deref(), Some(chunk(1000 + i, CHUNK).as_slice()));
    }
    assert_eq!(s2.corrupt_reads(), 0, "после перезагрузки тоже");
}

/// Чтение большого блоба насквозь НЕ должно тянуть его целиком в кэш.
///
/// Веха 111: индекс канала (941 кусок по 16 КиБ) читался `pkg search`-ем от начала до конца, и
/// каждый кусок оставался в кэше store — 15 МиБ в куче ядра, после чего система падала на
/// попытке запустить следующую программу. Кэш обязан иметь потолок; проверяем именно его.
#[test]
fn full_scan_of_big_blob_stays_within_cache_budget() {
    const CHUNK: usize = 16 * 1024;
    const N: usize = 941; // столько кусков у настоящего индекса канала

    let mut io = Ram::new(120_000);
    let mut s = Store::new();

    let ids: Vec<_> = (0..N).map(|i| s.put(&chunk(i, CHUNK))).collect();
    let blob = s.put_node(b"index", &ids);
    s.set_root("index", blob);
    assert!(s.commit(&mut io), "коммит блоба");
    assert_eq!(s.cached_bytes(), 0, "после коммита содержимое отпущено");

    let mut peak = 0usize;
    for (i, id) in ids.iter().enumerate() {
        let n = s.with(&mut io, id, |p| p.map(|b| b.len())).expect("кусок читается");
        assert_eq!(n, CHUNK, "кусок {} прочитался не целиком", i);
        peak = peak.max(s.cached_bytes());
    }
    assert_eq!(s.corrupt_reads(), 0);
    assert!(
        peak <= 4 * 1024 * 1024 + CHUNK,
        "кэш вырос до {} Б при проходе по {} МиБ — потолок не держит",
        peak,
        N * CHUNK / (1024 * 1024),
    );
}
