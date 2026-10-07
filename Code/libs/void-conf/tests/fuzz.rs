//! Фаззинг разбора конфига поколения (Веха 225.2).
//!
//! Конфиг читает ЯДРО на подъёме системы: по нему раздаются ПРАВА. Паника здесь — машина не
//! загрузилась. И это не чисто теоретический вход: конфиг лежит в store обычным объектом, а
//! писать туда может всякий с `store:rwx` — слабость, прямо названная в ADR 0023.

fn конфиг() -> &'static str {
    "service posixfs store:rwx dma\n\
     shell wm endpoint:posixfs store:rwx mmio:fb! power env\n\
     desktop wallpaper builtin:void\n\
     # строка-замечание\n\
     ui(\"language\", \"ru\")\n\
     packages(\"hello\", \"ripgrep\")\n\
     autostart welcome\n"
}

fn корпус() -> Vec<Vec<u8>> {
    vec![
        конфиг().as_bytes().to_vec(),
        b"service a b:c:d\n".to_vec(),
        b"shell x mmio:fb! power sysview:rwg! endpoint:y:sg\n".to_vec(),
    ]
}

#[test]
fn разбор_конфига_не_паникует_на_мусоре() {
    void_fuzz::run("conf::entries", 2026, void_fuzz::rounds(), &корпус(), |b| {
        // Конфиг — текст, но в store он байты: недопустимый UTF-8 туда попадает так же легко,
        // как и всё прочее. `from_utf8_lossy` даёт ровно то, что увидит разборщик.
        let s = String::from_utf8_lossy(b);
        for e in void_conf::entries(&s) {
            core::hint::black_box(&e);
        }
        for e in void_conf::of(&s, "service") {
            core::hint::black_box(&e);
        }
        let _ = void_conf::get(&s, "ui", "language");
        let _ = void_conf::num::<usize>(&s, "session", "space");
        let _ = void_conf::on(&s, "desktop", "bar");
    });
}
