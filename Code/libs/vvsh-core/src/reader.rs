//! Reader: текст S-выражений → `Vec<Value>` (последовательность форм верхнего уровня).
//!
//! Синтаксис: списки `( … )`, строки `"…"` (escape `\n \t \" \\`), целые (`-?[0-9]+`),
//! `#t`/`#f`, символы (всё прочее), `;` — комментарий до конца строки, `'x` → `(quote x)`.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::value::Value;

/// Ошибка разбора с человекочитаемым текстом.
#[derive(Clone, Debug)]
pub struct ReadError(pub String);

/// Прочитать ВСЕ формы верхнего уровня из исходника.
pub fn read_all(src: &str) -> Result<Vec<Value>, ReadError> {
    let mut p = Parser {
        chars: src.chars().collect(),
        pos: 0,
    };
    let mut forms = Vec::new();
    loop {
        p.skip_ws();
        if p.pos >= p.chars.len() {
            break;
        }
        forms.push(p.read_form()?);
    }
    Ok(forms)
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn skip_ws(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => self.pos += 1,
                Some(';') => {
                    while let Some(c) = self.bump() {
                        if c == '\n' {
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
    }

    fn read_form(&mut self) -> Result<Value, ReadError> {
        self.skip_ws();
        match self.peek() {
            None => Err(ReadError("неожиданный конец ввода".into())),
            Some('(') => self.read_list(),
            Some(')') => Err(ReadError("лишняя ')'".into())),
            Some('\'') => {
                self.pos += 1;
                let q = self.read_form()?;
                Ok(Value::list(vec![Value::sym("quote"), q]))
            }
            Some('"') => self.read_string(),
            Some(_) => self.read_atom(),
        }
    }

    fn read_list(&mut self) -> Result<Value, ReadError> {
        self.pos += 1; // '('
        let mut items = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                None => return Err(ReadError("незакрытая '('".into())),
                Some(')') => {
                    self.pos += 1;
                    break;
                }
                _ => items.push(self.read_form()?),
            }
        }
        Ok(Value::list(items))
    }

    fn read_string(&mut self) -> Result<Value, ReadError> {
        self.pos += 1; // открывающая кавычка
        let mut s = String::new();
        loop {
            match self.bump() {
                None => return Err(ReadError("незакрытая строка".into())),
                Some('"') => break,
                Some('\\') => match self.bump() {
                    Some('n') => s.push('\n'),
                    Some('t') => s.push('\t'),
                    Some('"') => s.push('"'),
                    Some('\\') => s.push('\\'),
                    Some(other) => s.push(other),
                    None => return Err(ReadError("оборванный escape в строке".into())),
                },
                Some(c) => s.push(c),
            }
        }
        Ok(Value::str(&s))
    }

    fn read_atom(&mut self) -> Result<Value, ReadError> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_whitespace() || matches!(c, '(' | ')' | ';' | '"' | '\'') {
                break;
            }
            self.pos += 1;
        }
        let atom: String = self.chars[start..self.pos].iter().collect();
        Ok(classify_atom(&atom))
    }
}

fn classify_atom(a: &str) -> Value {
    match a {
        "#t" => return Value::Bool(true),
        "#f" => return Value::Bool(false),
        _ => {}
    }
    if let Some(n) = parse_int(a) {
        return Value::Int(n);
    }
    Value::sym(a)
}

/// Целое: `-?[0-9]+` (и `+[0-9]+`). Иначе — не число (станет символом).
fn parse_int(a: &str) -> Option<i64> {
    let bytes = a.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let (neg, digits) = match bytes[0] {
        b'-' => (true, &a[1..]),
        b'+' => (false, &a[1..]),
        _ => (false, a),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut n: i64 = 0;
    for b in digits.bytes() {
        n = n.checked_mul(10)?.checked_add((b - b'0') as i64)?;
    }
    Some(if neg { -n } else { n })
}
