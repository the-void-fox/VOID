//! Вычислитель: метациркулярный eval + спец-формы + встроенные функции + `import` модулей.
//!
//! Программа — последовательность форм; `Interp::eval_program` вычисляет их по порядку в свежем
//! корневом окружении (со встроенными), результат — значение ПОСЛЕДНЕЙ формы (так `(define …)
//! (system …)` отдаёт систему). Спец-формы: `quote if define lambda let cond begin and or import`;
//! всё прочее — применение. Истинность — по Scheme (ложь только `#f`).
//!
//! `import` (M1b) читает и вычисляет ДРУГОЙ `.vv` в СВЕЖЕМ окружении и возвращает его значение —
//! I/O инъектируется через [`ModuleLoader`] (крейт чистый: бинарь даёт загрузчик поверх posixfs,
//! тесты — in-memory). Модуль возвращает свой ВКЛАД (обычно список записей), а `default.vv` их
//! СЛИВАЕТ через `append`. Кэш по имени модуля + стек загрузки для детекта циклов.

use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::reader::read_all;
use crate::value::{BuiltinFn, Closure, Env, EvalError, Value};

/// Источник исходников модулей для `import`. Реализуется потребителем (бинарь `vvsh` — поверх
/// posixfs; тесты — из карты в памяти). Крейт остаётся без I/O.
pub trait ModuleLoader {
    /// Вернуть исходник модуля по имени (как записано в `(import "имя")`), или текст ошибки.
    fn load(&self, name: &str) -> Result<String, String>;
}

/// Загрузчик-заглушка: любой `import` — ошибка. Для чисто-вычислительных вызовов/тестов.
pub struct NoLoader;

impl ModuleLoader for NoLoader {
    fn load(&self, name: &str) -> Result<String, String> {
        Err(alloc::format!(
            "import '{}' недоступен: загрузчик модулей не задан",
            name
        ))
    }
}

/// Вычислитель с контекстом: загрузчик модулей + кэш импортов + стек загрузки (детект циклов).
pub struct Interp<'a> {
    loader: &'a dyn ModuleLoader,
    cache: RefCell<Vec<(String, Value)>>,
    loading: RefCell<Vec<String>>,
}

impl<'a> Interp<'a> {
    pub fn new(loader: &'a dyn ModuleLoader) -> Self {
        Interp {
            loader,
            cache: RefCell::new(Vec::new()),
            loading: RefCell::new(Vec::new()),
        }
    }

    /// Вычислить программу (последовательность форм) в свежем корневом окружении.
    pub fn eval_program(&self, forms: &[Value]) -> Result<Value, EvalError> {
        let env = root_env();
        let mut last = Value::nil();
        for f in forms {
            last = self.eval(f, &env)?;
        }
        Ok(last)
    }

    /// Вычислить одну форму.
    pub fn eval(&self, expr: &Value, env: &Env) -> Result<Value, EvalError> {
        match expr {
            // Самовычислимые.
            Value::Bool(_)
            | Value::Int(_)
            | Value::Str(_)
            | Value::Builtin(..)
            | Value::Closure(_) => Ok(expr.clone()),
            Value::Sym(s) => env
                .lookup(s)
                .ok_or_else(|| EvalError::new(alloc::format!("неизвестный символ '{}'", s))),
            Value::List(items) => {
                if items.is_empty() {
                    return Ok(expr.clone()); // () → пустой список
                }
                if let Value::Sym(head) = &items[0] {
                    match &**head {
                        "quote" => return sf_quote(items),
                        "if" => return self.sf_if(items, env),
                        "define" => return self.sf_define(items, env),
                        "lambda" => return self.sf_lambda(items, env),
                        "let" => return self.sf_let(items, env),
                        "cond" => return self.sf_cond(items, env),
                        "begin" => return self.eval_body(&items[1..], env),
                        "and" => return self.sf_and(items, env),
                        "or" => return self.sf_or(items, env),
                        "|" => return self.sf_pipe(items, env),
                        "map" => return self.sf_map(items, env),
                        "filter" => return self.sf_filter(items, env),
                        "import" => return self.sf_import(items, env),
                        _ => {}
                    }
                }
                // Применение: вычислить голову и аргументы, применить.
                let func = self.eval(&items[0], env)?;
                let mut args = Vec::with_capacity(items.len() - 1);
                for a in &items[1..] {
                    args.push(self.eval(a, env)?);
                }
                self.apply(&func, &args)
            }
        }
    }

    fn apply(&self, func: &Value, args: &[Value]) -> Result<Value, EvalError> {
        match func {
            Value::Builtin(_, f) => f(args),
            Value::Closure(c) => {
                if args.len() != c.params.len() {
                    return Err(EvalError::new(alloc::format!(
                        "функция: нужно {} арг., дано {}",
                        c.params.len(),
                        args.len()
                    )));
                }
                let call_env = Env::child(&c.env);
                for (p, a) in c.params.iter().zip(args) {
                    call_env.define(p.clone(), a.clone());
                }
                self.eval_body(&c.body, &call_env)
            }
            other => Err(EvalError::new(alloc::format!(
                "нельзя вызвать {}",
                other.type_name()
            ))),
        }
    }

    /// Вычислить последовательность форм, вернуть значение последней (тело функции/`begin`/`let`).
    fn eval_body(&self, body: &[Value], env: &Env) -> Result<Value, EvalError> {
        let mut last = Value::nil();
        for f in body {
            last = self.eval(f, env)?;
        }
        Ok(last)
    }

    // ── спец-формы ──────────────────────────────────────────────────────────

    fn sf_if(&self, items: &[Value], env: &Env) -> Result<Value, EvalError> {
        if items.len() != 3 && items.len() != 4 {
            return Err(EvalError::new("if: (if условие тогда [иначе])"));
        }
        if self.eval(&items[1], env)?.truthy() {
            self.eval(&items[2], env)
        } else if items.len() == 4 {
            self.eval(&items[3], env)
        } else {
            Ok(Value::nil())
        }
    }

    fn sf_define(&self, items: &[Value], env: &Env) -> Result<Value, EvalError> {
        if items.len() < 3 {
            return Err(EvalError::new("define: (define имя выражение)"));
        }
        match &items[1] {
            // (define имя выражение)
            Value::Sym(name) => {
                if items.len() != 3 {
                    return Err(EvalError::new("define: (define имя выражение)"));
                }
                let v = self.eval(&items[2], env)?;
                env.define(name.clone(), v.clone());
                Ok(v)
            }
            // Сахар: (define (f a b) тело…) == (define f (lambda (a b) тело…))
            Value::List(sig) if !sig.is_empty() => {
                let fname = match &sig[0] {
                    Value::Sym(s) => s.clone(),
                    _ => return Err(EvalError::new("define: имя функции — символ")),
                };
                let params = parse_params(&sig[1..])?;
                let clos = Value::Closure(Rc::new(Closure {
                    params,
                    body: items[2..].to_vec(),
                    env: env.clone(),
                }));
                env.define(fname, clos.clone());
                Ok(clos)
            }
            _ => Err(EvalError::new("define: цель — символ или (имя параметры…)")),
        }
    }

    fn sf_lambda(&self, items: &[Value], env: &Env) -> Result<Value, EvalError> {
        if items.len() < 3 {
            return Err(EvalError::new("lambda: (lambda (параметры…) тело…)"));
        }
        let params = match &items[1] {
            Value::List(p) => parse_params(p)?,
            _ => return Err(EvalError::new("lambda: параметры — список символов")),
        };
        Ok(Value::Closure(Rc::new(Closure {
            params,
            body: items[2..].to_vec(),
            env: env.clone(),
        })))
    }

    fn sf_let(&self, items: &[Value], env: &Env) -> Result<Value, EvalError> {
        // (let ((имя знач)…) тело…): значения — в ВНЕШНЕМ окружении, тело — в дочернем.
        if items.len() < 3 {
            return Err(EvalError::new("let: (let ((имя знач)…) тело…)"));
        }
        let binds = match &items[1] {
            Value::List(b) => b,
            _ => return Err(EvalError::new("let: список привязок")),
        };
        // Внешний список привязок тоже приходит как `list(...)` (Веха 102: голых пар в новом
        // синтаксисе не написать), поэтому головной символ `list` пропускаем.
        let binds: &[Value] = match binds.first() {
            Some(Value::Sym(s)) if &**s == "list" => &binds[1..],
            _ => binds,
        };
        let child = Env::child(env);
        for b in binds.iter() {
            // Веха 102 — привязка пишется как `[имя, значение]`, то есть приезжает формой
            // `(list имя значение)`: пары без головы новый ридер породить не может.
            let pair: &[Value] = match b {
                Value::List(p) if p.len() == 2 => p,
                Value::List(p) if p.len() == 3
                    && matches!(p.first(), Some(Value::Sym(s)) if &**s == "list") => &p[1..],
                _ => return Err(EvalError::new("let: привязка — [имя, значение]")),
            };
            let name = match &pair[0] {
                Value::Sym(s) => s.clone(),
                _ => return Err(EvalError::new("let: имя привязки — символ")),
            };
            let v = self.eval(&pair[1], env)?;
            child.define(name, v);
        }
        self.eval_body(&items[2..], &child)
    }

    fn sf_cond(&self, items: &[Value], env: &Env) -> Result<Value, EvalError> {
        for clause in &items[1..] {
            let c = match clause {
                Value::List(c) if !c.is_empty() => c,
                _ => return Err(EvalError::new("cond: ветвь — [тест, выражение…]")),
            };
            // Веха 102 — ветвь пишется как `[тест, выражение]` и приезжает формой
            // `(list тест выражение)`: без этого головной `list` сам сходил бы за тест
            // (символ встроенной функции истинен — и срабатывала ПЕРВАЯ же ветвь).
            let c: &[Value] = match c.first() {
                Some(Value::Sym(s)) if &**s == "list" && c.len() > 1 => &c[1..],
                _ => c,
            };
            let is_else = matches!(&c[0], Value::Sym(s) if &**s == "else");
            if is_else || self.eval(&c[0], env)?.truthy() {
                return self.eval_body(&c[1..], env);
            }
        }
        Ok(Value::nil())
    }

    fn sf_and(&self, items: &[Value], env: &Env) -> Result<Value, EvalError> {
        let mut last = Value::Bool(true);
        for e in &items[1..] {
            last = self.eval(e, env)?;
            if !last.truthy() {
                return Ok(Value::Bool(false));
            }
        }
        Ok(last)
    }

    fn sf_or(&self, items: &[Value], env: &Env) -> Result<Value, EvalError> {
        for e in &items[1..] {
            let v = self.eval(e, env)?;
            if v.truthy() {
                return Ok(v);
            }
        }
        Ok(Value::Bool(false))
    }

    /// `(| seed этап…)` — конвейер (thread-last): значение течёт слева направо, вставляясь
    /// ПОСЛЕДНИМ аргументом каждого этапа. `(| (ls) (grep "x") count)` = `(count (grep "x" (ls)))`.
    /// Этап-список `(f a…)` → `(f a… acc)`; этап-символ/выражение `f` → `(f acc)`. Родной конвейер
    /// на ЗНАЧЕНИЯХ (в VOID нет захвата stdout — течёт Value, а не байты). Значение «вжимаем» как
    /// `(quote acc)` и вычисляем ВСЮ форму — так этапом годятся и функции, и спец-формы (`map`/`filter`).
    fn sf_pipe(&self, items: &[Value], env: &Env) -> Result<Value, EvalError> {
        if items.len() < 2 {
            return Err(EvalError::new("|: (| значение этап…)"));
        }
        let mut acc = self.eval(&items[1], env)?;
        for stage in &items[2..] {
            let quoted = Value::list(vec![Value::sym("quote"), acc]);
            let form = match stage {
                Value::List(call) if !call.is_empty() => {
                    let mut v = call.as_ref().clone();
                    v.push(quoted);
                    Value::list(v)
                }
                _ => Value::list(vec![stage.clone(), quoted]),
            };
            acc = self.eval(&form, env)?;
        }
        Ok(acc)
    }

    /// `(map f список)` — применить `f` к каждому элементу, собрать список результатов.
    fn sf_map(&self, items: &[Value], env: &Env) -> Result<Value, EvalError> {
        if items.len() != 3 {
            return Err(EvalError::new("map: (map функция список)"));
        }
        let f = self.eval(&items[1], env)?;
        let lst = self.eval(&items[2], env)?;
        let src = match &lst {
            Value::List(i) => i,
            _ => return Err(EvalError::new("map: второй аргумент — список")),
        };
        let mut out = Vec::with_capacity(src.len());
        for e in src.iter() {
            out.push(self.apply(&f, &[e.clone()])?);
        }
        Ok(Value::list(out))
    }

    /// `(filter предикат список)` — оставить элементы, на которых предикат истинен.
    fn sf_filter(&self, items: &[Value], env: &Env) -> Result<Value, EvalError> {
        if items.len() != 3 {
            return Err(EvalError::new("filter: (filter предикат список)"));
        }
        let f = self.eval(&items[1], env)?;
        let lst = self.eval(&items[2], env)?;
        let src = match &lst {
            Value::List(i) => i,
            _ => return Err(EvalError::new("filter: второй аргумент — список")),
        };
        let mut out = Vec::new();
        for e in src.iter() {
            if self.apply(&f, &[e.clone()])?.truthy() {
                out.push(e.clone());
            }
        }
        Ok(Value::list(out))
    }

    /// `(import "имя")` — прочитать и вычислить модуль в СВЕЖЕМ окружении, вернуть его значение.
    /// Кэш по имени (грузим раз), стек загрузки → детект циклов. Аргумент вычисляется (обычно
    /// строковый литерал, но может быть выражением).
    fn sf_import(&self, items: &[Value], env: &Env) -> Result<Value, EvalError> {
        if items.len() != 2 {
            return Err(EvalError::new("import: (import \"имя\")"));
        }
        let name = match self.eval(&items[1], env)? {
            Value::Str(s) => s,
            other => {
                return Err(EvalError::new(alloc::format!(
                    "import: имя модуля — строка, дано {}",
                    other.type_name()
                )))
            }
        };
        // Кэш: модуль уже вычислен?
        if let Some(v) = self
            .cache
            .borrow()
            .iter()
            .find(|(n, _)| n.as_str() == &*name)
            .map(|(_, v)| v.clone())
        {
            return Ok(v);
        }
        // Цикл?
        if self.loading.borrow().iter().any(|n| n.as_str() == &*name) {
            return Err(EvalError::new(alloc::format!(
                "import: цикл импорта модуля '{}'",
                name
            )));
        }
        let src = self.loader.load(&name).map_err(EvalError::new)?;
        self.loading.borrow_mut().push(String::from(&*name));
        let result = (|| {
            let forms = read_all(&src).map_err(|e| EvalError::new(e.0))?;
            self.eval_program(&forms)
        })();
        self.loading.borrow_mut().pop();
        let val = result?;
        self.cache
            .borrow_mut()
            .push((String::from(&*name), val.clone()));
        Ok(val)
    }
}

fn sf_quote(items: &[Value]) -> Result<Value, EvalError> {
    if items.len() != 2 {
        return Err(EvalError::new("quote: нужен ровно 1 аргумент"));
    }
    Ok(items[1].clone())
}

fn parse_params(ps: &[Value]) -> Result<Vec<Rc<str>>, EvalError> {
    let mut out = Vec::with_capacity(ps.len());
    for p in ps {
        match p {
            Value::Sym(s) => out.push(s.clone()),
            _ => return Err(EvalError::new("параметр — символ")),
        }
    }
    Ok(out)
}

/// Корневое окружение со всеми встроенными функциями.
pub fn root_env() -> Env {
    let env = Env::root();
    for (name, f) in BUILTINS {
        env.define(Rc::from(*name), Value::Builtin(name, *f));
    }
    env
}

/// Вычислить программу без загрузчика модулей (любой `import` — ошибка). Для тестов/простых вызовов.
pub fn eval_program(forms: &[Value]) -> Result<Value, EvalError> {
    Interp::new(&NoLoader).eval_program(forms)
}

// ── встроенные функции ──────────────────────────────────────────────────────

const BUILTINS: &[(&str, BuiltinFn)] = &[
    ("list", b_list),
    ("append", b_append),
    ("cons", b_cons),
    ("car", b_car),
    ("cdr", b_cdr),
    ("length", b_length),
    ("count", b_length), // алиас (шелл-дружелюбно: `(| … count)`)
    ("null?", b_null),
    ("not", b_not),
    ("=", b_eq),
    ("+", b_add),
    ("-", b_sub),
    ("*", b_mul),
    ("service", b_service),
    ("shell", b_shell),
    ("terminal", b_terminal),
    ("bind", b_bind),
    ("packages", b_packages),
    ("system", b_system),
];

fn b_list(args: &[Value]) -> Result<Value, EvalError> {
    Ok(Value::list(args.to_vec()))
}

fn b_append(args: &[Value]) -> Result<Value, EvalError> {
    let mut out = Vec::new();
    for a in args {
        match a {
            Value::List(items) => out.extend(items.iter().cloned()),
            _ => {
                return Err(EvalError::new(alloc::format!(
                    "append: аргумент не список ({})",
                    a.type_name()
                )))
            }
        }
    }
    Ok(Value::list(out))
}

fn b_cons(args: &[Value]) -> Result<Value, EvalError> {
    if args.len() != 2 {
        return Err(EvalError::new("cons: нужно 2 аргумента"));
    }
    let tail = match &args[1] {
        Value::List(items) => items,
        _ => return Err(EvalError::new("cons: второй аргумент — список")),
    };
    let mut out = Vec::with_capacity(tail.len() + 1);
    out.push(args[0].clone());
    out.extend(tail.iter().cloned());
    Ok(Value::list(out))
}

fn b_car(args: &[Value]) -> Result<Value, EvalError> {
    match args.first() {
        Some(Value::List(items)) if !items.is_empty() => Ok(items[0].clone()),
        _ => Err(EvalError::new("car: нужен непустой список")),
    }
}

fn b_cdr(args: &[Value]) -> Result<Value, EvalError> {
    match args.first() {
        Some(Value::List(items)) if !items.is_empty() => Ok(Value::list(items[1..].to_vec())),
        _ => Err(EvalError::new("cdr: нужен непустой список")),
    }
}

fn b_null(args: &[Value]) -> Result<Value, EvalError> {
    Ok(Value::Bool(matches!(args.first(), Some(Value::List(i)) if i.is_empty())))
}

fn b_length(args: &[Value]) -> Result<Value, EvalError> {
    match args.first() {
        Some(Value::List(items)) => Ok(Value::Int(items.len() as i64)),
        _ => Err(EvalError::new("length: нужен список")),
    }
}

fn b_not(args: &[Value]) -> Result<Value, EvalError> {
    Ok(Value::Bool(!args.first().map(Value::truthy).unwrap_or(false)))
}

fn b_eq(args: &[Value]) -> Result<Value, EvalError> {
    match args.first() {
        None => Ok(Value::Bool(true)),
        Some(first) => Ok(Value::Bool(args[1..].iter().all(|a| a == first))),
    }
}

fn as_int(v: &Value) -> Result<i64, EvalError> {
    match v {
        Value::Int(n) => Ok(*n),
        _ => Err(EvalError::new(alloc::format!(
            "нужно число, дано {}",
            v.type_name()
        ))),
    }
}

fn b_add(args: &[Value]) -> Result<Value, EvalError> {
    let mut acc = 0i64;
    for a in args {
        acc = acc.wrapping_add(as_int(a)?);
    }
    Ok(Value::Int(acc))
}

fn b_mul(args: &[Value]) -> Result<Value, EvalError> {
    let mut acc = 1i64;
    for a in args {
        acc = acc.wrapping_mul(as_int(a)?);
    }
    Ok(Value::Int(acc))
}

fn b_sub(args: &[Value]) -> Result<Value, EvalError> {
    if args.is_empty() {
        return Err(EvalError::new("-: нужен минимум 1 аргумент"));
    }
    let mut acc = as_int(&args[0])?;
    if args.len() == 1 {
        return Ok(Value::Int(-acc));
    }
    for a in &args[1..] {
        acc = acc.wrapping_sub(as_int(a)?);
    }
    Ok(Value::Int(acc))
}

/// `(service имя право…)` / `(shell имя право…)` → запись `(kind имя право…)`. Права — строки;
/// аргумент-СПИСОК строк «вливается» (для `(append …)`/`(if … (list …) (list))` из модулей).
/// Число тоже принимается и становится строкой: в нормализованном конфиге всё равно ТЕКСТ, а
/// писать `(terminal "font-size" 18)` естественнее, чем `"18"`.
fn build_entry(kind: &'static str, args: &[Value]) -> Result<Value, EvalError> {
    let name = match args.first() {
        Some(Value::Str(s)) => s.clone(),
        Some(_) => return Err(EvalError::new(alloc::format!("{}: имя — строка", kind))),
        None => return Err(EvalError::new(alloc::format!("{}: нужно имя", kind))),
    };
    let mut out = vec![Value::sym(kind), Value::Str(name)];
    for cap in &args[1..] {
        match cap {
            Value::Str(_) => out.push(cap.clone()),
            Value::Int(n) => out.push(Value::str(&alloc::format!("{}", n))),
            Value::List(items) => {
                for it in items.iter() {
                    match it {
                        Value::Str(_) => out.push(it.clone()),
                        Value::Int(n) => out.push(Value::str(&alloc::format!("{}", n))),
                        _ => return Err(EvalError::new(alloc::format!("{}: право — строка", kind))),
                    }
                }
            }
            _ => {
                return Err(EvalError::new(alloc::format!(
                    "{}: право — строка или список строк",
                    kind
                )))
            }
        }
    }
    Ok(Value::list(out))
}

fn b_service(args: &[Value]) -> Result<Value, EvalError> {
    build_entry("service", args)
}

fn b_shell(args: &[Value]) -> Result<Value, EvalError> {
    build_entry("shell", args)
}

/// `(terminal ключ значение)` — настройка терминала (`term`): `font-size`, `shell`, `shell-args`.
/// Запись адресована НЕ ядру, а программе; ядро такие строки пропускает (см. `normalize_config`).
fn b_terminal(args: &[Value]) -> Result<Value, EvalError> {
    build_entry("terminal", args)
}

/// `(bind режим клавиша действие)` — клавиша терминала: `(bind "pane" "|" "split-v")`.
/// Та же таблица биндингов, что у ereb на Linux, только записанная на языке конфига VOID.
fn b_bind(args: &[Value]) -> Result<Value, EvalError> {
    build_entry("bind", args)
}

/// `(packages имя…)` — пакеты, которые система обязана иметь (Веха 112). Читатель этой строки —
/// не ядро и не терминал, а `pkg sync`: он резолвит имена в пути store и собирает поколение
/// профиля, привязанное к поколению СИСТЕМЫ.
///
/// Своя сборка вместо [`build_entry`] нужна ради одного: у `packages` нет «имени и прав», есть
/// однородный СПИСОК. Поэтому первый аргумент такой же, как остальные, и список строк вливается
/// на любом месте — `packages(base, "jq")` пишется естественно, а через `build_entry` первым
/// аргументом обязана была бы стоять строка.
fn b_packages(args: &[Value]) -> Result<Value, EvalError> {
    let mut out = vec![Value::sym("packages")];
    for a in args {
        match a {
            Value::Str(_) => out.push(a.clone()),
            Value::List(items) => {
                for it in items.iter() {
                    match it {
                        Value::Str(_) => out.push(it.clone()),
                        _ => return Err(EvalError::new("packages: имя пакета — строка")),
                    }
                }
            }
            _ => return Err(EvalError::new("packages: имя пакета — строка или список строк")),
        }
    }
    // Пустой `packages()` — не «ни одного пакета», а почти наверняка опечатка: «ни одного»
    // записывается отсутствием записи или `[]`, как и у всех прочих модулей конфига.
    if out.len() == 1 {
        return Err(EvalError::new("packages: нужно хотя бы одно имя (пусто — просто не пиши запись)"));
    }
    Ok(Value::list(out))
}

/// `(system запись…|список-записей…)` → `(#system запись…)`: верхняя форма конфига. Принимает и
/// отдельные записи, и списки записей (от `(append …)`) — уплощает.
fn b_system(args: &[Value]) -> Result<Value, EvalError> {
    let mut out = vec![Value::sym("#system")];
    for a in args {
        match a {
            Value::List(items) if is_entry(items) => out.push(a.clone()),
            Value::List(items) => {
                for it in items.iter() {
                    match it {
                        Value::List(inner) if is_entry(inner) => out.push(it.clone()),
                        _ => {
                            return Err(EvalError::new(
                                "system: ожидались записи service/shell/terminal/bind/packages",
                            ))
                        }
                    }
                }
            }
            _ => return Err(EvalError::new("system: аргумент — запись или список записей")),
        }
    }
    Ok(Value::list(out))
}

/// Виды записей конфига. `service`/`shell` читает ЯДРО, `terminal`/`bind` — терминал,
/// `packages` — `pkg sync`: конфиг поколения один, читателей несколько, и каждый берёт свои
/// строки.
fn is_entry(items: &[Value]) -> bool {
    matches!(items.first(), Some(Value::Sym(s))
        if matches!(&**s, "service" | "shell" | "terminal" | "bind" | "packages"))
}
