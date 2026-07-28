//! Значения vvsh + окружение + ошибка вычисления. База для reader/eval/config.
//!
//! Гомоиконность: список [`Value::List`] — это и код (форма для вычисления), и данные (результат).
//! Значения дёшевы в клонировании (строки/списки/замыкания под `Rc`), окружение — цепочка
//! `Rc<RefCell<Scope>>` с родителем (лексический скоуп).

use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::fmt;

/// Ошибка вычисления/нормализации — человекочитаемый текст (по-русски, как весь VOID).
#[derive(Clone, Debug)]
pub struct EvalError(pub String);

impl EvalError {
    pub fn new(msg: impl Into<String>) -> Self {
        EvalError(msg.into())
    }
}

/// Встроенная функция: получает УЖЕ вычисленные аргументы, возвращает значение.
pub type BuiltinFn = fn(&[Value]) -> Result<Value, EvalError>;

/// Значение vvsh. `List` — proper list (Vec, не cons-ячейки): проще и достаточно для конфига.
#[derive(Clone)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Str(Rc<str>),
    Sym(Rc<str>),
    List(Rc<Vec<Value>>),
    /// Имя (для печати/ошибок) + указатель на реализацию.
    Builtin(&'static str, BuiltinFn),
    Closure(Rc<Closure>),
}

/// Пользовательская функция: параметры + тело (последовательность, результат — последней формы) +
/// захваченное окружение определения (замыкание).
pub struct Closure {
    pub params: Vec<Rc<str>>,
    pub body: Vec<Value>,
    pub env: Env,
}

impl Value {
    pub fn nil() -> Value {
        Value::List(Rc::new(Vec::new()))
    }
    pub fn str(s: &str) -> Value {
        Value::Str(Rc::from(s))
    }
    pub fn sym(s: &str) -> Value {
        Value::Sym(Rc::from(s))
    }
    pub fn list(items: Vec<Value>) -> Value {
        Value::List(Rc::new(items))
    }

    /// Истинность по Scheme: ложь ТОЛЬКО `#f`; всё прочее (в т.ч. пустой список) — истинно.
    pub fn truthy(&self) -> bool {
        !matches!(self, Value::Bool(false))
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Bool(_) => "bool",
            Value::Int(_) => "число",
            Value::Str(_) => "строка",
            Value::Sym(_) => "символ",
            Value::List(_) => "список",
            Value::Builtin(..) => "встроенная",
            Value::Closure(_) => "замыкание",
        }
    }
}

/// Структурное равенство (для `=` и тестов). Встроенные/замыкания сравнению не подлежат → не равны.
impl PartialEq for Value {
    fn eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Sym(a), Value::Sym(b)) => a == b,
            (Value::List(a), Value::List(b)) => a == b,
            _ => false,
        }
    }
}

/// Каноничная печать (код = данные): `(system (service "posixfs" …))`. Основа модели «значение →
/// объект store» (печатаем → `put` → content-id) и удобна для отладки/`=`.
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Bool(true) => f.write_str("#t"),
            Value::Bool(false) => f.write_str("#f"),
            Value::Int(n) => write!(f, "{}", n),
            Value::Str(s) => {
                f.write_str("\"")?;
                for c in s.chars() {
                    match c {
                        '"' => f.write_str("\\\"")?,
                        '\\' => f.write_str("\\\\")?,
                        '\n' => f.write_str("\\n")?,
                        '\t' => f.write_str("\\t")?,
                        _ => write!(f, "{}", c)?,
                    }
                }
                f.write_str("\"")
            }
            Value::Sym(s) => f.write_str(s),
            Value::List(items) => {
                f.write_str("(")?;
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" ")?;
                    }
                    write!(f, "{}", it)?;
                }
                f.write_str(")")
            }
            Value::Builtin(name, _) => write!(f, "#<встроенная {}>", name),
            Value::Closure(_) => f.write_str("#<замыкание>"),
        }
    }
}

/// `Debug` = каноничная печать (для `assert_eq!`/диагностики).
impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self)
    }
}

/// Лексическое окружение: цепочка областей с родителем. Клон дёшев (`Rc`) — замыкания делят область.
#[derive(Clone)]
pub struct Env(Rc<RefCell<Scope>>);

struct Scope {
    vars: Vec<(Rc<str>, Value)>,
    parent: Option<Env>,
}

impl Env {
    pub fn root() -> Env {
        Env(Rc::new(RefCell::new(Scope { vars: Vec::new(), parent: None })))
    }

    pub fn child(parent: &Env) -> Env {
        Env(Rc::new(RefCell::new(Scope {
            vars: Vec::new(),
            parent: Some(parent.clone()),
        })))
    }

    /// Определить/переопределить в ТЕКУЩЕЙ области (`define`, привязки `let`, параметры).
    pub fn define(&self, name: Rc<str>, val: Value) {
        let mut s = self.0.borrow_mut();
        if let Some(slot) = s.vars.iter_mut().find(|(n, _)| *n == name) {
            slot.1 = val;
        } else {
            s.vars.push((name, val));
        }
    }

    /// Поиск по цепочке областей вверх до корня. `None` — символ не связан.
    pub fn lookup(&self, name: &str) -> Option<Value> {
        let s = self.0.borrow();
        if let Some((_, v)) = s.vars.iter().find(|(n, _)| &**n == name) {
            return Some(v.clone());
        }
        let parent = s.parent.clone();
        drop(s);
        parent.and_then(|p| p.lookup(name))
    }
}
