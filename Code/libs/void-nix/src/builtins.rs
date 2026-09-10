//! Примитивы (`builtins`).
//!
//! Набор здесь не полный и полным быть не пытается: берётся то, на чём стоит выражение
//! деривации, и по одному добавляется дальше — ровно как растёт личность Linux (ADR 0019).
//! Отсутствующее лучше отсутствовать явно: `builtins.match`, которого нет, даёт «неизвестное
//! имя», а `builtins.match`, отвечающий неправдой, даёт неверный пакет.
//!
//! Имена из [`GLOBALS`] видны и без `builtins.` — так их выкладывает наружу сам nix.

use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::eval::{Attrs, EResult, Eval, NixStr, Prim, Thunk, Value};

/// Примитивы, видимые без приставки `builtins.` — список взят у nix.
pub const GLOBALS: &[&str] = &[
    "abort",
    "baseNameOf",
    "derivation",
    "dirOf",
    "import",
    "isNull",
    "map",
    "removeAttrs",
    "throw",
    "toString",
];

macro_rules! prim {
    ($m:expr, $name:literal, $arity:expr, $f:expr) => {
        $m.insert(
            String::from($name),
            Thunk::done(Value::Prim(Prim {
                name: $name,
                arity: $arity,
                args: Rc::new(Vec::new()),
                f: $f,
            })),
        );
    };
}

pub fn table() -> Attrs {
    let mut m = Attrs::new();

    m.insert(String::from("nixVersion"), Thunk::done(Value::Str(NixStr::plain("2.24.9"))));
    m.insert(String::from("langVersion"), Thunk::done(Value::Int(6)));
    m.insert(String::from("currentSystem"), Thunk::done(Value::Str(NixStr::plain(SYSTEM))));
    m.insert(String::from("true"), Thunk::done(Value::Bool(true)));
    m.insert(String::from("false"), Thunk::done(Value::Bool(false)));
    m.insert(String::from("null"), Thunk::done(Value::Null));

    prim!(m, "throw", 1, |ev, a| {
        let s = str_of(ev, &a[0])?;
        Err(s.s)
    });
    prim!(m, "abort", 1, |ev, a| {
        let s = str_of(ev, &a[0])?;
        Err(alloc::format!("прервано: {}", s.s))
    });
    prim!(m, "trace", 2, |ev, a| {
        // Печатать некуда: у библиотеки нет ни консоли, ни права её иметь. Значение проходит
        // насквозь — это и есть смысл `trace` для вычисления.
        let _ = ev.force(&a[0])?;
        ev.force(&a[1])
    });
    prim!(m, "seq", 2, |ev, a| {
        ev.force(&a[0])?;
        ev.force(&a[1])
    });
    prim!(m, "deepSeq", 2, |ev, a| {
        let v = ev.force(&a[0])?;
        ev.deep(&v)?;
        ev.force(&a[1])
    });

    prim!(m, "typeOf", 1, |ev, a| {
        let v = ev.force(&a[0])?;
        Ok(Value::Str(NixStr::plain(v.type_name())))
    });
    for (name, want) in [
        ("isAttrs", "set"),
        ("isList", "list"),
        ("isString", "string"),
        ("isInt", "int"),
        ("isFloat", "float"),
        ("isBool", "bool"),
        ("isPath", "path"),
        ("isFunction", "lambda"),
    ] {
        // Каждый предикат — своя функция, но тело у них одно; различает их накопленное имя.
        let f: fn(&mut Eval, &[Thunk]) -> EResult<Value> = match want {
            "set" => |ev, a| is_type(ev, a, "set"),
            "list" => |ev, a| is_type(ev, a, "list"),
            "string" => |ev, a| is_type(ev, a, "string"),
            "int" => |ev, a| is_type(ev, a, "int"),
            "float" => |ev, a| is_type(ev, a, "float"),
            "bool" => |ev, a| is_type(ev, a, "bool"),
            "path" => |ev, a| is_type(ev, a, "path"),
            _ => |ev, a| is_type(ev, a, "lambda"),
        };
        m.insert(
            String::from(name),
            Thunk::done(Value::Prim(Prim { name: "is", arity: 1, args: Rc::new(Vec::new()), f })),
        );
    }
    prim!(m, "isNull", 1, |ev, a| {
        Ok(Value::Bool(matches!(ev.force(&a[0])?, Value::Null)))
    });

    // ── строки ────────────────────────────────────────────────────────────────
    prim!(m, "toString", 1, |ev, a| {
        let v = ev.force(&a[0])?;
        let s = ev.coerce_str(&v, true)?;
        Ok(Value::Str(s))
    });
    prim!(m, "stringLength", 1, |ev, a| {
        let s = str_of(ev, &a[0])?;
        Ok(Value::Int(s.s.len() as i64))
    });
    prim!(m, "substring", 3, |ev, a| {
        let start = int_of(ev, &a[0])?;
        let len = int_of(ev, &a[1])?;
        let s = str_of(ev, &a[2])?;
        if start < 0 {
            return Err(String::from("substring: отрицательное начало"));
        }
        let b = s.s.as_bytes();
        let from = (start as usize).min(b.len());
        let to = if len < 0 { b.len() } else { (from + len as usize).min(b.len()) };
        Ok(Value::Str(NixStr {
            s: String::from_utf8_lossy(&b[from..to]).into_owned(),
            ctx: s.ctx,
        }))
    });
    prim!(m, "concatStringsSep", 2, |ev, a| {
        let sep = str_of(ev, &a[0])?;
        let items = list_of(ev, &a[1])?;
        let mut out = NixStr::default();
        for (i, t) in items.iter().enumerate() {
            if i > 0 {
                out.s.push_str(&sep.s);
            }
            let v = ev.force(t)?;
            let s = ev.coerce_str(&v, false)?;
            out.s.push_str(&s.s);
            out.ctx.extend(s.ctx);
        }
        Ok(Value::Str(out))
    });
    prim!(m, "replaceStrings", 3, |ev, a| {
        let from = list_of(ev, &a[0])?;
        let to = list_of(ev, &a[1])?;
        let s = str_of(ev, &a[2])?;
        let mut pairs: Vec<(String, NixStr)> = Vec::new();
        for i in 0..from.len() {
            let f = str_of(ev, &from[i])?;
            let t = str_of(ev, to.get(i).ok_or("replaceStrings: списки разной длины")?)?;
            pairs.push((f.s, t));
        }
        let src = s.s.as_bytes();
        let mut out = NixStr { s: String::new(), ctx: s.ctx };
        let mut i = 0usize;
        'outer: while i < src.len() {
            for (f, t) in &pairs {
                if !f.is_empty() && src[i..].starts_with(f.as_bytes()) {
                    out.s.push_str(&t.s);
                    out.ctx.extend(t.ctx.iter().cloned());
                    i += f.len();
                    continue 'outer;
                }
            }
            let start = i;
            i += 1;
            while i < src.len() && src[i] & 0xc0 == 0x80 {
                i += 1;
            }
            out.s.push_str(&String::from_utf8_lossy(&src[start..i]));
        }
        Ok(Value::Str(out))
    });
    prim!(m, "baseNameOf", 1, |ev, a| {
        let v = ev.force(&a[0])?;
        let s = ev.coerce_str(&v, true)?;
        let base = s.s.rsplit('/').next().unwrap_or("").to_string();
        Ok(Value::Str(NixStr { s: base, ctx: s.ctx }))
    });
    prim!(m, "dirOf", 1, |ev, a| {
        let v = ev.force(&a[0])?;
        let s = ev.coerce_str(&v, true)?;
        let dir = match s.s.rfind('/') {
            Some(0) => String::from("/"),
            Some(i) => s.s[..i].to_string(),
            None => String::from("."),
        };
        match v {
            Value::Path(_) => Ok(Value::Path(dir)),
            _ => Ok(Value::Str(NixStr { s: dir, ctx: s.ctx })),
        }
    });

    // ── списки ────────────────────────────────────────────────────────────────
    prim!(m, "length", 1, |ev, a| Ok(Value::Int(list_of(ev, &a[0])?.len() as i64)));
    prim!(m, "head", 1, |ev, a| {
        let l = list_of(ev, &a[0])?;
        let first = l.first().ok_or("head: список пуст")?.clone();
        ev.force(&first)
    });
    prim!(m, "tail", 1, |ev, a| {
        let l = list_of(ev, &a[0])?;
        if l.is_empty() {
            return Err(String::from("tail: список пуст"));
        }
        Ok(Value::List(Rc::new(l[1..].to_vec())))
    });
    prim!(m, "elemAt", 2, |ev, a| {
        let l = list_of(ev, &a[0])?;
        let i = int_of(ev, &a[1])?;
        let t = l.get(i.max(0) as usize).ok_or("elemAt: за пределами списка")?.clone();
        ev.force(&t)
    });
    prim!(m, "elem", 2, |ev, a| {
        let x = ev.force(&a[0])?;
        let l = list_of(ev, &a[1])?;
        for t in l.iter() {
            let v = ev.force(t)?;
            if ev.equal(&x, &v)? {
                return Ok(Value::Bool(true));
            }
        }
        Ok(Value::Bool(false))
    });
    prim!(m, "map", 2, |ev, a| {
        let f = ev.force(&a[0])?;
        let l = list_of(ev, &a[1])?;
        let mut out = Vec::with_capacity(l.len());
        for t in l.iter() {
            out.push(Thunk::done(ev.apply(f.clone(), t.clone())?));
        }
        Ok(Value::List(Rc::new(out)))
    });
    prim!(m, "filter", 2, |ev, a| {
        let f = ev.force(&a[0])?;
        let l = list_of(ev, &a[1])?;
        let mut out = Vec::new();
        for t in l.iter() {
            if bool_val(ev.apply(f.clone(), t.clone())?)? {
                out.push(t.clone());
            }
        }
        Ok(Value::List(Rc::new(out)))
    });
    prim!(m, "concatLists", 1, |ev, a| {
        let l = list_of(ev, &a[0])?;
        let mut out = Vec::new();
        for t in l.iter() {
            out.extend(list_of(ev, t)?.iter().cloned());
        }
        Ok(Value::List(Rc::new(out)))
    });
    prim!(m, "concatMap", 2, |ev, a| {
        let f = ev.force(&a[0])?;
        let l = list_of(ev, &a[1])?;
        let mut out = Vec::new();
        for t in l.iter() {
            let v = ev.apply(f.clone(), t.clone())?;
            let Value::List(items) = v else {
                return Err(String::from("concatMap: функция вернула не список"));
            };
            out.extend(items.iter().cloned());
        }
        Ok(Value::List(Rc::new(out)))
    });
    prim!(m, "foldl'", 3, |ev, a| {
        let f = ev.force(&a[0])?;
        let mut acc = a[1].clone();
        let l = list_of(ev, &a[2])?;
        for t in l.iter() {
            let step = ev.apply(f.clone(), acc)?;
            // Строгая свёртка: значение считается на каждом шаге, иначе на длинном списке
            // копится башня отложек, которая потом рушит стек разом.
            acc = Thunk::done(ev.apply(step, t.clone())?);
        }
        ev.force(&acc)
    });
    prim!(m, "genList", 2, |ev, a| {
        let f = ev.force(&a[0])?;
        let n = int_of(ev, &a[1])?;
        let mut out = Vec::new();
        for i in 0..n.max(0) {
            out.push(Thunk::done(ev.apply(f.clone(), Thunk::done(Value::Int(i)))?));
        }
        Ok(Value::List(Rc::new(out)))
    });
    prim!(m, "all", 2, |ev, a| {
        let f = ev.force(&a[0])?;
        for t in list_of(ev, &a[1])?.iter() {
            if !bool_val(ev.apply(f.clone(), t.clone())?)? {
                return Ok(Value::Bool(false));
            }
        }
        Ok(Value::Bool(true))
    });
    prim!(m, "any", 2, |ev, a| {
        let f = ev.force(&a[0])?;
        for t in list_of(ev, &a[1])?.iter() {
            if bool_val(ev.apply(f.clone(), t.clone())?)? {
                return Ok(Value::Bool(true));
            }
        }
        Ok(Value::Bool(false))
    });
    prim!(m, "sort", 2, |ev, a| {
        let f = ev.force(&a[0])?;
        let mut v = list_of(ev, &a[1])?.to_vec();
        // Сортировка вставками: список выражения nix короток, а чужой компаратор может врать
        // (не быть порядком), и от быстрой сортировки в этом случае бывает бесконечный цикл.
        for i in 1..v.len() {
            let mut j = i;
            while j > 0 {
                let step = ev.apply(f.clone(), v[j].clone())?;
                let less = bool_val(ev.apply(step, v[j - 1].clone())?)?;
                if !less {
                    break;
                }
                v.swap(j - 1, j);
                j -= 1;
            }
        }
        Ok(Value::List(Rc::new(v)))
    });

    // ── множества ─────────────────────────────────────────────────────────────
    prim!(m, "attrNames", 1, |ev, a| {
        let at = attrs_of(ev, &a[0])?;
        // Имена уже отсортированы: множество живёт в упорядоченной карте, и порядок обхода в
        // nix — тоже лексикографический. Это не совпадение, а требование: от него зависит,
        // каким выйдет напечатанный `.drv`.
        let out: Vec<Thunk> =
            at.keys().map(|k| Thunk::done(Value::Str(NixStr::plain(k.clone())))).collect();
        Ok(Value::List(Rc::new(out)))
    });
    prim!(m, "attrValues", 1, |ev, a| {
        let at = attrs_of(ev, &a[0])?;
        Ok(Value::List(Rc::new(at.values().cloned().collect())))
    });
    prim!(m, "getAttr", 2, |ev, a| {
        let name = str_of(ev, &a[0])?;
        let at = attrs_of(ev, &a[1])?;
        let t = at.get(&name.s).ok_or_else(|| alloc::format!("нет атрибута '{}'", name.s))?.clone();
        ev.force(&t)
    });
    prim!(m, "hasAttr", 2, |ev, a| {
        let name = str_of(ev, &a[0])?;
        Ok(Value::Bool(attrs_of(ev, &a[1])?.contains_key(&name.s)))
    });
    prim!(m, "removeAttrs", 2, |ev, a| {
        let at = attrs_of(ev, &a[0])?;
        let names = list_of(ev, &a[1])?;
        let mut out = (*at).clone();
        for t in names.iter() {
            out.remove(&str_of(ev, t)?.s);
        }
        Ok(Value::Attrs(Rc::new(out)))
    });
    prim!(m, "listToAttrs", 1, |ev, a| {
        let l = list_of(ev, &a[0])?;
        let mut out = Attrs::new();
        for t in l.iter() {
            let at = attrs_of(ev, t)?;
            let name = at.get("name").ok_or("listToAttrs: нет поля name")?.clone();
            let value = at.get("value").ok_or("listToAttrs: нет поля value")?.clone();
            // Первый выигрывает — так в nix.
            out.entry(str_of(ev, &name)?.s).or_insert(value);
        }
        Ok(Value::Attrs(Rc::new(out)))
    });
    prim!(m, "mapAttrs", 2, |ev, a| {
        let f = ev.force(&a[0])?;
        let at = attrs_of(ev, &a[1])?;
        let mut out = Attrs::new();
        for (k, t) in at.iter() {
            let step = ev.apply(f.clone(), Thunk::done(Value::Str(NixStr::plain(k.clone()))))?;
            out.insert(k.clone(), Thunk::done(ev.apply(step, t.clone())?));
        }
        Ok(Value::Attrs(Rc::new(out)))
    });
    prim!(m, "intersectAttrs", 2, |ev, a| {
        let x = attrs_of(ev, &a[0])?;
        let y = attrs_of(ev, &a[1])?;
        let mut out = Attrs::new();
        for (k, t) in y.iter() {
            if x.contains_key(k) {
                out.insert(k.clone(), t.clone());
            }
        }
        Ok(Value::Attrs(Rc::new(out)))
    });
    prim!(m, "catAttrs", 2, |ev, a| {
        let name = str_of(ev, &a[0])?;
        let l = list_of(ev, &a[1])?;
        let mut out = Vec::new();
        for t in l.iter() {
            if let Some(v) = attrs_of(ev, t)?.get(&name.s) {
                out.push(v.clone());
            }
        }
        Ok(Value::List(Rc::new(out)))
    });
    prim!(m, "functionArgs", 1, |ev, a| {
        let v = ev.force(&a[0])?;
        let Value::Lambda(c) = v else {
            return Err(String::from("functionArgs: не функция языка"));
        };
        let crate::parse::Expr::Lambda { param, .. } = &*c.lambda else {
            return Err(String::from("functionArgs: не функция"));
        };
        let mut out = Attrs::new();
        if let crate::parse::Param::Pattern { fields, .. } = param {
            for (n, d) in fields {
                out.insert(n.clone(), Thunk::done(Value::Bool(d.is_some())));
            }
        }
        Ok(Value::Attrs(Rc::new(out)))
    });

    // ── числа ─────────────────────────────────────────────────────────────────
    prim!(m, "add", 2, |ev, a| {
        let (x, y) = (int_of(ev, &a[0])?, int_of(ev, &a[1])?);
        Ok(Value::Int(x + y))
    });
    prim!(m, "sub", 2, |ev, a| {
        let (x, y) = (int_of(ev, &a[0])?, int_of(ev, &a[1])?);
        Ok(Value::Int(x - y))
    });
    prim!(m, "mul", 2, |ev, a| {
        let (x, y) = (int_of(ev, &a[0])?, int_of(ev, &a[1])?);
        Ok(Value::Int(x * y))
    });
    prim!(m, "lessThan", 2, |ev, a| {
        let (x, y) = (ev.force(&a[0])?, ev.force(&a[1])?);
        Ok(Value::Bool(ev.less(&x, &y)?))
    });

    // ── деривация ─────────────────────────────────────────────────────────────
    prim!(m, "derivation", 1, |ev, a| {
        let Some(sink) = ev.drv.clone() else {
            return Err(String::from(
                "деривацию строить некому: вычислитель запущен без store",
            ));
        };
        let at = attrs_of(ev, &a[0])?;
        sink.derivation(ev, &at)
    });

    m
}

/// Система, под которую вычисляем. Это не «мы Linux», а описание того, какие сборщики мы
/// умеем запускать: личность Linux исполняет линуксовые бинари этой архитектуры.
#[cfg(target_arch = "riscv64")]
const SYSTEM: &str = "riscv64-linux";
#[cfg(not(target_arch = "riscv64"))]
const SYSTEM: &str = "x86_64-linux";

fn is_type(ev: &mut Eval, a: &[Thunk], want: &str) -> EResult<Value> {
    Ok(Value::Bool(ev.force(&a[0])?.type_name() == want))
}

fn bool_val(v: Value) -> EResult<bool> {
    match v {
        Value::Bool(b) => Ok(b),
        o => Err(alloc::format!("нужен bool, а там {}", o.type_name())),
    }
}

pub fn str_of(ev: &mut Eval, t: &Thunk) -> EResult<NixStr> {
    match ev.force(t)? {
        Value::Str(s) => Ok(s),
        o => Err(alloc::format!("нужна строка, а там {}", o.type_name())),
    }
}

pub fn int_of(ev: &mut Eval, t: &Thunk) -> EResult<i64> {
    match ev.force(t)? {
        Value::Int(i) => Ok(i),
        o => Err(alloc::format!("нужно целое, а там {}", o.type_name())),
    }
}

pub fn list_of(ev: &mut Eval, t: &Thunk) -> EResult<Rc<Vec<Thunk>>> {
    match ev.force(t)? {
        Value::List(l) => Ok(l),
        o => Err(alloc::format!("нужен список, а там {}", o.type_name())),
    }
}

pub fn attrs_of(ev: &mut Eval, t: &Thunk) -> EResult<Rc<Attrs>> {
    match ev.force(t)? {
        Value::Attrs(a) => Ok(a),
        o => Err(alloc::format!("нужно множество, а там {}", o.type_name())),
    }
}

/// Имя атрибута печатается без кавычек, если оно похоже на имя.
pub fn bare_name(s: &str) -> bool {
    let mut it = s.bytes();
    match it.next() {
        Some(c) if c.is_ascii_alphabetic() || c == b'_' => {}
        _ => return false,
    }
    it.all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'\'' | b'-'))
}

/// Пустая карта — чтобы `table()` не был единственным способом её получить в тестах.
pub fn empty() -> BTreeMap<String, Thunk> {
    BTreeMap::new()
}
