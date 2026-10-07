//! `void-fuzz` — мутационный фаззер разборщиков ЧУЖИХ БАЙТОВ (Веха 225.1).
//!
//! ## Зачем
//!
//! Модель угроз (ADR 0023) называет противником не только программу на машине, но и **байты из
//! сети**: ответ сервера, пакет из бинарного кэша, кадр из эфира, картинка из чужих рук. Такие
//! байты разбирают `void-img` (PNG, JPEG), `void-nar` (архив пакета), `void-wpa` (кадры WPA2),
//! `void-nix` (язык), `void-fs` и `void-tree` (формат store).
//!
//! У всех у них есть тесты — и все тесты проверяют ПРАВИЛЬНЫЙ вход. Это разные вопросы: «умеет
//! ли разобрать» и «что делает с мусором». Второй и есть вопрос безопасности, потому что на VOID
//! разборщик живёт в `no_std` с `panic = "abort"`: паника в нём — не `Err`, а смерть процесса, а
//! в ядре (формат store читает оно) — смерть машины.
//!
//! ## Почему свой, а не `cargo-fuzz`
//!
//! `cargo-fuzz` (libFuzzer) требует nightly, а тулчейн проекта прибит к stable. Заводить второй
//! тулчейн ради фаззинга значит, что фаззинг не поедет в CI, а не поехавший в CI фаззер гоняют
//! ровно один раз — в тот день, когда написали.
//!
//! Цена своего мутатора — отсутствие покрытия как обратной связи: libFuzzer ведёт корпус по
//! новым путям, а этот бьёт вслепую. Для разборщиков с коротким входом это терпимо: ошибки в них
//! сидят на границах (длина, смещение, счётчик), а туда мутатор попадает и вслепую.
//!
//! ## Воспроизводимость — главное свойство
//!
//! Поток мутаций зависит ТОЛЬКО от зерна: своё ГСЧ-ядро (xorshift), никаких зависимостей,
//! никакого времени и никакого адреса. Находка печатает зерно, номер круга и сам вход шестнадцат-
//! еричным дампом — этого довольно, чтобы повторить её одной строкой и положить в обычный тест.
//!
//! ## Как пользоваться
//!
//! ```ignore
//! #[test]
//! fn png_не_паникует_на_мусоре() {
//!     void_fuzz::run("png", 2026, 20_000, &корпус(), |b| {
//!         let _ = void_img::decode(b, 64 << 20);
//!     });
//! }
//! ```

use std::panic::{self, AssertUnwindSafe};

/// Детерминированное ГСЧ-ядро (xorshift64*). Своё, потому что поток обязан совпадать между
/// машинами и версиями: чужой `rand` менял бы находку вместе с обновлением зависимости.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        // Ноль — неподвижная точка xorshift: из него поток не выходит никогда.
        Rng(seed | 1)
    }
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

/// Числа, на которых ломаются разборщики. Не случайные: это границы типов и «особые» значения
/// длин и счётчиков, вокруг которых и живут ошибки на единицу.
const INTERESTING: &[u8] = &[0x00, 0x01, 0x7f, 0x80, 0xff, 0xfe, 0x20, 0x0a];
const INTERESTING32: &[u32] = &[0, 1, 0x7fff_ffff, 0x8000_0000, 0xffff_ffff, 0xffff_fffe];

/// Потолок длины входа. Не экономия: разборщик, которому дали гигабайт, может честно работать
/// минуту, и тогда фаззер измеряет терпение, а не правильность.
const MAX_LEN: usize = 1 << 16;

/// Испортить затравку одним из приёмов. Приёмы классические и дешёвые — смысл в количестве
/// попыток, а не в изощрённости каждой.
fn mutate(rng: &mut Rng, seed: &[u8], extra: &[u8]) -> Vec<u8> {
    let mut b = seed.to_vec();
    match rng.below(9) {
        // Перевернуть бит: самая дешёвая и самая урожайная мутация.
        0 => {
            if !b.is_empty() {
                let i = rng.below(b.len());
                b[i] ^= 1 << rng.below(8);
            }
        }
        // Подставить особое значение байта.
        1 => {
            if !b.is_empty() {
                let i = rng.below(b.len());
                b[i] = INTERESTING[rng.below(INTERESTING.len())];
            }
        }
        // Подставить особое 32-битное — туда, где у формата обычно длина или счётчик.
        2 => {
            if b.len() >= 4 {
                let i = rng.below(b.len() - 3);
                let v = INTERESTING32[rng.below(INTERESTING32.len())];
                let le = rng.below(2) == 0;
                let bytes = if le { v.to_le_bytes() } else { v.to_be_bytes() };
                b[i..i + 4].copy_from_slice(&bytes);
            }
        }
        // Обрезать: половина ошибок разбора — это чтение за концом.
        3 => {
            if !b.is_empty() {
                b.truncate(rng.below(b.len()));
            }
        }
        // Удлинить мусором.
        4 => {
            let n = rng.below(64) + 1;
            for _ in 0..n {
                b.push(rng.next() as u8);
            }
        }
        // Занулить кусок.
        5 => {
            if !b.is_empty() {
                let i = rng.below(b.len());
                let n = rng.below(b.len() - i).min(64);
                for x in &mut b[i..i + n] {
                    *x = 0;
                }
            }
        }
        // Повторить кусок: ловит счётчики, которые верят входу.
        6 => {
            if !b.is_empty() {
                let i = rng.below(b.len());
                let n = rng.below(b.len() - i).min(256);
                let piece = b[i..i + n].to_vec();
                b.extend_from_slice(&piece);
            }
        }
        // Склейка с другой затравкой: даёт заголовок одного формата с телом другого.
        7 => {
            if !b.is_empty() && !extra.is_empty() {
                let cut = rng.below(b.len());
                let from = rng.below(extra.len());
                b.truncate(cut);
                b.extend_from_slice(&extra[from..]);
            }
        }
        // Совсем короткий вход: пустой, один байт, обрывок заголовка.
        _ => {
            b.truncate(rng.below(8));
        }
    }
    b.truncate(MAX_LEN);
    b
}

/// Шестнадцатеричный дамп находки — чтобы её можно было вставить в обычный тест.
fn dump(b: &[u8]) -> String {
    let head: Vec<String> = b.iter().take(256).map(|x| format!("{x:02x}")).collect();
    let more = if b.len() > 256 { format!(" … ещё {} байт", b.len() - 256) } else { String::new() };
    format!("{}{}", head.join(""), more)
}

/// Прогнать `rounds` искажённых входов через `target`.
///
/// Находка — ПАНИКА. Отказ (`Err`) находкой не является, он и есть правильный ответ: разборщик
/// обязан сказать «не разобрал», а не упасть.
///
/// Паникует сам с подробностями, пригодными для повтора: имя мишени, зерно, круг, длина и дамп.
/// Зерно и круг вместе задают вход однозначно — ГСЧ здесь детерминированное.
pub fn run<F>(name: &str, seed: u64, rounds: usize, corpus: &[Vec<u8>], mut target: F)
where
    F: FnMut(&[u8]),
{
    assert!(!corpus.is_empty(), "{name}: корпус пуст — фаззеру не от чего отталкиваться");
    let mut rng = Rng::new(seed);
    // Пока идёт обстрел, сообщения паники не печатаем: ожидаемых паник тут нет, а если случится
    // неожиданная, мы расскажем о ней сами и подробнее.
    let prev = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));
    let mut found: Option<(usize, Vec<u8>)> = None;
    // Отметки пути — ДО опасного действия, а не после.
    //
    // `catch_unwind` ловит панику и не ловит ОБРЫВ: выделение памяти «на 256 ТиБ» (первая же
    // находка этого фаззера, в `nar::walk`) кончается `abort`, то есть смертью всего прогона.
    // Без заранее оставленного следа от такой находки не остаётся ничего — ни круга, ни входа,
    // ни возможности повторить. Поэтому каждые [`CHECKPOINT`] кругов мы говорим, где идём:
    // обрыв ограничивает находку этим отрезком, а [`replay`] достаёт точный вход.
    //
    // Тот же довод, по которому `sysfuzz` печатает случай ПЕРЕД вызовом: опасное действие может
    // лишить вас возможности о нём рассказать.
    const CHECKPOINT: usize = 1000;
    for round in 0..rounds {
        if round % CHECKPOINT == 0 && round > 0 {
            eprintln!("  [fuzz] {name}: зерно {seed}, круг {round}/{rounds}");
        }
        let base = &corpus[rng.below(corpus.len())];
        let other = &corpus[rng.below(corpus.len())];
        let input = mutate(&mut rng, base, other);
        let r = panic::catch_unwind(AssertUnwindSafe(|| target(&input)));
        if r.is_err() {
            found = Some((round, input));
            break;
        }
    }
    panic::set_hook(prev);
    if let Some((round, input)) = found {
        panic!(
            "{name}: ПАНИКА на мусоре — это находка.\n\
             \u{20}  зерно: {seed}, круг: {round}, длина: {}\n\
             \u{20}  вход (hex): {}\n\
             \u{20}  повторить: void_fuzz::replay(зерно, круг) даёт тот же вход",
            input.len(),
            dump(&input)
        );
    }
}

/// Вход, который был на круге `round` при зерне `seed`, — для повтора находки.
///
/// Отдельной функцией, а не «запишите дамп»: дамп стареет вместе с корпусом, а пара
/// «зерно + круг» воспроизводит вход ровно так же, как его получил прогон.
pub fn replay(seed: u64, round: usize, corpus: &[Vec<u8>]) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    let mut last = Vec::new();
    for _ in 0..=round {
        let base = &corpus[rng.below(corpus.len())];
        let other = &corpus[rng.below(corpus.len())];
        last = mutate(&mut rng, base, other);
    }
    last
}

/// Сколько кругов гонять по умолчанию.
///
/// Разное число для отладочной и оптимизированной сборки, и это не произвол, а замер: двадцать
/// тысяч кругов по декодеру картинок занимают 333 с в отладке и 29 с в release. Поэтому
/// обычный `cargo test` разработчика идёт неглубоко и быстро (тест ВСЕГДА исполняется, а не
/// помечен `ignore` — молча пропущенный тест и есть та самая ложная уверенность), а в CI и для
/// долгого поиска гоняется `cargo test --release`.
///
/// `VOID_FUZZ_ROUNDS` перебивает оба числа — для кампании на часы.
pub fn rounds() -> usize {
    if let Ok(n) = std::env::var("VOID_FUZZ_ROUNDS").map(|s| s.parse::<usize>()) {
        if let Ok(n) = n {
            return n;
        }
    }
    if cfg!(debug_assertions) {
        500
    } else {
        20_000
    }
}

/// Прочитать корпус из каталога (все файлы, нерекурсивно). Пустых файлов не берём: мутировать
/// пустоту смысла нет, а «корпус не пуст» — проверка, что каталог вообще нашёлся.
pub fn corpus_dir(dir: &std::path::Path) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        let mut paths: Vec<_> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
        paths.sort(); // порядок обязан быть одинаков на всех машинах
        for p in paths {
            if let Ok(b) = std::fs::read(&p) {
                if !b.is_empty() {
                    out.push(b);
                }
            }
        }
    }
    out
}
