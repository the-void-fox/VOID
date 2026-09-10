//! Ленивое вычисление: значения, отложки, окружение.
//!
//! ## Лень — не оптимизация, а семантика
//!
//! `{ a = throw "нет"; b = 1; }.b` в nix даёт `1`, а не ошибку, и `rec { x = y; y = 1; }` вообще
//! не имело бы смысла при вычислении по порядку. Поэтому каждое связанное имя — ОТЛОЖКА
//! (`Thunk`): выражение вместе с окружением, которое вычислится не раньше, чем понадобится, и
//! ровно один раз.
//!
//! Отложка, которую начали вычислять и попросили снова, — это бесконечная рекурсия, и молчать о
//! ней нельзя: программа просто зависла бы. Поэтому на время вычисления в ячейку кладётся
//! «чёрная дыра», и второй заход по ней узнаёт себя.
//!
//! ## `with` ищется ВТОРЫМ проходом
//!
//! Имя из `with e; …` слабее любого лексического: `let x = 1; in with { x = 2; }; x` даёт `1`.
//! Поэтому поиск идёт дважды по одной цепочке — сперва по кадрам-связкам, и лишь потом по
//! кадрам-`with`, изнутри наружу. Одним проходом это не выражается: иначе внутренний `with`
//! перебил бы внешнюю связку.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::parse::{AttrName, Bind, Expr, Op, Param, StrPart};

pub type EResult<T> = Result<T, String>;

/// Строка со СТРОКОВЫМ КОНТЕКСТОМ — множеством путей store, от которых она зависит.
///
/// Контекст не украшение: `"${drv}/bin/x"` обязано превратить деривацию во ВХОД той сборки, куда
/// эта строка попадёт. Без него текст выглядел бы как обычный путь, а зависимость исчезла бы —
/// и сборка полезла бы в никуда.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NixStr {
    pub s: String,
    pub ctx: BTreeSet<String>,
}

impl NixStr {
    pub fn plain(s: impl Into<String>) -> Self {
        NixStr { s: s.into(), ctx: BTreeSet::new() }
    }
}

pub type Attrs = BTreeMap<String, Thunk>;

#[derive(Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    Str(NixStr),
    Path(String),
    List(Rc<Vec<Thunk>>),
    Attrs(Rc<Attrs>),
    Lambda(Rc<Closure>),
    Prim(Prim),
}

pub struct Closure {
    pub lambda: Rc<Expr>,
    pub env: Env,
}

/// Примитив: имя, сколько аргументов ждёт и что уже накоплено. Частичное применение у примитивов
/// такое же, как у функций языка, — иначе `map f` перестал бы быть значением.
#[derive(Clone)]
pub struct Prim {
    pub name: &'static str,
    pub arity: usize,
    pub args: Rc<Vec<Thunk>>,
    pub f: fn(&mut Eval, &[Thunk]) -> EResult<Value>,
}

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Bool(_) => "bool",
            Value::Null => "null",
            Value::Str(_) => "string",
            Value::Path(_) => "path",
            Value::List(_) => "list",
            Value::Attrs(_) => "set",
            Value::Lambda(_) | Value::Prim(_) => "lambda",
        }
    }
}

// ── отложки ────────────────────────────────────────────────────────────────────

enum Th {
    Susp(Rc<Expr>, Env),
    Black,
    Done(Value),
}

#[derive(Clone)]
pub struct Thunk(Rc<RefCell<Th>>);

impl Thunk {
    pub fn susp(e: Rc<Expr>, env: Env) -> Self {
        Thunk(Rc::new(RefCell::new(Th::Susp(e, env))))
    }
    pub fn done(v: Value) -> Self {
        Thunk(Rc::new(RefCell::new(Th::Done(v))))
    }
}

// ── окружение ──────────────────────────────────────────────────────────────────

/// Связки кадра лежат в ЯЧЕЙКЕ, и это не про изменяемость, а про рекурсию: в `rec { a = b; }`
/// отложка `a` обязана смотреть на окружение, которого в момент её создания ещё нет. Ячейка
/// разделена, поэтому кадр можно создать пустым, раздать его отложкам и заполнить последним
/// действием. Другого способа замкнуть эту петлю без сборщика мусора нет.
enum FrameKind {
    Vars(Rc<RefCell<BTreeMap<String, Thunk>>>),
    With(Thunk),
}

struct Frame {
    kind: FrameKind,
    up: Env,
}

#[derive(Clone, Default)]
pub struct Env(Option<Rc<Frame>>);

impl Env {
    pub fn with_vars(&self, vars: BTreeMap<String, Thunk>) -> Env {
        self.with_cell(Rc::new(RefCell::new(vars)))
    }
    fn with_cell(&self, cell: Rc<RefCell<BTreeMap<String, Thunk>>>) -> Env {
        Env(Some(Rc::new(Frame { kind: FrameKind::Vars(cell), up: self.clone() })))
    }
    fn with_scope(&self, t: Thunk) -> Env {
        Env(Some(Rc::new(Frame { kind: FrameKind::With(t), up: self.clone() })))
    }
    fn lexical(&self, name: &str) -> Option<Thunk> {
        let mut cur = self;
        while let Some(f) = &cur.0 {
            if let FrameKind::Vars(m) = &f.kind {
                if let Some(t) = m.borrow().get(name) {
                    return Some(t.clone());
                }
            }
            cur = &f.up;
        }
        None
    }
    fn withs(&self) -> Vec<Thunk> {
        let mut out = Vec::new();
        let mut cur = self;
        while let Some(f) = &cur.0 {
            if let FrameKind::With(t) = &f.kind {
                out.push(t.clone());
            }
            cur = &f.up;
        }
        out
    }
}

// ── вычислитель ────────────────────────────────────────────────────────────────

pub struct Eval {
    pub builtins: Rc<Attrs>,
    depth: usize,
    /// Что делает `derivation`. Пока вычислителя деривации нет, он честно отказывает.
    pub drv: Option<Rc<dyn crate::DrvSink>>,
}

/// Глубина рекурсии, после которой мы объявляем зацикливание. Стек у программы VOID конечен, и
/// переполнить его значит убить процесс без объяснения; отказ с внятным словом лучше.
const MAX_DEPTH: usize = 2000;

impl Eval {
    pub fn new() -> Self {
        let mut ev = Eval { builtins: Rc::new(Attrs::new()), depth: 0, drv: None };
        ev.builtins = Rc::new(crate::builtins::table());
        ev
    }

    /// Корневое окружение: `builtins` и вынесенные наружу имена (`true`, `map`, `import`, …).
    pub fn root_env(&self) -> Env {
        let mut vars = BTreeMap::new();
        vars.insert(String::from("builtins"), Thunk::done(Value::Attrs(self.builtins.clone())));
        for name in crate::builtins::GLOBALS {
            if let Some(t) = self.builtins.get(*name) {
                vars.insert(String::from(*name), t.clone());
            }
        }
        vars.insert(String::from("true"), Thunk::done(Value::Bool(true)));
        vars.insert(String::from("false"), Thunk::done(Value::Bool(false)));
        vars.insert(String::from("null"), Thunk::done(Value::Null));
        Env::default().with_vars(vars)
    }

    /// Довычислить отложку до значения (WHNF).
    pub fn force(&mut self, t: &Thunk) -> EResult<Value> {
        let state = core::mem::replace(&mut *t.0.borrow_mut(), Th::Black);
        match state {
            Th::Done(v) => {
                *t.0.borrow_mut() = Th::Done(v.clone());
                Ok(v)
            }
            Th::Black => Err(String::from("бесконечная рекурсия при вычислении")),
            Th::Susp(e, env) => {
                let r = self.eval(&e, &env);
                match r {
                    Ok(v) => {
                        *t.0.borrow_mut() = Th::Done(v.clone());
                        Ok(v)
                    }
                    Err(e) => {
                        // Ошибку не запоминаем: повторный запрос должен снова её показать, а не
                        // наткнуться на чёрную дыру и соврать про рекурсию.
                        *t.0.borrow_mut() = Th::Black;
                        Err(e)
                    }
                }
            }
        }
    }

    pub fn eval(&mut self, e: &Rc<Expr>, env: &Env) -> EResult<Value> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            return Err(String::from("слишком глубокая рекурсия"));
        }
        let r = self.eval_inner(e, env);
        self.depth -= 1;
        r
    }

    fn eval_inner(&mut self, e: &Rc<Expr>, env: &Env) -> EResult<Value> {
        match &**e {
            Expr::Int(v) => Ok(Value::Int(*v)),
            Expr::Float(v) => Ok(Value::Float(*v)),
            Expr::Path(p) => Ok(Value::Path(abs_path(p))),
            Expr::SPath(p) => Err(alloc::format!("путь <{}> искать негде: NIX_PATH у нас нет", p)),
            Expr::Str(parts) => self.eval_str(parts, env).map(Value::Str),
            Expr::Var(name) => {
                if let Some(t) = env.lexical(name) {
                    return self.force(&t);
                }
                for w in env.withs() {
                    let v = self.force(&w)?;
                    if let Value::Attrs(a) = v {
                        if let Some(t) = a.get(name) {
                            return self.force(t);
                        }
                    }
                }
                Err(alloc::format!("неизвестное имя '{}'", name))
            }
            Expr::List(items) => {
                let v: Vec<Thunk> =
                    items.iter().map(|it| Thunk::susp(it.clone(), env.clone())).collect();
                Ok(Value::List(Rc::new(v)))
            }
            Expr::Attrs { rec, binds } => self.eval_attrs(*rec, binds, env),
            Expr::Lambda { .. } => {
                Ok(Value::Lambda(Rc::new(Closure { lambda: e.clone(), env: env.clone() })))
            }
            Expr::Apply(f, a) => {
                let fv = self.eval(f, env)?;
                let at = Thunk::susp(a.clone(), env.clone());
                self.apply(fv, at)
            }
            Expr::Select { on, path, default } => {
                let mut cur = self.eval(on, env)?;
                for name in path {
                    let key = self.attr_key(name, env)?;
                    let next = match &cur {
                        Value::Attrs(a) => a.get(&key).cloned(),
                        _ => None,
                    };
                    match next {
                        Some(t) => cur = self.force(&t)?,
                        None => {
                            return match default {
                                Some(d) => self.eval(d, env),
                                None => Err(alloc::format!("нет атрибута '{}'", key)),
                            }
                        }
                    }
                }
                Ok(cur)
            }
            Expr::HasAttr { on, path } => {
                let mut cur = self.eval(on, env)?;
                for name in path {
                    let key = self.attr_key(name, env)?;
                    let next = match &cur {
                        Value::Attrs(a) => a.get(&key).cloned(),
                        _ => return Ok(Value::Bool(false)),
                    };
                    match next {
                        Some(t) => cur = self.force(&t)?,
                        None => return Ok(Value::Bool(false)),
                    }
                }
                Ok(Value::Bool(true))
            }
            Expr::Not(x) => {
                let v = self.eval(x, env)?;
                match v {
                    Value::Bool(b) => Ok(Value::Bool(!b)),
                    other => Err(alloc::format!("'!' применили к {}", other.type_name())),
                }
            }
            Expr::Neg(x) => match self.eval(x, env)? {
                Value::Int(i) => Ok(Value::Int(-i)),
                Value::Float(f) => Ok(Value::Float(-f)),
                other => Err(alloc::format!("минус применили к {}", other.type_name())),
            },
            Expr::If(c, t, f) => {
                let cv = self.eval(c, env)?;
                match cv {
                    Value::Bool(true) => self.eval(t, env),
                    Value::Bool(false) => self.eval(f, env),
                    other => Err(alloc::format!("условие — {}, а нужен bool", other.type_name())),
                }
            }
            Expr::Let { binds, body } => {
                let (env2, _) = self.rec_env(binds, env)?;
                self.eval(body, &env2)
            }
            Expr::With(e2, body) => {
                let t = Thunk::susp(e2.clone(), env.clone());
                self.eval(body, &env.with_scope(t))
            }
            Expr::Assert(c, body) => {
                let cv = self.eval(c, env)?;
                match cv {
                    Value::Bool(true) => self.eval(body, env),
                    _ => Err(String::from("assert не выполнен")),
                }
            }
            Expr::Bin(op, l, r) => self.binop(*op, l, r, env),
        }
    }

    // ── применение ─────────────────────────────────────────────────────────────
    pub fn apply(&mut self, f: Value, arg: Thunk) -> EResult<Value> {
        match f {
            Value::Prim(p) => {
                let mut args = (*p.args).clone();
                args.push(arg);
                if args.len() == p.arity {
                    (p.f)(self, &args)
                } else {
                    Ok(Value::Prim(Prim { args: Rc::new(args), ..p }))
                }
            }
            Value::Lambda(c) => {
                let Expr::Lambda { param, body } = &*c.lambda else {
                    return Err(String::from("не функция"));
                };
                let mut vars: BTreeMap<String, Thunk> = BTreeMap::new();
                match param {
                    Param::Ident(n) => {
                        vars.insert(n.clone(), arg);
                    }
                    Param::Pattern { fields, ellipsis, bind } => {
                        let v = self.force(&arg)?;
                        let Value::Attrs(a) = v else {
                            return Err(alloc::format!(
                                "функция ждёт множество, а дали {}",
                                v.type_name()
                            ));
                        };
                        if let Some(b) = bind {
                            vars.insert(b.clone(), arg.clone());
                        }
                        // Окружение образца РЕКУРСИВНОЕ: умолчание одного поля вправе смотреть
                        // на другое (`{ a, b ? a }`), и это сплошь и рядом в nixpkgs. Кадр
                        // поэтому создаётся пустым и заполняется в конце — та же петля, что у
                        // `rec`.
                        let cell: Rc<RefCell<Attrs>> = Rc::new(RefCell::new(Attrs::new()));
                        let env2 = c.env.with_cell(cell.clone());
                        for (name, default) in fields {
                            match a.get(name) {
                                Some(t) => {
                                    vars.insert(name.clone(), t.clone());
                                }
                                None => match default {
                                    Some(d) => {
                                        vars.insert(
                                            name.clone(),
                                            Thunk::susp(d.clone(), env2.clone()),
                                        );
                                    }
                                    None => {
                                        return Err(alloc::format!(
                                            "функции не хватает аргумента '{}'",
                                            name
                                        ))
                                    }
                                },
                            }
                        }
                        if !ellipsis {
                            for k in a.keys() {
                                if !fields.iter().any(|(n, _)| n == k) {
                                    return Err(alloc::format!("лишний аргумент '{}'", k));
                                }
                            }
                        }
                        *cell.borrow_mut() = vars;
                        return self.eval(body, &env2);
                    }
                }
                let env2 = c.env.with_vars(vars);
                self.eval(body, &env2)
            }
            other => Err(alloc::format!("применили не функцию, а {}", other.type_name())),
        }
    }

    // ── множества ──────────────────────────────────────────────────────────────
    fn eval_attrs(&mut self, rec: bool, binds: &[Bind], env: &Env) -> EResult<Value> {
        if rec {
            let (env2, cell) = self.rec_env(binds, env)?;
            let _ = env2;
            let m = cell.borrow().clone();
            return Ok(Value::Attrs(Rc::new(m)));
        }
        let mut out = Attrs::new();
        for b in binds {
            self.bind_into(&mut out, b, env, env)?;
        }
        Ok(Value::Attrs(Rc::new(out)))
    }

    /// Окружение, в котором связки видят друг друга (`rec { … }` и `let … in`).
    ///
    /// Держатель нужен потому, что отложка хранит КЛОН окружения, а окружение ещё не построено:
    /// сперва создаём пустое, потом кладём в него связки, и только затем — записываем результат
    /// туда, куда смотрят отложки. Без этой петли `rec { a = b; b = 1; }` не работал бы.
    fn rec_env(
        &mut self,
        binds: &[Bind],
        env: &Env,
    ) -> EResult<(Env, Rc<RefCell<BTreeMap<String, Thunk>>>)> {
        let cell: Rc<RefCell<Attrs>> = Rc::new(RefCell::new(Attrs::new()));
        let env2 = env.with_cell(cell.clone());
        let mut out = Attrs::new();
        for b in binds {
            // Имена — из ОБЪЕМЛЮЩЕГО окружения, значения — из своего. В `rec { ${k} = 1; }`
            // ключ `k` берётся снаружи: динамические имена в рекурсивную область не входят.
            self.bind_into(&mut out, b, env, &env2)?;
        }
        *cell.borrow_mut() = out;
        Ok((env2, cell))
    }

    /// Положить связку в множество. `venv` — окружение ЗНАЧЕНИЙ, `kenv` — окружение имён
    /// (у `rec` они разные: имя вычисляется снаружи, значение — внутри).
    fn bind_into(
        &mut self,
        out: &mut Attrs,
        b: &Bind,
        kenv: &Env,
        venv: &Env,
    ) -> EResult<()> {
        match b {
            Bind::Set { path, value } => {
                let keys = self.attr_keys(path, kenv)?;
                insert_path(out, &keys, Thunk::susp(value.clone(), venv.clone()))
            }
            Bind::Inherit { from, names } => {
                for n in names {
                    let key = self.attr_key(n, kenv)?;
                    let t = match from {
                        // `inherit (e) a;` — взять `a` из `e`, лениво.
                        Some(src) => {
                            let se = Rc::new(Expr::Select {
                                on: src.clone(),
                                path: alloc::vec![AttrName::Fixed(key.clone())],
                                default: None,
                            });
                            Thunk::susp(se, venv.clone())
                        }
                        // `inherit a;` — взять имя из ОБЪЕМЛЮЩЕГО окружения, а не из своего.
                        None => Thunk::susp(Rc::new(Expr::Var(key.clone())), kenv.clone()),
                    };
                    out.insert(key, t);
                }
                Ok(())
            }
        }
    }

    fn attr_keys(&mut self, path: &[AttrName], env: &Env) -> EResult<Vec<String>> {
        let mut out = Vec::with_capacity(path.len());
        for n in path {
            out.push(self.attr_key(n, env)?);
        }
        Ok(out)
    }

    fn attr_key(&mut self, n: &AttrName, env: &Env) -> EResult<String> {
        match n {
            AttrName::Fixed(s) => Ok(s.clone()),
            AttrName::Dyn(e) => {
                let v = self.eval(e, env)?;
                Ok(self.coerce_str(&v, false)?.s)
            }
        }
    }

    // ── строки ─────────────────────────────────────────────────────────────────
    fn eval_str(&mut self, parts: &[StrPart], env: &Env) -> EResult<NixStr> {
        let mut out = NixStr::default();
        for p in parts {
            match p {
                StrPart::Lit(s) => out.s.push_str(s),
                StrPart::Interp(e) => {
                    let v = self.eval(e, env)?;
                    let s = self.coerce_str(&v, false)?;
                    out.s.push_str(&s.s);
                    out.ctx.extend(s.ctx);
                }
            }
        }
        Ok(out)
    }

    /// Привести значение к строке. `more` — «широкое» приведение (`toString`): числа, `bool`,
    /// `null` и списки тоже становятся текстом. Внутри `"${…}"` его НЕТ намеренно: `"${1}"` в
    /// nix ошибка, и молча превращать число в текст значило бы разойтись с языком.
    pub fn coerce_str(&mut self, v: &Value, more: bool) -> EResult<NixStr> {
        match v {
            Value::Str(s) => Ok(s.clone()),
            Value::Path(p) => Ok(NixStr::plain(p.clone())),
            Value::Attrs(a) => {
                if let Some(t) = a.get("__toString") {
                    let f = self.force(t)?;
                    let r = self.apply(f, Thunk::done(v.clone()))?;
                    return self.coerce_str(&r, more);
                }
                if let Some(t) = a.get("outPath") {
                    let o = self.force(t)?;
                    return self.coerce_str(&o, more);
                }
                Err(String::from("множество без outPath строкой не станет"))
            }
            Value::Int(i) if more => Ok(NixStr::plain(i.to_string())),
            Value::Float(f) if more => Ok(NixStr::plain(fmt_float(*f))),
            Value::Bool(b) if more => Ok(NixStr::plain(if *b { "1" } else { "" })),
            Value::Null if more => Ok(NixStr::plain("")),
            Value::List(items) if more => {
                let items = items.clone();
                let mut out = NixStr::default();
                for (i, t) in items.iter().enumerate() {
                    if i > 0 {
                        out.s.push(' ');
                    }
                    let x = self.force(t)?;
                    let s = self.coerce_str(&x, more)?;
                    out.s.push_str(&s.s);
                    out.ctx.extend(s.ctx);
                }
                Ok(out)
            }
            other => Err(alloc::format!("{} строкой не станет", other.type_name())),
        }
    }

    // ── операторы ──────────────────────────────────────────────────────────────
    fn binop(&mut self, op: Op, l: &Rc<Expr>, r: &Rc<Expr>, env: &Env) -> EResult<Value> {
        // Ленивые по правому краю — сперва они, иначе `false && throw "x"` взорвался бы.
        match op {
            Op::And => {
                return match self.eval(l, env)? {
                    Value::Bool(false) => Ok(Value::Bool(false)),
                    Value::Bool(true) => self.as_bool(r, env),
                    o => Err(alloc::format!("'&&' применили к {}", o.type_name())),
                }
            }
            Op::Or => {
                return match self.eval(l, env)? {
                    Value::Bool(true) => Ok(Value::Bool(true)),
                    Value::Bool(false) => self.as_bool(r, env),
                    o => Err(alloc::format!("'||' применили к {}", o.type_name())),
                }
            }
            Op::Impl => {
                return match self.eval(l, env)? {
                    Value::Bool(false) => Ok(Value::Bool(true)),
                    Value::Bool(true) => self.as_bool(r, env),
                    o => Err(alloc::format!("'->' применили к {}", o.type_name())),
                }
            }
            _ => {}
        }
        let a = self.eval(l, env)?;
        let b = self.eval(r, env)?;
        match op {
            Op::Add => self.add(a, b),
            Op::Sub => num2(a, b, |x, y| x - y, |x, y| x - y),
            Op::Mul => num2(a, b, |x, y| x * y, |x, y| x * y),
            Op::Div => match (&a, &b) {
                (_, Value::Int(0)) => Err(String::from("деление на ноль")),
                _ => num2(a, b, |x, y| x / y, |x, y| x / y),
            },
            Op::Eq => Ok(Value::Bool(self.equal(&a, &b)?)),
            Op::Ne => Ok(Value::Bool(!self.equal(&a, &b)?)),
            Op::Lt => Ok(Value::Bool(self.less(&a, &b)?)),
            Op::Gt => Ok(Value::Bool(self.less(&b, &a)?)),
            Op::Le => Ok(Value::Bool(!self.less(&b, &a)?)),
            Op::Ge => Ok(Value::Bool(!self.less(&a, &b)?)),
            Op::Update => match (a, b) {
                (Value::Attrs(x), Value::Attrs(y)) => {
                    let mut m = (*x).clone();
                    for (k, v) in y.iter() {
                        m.insert(k.clone(), v.clone());
                    }
                    Ok(Value::Attrs(Rc::new(m)))
                }
                (x, y) => Err(alloc::format!(
                    "'//' хочет два множества, а дали {} и {}",
                    x.type_name(),
                    y.type_name()
                )),
            },
            Op::Concat => match (a, b) {
                (Value::List(x), Value::List(y)) => {
                    let mut v = (*x).clone();
                    v.extend(y.iter().cloned());
                    Ok(Value::List(Rc::new(v)))
                }
                (x, y) => Err(alloc::format!(
                    "'++' хочет два списка, а дали {} и {}",
                    x.type_name(),
                    y.type_name()
                )),
            },
            Op::And | Op::Or | Op::Impl => unreachable!("разобраны выше"),
        }
    }

    fn as_bool(&mut self, e: &Rc<Expr>, env: &Env) -> EResult<Value> {
        match self.eval(e, env)? {
            v @ Value::Bool(_) => Ok(v),
            o => Err(alloc::format!("нужен bool, а там {}", o.type_name())),
        }
    }

    fn add(&mut self, a: Value, b: Value) -> EResult<Value> {
        match (&a, &b) {
            (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
                num2(a, b, |x, y| x + y, |x, y| x + y)
            }
            // Путь слева даёт ПУТЬ: `/a + "/b"` — это `/a/b`, а не строка.
            (Value::Path(p), _) => {
                let s = self.coerce_str(&b, false)?;
                Ok(Value::Path(join_path(p, &s.s)))
            }
            _ => {
                let x = self.coerce_str(&a, false)?;
                let y = self.coerce_str(&b, false)?;
                let mut out = NixStr { s: x.s, ctx: x.ctx };
                out.s.push_str(&y.s);
                out.ctx.extend(y.ctx);
                Ok(Value::Str(out))
            }
        }
    }

    pub fn equal(&mut self, a: &Value, b: &Value) -> EResult<bool> {
        Ok(match (a, b) {
            (Value::Int(x), Value::Int(y)) => x == y,
            (Value::Float(x), Value::Float(y)) => x == y,
            (Value::Int(x), Value::Float(y)) | (Value::Float(y), Value::Int(x)) => *x as f64 == *y,
            (Value::Bool(x), Value::Bool(y)) => x == y,
            (Value::Null, Value::Null) => true,
            // Контекст в сравнении не участвует — сравниваются ТЕКСТЫ, как в nix.
            (Value::Str(x), Value::Str(y)) => x.s == y.s,
            (Value::Path(x), Value::Path(y)) => x == y,
            (Value::List(x), Value::List(y)) => {
                if x.len() != y.len() {
                    return Ok(false);
                }
                let (x, y) = (x.clone(), y.clone());
                for i in 0..x.len() {
                    let (u, v) = (self.force(&x[i])?, self.force(&y[i])?);
                    if !self.equal(&u, &v)? {
                        return Ok(false);
                    }
                }
                true
            }
            (Value::Attrs(x), Value::Attrs(y)) => {
                if x.len() != y.len() {
                    return Ok(false);
                }
                let (x, y) = (x.clone(), y.clone());
                for (k, tu) in x.iter() {
                    let Some(tv) = y.get(k) else { return Ok(false) };
                    let (u, v) = (self.force(tu)?, self.force(tv)?);
                    if !self.equal(&u, &v)? {
                        return Ok(false);
                    }
                }
                true
            }
            // Функции неравны даже сами себе — так в nix, и это не небрежность: равенство
            // функций неразрешимо, а сравнение по адресу зависело бы от того, сколько раз
            // вычислитель их скопировал.
            (Value::Lambda(_) | Value::Prim(_), _) => false,
            (_, Value::Lambda(_) | Value::Prim(_)) => false,
            _ => false,
        })
    }

    pub fn less(&mut self, a: &Value, b: &Value) -> EResult<bool> {
        Ok(match (a, b) {
            (Value::Int(x), Value::Int(y)) => x < y,
            (Value::Float(x), Value::Float(y)) => x < y,
            (Value::Int(x), Value::Float(y)) => (*x as f64) < *y,
            (Value::Float(x), Value::Int(y)) => *x < (*y as f64),
            (Value::Str(x), Value::Str(y)) => x.s < y.s,
            (Value::Path(x), Value::Path(y)) => x < y,
            (Value::List(x), Value::List(y)) => {
                let (x, y) = (x.clone(), y.clone());
                for i in 0..x.len().min(y.len()) {
                    let (u, v) = (self.force(&x[i])?, self.force(&y[i])?);
                    if self.less(&u, &v)? {
                        return Ok(true);
                    }
                    if self.less(&v, &u)? {
                        return Ok(false);
                    }
                }
                x.len() < y.len()
            }
            (x, y) => {
                return Err(alloc::format!(
                    "{} и {} несравнимы",
                    x.type_name(),
                    y.type_name()
                ))
            }
        })
    }

    /// Довычислить значение ЦЕЛИКОМ (`--strict`): списки и множества до дна.
    pub fn deep(&mut self, v: &Value) -> EResult<()> {
        match v {
            Value::List(items) => {
                let items = items.clone();
                for t in items.iter() {
                    let x = self.force(t)?;
                    self.deep(&x)?;
                }
            }
            Value::Attrs(a) => {
                let a = a.clone();
                for t in a.values() {
                    let x = self.force(t)?;
                    self.deep(&x)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn num2(
    a: Value,
    b: Value,
    fi: fn(i64, i64) -> i64,
    ff: fn(f64, f64) -> f64,
) -> EResult<Value> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(Value::Int(fi(x, y))),
        (Value::Float(x), Value::Float(y)) => Ok(Value::Float(ff(x, y))),
        (Value::Int(x), Value::Float(y)) => Ok(Value::Float(ff(x as f64, y))),
        (Value::Float(x), Value::Int(y)) => Ok(Value::Float(ff(x, y as f64))),
        (x, y) => Err(alloc::format!(
            "числа ждали, а дали {} и {}",
            x.type_name(),
            y.type_name()
        )),
    }
}

/// Вложить значение по пути имён: `a.b.c = v` создаёт промежуточные множества.
fn insert_path(out: &mut Attrs, keys: &[String], v: Thunk) -> EResult<()> {
    let (first, rest) = keys.split_first().expect("путь атрибута не бывает пустым");
    if rest.is_empty() {
        if out.contains_key(first) {
            return Err(alloc::format!("атрибут '{}' задан дважды", first));
        }
        out.insert(first.clone(), v);
        return Ok(());
    }
    // Промежуточное множество могли уже создать соседней связкой (`a.b = 1; a.c = 2;`).
    let mut inner: Attrs = match out.get(first) {
        Some(t) => match &*t.0.borrow() {
            Th::Done(Value::Attrs(a)) => (**a).clone(),
            _ => return Err(alloc::format!("атрибут '{}' задан дважды", first)),
        },
        None => Attrs::new(),
    };
    insert_path(&mut inner, rest, v)?;
    out.insert(first.clone(), Thunk::done(Value::Attrs(Rc::new(inner))));
    Ok(())
}

fn join_path(base: &str, rest: &str) -> String {
    let mut s = String::from(base);
    if !rest.starts_with('/') && !s.ends_with('/') {
        s.push('/');
    }
    s.push_str(rest);
    normalize_path(&s)
}

/// Путь в nix всегда абсолютный. Своего текущего каталога у нас нет, поэтому относительный
/// считается от корня — и это честнее, чем выдумать каталог, которого у процесса не бывает.
fn abs_path(p: &str) -> String {
    if p.starts_with('/') {
        return normalize_path(p);
    }
    let rest = p.strip_prefix("./").unwrap_or(p);
    normalize_path(&alloc::format!("/{}", rest))
}

fn normalize_path(p: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for c in p.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    let mut s = String::new();
    for c in parts {
        s.push('/');
        s.push_str(c);
    }
    if s.is_empty() {
        s.push('/');
    }
    s
}

/// Печать числа с плавающей точкой так же, как это делает nix.
pub fn fmt_float(f: f64) -> String {
    let s = alloc::format!("{}", f);
    s
}
