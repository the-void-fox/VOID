//! Фаззинг разбора дерева объектов store (Веха 225.2).
//!
//! Формат читает ЯДРО: по нему оно ходит, разрешая корни и запуская программы. Паника здесь —
//! смерть машины. Сам узел приходит из store, а писать в store может всякий с правом `store:rwx`
//! (слабость, прямо записанная в ADR 0023), — то есть данные тут не «свои по определению».

fn узел() -> Vec<u8> {
    let mut v = void_tree::head(3).to_vec();
    void_tree::push(&mut v, void_tree::K_DIR, b"bin", 0);
    void_tree::push(&mut v, void_tree::K_FILE | void_tree::F_BLOB, b"hello", 4096);
    void_tree::push(&mut v, void_tree::K_LINK, "имя".as_bytes(), 7);
    v
}

fn корпус() -> Vec<Vec<u8>> {
    vec![узел(), void_tree::head(0).to_vec(), void_tree::head(0xffff_ffff).to_vec()]
}

#[test]
fn обход_дерева_не_паникует_на_мусоре() {
    void_fuzz::run("tree::iter", 2026, void_fuzz::rounds(), &корпус(), |b| {
        if let Some(it) = void_tree::iter(b) {
            // Счётчик записей лежит В САМОМ узле: обход обязан кончиться и тогда, когда тот
            // врёт. Берём все записи, а не `count()`, — иначе длины имён никто не прочтёт.
            for e in it {
                core::hint::black_box(e);
            }
        }
        let _ = void_tree::count(b);
        let _ = void_tree::find(b, b"hello");
    });
}
