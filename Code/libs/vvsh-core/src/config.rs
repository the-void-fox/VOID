//! Нормализатор: вычисленное значение конфига → текст для init'а VOID (`kernel/src/init.rs`).
//!
//! Верхняя форма — `(#system запись…)`, запись — `(kind имя право…)`. Печатаем построчно
//! `kind имя право право` — ровно формат, который сегодня генерирует `nix/system.nix` и читает
//! `apply()`. Это выход этапа «сборки» (`rebuild`): его коммитят поколением, ядро грузит на буте.

use alloc::string::String;
use core::fmt::Write;

use crate::value::{EvalError, Value};

/// Значение `(#system …)` → нормализованный текст конфига (строки `service …` / `shell …`).
pub fn normalize_config(v: &Value) -> Result<String, EvalError> {
    let items = match v {
        Value::List(items) if head_is(items, "#system") => items,
        _ => return Err(EvalError::new("верхняя форма конфига должна быть (system …)")),
    };
    let mut out = String::new();
    for e in &items[1..] {
        let entry = match e {
            Value::List(x) => x,
            _ => return Err(EvalError::new("запись конфига — список")),
        };
        if entry.len() < 2 {
            return Err(EvalError::new("запись: (kind имя право…)"));
        }
        let kind = match &entry[0] {
            Value::Sym(s) => &**s,
            _ => return Err(EvalError::new("kind записи — символ")),
        };
        let name = match &entry[1] {
            Value::Str(s) => &**s,
            _ => return Err(EvalError::new("имя записи — строка")),
        };
        let _ = write!(out, "{} {}", kind, name);
        for cap in &entry[2..] {
            match cap {
                Value::Str(s) => {
                    let _ = write!(out, " {}", &**s);
                }
                _ => return Err(EvalError::new("право записи — строка")),
            }
        }
        out.push('\n');
    }
    Ok(out)
}

fn head_is(items: &[Value], sym: &str) -> bool {
    matches!(items.first(), Some(Value::Sym(s)) if &**s == sym)
}
