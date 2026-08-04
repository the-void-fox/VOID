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
//! ## Почему обход, а не список файлов
//!
//! Хостовая версия собирала `Vec<NarFile>` со всем содержимым сразу — на хосте это безобидно.
//! На VOID так нельзя: замыкание настоящего пакета — десятки мегабайт, и второй копии в куче
//! программы просто нет места. Поэтому здесь **обход с обратным вызовом**: каждый файл отдаётся
//! ломтём ИСХОДНОГО буфера, и потребитель сразу кладёт его в store, ничего не накапливая.
//!
//! Оговорка честная: сам архив всё ещё должен лежать в памяти целиком. Настоящая потоковая
//! распаковка (кусками, без полного буфера) — следующий шаг, и она потребует другого источника
//! входа, а не другой грамматики.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};

/// Ошибка разбора — человекочитаемый текст (как везде в VOID).
#[derive(Clone, Debug, PartialEq)]
pub struct NarError(pub String);

impl NarError {
    fn new(msg: impl Into<String>) -> Self {
        NarError(msg.into())
    }
}

/// Узел архива, отдаваемый обходчику. `path` — путь внутри архива (пустой у корня-файла).
#[derive(Debug, PartialEq)]
pub enum Entry<'a> {
    /// Обычный файл: содержимое — ломоть исходного буфера, копий не делаем.
    File { path: &'a str, data: &'a [u8], exec: bool },
    /// Символическая ссылка. Store их не хранит (нет такого понятия), но пропустить молча
    /// нельзя: потребитель должен знать, что в дереве было не только то, что он положил.
    Symlink { path: &'a str, target: &'a str },
    /// Каталог — приходит ДО своего содержимого (потребителю бывает нужно создать индекс).
    Dir { path: &'a str },
}

struct Reader<'a> {
    b: &'a [u8],
    off: usize,
}

impl<'a> Reader<'a> {
    fn bytes(&mut self) -> Result<&'a [u8], NarError> {
        if self.off + 8 > self.b.len() {
            return Err(NarError::new("NAR обрезан (нет длины токена)"));
        }
        let mut len8 = [0u8; 8];
        len8.copy_from_slice(&self.b[self.off..self.off + 8]);
        let len = u64::from_le_bytes(len8) as usize;
        self.off += 8;
        if self.off + len > self.b.len() {
            return Err(NarError::new("NAR обрезан (нет тела токена)"));
        }
        let s = &self.b[self.off..self.off + len];
        self.off += len + (8 - len % 8) % 8;
        Ok(s)
    }

    fn tok(&mut self) -> Result<&'a str, NarError> {
        core::str::from_utf8(self.bytes()?).map_err(|_| NarError::new("NAR: токен не UTF-8"))
    }

    fn expect(&mut self, want: &str) -> Result<(), NarError> {
        let got = self.tok()?;
        if got == want {
            Ok(())
        } else {
            Err(NarError::new(format!("NAR: ожидал «{}», увидел «{}»", want, got)))
        }
    }
}

/// Обойти архив, отдавая узлы `visit`. Возврат `Err` из обхода прерывает разбор — так
/// потребитель сообщает о своей беде (например «store не принял объект»), не выдумывая
/// способа достучаться наружу.
pub fn walk<F>(nar: &[u8], mut visit: F) -> Result<(), NarError>
where
    F: FnMut(Entry<'_>) -> Result<(), NarError>,
{
    let mut r = Reader { b: nar, off: 0 };
    r.expect("nix-archive-1")?;
    node(&mut r, String::new(), &mut visit)
}

fn node<F>(r: &mut Reader<'_>, path: String, visit: &mut F) -> Result<(), NarError>
where
    F: FnMut(Entry<'_>) -> Result<(), NarError>,
{
    r.expect("(")?;
    r.expect("type")?;
    match r.tok()? {
        "regular" => {
            let mut exec = false;
            let mut t = r.tok()?;
            if t == "executable" {
                r.expect("")?;
                exec = true;
                t = r.tok()?;
            }
            if t != "contents" {
                return Err(NarError::new(format!("NAR: ожидал «contents», увидел «{}»", t)));
            }
            let data = r.bytes()?;
            r.expect(")")?;
            visit(Entry::File { path: &path, data, exec })?;
        }
        "symlink" => {
            r.expect("target")?;
            let target = r.tok()?;
            r.expect(")")?;
            visit(Entry::Symlink { path: &path, target })?;
        }
        "directory" => {
            visit(Entry::Dir { path: &path })?;
            loop {
                match r.tok()? {
                    ")" => break,
                    "entry" => {
                        r.expect("(")?;
                        r.expect("name")?;
                        let name = r.tok()?.to_string();
                        r.expect("node")?;
                        let sub = if path.is_empty() {
                            name
                        } else {
                            format!("{}/{}", path, name)
                        };
                        node(r, sub, visit)?;
                        r.expect(")")?;
                    }
                    t => {
                        return Err(NarError::new(format!(
                            "NAR: неожиданный токен «{}» в каталоге",
                            t
                        )))
                    }
                }
            }
        }
        t => return Err(NarError::new(format!("NAR: неизвестный тип узла «{}»", t))),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

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
        let mut nar = Vec::new();
        tok(&mut nar, b"nix-archive-1");
        tok(&mut nar, b"(");
        tok(&mut nar, b"type");
        tok(&mut nar, b"directory");
        // entry bin/ → каталог с исполняемым файлом
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
        // entry link → симлинк
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
}
