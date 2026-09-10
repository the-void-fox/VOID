//! Язык nix: разбор и ленивое вычисление.
//!
//! ## Зачем это в системе без сборщика мусора и без std
//!
//! Пакеты в VOID объявляются конфигом, но назвать можно только то, что уже собрал чужой канал
//! ([[0019-nix-on-device]]). Всё, чем nix является помимо кэша — выражения, оверлеи, свой пакет,
//! изменённая зависимость, — начинается с ВЫЧИСЛЕНИЯ. Оно и живёт здесь.
//!
//! ## Чем проверяется правильность
//!
//! Спецификации у языка нет — есть реализация. Поэтому единственное осмысленное утверждение о
//! правильности звучит так: **на тех же выражениях мы отвечаем то же, что `nix-instantiate`**.
//! Тесты внизу держат ожидаемый ответ рядом с выражением, и каждый такой ответ снят с живого
//! nix, а не придуман.
//!
//! ## Что уже есть и чего ещё нет
//!
//! Есть: полный синтаксис (функции с образцами, `rec`, `with`, `inherit`, вставки в строках,
//! отступные строки, динамические имена атрибутов), приоритеты операторов как у nix, ленивые
//! связки с обнаружением рекурсии, около полусотни примитивов.
//!
//! Нет: регулярных выражений (`match`, `split`), JSON/TOML, `tryEval`, `genericClosure`,
//! `fetch*`, путей `<nixpkgs>`. Отсутствующее отсутствует ЯВНО — «неизвестное имя» честнее
//! примитива, который отвечает неправдой.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::rc::Rc;
use alloc::string::String;

pub mod builtins;
pub mod eval;
pub mod lex;
pub mod parse;

pub use eval::{Attrs, EResult, Env, Eval, NixStr, Thunk, Value};
pub use parse::parse;

/// Кто строит деривации. Вычислителю языка знать про store нечего: он умеет считать выражения,
/// а превращать их в `.drv` — работа того, у кого есть store и правила именования путей.
pub trait DrvSink {
    fn derivation(&self, ev: &mut Eval, args: &Attrs) -> EResult<Value>;
}

/// Кто читает файлы для `import`. Тот же довод: у библиотеки нет ни файловой системы, ни права
/// её иметь.
pub trait Loader {
    fn load(&self, path: &str) -> Option<String>;
}

/// Вычислить выражение целиком и напечатать так же, как `nix-instantiate --eval --strict`.
pub fn eval_str(src: &str) -> Result<String, String> {
    let mut ev = Eval::new();
    let e = parse(src)?;
    let env = ev.root_env();
    let v = ev.eval(&e, &env)?;
    ev.deep(&v)?;
    print(&mut ev, &v)
}

/// Печать значения в записи nix.
pub fn print(ev: &mut Eval, v: &Value) -> EResult<String> {
    let mut out = String::new();
    write_value(ev, v, &mut out)?;
    Ok(out)
}

fn write_value(ev: &mut Eval, v: &Value, out: &mut String) -> EResult<()> {
    match v {
        Value::Int(i) => out.push_str(&alloc::format!("{}", i)),
        Value::Float(f) => out.push_str(&eval::fmt_float(*f)),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Null => out.push_str("null"),
        Value::Path(p) => out.push_str(p),
        Value::Lambda(_) | Value::Prim(_) => out.push_str("<LAMBDA>"),
        Value::Str(s) => write_str(&s.s, out),
        Value::List(items) => {
            let items = items.clone();
            out.push('[');
            for t in items.iter() {
                out.push(' ');
                let x = ev.force(t)?;
                write_value(ev, &x, out)?;
            }
            out.push_str(" ]");
        }
        Value::Attrs(a) => {
            // Деривация печатается ссылкой на своё задание, а не содержимым: развернуть её
            // значит развернуть весь граф зависимостей, чего никто не просил.
            if let (Some(t), Some(p)) = (a.get("type"), a.get("drvPath")) {
                let ty = ev.force(t)?;
                if matches!(&ty, Value::Str(s) if s.s == "derivation") {
                    let path = ev.force(p)?;
                    let s = ev.coerce_str(&path, true)?;
                    out.push_str(&alloc::format!("<derivation {}>", s.s));
                    return Ok(());
                }
            }
            let a = a.clone();
            out.push('{');
            for (k, t) in a.iter() {
                out.push(' ');
                if builtins::bare_name(k) {
                    out.push_str(k);
                } else {
                    write_str(k, out);
                }
                out.push_str(" = ");
                let x = ev.force(t)?;
                write_value(ev, &x, out)?;
                out.push(';');
            }
            out.push_str(" }");
        }
    }
    Ok(())
}

/// Экранирование строки в записи nix: пять случаев плюс `${`, который иначе прочтётся вставкой.
fn write_str(s: &str, out: &mut String) {
    out.push('"');
    let b = s.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            b'$' if i + 1 < b.len() && b[i + 1] == b'{' => out.push_str("\\${"),
            c => {
                let start = i;
                i += 1;
                while i < b.len() && b[i] & 0xc0 == 0x80 {
                    i += 1;
                }
                out.push_str(&String::from_utf8_lossy(&b[start..i]));
                let _ = c;
                continue;
            }
        }
        i += 1;
    }
    out.push('"');
}

/// Собрать вычислитель со сборщиком деривации и загрузчиком файлов.
pub fn eval_with(
    src: &str,
    drv: Option<Rc<dyn DrvSink>>,
) -> Result<(Eval, Value), String> {
    let mut ev = Eval::new();
    ev.drv = drv;
    let e = parse(src)?;
    let env = ev.root_env();
    let v = ev.eval(&e, &env)?;
    Ok((ev, v))
}

#[cfg(test)]
mod tests {
    use super::eval_str;

    /// Каждая пара — выражение и то, что на нём отвечает НАСТОЯЩИЙ `nix-instantiate --eval
    /// --strict`. Проверено на хосте; выдумывать ожидаемое здесь нельзя — тогда тест проверял бы
    /// наше представление о nix, а не совпадение с ним.
    fn same(src: &str, want: &str) {
        match eval_str(src) {
            Ok(got) => assert_eq!(got, want, "выражение: {}", src),
            Err(e) => panic!("выражение {} не вычислилось: {}", src, e),
        }
    }

    #[test]
    fn числа_и_операторы() {
        same("1 + 2 * 3", "7");
        same("(1 + 2) * 3", "9");
        same("7 / 2", "3");
        same("1.5 + 1", "2.5");
        same("-3 + 1", "-2");
        same("1 < 2", "true");
        same("2 <= 2", "true");
        same("1 == 1.0", "true");
        same("[ 1 2 ] == [ 1 2 ]", "true");
        same("true && false", "false");
        same("false || true", "true");
        same("false -> false", "true");
        same("!true", "false");
    }

    #[test]
    fn строки() {
        same(r#""a" + "b""#, r#""ab""#);
        same(r#""a${"b"}c""#, r#""abc""#);
        same(r#"let x = "y"; in "a${x}b""#, r#""ayb""#);
        same(r#"builtins.stringLength "абв""#, "6");
        same(r#"builtins.substring 1 2 "abcd""#, r#""bc""#);
        same(r#"builtins.concatStringsSep "," [ "a" "b" ]"#, r#""a,b""#);
        same(r#"builtins.replaceStrings [ "a" ] [ "z" ] "aba""#, r#""zbz""#);
        same(r#"toString 12"#, r#""12""#);
    }

    #[test]
    fn списки_и_множества() {
        same("[ ]", "[ ]");
        same("[ 1 2 3 ]", "[ 1 2 3 ]");
        same("[ 1 ] ++ [ 2 ]", "[ 1 2 ]");
        same("{ }", "{ }");
        same("{ b = 2; a = 1; }", "{ a = 1; b = 2; }");
        same("{ a.b.c = 1; }", "{ a = { b = { c = 1; }; }; }");
        same("{ a = 1; } // { a = 2; b = 3; }", "{ a = 2; b = 3; }");
        same("{ a = 1; } ? a", "true");
        same("{ a = 1; }.b or 5", "5");
        same("builtins.attrNames { b = 1; a = 2; }", r#"[ "a" "b" ]"#);
        same("builtins.length [ 1 2 3 ]", "3");
        same("builtins.head [ 1 2 ]", "1");
        same("builtins.tail [ 1 2 3 ]", "[ 2 3 ]");
        same("map (x: x * 2) [ 1 2 ]", "[ 2 4 ]");
        same("builtins.filter (x: x > 1) [ 1 2 3 ]", "[ 2 3 ]");
        same("builtins.foldl' (a: b: a + b) 0 [ 1 2 3 ]", "6");
        same("builtins.genList (i: i * i) 4", "[ 0 1 4 9 ]");
        same("builtins.sort (a: b: a < b) [ 3 1 2 ]", "[ 1 2 3 ]");
    }

    #[test]
    fn связывание_и_лень() {
        same("let x = 1; y = x + 1; in y", "2");
        same("rec { a = b; b = 1; }.a", "1");
        same("let x = 1; in with { x = 2; }; x", "1");
        same("with { x = 2; }; x", "2");
        same("{ a = throw \"нет\"; b = 1; }.b", "1");
        same("false && throw \"нет\"", "false");
        same("let f = { a, b ? a + 1 }: b; in f { a = 1; }", "2");
        same("let f = args@{ a, ... }: args.c; in f { a = 1; c = 3; }", "3");
        same("(x: y: x + y) 1 2", "3");
        same("let s = { a = 1; b = 2; }; in with s; a + b", "3");
        same("let a = 1; in { inherit a; }", "{ a = 1; }");
        same("let s = { a = 1; }; in { inherit (s) a; }", "{ a = 1; }");
        same("if 1 < 2 then \"да\" else \"нет\"", r#""да""#);
    }

    #[test]
    fn динамические_имена() {
        same(r#"let k = "a"; in { ${k} = 1; }"#, "{ a = 1; }");
        same(r#"{ "a b" = 1; }"#, r#"{ "a b" = 1; }"#);
    }

    /// Косая — самый тонкий символ языка: `//` обновляет множество, `a / b` делит, `a/b` это
    /// ПУТЬ. Разбирается это в лексере, и ошибка там стоила бесконечного цикла — путь из одних
    /// косых давал пустую лексему, счётчик не двигался, и поток токенов рос, пока не кончалась
    /// память. Тест держит все три случая рядом.
    #[test]
    fn косая_черта() {
        same("{ a = 1; } // { b = 2; }", "{ a = 1; b = 2; }");
        same("6 / 2", "3");
        same("let x = 6; y = 2; in x / y", "3");
        same("/nix/store", "/nix/store");
        same("builtins.isPath ./a/b", "true");
        // ОТЛИЧИЕ ОТ NIX, и намеренное: относительный путь nix считает от текущего каталога, а
        // текущего каталога у процесса VOID не бывает (см. [[no-cwd]] в `posix::cwd`). Считаем
        // от корня — это честнее, чем выдумать каталог.
        same("./x", "/x");
        same("1.5", "1.5");
    }

    #[test]
    fn ошибки_называются() {
        assert!(eval_str("нетакого").is_err());
        assert!(eval_str("1 + \"a\"").is_err());
        assert!(eval_str("let x = x; in x").is_err());
        assert!(eval_str("{ a = 1; a = 2; }").is_err());
    }
}
