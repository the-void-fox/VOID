//! Фаззинг разбора каталогов посикс-персоналии (Веха 225.2).
//!
//! **Этот формат читает ЯДРО** (личность Linux живёт в нём), а не только программа. Паника здесь
//! — не отказ и не смерть процесса, а смерть МАШИНЫ: в ядре `panic = "abort"`, и падать ему
//! некуда. Поэтому мишень важнее тех, что разбирают картинки.
//!
//! Отдельная тонкость формата, ради которой фаззинг и нужен: почти все функции берут буфер И
//! ДЛИНУ РАЗНЫМИ аргументами (`dir: &[u8], len: usize`). Длина приходит не из буфера, а со
//! стороны — из метаданных узла store, то есть тоже из данных. Пара «короткий буфер + большая
//! длина» и есть тот случай, который руками не пишут, а фаззер подставляет сразу.

/// Правильный каталог: счётчик записей, затем записи `(тип, длина имени, имя)`.
fn dir() -> Vec<u8> {
    let mut d = vec![0u8; 512];
    let mut len = 2;
    for (name, is_dir) in [(&b"bin"[..], true), (b"hello", false), (b"\xd0\xb8\xd0\xbc\xd1\x8f", false)] {
        len = void_fs::idx_add(&mut d, len, name, is_dir);
    }
    d.truncate(len);
    d
}

fn corpus() -> Vec<Vec<u8>> {
    vec![dir(), vec![0u8, 0], vec![1u8, 0, 0, 3, b'a', b'b', b'c']]
}

#[test]
fn разбор_каталога_не_паникует_на_мусоре() {
    void_fuzz::run("fs::каталог", 2026, void_fuzz::rounds(), &corpus(), |b| {
        // Длина — В ПРЕДЕЛАХ ДОГОВОРА (`len <= dir.len()`), и это не поблажка разборщику.
        // Первый заход бил длиной `usize::MAX`, получил три «находки» — и все три оказались
        // негодным опытом: ВСЕ вызывающие длину ограничивают буфером (`if n > t.len() { 0 }`).
        // Проверять надо то, что бывает, иначе отчёт полон того, чего не бывает.
        let len = if b.is_empty() { 0 } else { (b[0] as usize * 7).min(b.len()) };
        for l in [b.len(), len, len / 2] {
            let _ = void_fs::entries(b, l).count();
            let _ = void_fs::idx_type(b, l, b"hello");
            let _ = void_fs::idx_empty(b, l);
        }
    });
}

#[test]
fn правка_каталога_не_паникует_на_мусоре() {
    void_fuzz::run("fs::правка", 13, void_fuzz::rounds(), &corpus(), |b| {
        let mut copy = b.to_vec();
        if copy.is_empty() {
            return;
        }
        let len = (copy[0] as usize * 7).min(copy.len());
        let _ = void_fs::idx_add(&mut copy, len, "новое".as_bytes(), false);
        let _ = void_fs::idx_remove(&mut copy, len, b"hello");
    });
}

#[test]
fn пути_не_паникуют_на_мусоре() {
    void_fuzz::run("fs::пути", 29, void_fuzz::rounds(), &corpus(), |b| {
        // Пути приходят байтами, а не строкой: имя файла на VOID — это байты, и разборщик
        // обязан пережить любые, включая недопустимый UTF-8 и ведущие слэши пачками.
        let mut buf = [0u8; void_fs::PATH_MAX];
        let _ = void_fs::normalize(b, &mut buf);
        let _ = void_fs::parent(b);
        let _ = void_fs::leaf(b);
        let mut root = [0u8; void_fs::ROOT_MAX];
        for kind in [0u8, 1, 2, 255] {
            let _ = void_fs::root_name(kind, b, &mut root);
        }
    });
}

#[test]
fn времена_не_паникуют_на_мусоре() {
    void_fuzz::run("fs::времена", 31, void_fuzz::rounds(), &corpus(), |b| {
        let len = if b.is_empty() { 0 } else { (b[0] as usize * 7).min(b.len()) };
        for l in [b.len(), len, len / 2] {
            let _ = void_fs::mt_find(b, l, b"hello");
        }
    });
}
