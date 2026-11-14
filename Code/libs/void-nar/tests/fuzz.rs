//! Фаззинг разбора NAR (Веха 225.1): что обход архива делает с МУСОРОМ.
//!
//! NAR — это байты ИЗ СЕТИ в самом прямом смысле: архив пакета, скачанный из бинарного кэша.
//! Модель угроз (ADR 0023) называет такие байты противником наравне с программой на машине, а
//! подпись проверяется ДО распаковки — значит разбор обязан пережить и то, что подписи не имело.
//!
//! Находка — только ПАНИКА. `NarError` на мусоре и есть правильный ответ.

/// Токен NAR: длина (8 байт LE) + байты + выравнивание до восьми. Тот же кирпич, которым
/// складывают архив штатные тесты крейта.
fn tok(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u64).to_le_bytes());
    out.extend_from_slice(s);
    let pad = (8 - s.len() % 8) % 8;
    out.extend(std::iter::repeat(0).take(pad));
}

fn file(out: &mut Vec<u8>, data: &[u8], exec: bool) {
    tok(out, b"(");
    tok(out, b"type");
    tok(out, b"regular");
    if exec {
        tok(out, b"executable");
        tok(out, b"");
    }
    tok(out, b"contents");
    tok(out, data);
    tok(out, b")");
}

/// Корпус: ПРАВИЛЬНЫЕ архивы как отправная точка. Из случайного шума мутатор дальше проверки
/// заголовка `nix-archive-1` не уедет, и фаззинг выродился бы в проверку одной строки.
fn corpus() -> Vec<Vec<u8>> {
    let mut one = Vec::new();
    tok(&mut one, b"nix-archive-1");
    file(&mut one, b"hello", false);

    // Дерево: каталог, исполняемый файл и симлинк — все три рода узлов сразу.
    let mut tree = Vec::new();
    tok(&mut tree, b"nix-archive-1");
    for t in [&b"("[..], b"type", b"directory", b"entry", b"(", b"name", b"bin", b"node", b"("] {
        tok(&mut tree, t);
    }
    tok(&mut tree, b"type");
    tok(&mut tree, b"directory");
    tok(&mut tree, b"entry");
    tok(&mut tree, b"(");
    tok(&mut tree, b"name");
    tok(&mut tree, b"hello");
    tok(&mut tree, b"node");
    file(&mut tree, b"ELF", true);
    for _ in 0..3 {
        tok(&mut tree, b")");
    }
    tok(&mut tree, b"entry");
    tok(&mut tree, b"(");
    tok(&mut tree, b"name");
    tok(&mut tree, b"link");
    tok(&mut tree, b"node");
    tok(&mut tree, b"(");
    tok(&mut tree, b"type");
    tok(&mut tree, b"symlink");
    tok(&mut tree, b"target");
    tok(&mut tree, b"bin/hello");
    for _ in 0..3 {
        tok(&mut tree, b")");
    }

    // Файл подлиннее: мутации длины содержимого — отдельный интересный случай.
    let mut long = Vec::new();
    tok(&mut long, b"nix-archive-1");
    file(&mut long, &vec![0x41u8; 1000], false);

    vec![one, tree, long]
}

#[test]
fn обход_не_паникует_на_мусоре() {
    void_fuzz::run("nar::walk", 2026, void_fuzz::rounds(), &corpus(), |b| {
        let _ = void_nar::walk(b, |_| Ok(()));
    });
}
