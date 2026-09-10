//! Лексер языка nix.
//!
//! ## Строки лексер разбирает ЦЕЛИКОМ
//!
//! `"a${x + "b"}c"` — это одна лексема, внутри которой живёт другое выражение, внутри которого
//! живёт другая строка. Обычный поток токенов такого не выражает: закрывающая кавычка зависит от
//! того, сколько фигурных скобок мы прошли внутри вставки.
//!
//! Поэтому строка вычитывается здесь до конца, со счётом вложенности, и распадается на КУСКИ:
//! литералы и исходный текст вставок. Текст вставки разбирает потом парсер — тем же лексером,
//! рекурсивно. Плата — повторный проход по вставкам, выигрыш — отсутствие лексера с состоянием,
//! в котором ошибка вылезает не там, где сделана.

use alloc::string::String;
use alloc::vec::Vec;

/// Кусок строки: литеральный текст либо исходник вставки `${…}`.
#[derive(Clone, Debug, PartialEq)]
pub enum Part {
    Lit(String),
    Interp(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Int(i64),
    Float(f64),
    Ident(String),
    /// Строка (обычная или отступная) — уже разобранная на куски.
    Str(Vec<Part>),
    /// Путь: `./x`, `/x`, `~/x`. В nix это отдельный тип значения, а не строка.
    Path(String),
    /// Путь в угловых скобках: `<nixpkgs>`.
    SPath(String),
    /// Ключевое слово.
    Kw(&'static str),
    /// Оператор или знак препинания.
    Op(&'static str),
    End,
}

const KEYWORDS: &[&str] =
    &["if", "then", "else", "assert", "with", "let", "in", "rec", "inherit", "or"];

/// Операторы. Порядок важен: длинные раньше коротких, иначе `//` прочтётся как два `/`.
const OPS: &[&str] = &[
    "...", "||", "&&", "==", "!=", "<=", ">=", "->", "//", "++", "@", "?", "!", "+", "-", "*", "/",
    "<", ">", "=", ";", ":", ",", ".", "(", ")", "[", "]", "{", "}",
];

pub struct Lexer<'a> {
    b: &'a [u8],
    i: usize,
}

pub type LexResult<T> = Result<T, String>;

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Lexer { b: src.as_bytes(), i: 0 }
    }

    /// Весь поток лексем разом. Отдельного «потокового» режима нет намеренно: выражения nix
    /// невелики, а хранить их целиком проще, чем тащить состояние лексера сквозь парсер.
    pub fn all(mut self) -> LexResult<Vec<Tok>> {
        let mut out = Vec::new();
        loop {
            let t = self.next_tok()?;
            let end = t == Tok::End;
            out.push(t);
            if end {
                return Ok(out);
            }
        }
    }

    fn skip_trivia(&mut self) {
        loop {
            while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
                self.i += 1;
            }
            if self.b[self.i..].starts_with(b"#") {
                while self.i < self.b.len() && self.b[self.i] != b'\n' {
                    self.i += 1;
                }
                continue;
            }
            if self.b[self.i..].starts_with(b"/*") {
                self.i += 2;
                while self.i < self.b.len() && !self.b[self.i..].starts_with(b"*/") {
                    self.i += 1;
                }
                self.i = (self.i + 2).min(self.b.len());
                continue;
            }
            return;
        }
    }

    fn next_tok(&mut self) -> LexResult<Tok> {
        self.skip_trivia();
        if self.i >= self.b.len() {
            return Ok(Tok::End);
        }
        let c = self.b[self.i];

        if c == b'"' {
            self.i += 1;
            return Ok(Tok::Str(self.string(b"\"", false)?));
        }
        if self.b[self.i..].starts_with(b"''") {
            self.i += 2;
            let parts = self.string(b"''", true)?;
            return Ok(Tok::Str(dedent(parts)));
        }
        if c == b'<' && self.is_spath() {
            self.i += 1;
            let start = self.i;
            while self.i < self.b.len() && self.b[self.i] != b'>' {
                self.i += 1;
            }
            let name = str_of(&self.b[start..self.i]);
            self.i += 1;
            return Ok(Tok::SPath(name));
        }
        let plen = path_len(&self.b[self.i..]);
        if plen > 0 {
            let start = self.i;
            self.i += plen;
            return Ok(Tok::Path(str_of(&self.b[start..self.i])));
        }
        if c.is_ascii_digit() {
            let start = self.i;
            while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
                self.i += 1;
            }
            // Дробная часть — только если за точкой цифра: `1.x` это не число.
            if self.i + 1 < self.b.len()
                && self.b[self.i] == b'.'
                && self.b[self.i + 1].is_ascii_digit()
            {
                self.i += 1;
                while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
                    self.i += 1;
                }
                let text = str_of(&self.b[start..self.i]);
                return text.parse::<f64>().map(Tok::Float).map_err(|_| String::from("плохое число"));
            }
            let text = str_of(&self.b[start..self.i]);
            return text.parse::<i64>().map(Tok::Int).map_err(|_| String::from("число не влезло"));
        }
        if is_ident_start(c) {
            let start = self.i;
            while self.i < self.b.len() && is_ident_byte(self.b[self.i]) {
                self.i += 1;
            }
            let word = str_of(&self.b[start..self.i]);
            if let Some(k) = KEYWORDS.iter().find(|k| **k == word) {
                return Ok(Tok::Kw(k));
            }
            return Ok(Tok::Ident(word));
        }
        // Вставка `${` встречается и вне строк — в именах атрибутов (`{ ${k} = 1; }`).
        if self.b[self.i..].starts_with(b"${") {
            self.i += 2;
            return Ok(Tok::Op("${"));
        }
        for op in OPS {
            if self.b[self.i..].starts_with(op.as_bytes()) {
                self.i += op.len();
                return Ok(Tok::Op(op));
            }
        }
        Err(alloc::format!("непонятный символ '{}'", c as char))
    }

    /// `<nixpkgs>` против оператора «меньше»: угловой путь — это буквы, цифры и `/.-_` до `>`.
    fn is_spath(&self) -> bool {
        let mut j = self.i + 1;
        let mut any = false;
        while j < self.b.len() {
            match self.b[j] {
                b'>' => return any,
                c if is_ident_byte(c) || c == b'/' || c == b'.' => {
                    any = true;
                    j += 1;
                }
                _ => return false,
            }
        }
        false
    }

    /// Тело строки до закрывателя `end`. `raw` — отступная строка (`''`): в ней иные правила
    /// экранирования (`''$`, `'''`) и обратная косая сама по себе не спецсимвол.
    fn string(&mut self, end: &[u8], raw: bool) -> LexResult<Vec<Part>> {
        let mut parts: Vec<Part> = Vec::new();
        let mut lit = String::new();
        loop {
            if self.i >= self.b.len() {
                return Err(String::from("строка не закрыта"));
            }
            if self.b[self.i..].starts_with(end) {
                self.i += end.len();
                if !lit.is_empty() || parts.is_empty() {
                    parts.push(Part::Lit(lit));
                }
                return Ok(parts);
            }
            if self.b[self.i..].starts_with(b"${") {
                if !lit.is_empty() {
                    parts.push(Part::Lit(core::mem::take(&mut lit)));
                }
                self.i += 2;
                let src = self.interp_src()?;
                parts.push(Part::Interp(src));
                continue;
            }
            if raw {
                // `''$` даёт `$`, `'''` даёт `''`, `''\n` — экранирование как в обычной строке.
                if self.b[self.i..].starts_with(b"''$") {
                    lit.push('$');
                    self.i += 3;
                    continue;
                }
                if self.b[self.i..].starts_with(b"'''") {
                    lit.push_str("''");
                    self.i += 3;
                    continue;
                }
                if self.b[self.i..].starts_with(b"''\\") && self.i + 3 < self.b.len() {
                    lit.push(unescape(self.b[self.i + 3]));
                    self.i += 4;
                    continue;
                }
            } else if self.b[self.i] == b'\\' {
                self.i += 1;
                if self.i >= self.b.len() {
                    return Err(String::from("строка не закрыта"));
                }
                lit.push(unescape(self.b[self.i]));
                self.i += 1;
                continue;
            }
            // Байты копируем как есть: UTF-8 переживёт это без разбора на символы.
            let start = self.i;
            self.i += 1;
            while self.i < self.b.len() && self.b[self.i] & 0xc0 == 0x80 {
                self.i += 1;
            }
            lit.push_str(&str_of(&self.b[start..self.i]));
        }
    }

    /// Исходник вставки от `${` до парной `}` — со счётом вложенности и с пропуском строк
    /// внутри (в них скобки не считаются).
    fn interp_src(&mut self) -> LexResult<String> {
        let start = self.i;
        let mut depth = 1usize;
        while self.i < self.b.len() {
            match self.b[self.i] {
                b'{' => {
                    depth += 1;
                    self.i += 1;
                }
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        let src = str_of(&self.b[start..self.i]);
                        self.i += 1;
                        return Ok(src);
                    }
                    self.i += 1;
                }
                b'"' => {
                    self.i += 1;
                    self.skip_string(b"\"", false)?;
                }
                b'\'' if self.b[self.i..].starts_with(b"''") => {
                    self.i += 2;
                    self.skip_string(b"''", true)?;
                }
                b'#' => {
                    while self.i < self.b.len() && self.b[self.i] != b'\n' {
                        self.i += 1;
                    }
                }
                _ => self.i += 1,
            }
        }
        Err(String::from("вставка не закрыта"))
    }

    /// Пробежать строку, не разбирая её: нужно только не считать скобки внутри неё.
    fn skip_string(&mut self, end: &[u8], raw: bool) -> LexResult<()> {
        while self.i < self.b.len() {
            if self.b[self.i..].starts_with(end) {
                self.i += end.len();
                return Ok(());
            }
            if self.b[self.i..].starts_with(b"${") {
                self.i += 2;
                self.interp_src()?;
                continue;
            }
            if raw && (self.b[self.i..].starts_with(b"''$") || self.b[self.i..].starts_with(b"'''"))
            {
                self.i += 3;
                continue;
            }
            if !raw && self.b[self.i] == b'\\' {
                self.i += 2;
                continue;
            }
            self.i += 1;
        }
        Err(String::from("строка не закрыта"))
    }
}

fn unescape(c: u8) -> char {
    match c {
        b'n' => '\n',
        b'r' => '\r',
        b't' => '\t',
        other => other as char,
    }
}

fn str_of(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'\'' | b'-')
}

fn is_path_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-' | b'+' | b'/' | b'~')
}

/// Длина лексемы-пути в начале `b`, 0 — здесь не путь.
///
/// Правило nix: `символ* ('/' символ+)+`, где символ — буква, цифра или `._+-~`. КАЖДАЯ косая
/// обязана иметь продолжение, и это не придирка: без неё `//` (обновление множества) читается
/// как путь из двух косых, а `a / b` — как путь вместо деления. Первое к тому же зацикливало
/// лексер: путь из одних косых после отсечения хвоста давал пустую лексему, счётчик не двигался,
/// и поток токенов рос, пока не кончалась память.
fn path_len(b: &[u8]) -> usize {
    let seg = |b: &[u8], mut i: usize| {
        while i < b.len() && is_path_byte(b[i]) && b[i] != b'/' {
            i += 1;
        }
        i
    };
    let mut i = seg(b, 0);
    let mut end = 0usize;
    while i < b.len() && b[i] == b'/' {
        let after = seg(b, i + 1);
        if after == i + 1 {
            break; // косая без продолжения — путь кончился раньше неё
        }
        i = after;
        end = i;
    }
    end
}

/// Отступная строка: снять общий отступ и первый перевод строки.
///
/// Правило nix: считается минимальный отступ по строкам, в которых есть хоть что-то кроме
/// пробелов; строка целиком из пробелов усекается до пустой. Кусок-вставка отступом не
/// считается — она может дать что угодно.
fn dedent(parts: Vec<Part>) -> Vec<Part> {
    // Текст, по которому меряем отступ: только литералы, вставки — как непробельные метки.
    let mut min = usize::MAX;
    let mut at_line_start = true;
    let mut indent = 0usize;
    let mut blank = true;
    for p in &parts {
        match p {
            Part::Lit(s) => {
                for c in s.chars() {
                    if at_line_start && c == ' ' {
                        indent += 1;
                        continue;
                    }
                    if c == '\n' {
                        at_line_start = true;
                        indent = 0;
                        blank = true;
                        continue;
                    }
                    if at_line_start {
                        at_line_start = false;
                        blank = false;
                        min = min.min(indent);
                    }
                }
            }
            Part::Interp(_) => {
                if at_line_start {
                    at_line_start = false;
                    blank = false;
                    min = min.min(indent);
                }
            }
        }
    }
    let _ = blank;
    let min = if min == usize::MAX { 0 } else { min };

    let mut out: Vec<Part> = Vec::new();
    let mut at_line_start = true;
    let mut left = 0usize;
    for p in parts {
        match p {
            Part::Lit(s) => {
                let mut o = String::new();
                for c in s.chars() {
                    if at_line_start && c == ' ' && left > 0 {
                        left -= 1;
                        continue;
                    }
                    at_line_start = false;
                    if c == '\n' {
                        at_line_start = true;
                        left = min;
                    }
                    o.push(c);
                }
                out.push(Part::Lit(o));
            }
            Part::Interp(s) => {
                at_line_start = false;
                out.push(Part::Interp(s));
            }
        }
    }
    // Первый перевод строки сразу после `''` не входит в значение.
    if let Some(Part::Lit(first)) = out.first_mut() {
        if let Some(rest) = first.strip_prefix('\n') {
            *first = String::from(rest);
        }
    }
    out
}

/// Начальный отступ считается ОТ НАЧАЛА строки, а `''` стоит не в её начале. Поэтому первая
/// строка (до первого перевода) в подсчёте отступа не участвует — её и снимаем заранее.
pub fn lex(src: &str) -> LexResult<Vec<Tok>> {
    Lexer::new(src).all()
}
