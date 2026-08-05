//! void-nar — разбор **NAR** (Nix ARchive): детерминированной сериализации дерева файлов,
//! в которой nix отдаёт содержимое пакетов.
//!
//! Веха 105 (Фаза 8) — формат вынесен из хостового моста (`tools/void-store-import`) в общий
//! `no_std`-крейт по той же причине, по которой когда-то вынесли сам store ([[store-bridge]]):
//! **одна реализация формата на всех потребителей**. Разбирать NAR теперь умеют оба конца —
//! хост, который кладёт архив в образ, и сама VOID, которой предстоит распаковывать скачанное
//! из бинарного кэша nixpkgs.
//!
//! ## Формат
//!
//! Токен = длина (u64 LE) + байты + добивка нулями до кратности 8. Грамматика узла:
//!
//! ```text
//! "(" "type" ( "regular" ["executable" ""] "contents" <данные>
//!            | "symlink" "target" <цель>
//!            | "directory" { "entry" "(" "name" <имя> "node" <узел> ")" } ) ")"
//! ```
//!
//! ## Разбор — АВТОМАТ, которому байты ПОДАЮТ (Веха 108)
//!
//! Сначала здесь был рекурсивный разбор по готовому слайсу, и оговорка стояла честная: «сам архив
//! всё ещё должен лежать в памяти целиком». Для настоящих пакетов это оказалось не оговоркой, а
//! потолком: NAR glibc — 35 МБ, куча программы — 32 МиБ, и распаковать его было нечем.
//!
//! Автомат ([`Parser`]) перевёрнут: не он читает источник, а ему **подают** байты по мере их
//! появления, и он отдаёт события. Причина именно такая, а не эстетическая — распаковщики у нас
//! разной полярности: `ruzstd` даёт `Read` (тянуть можно), а `lzma-rs` пишет в `Write` (только
//! толкает). Тянущий разбор потребовал бы обратить управление — то есть нити или корутины ради
//! разбора архива. Подающий работает с обоими даром.
//!
//! Содержимое файла отдаётся **кусками** ([`Event::FileData`]) между [`Event::FileStart`] и
//! [`Event::FileEnd`]: файл на 2 МБ нигде целиком не собирается.
//!
//! [`walk`] остался как удобная обёртка для тех, у кого архив и так в памяти (хостовый мост), —
//! но это ровно обёртка над тем же автоматом: **второй реализации формата в проекте нет**.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Ошибка разбора — человекочитаемый текст (как везде в VOID).
#[derive(Clone, Debug, PartialEq)]
pub struct NarError(pub String);

impl NarError {
    fn new(msg: impl Into<String>) -> Self {
        NarError(msg.into())
    }
}

/// Потолок короткого токена (имя, цель симлинка, ключевое слово). Настоящие — десятки байт;
/// потолок стоит затем, чтобы чужой архив не заказал нам гигабайтный `Vec` одной длиной.
const TOKEN_MAX: usize = 64 * 1024;

/// Потолок вложенности каталогов. Разбор рекурсии не использует, но строители деревьев обычно
/// держат стек, а бесконечная вложенность в чужом архиве — дешёвая пакость.
const DEPTH_MAX: usize = 64;

/// Узел архива, отдаваемый обходчику [`walk`]. `path` — путь внутри архива (пустой у корня-файла).
#[derive(Debug, PartialEq)]
pub enum Entry<'a> {
    /// Обычный файл: содержимое целиком (обход собирает его в памяти — см. [`walk`]).
    File { path: &'a str, data: &'a [u8], exec: bool },
    /// Символическая ссылка. Store их не хранит (нет такого понятия), но пропустить молча
    /// нельзя: потребитель должен знать, что в дереве было не только то, что он положил.
    Symlink { path: &'a str, target: &'a str },
    /// Каталог — приходит ДО своего содержимого (потребителю бывает нужно создать индекс).
    Dir { path: &'a str },
}

/// Событие потокового разбора. Содержимое файла приходит кусками — сколько байт подали, столько
/// и отдастся; ни размер куска, ни их число смысла не несут.
#[derive(Debug, PartialEq)]
pub enum Event<'a> {
    /// Каталог начался (до его содержимого).
    Dir { path: &'a str },
    /// Каталог кончился — все его записи уже отданы.
    DirEnd { path: &'a str },
    /// Символическая ссылка.
    Symlink { path: &'a str, target: &'a str },
    /// Файл начался: размер известен ЗАРАНЕЕ (он записан в архиве до содержимого).
    FileStart { path: &'a str, size: u64, exec: bool },
    /// Очередной кусок содержимого файла.
    FileData { data: &'a [u8] },
    /// Файл кончился.
    FileEnd,
}

/// Что автомат ждёт следующим. Стек этих ожиданий заменяет рекурсию: разбор идёт по чужому
/// архиву, и глубина дерева не должна становиться глубиной нашего стека.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Expect {
    Magic,
    /// Начало узла: `(`.
    NodeOpen,
    /// Ключ `type`.
    TypeKey,
    /// Значение типа: `regular` | `symlink` | `directory`.
    TypeVal,
    /// У обычного файла: `executable` либо сразу `contents`.
    RegAfterType,
    /// Пустой токен после `executable`.
    RegExecEmpty,
    /// Ключ `contents` (когда `executable` уже был).
    RegContents,
    /// Ключ `target` у симлинка.
    SymTargetKey,
    /// Цель симлинка.
    SymTarget,
    /// Закрывающая скобка узла.
    NodeClose,
    /// Внутри каталога: `entry` либо `)`.
    DirItem,
    /// Начало записи каталога: `(`.
    EntryOpen,
    /// Ключ `name`.
    EntryNameKey,
    /// Имя записи.
    EntryName,
    /// Ключ `node`.
    EntryNodeKey,
    /// Закрывающая скобка записи каталога.
    EntryClose,
}

/// Что сейчас собирает лексер.
enum Lex {
    /// Восемь байт длины токена.
    Len { buf: [u8; 8], have: usize },
    /// Тело короткого токена (ключевое слово, имя, цель).
    Body { need: usize, buf: Vec<u8> },
    /// Содержимое файла — наружу кусками, в памяти не копится.
    Data { left: u64 },
    /// Добивка нулями до кратности 8.
    Pad { left: usize },
}

/// Потоковый разбор NAR: автомат, которому подают байты ([`Parser::push`]) и который отдаёт
/// [`Event`]. Смысл — в шапке модуля.
pub struct Parser {
    lex: Lex,
    /// Стек ожиданий; вершина — то, что должно прийти следующим.
    expect: Vec<Expect>,
    /// Текущий путь внутри архива (`bin/hello`); пустой у корня.
    path: String,
    /// Длины `path` для отката при выходе из каталога.
    marks: Vec<usize>,
    /// Исполняемый бит текущего файла.
    exec: bool,
    /// Следующий токен — содержимое файла, а не короткое слово.
    data_next: bool,
    /// Сколько нулей добивки идёт за содержимым текущего файла. Считается от его полной длины,
    /// а её к концу чтения мы уже не помним — потому и запоминается заранее.
    pad_after_data: usize,
    /// Разбор завершён (пришла закрывающая скобка корневого узла).
    done: bool,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    pub fn new() -> Self {
        Parser {
            lex: Lex::Len { buf: [0; 8], have: 0 },
            // Ожидания кладутся так, чтобы вершина стека была ближайшим токеном.
            expect: alloc::vec![Expect::TypeVal, Expect::TypeKey, Expect::NodeOpen, Expect::Magic],
            path: String::new(),
            marks: Vec::new(),
            exec: false,
            data_next: false,
            pad_after_data: 0,
            done: false,
        }
    }

    /// Подать очередную порцию байт архива. `visit` вызывается столько раз, сколько событий
    /// в ней уместилось; его ошибка прерывает разбор — так «store не принял объект» доедет до
    /// вызывающего, не выдумывая способа достучаться наружу.
    pub fn push<F>(&mut self, mut bytes: &[u8], visit: &mut F) -> Result<(), NarError>
    where
        F: FnMut(Event<'_>) -> Result<(), NarError>,
    {
        while !bytes.is_empty() {
            match &mut self.lex {
                Lex::Len { buf, have } => {
                    // Хвост добивки последнего токена — законная часть архива, а вот НОВЫЙ токен
                    // после закрытия корневого узла означает, что нам подали что-то ещё.
                    if self.done {
                        return Err(NarError::new("NAR: байты после конца архива"));
                    }
                    let n = (8 - *have).min(bytes.len());
                    buf[*have..*have + n].copy_from_slice(&bytes[..n]);
                    *have += n;
                    bytes = &bytes[n..];
                    if *have == 8 {
                        let len = u64::from_le_bytes(*buf);
                        self.lex = if self.data_next {
                            self.data_next = false;
                            self.pad_after_data = ((8 - (len % 8)) % 8) as usize;
                            let path = self.path.clone();
                            let exec = self.exec;
                            visit(Event::FileStart { path: &path, size: len, exec })?;
                            if len == 0 {
                                visit(Event::FileEnd)?;
                                Lex::Len { buf: [0; 8], have: 0 }
                            } else {
                                Lex::Data { left: len }
                            }
                        } else {
                            if len > TOKEN_MAX as u64 {
                                return Err(NarError::new(format!(
                                    "NAR: токен длиной {} — это не наш архив",
                                    len
                                )));
                            }
                            Lex::Body { need: len as usize, buf: Vec::with_capacity(len as usize) }
                        };
                    }
                }
                Lex::Body { need, buf } => {
                    let n = (*need - buf.len()).min(bytes.len());
                    buf.extend_from_slice(&bytes[..n]);
                    bytes = &bytes[n..];
                    if buf.len() == *need {
                        let tok = core::mem::take(buf);
                        let pad = (8 - tok.len() % 8) % 8;
                        self.lex = Lex::Pad { left: pad };
                        let tok = String::from_utf8(tok)
                            .map_err(|_| NarError::new("NAR: токен не UTF-8"))?;
                        self.token(&tok, visit)?;
                    }
                }
                Lex::Data { left } => {
                    let n = (*left).min(bytes.len() as u64) as usize;
                    visit(Event::FileData { data: &bytes[..n] })?;
                    bytes = &bytes[n..];
                    *left -= n as u64;
                    if *left == 0 {
                        visit(Event::FileEnd)?;
                        // Добивка считается от полной длины содержимого — её мы уже не помним,
                        // поэтому храним остаток добивки прямо в состоянии перехода ниже.
                        self.lex = Lex::Pad { left: self.pad_after_data };
                    }
                }
                Lex::Pad { left } => {
                    let n = (*left).min(bytes.len());
                    bytes = &bytes[n..];
                    *left -= n;
                    if *left == 0 {
                        self.lex = Lex::Len { buf: [0; 8], have: 0 };
                    }
                }
            }
        }
        Ok(())
    }

    /// Разбор кончился ровно на границе архива? Не «кончились байты», а «дерево закрыто».
    pub fn finish(&self) -> Result<(), NarError> {
        if self.done && self.expect.is_empty() {
            Ok(())
        } else {
            Err(NarError::new("NAR обрезан (архив кончился посреди узла)"))
        }
    }

    /// Очередной короткий токен пришёл целиком — двигаем грамматику.
    fn token<F>(&mut self, tok: &str, visit: &mut F) -> Result<(), NarError>
    where
        F: FnMut(Event<'_>) -> Result<(), NarError>,
    {
        let Some(exp) = self.expect.pop() else {
            return Err(NarError::new("NAR: лишний токен после конца архива"));
        };
        match exp {
            Expect::Magic => want(tok, "nix-archive-1")?,
            Expect::NodeOpen | Expect::EntryOpen => want(tok, "(")?,
            Expect::TypeKey => want(tok, "type")?,
            Expect::EntryNameKey => want(tok, "name")?,
            Expect::EntryNodeKey => want(tok, "node")?,
            Expect::RegExecEmpty => want(tok, "")?,
            Expect::RegContents => {
                want(tok, "contents")?;
                self.data_next = true;
                self.expect.push(Expect::NodeClose);
            }
            Expect::SymTargetKey => want(tok, "target")?,
            Expect::NodeClose | Expect::EntryClose => want(tok, ")")?,
            Expect::TypeVal => match tok {
                "regular" => {
                    self.exec = false;
                    self.expect.push(Expect::RegAfterType);
                }
                "symlink" => {
                    self.expect.push(Expect::NodeClose);
                    self.expect.push(Expect::SymTarget);
                    self.expect.push(Expect::SymTargetKey);
                }
                "directory" => {
                    if self.marks.len() >= DEPTH_MAX {
                        return Err(NarError::new("NAR: каталоги вложены слишком глубоко"));
                    }
                    let path = self.path.clone();
                    visit(Event::Dir { path: &path })?;
                    self.marks.push(self.path.len());
                    self.expect.push(Expect::DirItem);
                }
                t => return Err(NarError::new(format!("NAR: неизвестный тип узла «{}»", t))),
            },
            Expect::RegAfterType => match tok {
                "executable" => {
                    self.exec = true;
                    self.expect.push(Expect::RegContents);
                    self.expect.push(Expect::RegExecEmpty);
                }
                "contents" => {
                    self.data_next = true;
                    self.expect.push(Expect::NodeClose);
                }
                t => {
                    return Err(NarError::new(format!(
                        "NAR: ожидал «contents», увидел «{}»",
                        t
                    )))
                }
            },
            Expect::SymTarget => {
                let path = self.path.clone();
                visit(Event::Symlink { path: &path, target: tok })?;
            }
            Expect::DirItem => match tok {
                "entry" => {
                    // Запись каталога: `( name <имя> node <узел> )`, после неё снова DirItem.
                    self.expect.push(Expect::DirItem);
                    self.expect.push(Expect::EntryClose);
                    self.expect.push(Expect::TypeVal);
                    self.expect.push(Expect::TypeKey);
                    self.expect.push(Expect::NodeOpen);
                    self.expect.push(Expect::EntryNodeKey);
                    self.expect.push(Expect::EntryName);
                    self.expect.push(Expect::EntryNameKey);
                    self.expect.push(Expect::EntryOpen);
                }
                ")" => {
                    let path = self.path.clone();
                    visit(Event::DirEnd { path: &path })?;
                    let mark = self.marks.pop().unwrap_or(0);
                    self.path.truncate(mark);
                }
                t => {
                    return Err(NarError::new(format!(
                        "NAR: неожиданный токен «{}» в каталоге",
                        t
                    )))
                }
            },
            Expect::EntryName => {
                if tok.is_empty() || tok.contains('/') || tok == "." || tok == ".." {
                    return Err(NarError::new(format!("NAR: недопустимое имя «{}»", tok)));
                }
                if !self.path.is_empty() {
                    self.path.push('/');
                }
                self.path.push_str(tok);
            }
        }

        // Имя записи живёт до конца её узла; откатываем путь, когда запись закрылась.
        if exp == Expect::EntryClose {
            let keep = self.marks.last().copied().unwrap_or(0);
            self.path.truncate(keep);
        }
        if self.expect.is_empty() {
            self.done = true;
        }
        Ok(())
    }
}

fn want(got: &str, expected: &str) -> Result<(), NarError> {
    if got == expected {
        Ok(())
    } else {
        Err(NarError::new(format!("NAR: ожидал «{}», увидел «{}»", expected, got)))
    }
}

/// Обойти архив, лежащий в памяти целиком, отдавая узлы `visit`.
///
/// Обёртка над [`Parser`] для тех, у кого архив и так в памяти (хостовый мост, `nar-unpack` из
/// vvsh): содержимое файла здесь собирается в буфер, чтобы отдаться одним ломтём. На VOID для
/// пакетов это НЕ годится (файл может быть в мегабайты) — там подают [`Parser`] напрямую.
pub fn walk<F>(nar: &[u8], mut visit: F) -> Result<(), NarError>
where
    F: FnMut(Entry<'_>) -> Result<(), NarError>,
{
    let mut p = Parser::new();
    let mut path = String::new();
    let mut data: Vec<u8> = Vec::new();
    let mut exec = false;
    p.push(nar, &mut |e| {
        match e {
            Event::Dir { path } => visit(Entry::Dir { path })?,
            Event::Symlink { path, target } => visit(Entry::Symlink { path, target })?,
            Event::FileStart { path: pth, size, exec: x } => {
                path = pth.to_string();
                exec = x;
                data.clear();
                data.reserve(size as usize);
            }
            Event::FileData { data: d } => data.extend_from_slice(d),
            Event::FileEnd => visit(Entry::File { path: &path, data: &data, exec })?,
            Event::DirEnd { .. } => {}
        }
        Ok(())
    })?;
    p.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Собрать токен NAR: длина + байты + добивка до 8.
    fn tok(out: &mut Vec<u8>, s: &[u8]) {
        out.extend_from_slice(&(s.len() as u64).to_le_bytes());
        out.extend_from_slice(s);
        out.extend(core::iter::repeat(0).take((8 - s.len() % 8) % 8));
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

    /// Дерево из демонстрации: `bin/hello` (исполняемый) и симлинк `link` на него.
    fn tree() -> Vec<u8> {
        let mut nar = Vec::new();
        tok(&mut nar, b"nix-archive-1");
        tok(&mut nar, b"(");
        tok(&mut nar, b"type");
        tok(&mut nar, b"directory");
        tok(&mut nar, b"entry");
        tok(&mut nar, b"(");
        tok(&mut nar, b"name");
        tok(&mut nar, b"bin");
        tok(&mut nar, b"node");
        tok(&mut nar, b"(");
        tok(&mut nar, b"type");
        tok(&mut nar, b"directory");
        tok(&mut nar, b"entry");
        tok(&mut nar, b"(");
        tok(&mut nar, b"name");
        tok(&mut nar, b"hello");
        tok(&mut nar, b"node");
        file(&mut nar, b"ELF", true);
        tok(&mut nar, b")");
        tok(&mut nar, b")");
        tok(&mut nar, b")");
        tok(&mut nar, b"entry");
        tok(&mut nar, b"(");
        tok(&mut nar, b"name");
        tok(&mut nar, b"link");
        tok(&mut nar, b"node");
        tok(&mut nar, b"(");
        tok(&mut nar, b"type");
        tok(&mut nar, b"symlink");
        tok(&mut nar, b"target");
        tok(&mut nar, b"bin/hello");
        tok(&mut nar, b")");
        tok(&mut nar, b")");
        tok(&mut nar, b")");
        nar
    }

    #[test]
    fn single_file() {
        let mut nar = Vec::new();
        tok(&mut nar, b"nix-archive-1");
        file(&mut nar, b"hello", false);
        let mut seen = Vec::new();
        walk(&nar, |e| {
            if let Entry::File { path, data, exec } = e {
                seen.push((path.to_string(), data.to_vec(), exec));
            }
            Ok(())
        })
        .expect("разбор");
        assert_eq!(seen, [(String::new(), b"hello".to_vec(), false)]);
    }

    #[test]
    fn directory_tree_with_exec_and_symlink() {
        let nar = tree();
        let mut files = Vec::new();
        let mut dirs = Vec::new();
        let mut links = Vec::new();
        walk(&nar, |e| {
            match e {
                Entry::File { path, data, exec } => {
                    files.push((path.to_string(), data.to_vec(), exec))
                }
                Entry::Dir { path } => dirs.push(path.to_string()),
                Entry::Symlink { path, target } => {
                    links.push((path.to_string(), target.to_string()))
                }
            }
            Ok(())
        })
        .expect("разбор");
        assert_eq!(files, [("bin/hello".to_string(), b"ELF".to_vec(), true)]);
        assert_eq!(dirs, [String::new(), "bin".to_string()]);
        assert_eq!(links, [("link".to_string(), "bin/hello".to_string())]);
    }

    #[test]
    fn truncated_is_error_not_panic() {
        let mut nar = Vec::new();
        tok(&mut nar, b"nix-archive-1");
        file(&mut nar, b"hello", false);
        nar.truncate(nar.len() - 9); // отрезать хвост последнего токена
        let err = walk(&nar, |_| Ok(())).unwrap_err();
        assert!(err.0.contains("обрезан"), "получили: {}", err.0);
    }

    #[test]
    fn not_a_nar_is_error() {
        let err = walk("мусор".as_bytes(), |_| Ok(())).unwrap_err();
        assert!(err.0.contains("NAR"), "получили: {}", err.0);
    }

    /// Ошибка ПОТРЕБИТЕЛЯ прерывает обход: так «store не принял объект» доедет до вызывающего.
    #[test]
    fn visitor_error_stops_walk() {
        let mut nar = Vec::new();
        tok(&mut nar, b"nix-archive-1");
        file(&mut nar, b"hello", false);
        let err = walk(&nar, |_| Err(NarError::new("store не принял"))).unwrap_err();
        assert_eq!(err.0, "store не принял");
    }

    /// Главное свойство потокового разбора: результат НЕ ЗАВИСИТ от того, какими порциями подали
    /// байты. Проверяем на всех размерах порции от 1 до 17 — границы токенов, длин и добивки
    /// попадают внутрь порции во всех сочетаниях.
    #[test]
    fn chunking_does_not_change_events() {
        let nar = tree();
        let reference = events_of(&nar, nar.len());
        for step in 1..=17 {
            assert_eq!(events_of(&nar, step), reference, "порция {} байт", step);
        }
    }

    /// Содержимое файла склеивается из кусков, а не приходит одним ломтём.
    #[test]
    fn file_data_arrives_in_pieces() {
        let mut nar = Vec::new();
        tok(&mut nar, b"nix-archive-1");
        let body: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
        file(&mut nar, &body, false);

        let mut got: Vec<u8> = Vec::new();
        let mut pieces = 0;
        let mut p = Parser::new();
        for chunk in nar.chunks(7) {
            p.push(chunk, &mut |e| {
                if let Event::FileData { data } = e {
                    got.extend_from_slice(data);
                    pieces += 1;
                }
                Ok(())
            })
            .expect("разбор");
        }
        p.finish().expect("конец архива");
        assert_eq!(got, body);
        assert!(pieces > 100, "кусков должно быть много, было {}", pieces);
    }

    /// Пустой файл — граничный случай: содержимое нулевой длины, `FileEnd` без единого куска.
    #[test]
    fn empty_file() {
        let mut nar = Vec::new();
        tok(&mut nar, b"nix-archive-1");
        file(&mut nar, b"", false);
        let mut seen = Vec::new();
        walk(&nar, |e| {
            if let Entry::File { path, data, .. } = e {
                seen.push((path.to_string(), data.len()));
            }
            Ok(())
        })
        .expect("разбор");
        assert_eq!(seen, [(String::new(), 0)]);
    }

    /// Каталог закрывается событием — на нём строитель дерева закрывает свой узел.
    #[test]
    fn dir_end_matches_dir() {
        let nar = tree();
        let mut opened = Vec::new();
        let mut closed = Vec::new();
        let mut p = Parser::new();
        p.push(&nar, &mut |e| {
            match e {
                Event::Dir { path } => opened.push(path.to_string()),
                Event::DirEnd { path } => closed.push(path.to_string()),
                _ => {}
            }
            Ok(())
        })
        .expect("разбор");
        p.finish().expect("конец архива");
        assert_eq!(opened, [String::new(), "bin".to_string()]);
        assert_eq!(closed, ["bin".to_string(), String::new()]);
    }

    /// Чужой архив не должен уметь заказать нам гигабайтный буфер одной длиной.
    #[test]
    fn absurd_token_length_rejected() {
        let mut nar = Vec::new();
        tok(&mut nar, b"nix-archive-1");
        nar.extend_from_slice(&(1u64 << 40).to_le_bytes());
        let mut p = Parser::new();
        let err = p.push(&nar, &mut |_| Ok(())).unwrap_err();
        assert!(err.0.contains("не наш архив"), "получили: {}", err.0);
    }

    /// Имя записи каталога с `/` — путь наружу дерева; такое отвергаем.
    #[test]
    fn entry_name_with_slash_rejected() {
        let mut nar = Vec::new();
        tok(&mut nar, b"nix-archive-1");
        tok(&mut nar, b"(");
        tok(&mut nar, b"type");
        tok(&mut nar, b"directory");
        tok(&mut nar, b"entry");
        tok(&mut nar, b"(");
        tok(&mut nar, b"name");
        tok(&mut nar, "../беда".as_bytes());
        let mut p = Parser::new();
        let err = p.push(&nar, &mut |_| Ok(())).unwrap_err();
        assert!(err.0.contains("недопустимое имя"), "получили: {}", err.0);
    }

    /// Собрать список событий, подавая архив порциями по `step` байт.
    fn events_of(nar: &[u8], step: usize) -> Vec<String> {
        let mut out = Vec::new();
        let mut p = Parser::new();
        for chunk in nar.chunks(step) {
            p.push(chunk, &mut |e| {
                match e {
                    Event::Dir { path } => out.push(format!("dir {}", path)),
                    Event::DirEnd { path } => out.push(format!("dirend {}", path)),
                    Event::Symlink { path, target } => {
                        out.push(format!("link {} -> {}", path, target))
                    }
                    Event::FileStart { path, size, exec } => {
                        out.push(format!("file {} {} {}", path, size, exec))
                    }
                    Event::FileData { data } => out.push(format!("data {}", data.len())),
                    Event::FileEnd => out.push(String::from("fileend")),
                }
                Ok(())
            })
            .expect("разбор");
        }
        p.finish().expect("конец архива");
        // Куски содержимого склеиваем: их дробление зависит от порции и сравнению не подлежит.
        let mut merged: Vec<String> = Vec::new();
        let mut acc = 0usize;
        for e in out {
            if let Some(n) = e.strip_prefix("data ") {
                acc += n.parse::<usize>().unwrap();
                continue;
            }
            if acc > 0 {
                merged.push(format!("data {}", acc));
                acc = 0;
            }
            merged.push(e);
        }
        merged
    }
}
