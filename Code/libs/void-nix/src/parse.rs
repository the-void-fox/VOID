//! Синтаксис языка nix: дерево выражений и разбор потока лексем.
//!
//! ## Приоритеты взяты у самого nix
//!
//! Таблица ниже — построчный перевод `%left`/`%right`/`%nonassoc` из грамматики nix, от низшего
//! к высшему: `->`, `||`, `&&`, `==`/`!=`, сравнения, `//`, `!`, `+`/`-`, `*`//, `++`, `?`,
//! унарный минус, применение, выборка. Придумывать её заново нельзя: `!a + b` в nix значит
//! `!(a + b)`, и всякий, кто «исправит» это на очевидное, получит тихо другой смысл у чужого
//! кода.
//!
//! ## Где нужен заглядыватель вперёд
//!
//! Ровно в одном месте: `{` начинает и множество атрибутов, и образец аргумента функции
//! (`{ a, b }: …`). Различить их по первым лексемам нельзя — `{ }` может оказаться и тем и
//! другим. Поэтому парсер находит ПАРНУЮ скобку и смотрит, что за ней: `:` или `@` значит
//! функцию. Проход лишний, зато решение принимается по факту, а не по догадке.

use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

use crate::lex::{lex, Part, Tok};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    Add,
    Sub,
    Mul,
    Div,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    And,
    Or,
    Impl,
    Update,
    Concat,
}

#[derive(Debug)]
pub enum StrPart {
    Lit(String),
    Interp(Rc<Expr>),
}

#[derive(Debug)]
pub enum AttrName {
    Fixed(String),
    /// Динамическое имя: `${e}` или строка со вставкой.
    Dyn(Rc<Expr>),
}

#[derive(Debug)]
pub enum Bind {
    /// `a.b.c = e;`
    Set { path: Vec<AttrName>, value: Rc<Expr> },
    /// `inherit a b;` или `inherit (e) a b;`
    Inherit { from: Option<Rc<Expr>>, names: Vec<AttrName> },
}

#[derive(Debug)]
pub enum Param {
    Ident(String),
    Pattern {
        fields: Vec<(String, Option<Rc<Expr>>)>,
        ellipsis: bool,
        /// Имя для всего аргумента целиком (`args@{ … }`).
        bind: Option<String>,
    },
}

#[derive(Debug)]
pub enum Expr {
    Int(i64),
    Float(f64),
    Str(Vec<StrPart>),
    Path(String),
    SPath(String),
    Var(String),
    List(Vec<Rc<Expr>>),
    Attrs { rec: bool, binds: Vec<Bind> },
    Lambda { param: Param, body: Rc<Expr> },
    Apply(Rc<Expr>, Rc<Expr>),
    Select { on: Rc<Expr>, path: Vec<AttrName>, default: Option<Rc<Expr>> },
    HasAttr { on: Rc<Expr>, path: Vec<AttrName> },
    Bin(Op, Rc<Expr>, Rc<Expr>),
    Not(Rc<Expr>),
    Neg(Rc<Expr>),
    If(Rc<Expr>, Rc<Expr>, Rc<Expr>),
    Let { binds: Vec<Bind>, body: Rc<Expr> },
    With(Rc<Expr>, Rc<Expr>),
    Assert(Rc<Expr>, Rc<Expr>),
}

pub type PResult<T> = Result<T, String>;

pub fn parse(src: &str) -> PResult<Rc<Expr>> {
    let toks = lex(src)?;
    let mut p = P { t: toks, i: 0 };
    let e = p.expr()?;
    if p.peek() != &Tok::End {
        return Err(alloc::format!("лишнее в конце выражения: {:?}", p.peek()));
    }
    Ok(e)
}

struct P {
    t: Vec<Tok>,
    i: usize,
}

impl P {
    fn peek(&self) -> &Tok {
        self.t.get(self.i).unwrap_or(&Tok::End)
    }
    fn at(&self, k: usize) -> &Tok {
        self.t.get(self.i + k).unwrap_or(&Tok::End)
    }
    fn bump(&mut self) -> Tok {
        let t = self.t.get(self.i).cloned().unwrap_or(Tok::End);
        self.i += 1;
        t
    }
    fn eat_op(&mut self, s: &str) -> bool {
        if matches!(self.peek(), Tok::Op(o) if *o == s) {
            self.i += 1;
            return true;
        }
        false
    }
    fn is_op(&self, s: &str) -> bool {
        matches!(self.peek(), Tok::Op(o) if *o == s)
    }
    fn eat_kw(&mut self, s: &str) -> bool {
        if matches!(self.peek(), Tok::Kw(k) if *k == s) {
            self.i += 1;
            return true;
        }
        false
    }
    fn want_op(&mut self, s: &str) -> PResult<()> {
        if self.eat_op(s) {
            Ok(())
        } else {
            Err(alloc::format!("ожидалось '{}', а там {:?}", s, self.peek()))
        }
    }
    fn want_kw(&mut self, s: &str) -> PResult<()> {
        if self.eat_kw(s) {
            Ok(())
        } else {
            Err(alloc::format!("ожидалось '{}', а там {:?}", s, self.peek()))
        }
    }

    // ── выражение целиком ──────────────────────────────────────────────────────
    fn expr(&mut self) -> PResult<Rc<Expr>> {
        // Функция: `x: …`, `x@{…}: …`, `{…}: …`.
        if let Tok::Ident(name) = self.peek().clone() {
            if matches!(self.at(1), Tok::Op(o) if *o == ":") {
                self.i += 2;
                let body = self.expr()?;
                return Ok(Rc::new(Expr::Lambda { param: Param::Ident(name), body }));
            }
            if matches!(self.at(1), Tok::Op(o) if *o == "@") {
                self.i += 2;
                let mut param = self.pattern()?;
                if let Param::Pattern { bind, .. } = &mut param {
                    *bind = Some(name);
                }
                self.want_op(":")?;
                let body = self.expr()?;
                return Ok(Rc::new(Expr::Lambda { param, body }));
            }
        }
        if self.is_op("{") && self.brace_is_pattern() {
            let mut param = self.pattern()?;
            if self.eat_op("@") {
                let Tok::Ident(name) = self.bump() else {
                    return Err(String::from("после @ нужно имя"));
                };
                if let Param::Pattern { bind, .. } = &mut param {
                    *bind = Some(name);
                }
            }
            self.want_op(":")?;
            let body = self.expr()?;
            return Ok(Rc::new(Expr::Lambda { param, body }));
        }
        if self.eat_kw("let") {
            let binds = self.binds("in")?;
            self.want_kw("in")?;
            let body = self.expr()?;
            return Ok(Rc::new(Expr::Let { binds, body }));
        }
        if self.eat_kw("if") {
            let c = self.expr()?;
            self.want_kw("then")?;
            let t = self.expr()?;
            self.want_kw("else")?;
            let f = self.expr()?;
            return Ok(Rc::new(Expr::If(c, t, f)));
        }
        if self.eat_kw("with") {
            let e = self.expr()?;
            self.want_op(";")?;
            let body = self.expr()?;
            return Ok(Rc::new(Expr::With(e, body)));
        }
        if self.eat_kw("assert") {
            let c = self.expr()?;
            self.want_op(";")?;
            let body = self.expr()?;
            return Ok(Rc::new(Expr::Assert(c, body)));
        }
        self.impl_()
    }

    /// Стоит ли за парной `}` двоеточие или `@` — то есть образец аргумента, а не множество.
    fn brace_is_pattern(&self) -> bool {
        let mut depth = 0usize;
        let mut j = self.i;
        loop {
            match self.t.get(j) {
                None | Some(Tok::End) => return false,
                Some(Tok::Op("{")) | Some(Tok::Op("${")) => depth += 1,
                Some(Tok::Op("}")) => {
                    depth -= 1;
                    if depth == 0 {
                        return matches!(self.t.get(j + 1), Some(Tok::Op(":")) | Some(Tok::Op("@")));
                    }
                }
                _ => {}
            }
            j += 1;
        }
    }

    fn pattern(&mut self) -> PResult<Param> {
        self.want_op("{")?;
        let mut fields: Vec<(String, Option<Rc<Expr>>)> = Vec::new();
        let mut ellipsis = false;
        loop {
            if self.eat_op("}") {
                break;
            }
            if self.eat_op("...") {
                ellipsis = true;
                self.want_op("}")?;
                break;
            }
            let name = match self.bump() {
                Tok::Ident(n) => n,
                other => return Err(alloc::format!("в образце ожидалось имя, а там {:?}", other)),
            };
            let default = if self.eat_op("?") { Some(self.expr()?) } else { None };
            fields.push((name, default));
            if !self.eat_op(",") {
                self.want_op("}")?;
                break;
            }
        }
        Ok(Param::Pattern { fields, ellipsis, bind: None })
    }

    // ── уровни приоритета ──────────────────────────────────────────────────────
    fn impl_(&mut self) -> PResult<Rc<Expr>> {
        let l = self.or_()?;
        if self.eat_op("->") {
            let r = self.impl_()?;
            return Ok(Rc::new(Expr::Bin(Op::Impl, l, r)));
        }
        Ok(l)
    }
    fn or_(&mut self) -> PResult<Rc<Expr>> {
        let mut l = self.and_()?;
        while self.eat_op("||") {
            let r = self.and_()?;
            l = Rc::new(Expr::Bin(Op::Or, l, r));
        }
        Ok(l)
    }
    fn and_(&mut self) -> PResult<Rc<Expr>> {
        let mut l = self.eq_()?;
        while self.eat_op("&&") {
            let r = self.eq_()?;
            l = Rc::new(Expr::Bin(Op::And, l, r));
        }
        Ok(l)
    }
    fn eq_(&mut self) -> PResult<Rc<Expr>> {
        let l = self.cmp_()?;
        if self.eat_op("==") {
            return Ok(Rc::new(Expr::Bin(Op::Eq, l, self.cmp_()?)));
        }
        if self.eat_op("!=") {
            return Ok(Rc::new(Expr::Bin(Op::Ne, l, self.cmp_()?)));
        }
        Ok(l)
    }
    fn cmp_(&mut self) -> PResult<Rc<Expr>> {
        let l = self.update_()?;
        for (s, o) in [("<=", Op::Le), (">=", Op::Ge), ("<", Op::Lt), (">", Op::Gt)] {
            if self.eat_op(s) {
                return Ok(Rc::new(Expr::Bin(o, l, self.update_()?)));
            }
        }
        Ok(l)
    }
    fn update_(&mut self) -> PResult<Rc<Expr>> {
        let l = self.not_()?;
        if self.eat_op("//") {
            let r = self.update_()?;
            return Ok(Rc::new(Expr::Bin(Op::Update, l, r)));
        }
        Ok(l)
    }
    fn not_(&mut self) -> PResult<Rc<Expr>> {
        if self.eat_op("!") {
            return Ok(Rc::new(Expr::Not(self.not_()?)));
        }
        self.add_()
    }
    fn add_(&mut self) -> PResult<Rc<Expr>> {
        let mut l = self.mul_()?;
        loop {
            if self.eat_op("+") {
                l = Rc::new(Expr::Bin(Op::Add, l, self.mul_()?));
            } else if self.eat_op("-") {
                l = Rc::new(Expr::Bin(Op::Sub, l, self.mul_()?));
            } else {
                return Ok(l);
            }
        }
    }
    fn mul_(&mut self) -> PResult<Rc<Expr>> {
        let mut l = self.concat_()?;
        loop {
            if self.eat_op("*") {
                l = Rc::new(Expr::Bin(Op::Mul, l, self.concat_()?));
            } else if self.eat_op("/") {
                l = Rc::new(Expr::Bin(Op::Div, l, self.concat_()?));
            } else {
                return Ok(l);
            }
        }
    }
    fn concat_(&mut self) -> PResult<Rc<Expr>> {
        let l = self.hasattr_()?;
        if self.eat_op("++") {
            let r = self.concat_()?;
            return Ok(Rc::new(Expr::Bin(Op::Concat, l, r)));
        }
        Ok(l)
    }
    fn hasattr_(&mut self) -> PResult<Rc<Expr>> {
        let l = self.neg_()?;
        if self.eat_op("?") {
            let path = self.attrpath()?;
            return Ok(Rc::new(Expr::HasAttr { on: l, path }));
        }
        Ok(l)
    }
    fn neg_(&mut self) -> PResult<Rc<Expr>> {
        if self.eat_op("-") {
            return Ok(Rc::new(Expr::Neg(self.neg_()?)));
        }
        self.apply_()
    }

    fn apply_(&mut self) -> PResult<Rc<Expr>> {
        let mut f = self.select_()?;
        while self.starts_atom() {
            let a = self.select_()?;
            f = Rc::new(Expr::Apply(f, a));
        }
        Ok(f)
    }

    /// Начинается ли здесь операнд применения. Оператор операндом быть не может — иначе
    /// `f -1` прочлось бы как применение к `-1`, а в nix это вычитание.
    fn starts_atom(&self) -> bool {
        match self.peek() {
            Tok::Int(_) | Tok::Float(_) | Tok::Ident(_) | Tok::Str(_) | Tok::Path(_)
            | Tok::SPath(_) => true,
            Tok::Op(o) => matches!(*o, "(" | "[" | "{"),
            Tok::Kw(k) => matches!(*k, "rec" | "let" | "if" | "with" | "assert"),
            _ => false,
        }
    }

    fn select_(&mut self) -> PResult<Rc<Expr>> {
        let mut e = self.atom()?;
        if self.is_op(".") {
            self.i += 1;
            let path = self.attrpath()?;
            let default = if self.eat_kw("or") { Some(self.select_()?) } else { None };
            e = Rc::new(Expr::Select { on: e, path, default });
        }
        Ok(e)
    }

    fn attrpath(&mut self) -> PResult<Vec<AttrName>> {
        let mut path = Vec::new();
        loop {
            path.push(self.attrname()?);
            if self.is_op(".") {
                self.i += 1;
                continue;
            }
            return Ok(path);
        }
    }

    fn attrname(&mut self) -> PResult<AttrName> {
        match self.bump() {
            Tok::Ident(n) => Ok(AttrName::Fixed(n)),
            // Ключевое слово в позиции имени атрибута — обычное имя (`{ or = 1; }.or`).
            Tok::Kw(k) => Ok(AttrName::Fixed(String::from(k))),
            Tok::Str(parts) => {
                let e = self.str_expr(parts)?;
                match &*e {
                    Expr::Str(ps) if ps.len() == 1 => match &ps[0] {
                        StrPart::Lit(s) => Ok(AttrName::Fixed(s.clone())),
                        StrPart::Interp(_) => Ok(AttrName::Dyn(e.clone())),
                    },
                    _ => Ok(AttrName::Dyn(e)),
                }
            }
            Tok::Op("${") => {
                let e = self.expr()?;
                self.want_op("}")?;
                Ok(AttrName::Dyn(e))
            }
            other => Err(alloc::format!("ожидалось имя атрибута, а там {:?}", other)),
        }
    }

    fn str_expr(&mut self, parts: Vec<Part>) -> PResult<Rc<Expr>> {
        let mut out = Vec::with_capacity(parts.len());
        for p in parts {
            out.push(match p {
                Part::Lit(s) => StrPart::Lit(s),
                // Вставка разбирается ЗАНОВО, с нуля: лексер отдал её исходником, потому что
                // границы вставки знает только он (см. его шапку).
                Part::Interp(src) => StrPart::Interp(parse(&src)?),
            });
        }
        Ok(Rc::new(Expr::Str(out)))
    }

    fn atom(&mut self) -> PResult<Rc<Expr>> {
        match self.bump() {
            Tok::Int(v) => Ok(Rc::new(Expr::Int(v))),
            Tok::Float(v) => Ok(Rc::new(Expr::Float(v))),
            Tok::Path(p) => Ok(Rc::new(Expr::Path(p))),
            Tok::SPath(p) => Ok(Rc::new(Expr::SPath(p))),
            Tok::Ident(n) => Ok(Rc::new(Expr::Var(n))),
            Tok::Str(parts) => self.str_expr(parts),
            Tok::Kw("rec") => {
                self.want_op("{")?;
                let binds = self.binds("}")?;
                self.want_op("}")?;
                Ok(Rc::new(Expr::Attrs { rec: true, binds }))
            }
            Tok::Op("(") => {
                let e = self.expr()?;
                self.want_op(")")?;
                Ok(e)
            }
            Tok::Op("[") => {
                let mut items = Vec::new();
                while !self.eat_op("]") {
                    if self.peek() == &Tok::End {
                        return Err(String::from("список не закрыт"));
                    }
                    // Элементы списка — на уровне ВЫБОРКИ, а не применения: `[ f x ]` это два
                    // элемента, а не вызов. Так в nix, и на этом стоит запись зависимостей.
                    items.push(self.select_()?);
                }
                Ok(Rc::new(Expr::List(items)))
            }
            Tok::Op("{") => {
                let binds = self.binds("}")?;
                self.want_op("}")?;
                Ok(Rc::new(Expr::Attrs { rec: false, binds }))
            }
            other => Err(alloc::format!("не выражение: {:?}", other)),
        }
    }

    fn binds(&mut self, stop: &str) -> PResult<Vec<Bind>> {
        let mut out = Vec::new();
        loop {
            let done = match self.peek() {
                Tok::End => true,
                Tok::Op(o) => *o == stop,
                Tok::Kw(k) => *k == stop,
                _ => false,
            };
            if done {
                return Ok(out);
            }
            if self.eat_kw("inherit") {
                let from = if self.eat_op("(") {
                    let e = self.expr()?;
                    self.want_op(")")?;
                    Some(e)
                } else {
                    None
                };
                let mut names = Vec::new();
                while !self.eat_op(";") {
                    names.push(self.attrname()?);
                }
                out.push(Bind::Inherit { from, names });
                continue;
            }
            let path = self.attrpath()?;
            self.want_op("=")?;
            let value = self.expr()?;
            self.want_op(";")?;
            out.push(Bind::Set { path, value });
        }
    }
}

