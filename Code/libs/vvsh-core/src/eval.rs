//! Вычислитель: метациркулярный eval + спец-формы + встроенные функции.
//!
//! Программа — последовательность форм; `eval_program` вычисляет их по порядку в корневом
//! окружении (со встроенными), результат — значение ПОСЛЕДНЕЙ формы (так `(define …) (system …)`
//! отдаёт систему). Спец-формы: `quote if define lambda let cond begin and or`; всё прочее —
//! применение. Истинность — по Scheme (ложь только `#f`).

use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;

use crate::value::{BuiltinFn, Closure, Env, EvalError, Value};

/// Вычислить программу (последовательность форм) в свежем корневом окружении.
pub fn eval_program(forms: &[Value]) -> Result<Value, EvalError> {
    let env = root_env();
    let mut last = Value::nil();
    for f in forms {
        last = eval(f, &env)?;
    }
    Ok(last)
}

/// Корневое окружение со всеми встроенными функциями.
pub fn root_env() -> Env {
    let env = Env::root();
    for (name, f) in BUILTINS {
        env.define(Rc::from(*name), Value::Builtin(name, *f));
    }
    env
}

/// Вычислить одну форму.
pub fn eval(expr: &Value, env: &Env) -> Result<Value, EvalError> {
    match expr {
        // Самовычислимые.
        Value::Bool(_) | Value::Int(_) | Value::Str(_) | Value::Builtin(..) | Value::Closure(_) => {
            Ok(expr.clone())
        }
        Value::Sym(s) => env
            .lookup(s)
            .ok_or_else(|| EvalError::new(alloc::format!("неизвестный символ '{}'", s))),
        Value::List(items) => {
            if items.is_empty() {
                return Ok(expr.clone()); // () → пустой список
            }
            if let Value::Sym(head) = &items[0] {
                match &**head {
                    "quote" => return special_quote(items),
                    "if" => return special_if(items, env),
                    "define" => return special_define(items, env),
                    "lambda" => return special_lambda(items, env),
                    "let" => return special_let(items, env),
                    "cond" => return special_cond(items, env),
                    "begin" => return eval_body(&items[1..], env),
                    "and" => return special_and(items, env),
                    "or" => return special_or(items, env),
                    _ => {}
                }
            }
            // Применение: вычислить голову и аргументы, применить.
            let func = eval(&items[0], env)?;
            let mut args = Vec::with_capacity(items.len() - 1);
            for a in &items[1..] {
                args.push(eval(a, env)?);
            }
            apply(&func, &args)
        }
    }
}

fn apply(func: &Value, args: &[Value]) -> Result<Value, EvalError> {
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
            eval_body(&c.body, &call_env)
        }
        other => Err(EvalError::new(alloc::format!(
            "нельзя вызвать {}",
            other.type_name()
        ))),
    }
}

/// Вычислить последовательность форм, вернуть значение последней (тело функции/`begin`/`let`).
fn eval_body(body: &[Value], env: &Env) -> Result<Value, EvalError> {
    let mut last = Value::nil();
    for f in body {
        last = eval(f, env)?;
    }
    Ok(last)
}

// ── спец-формы ──────────────────────────────────────────────────────────────

fn special_quote(items: &[Value]) -> Result<Value, EvalError> {
    if items.len() != 2 {
        return Err(EvalError::new("quote: нужен ровно 1 аргумент"));
    }
    Ok(items[1].clone())
}

fn special_if(items: &[Value], env: &Env) -> Result<Value, EvalError> {
    if items.len() != 3 && items.len() != 4 {
        return Err(EvalError::new("if: (if условие тогда [иначе])"));
    }
    if eval(&items[1], env)?.truthy() {
        eval(&items[2], env)
    } else if items.len() == 4 {
        eval(&items[3], env)
    } else {
        Ok(Value::nil())
    }
}

fn special_define(items: &[Value], env: &Env) -> Result<Value, EvalError> {
    if items.len() < 3 {
        return Err(EvalError::new("define: (define имя выражение)"));
    }
    match &items[1] {
        // (define имя выражение)
        Value::Sym(name) => {
            if items.len() != 3 {
                return Err(EvalError::new("define: (define имя выражение)"));
            }
            let v = eval(&items[2], env)?;
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

fn special_lambda(items: &[Value], env: &Env) -> Result<Value, EvalError> {
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

fn special_let(items: &[Value], env: &Env) -> Result<Value, EvalError> {
    // (let ((имя знач)…) тело…): значения — в ВНЕШНЕМ окружении, тело — в дочернем.
    if items.len() < 3 {
        return Err(EvalError::new("let: (let ((имя знач)…) тело…)"));
    }
    let binds = match &items[1] {
        Value::List(b) => b,
        _ => return Err(EvalError::new("let: список привязок")),
    };
    let child = Env::child(env);
    for b in binds.iter() {
        let pair = match b {
            Value::List(p) if p.len() == 2 => p,
            _ => return Err(EvalError::new("let: привязка — (имя значение)")),
        };
        let name = match &pair[0] {
            Value::Sym(s) => s.clone(),
            _ => return Err(EvalError::new("let: имя привязки — символ")),
        };
        let v = eval(&pair[1], env)?;
        child.define(name, v);
    }
    eval_body(&items[2..], &child)
}

fn special_cond(items: &[Value], env: &Env) -> Result<Value, EvalError> {
    for clause in &items[1..] {
        let c = match clause {
            Value::List(c) if !c.is_empty() => c,
            _ => return Err(EvalError::new("cond: ветвь — (тест выражения…)")),
        };
        let is_else = matches!(&c[0], Value::Sym(s) if &**s == "else");
        if is_else || eval(&c[0], env)?.truthy() {
            return eval_body(&c[1..], env);
        }
    }
    Ok(Value::nil())
}

fn special_and(items: &[Value], env: &Env) -> Result<Value, EvalError> {
    let mut last = Value::Bool(true);
    for e in &items[1..] {
        last = eval(e, env)?;
        if !last.truthy() {
            return Ok(Value::Bool(false));
        }
    }
    Ok(last)
}

fn special_or(items: &[Value], env: &Env) -> Result<Value, EvalError> {
    for e in &items[1..] {
        let v = eval(e, env)?;
        if v.truthy() {
            return Ok(v);
        }
    }
    Ok(Value::Bool(false))
}

// ── встроенные функции ──────────────────────────────────────────────────────

const BUILTINS: &[(&str, BuiltinFn)] = &[
    ("list", b_list),
    ("append", b_append),
    ("cons", b_cons),
    ("car", b_car),
    ("cdr", b_cdr),
    ("null?", b_null),
    ("not", b_not),
    ("=", b_eq),
    ("+", b_add),
    ("-", b_sub),
    ("*", b_mul),
    ("service", b_service),
    ("shell", b_shell),
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
            Value::List(items) => {
                for it in items.iter() {
                    match it {
                        Value::Str(_) => out.push(it.clone()),
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
                                "system: ожидались записи service/shell",
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

fn is_entry(items: &[Value]) -> bool {
    matches!(items.first(), Some(Value::Sym(s)) if &**s == "service" || &**s == "shell")
}
