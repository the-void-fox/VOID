//! Reader: текст `.vv` → `Vec<Value>` (последовательность форм верхнего уровня).
//!
//! Веха 102 (ADR 0013) — синтаксис **инфиксный, семьи Rust/Python**, вместо S-выражений. Меняется
//! только он: дерево [`Value`] прежнее, поэтому вычислитель, стдлиб, нормализация конфига и ядро
//! не тронуты вовсе. Скобки не исчезли — они стали скобками ВЫЗОВА, то есть той записью, которую
//! человек читает не задумываясь.
//!
//! ```text
//!   f(a, b)                 → (f a b)
//!   [a, b]                  → (list a b)
//!   имя = выражение         → (define имя выражение)
//!   |x, y| тело             → (lambda (x y) тело)
//!   if c { a } else { b }   → (if c a b)
//!   a + b · a == b          → (+ a b) · (= a b)
//!   a |> f(b)               → (| a (f b))   конвейер thread-last
//!   true / false            → #t / #f
//!   # до конца строки       — комментарий
//! ```
//!
//! Три следствия выбора, названные в ADR: равенство — `==` (одиночное `=` занято присваиванием),
//! конвейер — `|>` (одиночное `|` занято лямбдой), булевы — словами.
//!
//! Разбор выражений — Пратт (приоритеты снизу вверх: `|>` → `==` → `+ -` → `* /` → вызов). Он же
//! убирает скобки вокруг арифметики, ради которых S-выражения и раздражали.

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::value::Value;

/// Ошибка разбора с человекочитаемым текстом.
#[derive(Clone, Debug)]
pub struct ReadError(pub String);

fn err<T>(msg: impl Into<String>) -> Result<T, ReadError> {
    Err(ReadError(msg.into()))
}

// ── лексер ───────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Int(i64),
    Str(String),
    Ident(String),
    /// Пунктуация и операторы: `( ) [ ] { } , = == + - * / | |>`.
    Punct(&'static str),
}

struct Lexer {
    src: Vec<char>,
    pos: usize,
}

impl Lexer {
    fn tokens(src: &str) -> Result<Vec<Tok>, ReadError> {
        let mut lx = Lexer { src: src.chars().collect(), pos: 0 };
        let mut out = Vec::new();
        loop {
            lx.skip_ws();
            if lx.pos >= lx.src.len() {
                return Ok(out);
            }
            out.push(lx.next_tok()?);
        }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.src.len() {
            let c = self.src[self.pos];
            if c == '#' {
                while self.pos < self.src.len() && self.src[self.pos] != '\n' {
                    self.pos += 1;
                }
            } else if c.is_whitespace() {
                self.pos += 1;
            } else {
                return;
            }
        }
    }

    fn next_tok(&mut self) -> Result<Tok, ReadError> {
        let c = self.src[self.pos];
        // Двухсимвольные операторы — раньше односимвольных, иначе `==` прочтётся как два `=`.
        let two: String = self.src[self.pos..].iter().take(2).collect();
        if two == "==" {
            self.pos += 2;
            return Ok(Tok::Punct("=="));
        }
        if two == "|>" {
            self.pos += 2;
            return Ok(Tok::Punct("|>"));
        }
        for (ch, name) in [
            ('(', "("), (')', ")"), ('[', "["), (']', "]"), ('{', "{"), ('}', "}"),
            (',', ","), ('=', "="), ('+', "+"), ('*', "*"), ('/', "/"), ('|', "|"),
        ] {
            if c == ch {
                self.pos += 1;
                return Ok(Tok::Punct(name));
            }
        }
        if c == '-' {
            // Минус — часть числа, если сразу за ним цифра; иначе оператор.
            let next = self.src.get(self.pos + 1).copied().unwrap_or(' ');
            if !next.is_ascii_digit() {
                self.pos += 1;
                return Ok(Tok::Punct("-"));
            }
            return self.number();
        }
        if c == '"' {
            return self.string();
        }
        if c.is_ascii_digit() {
            return self.number();
        }
        self.ident()
    }

    fn string(&mut self) -> Result<Tok, ReadError> {
        self.pos += 1; // открывающая кавычка
        let mut s = String::new();
        while self.pos < self.src.len() {
            let c = self.src[self.pos];
            self.pos += 1;
            match c {
                '"' => return Ok(Tok::Str(s)),
                '\\' => {
                    let e = *self.src.get(self.pos).unwrap_or(&'"');
                    self.pos += 1;
                    s.push(match e {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        other => other,
                    });
                }
                _ => s.push(c),
            }
        }
        err("незакрытая строка")
    }

    fn number(&mut self) -> Result<Tok, ReadError> {
        let start = self.pos;
        if self.src[self.pos] == '-' {
            self.pos += 1;
        }
        while self.pos < self.src.len() && self.src[self.pos].is_ascii_digit() {
            self.pos += 1;
        }
        let text: String = self.src[start..self.pos].iter().collect();
        match text.parse::<i64>() {
            Ok(n) => Ok(Tok::Int(n)),
            Err(_) => err(alloc::format!("не число: {}", text)),
        }
    }

    /// Имя: всё до пробела, скобки, запятой или оператора. **Дефис внутри имени разрешён**
    /// (`font-size`, `split-v`, `net-srv` — так пишутся ключи конфига), поэтому минус как
    /// оператор обязан отделяться пробелами: `a - b`, а не `a-b`. В семье Rust/Python это
    /// нетипично, но конфиг важнее арифметики — а вычитание в `.vv` встречается почти никогда.
    fn ident(&mut self) -> Result<Tok, ReadError> {
        let start = self.pos;
        while self.pos < self.src.len() {
            let c = self.src[self.pos];
            if c.is_whitespace() || "()[]{},=+*/|\"#".contains(c) {
                break;
            }
            self.pos += 1;
        }
        if start == self.pos {
            return err(alloc::format!("непонятный символ '{}'", self.src[self.pos]));
        }
        Ok(Tok::Ident(self.src[start..self.pos].iter().collect()))
    }
}

// ── парсер (Пратт) ───────────────────────────────────────────────────────────

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

/// Приоритет инфиксного оператора (больше — крепче связывает) и имя формы в дереве.
fn infix_prec(t: &Tok) -> Option<(u8, &'static str)> {
    match t {
        Tok::Punct("|>") => Some((1, "|")),
        Tok::Punct("==") => Some((2, "=")),
        Tok::Punct("+") => Some((3, "+")),
        Tok::Punct("-") => Some((3, "-")),
        Tok::Punct("*") => Some((4, "*")),
        Tok::Punct("/") => Some((4, "/")),
        _ => None,
    }
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn eat(&mut self, p: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Punct(x)) if *x == p) {
            self.pos += 1;
            return true;
        }
        false
    }

    fn expect(&mut self, p: &str) -> Result<(), ReadError> {
        if self.eat(p) {
            Ok(())
        } else {
            err(alloc::format!("ожидалось '{}'", p))
        }
    }

    /// Форма верхнего уровня: либо `имя = выражение` (определение), либо выражение.
    fn form(&mut self) -> Result<Value, ReadError> {
        if let (Some(Tok::Ident(name)), Some(Tok::Punct("="))) =
            (self.peek().cloned(), self.toks.get(self.pos + 1))
        {
            self.pos += 2;
            let body = self.expr(0)?;
            return Ok(Value::list(vec![Value::sym("define"), Value::sym(&name), body]));
        }
        self.expr(0)
    }

    fn expr(&mut self, min_prec: u8) -> Result<Value, ReadError> {
        let mut left = self.primary()?;
        while let Some((prec, op)) = self.peek().and_then(infix_prec) {
            if prec < min_prec {
                break;
            }
            self.pos += 1;
            let right = self.expr(prec + 1)?;
            left = Value::list(vec![Value::sym(op), left, right]);
        }
        Ok(left)
    }

    fn primary(&mut self) -> Result<Value, ReadError> {
        let t = match self.peek().cloned() {
            Some(t) => t,
            None => return err("выражение оборвалось"),
        };
        let v = match t {
            Tok::Int(n) => {
                self.pos += 1;
                Value::Int(n)
            }
            Tok::Str(s) => {
                self.pos += 1;
                Value::str(&s)
            }
            Tok::Punct("[") => {
                self.pos += 1;
                let items = self.args("]")?;
                let mut out = vec![Value::sym("list")];
                out.extend(items);
                Value::list(out)
            }
            Tok::Punct("(") => {
                self.pos += 1;
                let inner = self.expr(0)?;
                self.expect(")")?;
                inner
            }
            // Лямбда: `|x, y| тело`; без параметров — `|| тело`.
            Tok::Punct("|") => {
                self.pos += 1;
                let mut params = Vec::new();
                while !self.eat("|") {
                    match self.peek().cloned() {
                        Some(Tok::Ident(p)) => {
                            self.pos += 1;
                            params.push(Value::sym(&p));
                        }
                        _ => return err("параметр лямбды — имя"),
                    }
                    if !self.eat(",") && !matches!(self.peek(), Some(Tok::Punct("|"))) {
                        return err("параметры лямбды — через запятую");
                    }
                }
                let body = self.expr(0)?;
                Value::list(vec![Value::sym("lambda"), Value::list(params), body])
            }
            Tok::Punct("-") => {
                self.pos += 1;
                let v = self.primary()?;
                Value::list(vec![Value::sym("-"), v])
            }
            Tok::Ident(name) if name == "if" => {
                self.pos += 1;
                let cond = self.expr(0)?;
                self.expect("{")?;
                let then = self.expr(0)?;
                self.expect("}")?;
                let mut form = vec![Value::sym("if"), cond, then];
                if matches!(self.peek(), Some(Tok::Ident(k)) if k == "else") {
                    self.pos += 1;
                    self.expect("{")?;
                    form.push(self.expr(0)?);
                    self.expect("}")?;
                }
                Value::list(form)
            }
            Tok::Ident(name) => {
                self.pos += 1;
                match name.as_str() {
                    "true" => Value::Bool(true),
                    "false" => Value::Bool(false),
                    _ => Value::sym(&name),
                }
            }
            other => return err(alloc::format!("неожиданное {:?}", other)),
        };
        // Постфикс — вызов `f(...)`; вызывать можно и результат вызова (`f(a)(b)`).
        let mut v = v;
        while self.eat("(") {
            let args = self.args(")")?;
            let mut call = vec![v];
            call.extend(args);
            v = Value::list(call);
        }
        Ok(v)
    }

    /// Аргументы через запятую до закрывающей скобки (хвостовая запятая разрешена).
    fn args(&mut self, close: &str) -> Result<Vec<Value>, ReadError> {
        let mut out = Vec::new();
        loop {
            if self.eat(close) {
                return Ok(out);
            }
            out.push(self.expr(0)?);
            if self.eat(",") {
                continue;
            }
            self.expect(close)?;
            return Ok(out);
        }
    }
}

/// Прочитать ВСЕ формы верхнего уровня из исходника.
pub fn read_all(src: &str) -> Result<Vec<Value>, ReadError> {
    let mut p = Parser { toks: Lexer::tokens(src)?, pos: 0 };
    let mut forms = Vec::new();
    while p.pos < p.toks.len() {
        forms.push(p.form()?);
    }
    Ok(forms)
}

/// Напечатать значение НОВЫМ синтаксисом — обратно к [`read_all`]. Нужна везде, где текст видит
/// человек: REPL, ошибки, `quote`. Печатать скобочно то, что читается инфиксно, значит показывать
/// пользователю язык, которого больше нет.
pub fn print(v: &Value) -> String {
    match v {
        Value::List(items) => match items.first() {
            Some(Value::Sym(s)) if &**s == "list" => {
                let parts: Vec<String> = items[1..].iter().map(print).collect();
                alloc::format!("[{}]", parts.join(", "))
            }
            Some(Value::Sym(s)) if &**s == "define" && items.len() == 3 => {
                alloc::format!("{} = {}", print(&items[1]), print(&items[2]))
            }
            Some(Value::Sym(s)) if &**s == "lambda" && items.len() == 3 => {
                let ps = match &items[1] {
                    Value::List(p) => p.iter().map(print).collect::<Vec<_>>().join(", "),
                    other => print(other),
                };
                alloc::format!("|{}| {}", ps, print(&items[2]))
            }
            Some(Value::Sym(s)) if &**s == "if" && items.len() >= 3 => {
                let mut out = alloc::format!("if {} {{ {} }}", print(&items[1]), print(&items[2]));
                if let Some(e) = items.get(3) {
                    out.push_str(&alloc::format!(" else {{ {} }}", print(e)));
                }
                out
            }
            Some(Value::Sym(s))
                if items.len() == 3 && matches!(&**s, "+" | "-" | "*" | "/" | "=" | "|") =>
            {
                let op = match &**s {
                    "=" => "==",
                    "|" => "|>",
                    other => other,
                };
                alloc::format!("{} {} {}", print(&items[1]), op, print(&items[2]))
            }
            Some(head) => {
                let parts: Vec<String> = items[1..].iter().map(print).collect();
                alloc::format!("{}({})", print(head), parts.join(", "))
            }
            None => "[]".to_string(),
        },
        other => alloc::format!("{}", other),
    }
}
