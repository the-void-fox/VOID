//! Деривация nix — `.drv` в формате ATerm.
//!
//! Деривация это ЗАДАНИЕ НА СБОРКУ: чем собирать (`builder`), с какими аргументами, в каком
//! окружении и что должно получиться (`outputs`). Ни одного вычисления в ней уже нет — язык
//! nix кончается ровно тогда, когда получается `.drv`, и дальше работает сборщик.
//!
//! ## Почему формат разбирается, а не придумывается свой
//!
//! Путь в store вычисляется ИЗ ТЕКСТА деривации: nix хэширует её напечатанное представление.
//! Значит формат — не сериализация по вкусу, а часть арифметики адресов: лишний пробел даст
//! другой хэш, то есть другой пакет. Свой формат означал бы, что собранное здесь никогда не
//! совпадёт с собранным где-либо ещё, — а вся ценность nix ровно в обратном.
//!
//! Грамматика:
//!
//! ```text
//! Derive( [(имя_выхода, путь, алг_хэша, хэш)…],
//!         [(путь_к_drv, [имена_выходов…])…],
//!         [путь_исходника…],
//!         "система", "сборщик", [аргумент…], [(ключ, значение)…] )
//! ```
//!
//! Строки — в двойных кавычках с экранированием `\"`, `\\`, `\n`, `\r`, `\t`; больше nix не
//! экранирует ничего (`printString`), поэтому и мы не должны.
//!
//! ## Разбор терпим к пробелам, печать — нет
//!
//! Настоящий `.drv` пишется в одну строку без единого пробела, и [`print`] делает ровно так.
//! А вот [`parse`] пробелы между лексемами пропускает: деривацию, написанную человеком руками
//! (а первые проверки шага 4 именно такие), иначе не прочесть. Внутри строки пробелы,
//! разумеется, значимы и сохраняются.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// Выход деривации: как он зовётся, куда встанет и (для фиксированных) чем проверяется.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    pub name: String,
    pub path: String,
    /// Алгоритм хэша у фиксированного выхода (`sha256`); пусто у обычного.
    pub hash_algo: String,
    /// Ожидаемый хэш у фиксированного выхода; пусто у обычного.
    pub hash: String,
}

/// Задание на сборку целиком.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Drv {
    pub outputs: Vec<Output>,
    /// Деривации-входы: путь к чужому `.drv` и какие его выходы нам нужны.
    pub input_drvs: Vec<(String, Vec<String>)>,
    /// Пути-исходники, которые уже лежат в store.
    pub input_srcs: Vec<String>,
    pub system: String,
    pub builder: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl Drv {
    /// Путь выхода по имени (`out` — главный).
    pub fn output(&self, name: &str) -> Option<&str> {
        self.outputs.iter().find(|o| o.name == name).map(|o| o.path.as_str())
    }

    /// Значение переменной окружения деривации.
    pub fn env_get(&self, key: &str) -> Option<&str> {
        self.env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

/// Разобрать текст `.drv`.
pub fn parse(text: &[u8]) -> Result<Drv, &'static str> {
    let mut p = P { b: text, i: 0 };
    p.word(b"Derive")?;
    p.eat(b'(')?;
    let outputs = p.list(|p| {
        p.eat(b'(')?;
        let name = p.string()?;
        p.eat(b',')?;
        let path = p.string()?;
        p.eat(b',')?;
        let hash_algo = p.string()?;
        p.eat(b',')?;
        let hash = p.string()?;
        p.eat(b')')?;
        Ok(Output { name, path, hash_algo, hash })
    })?;
    p.eat(b',')?;
    let input_drvs = p.list(|p| {
        p.eat(b'(')?;
        let path = p.string()?;
        p.eat(b',')?;
        let outs = p.list(|p| p.string())?;
        p.eat(b')')?;
        Ok((path, outs))
    })?;
    p.eat(b',')?;
    let input_srcs = p.list(|p| p.string())?;
    p.eat(b',')?;
    let system = p.string()?;
    p.eat(b',')?;
    let builder = p.string()?;
    p.eat(b',')?;
    let args = p.list(|p| p.string())?;
    p.eat(b',')?;
    let env = p.list(|p| {
        p.eat(b'(')?;
        let k = p.string()?;
        p.eat(b',')?;
        let v = p.string()?;
        p.eat(b')')?;
        Ok((k, v))
    })?;
    p.eat(b')')?;
    p.skip_ws();
    if p.i != p.b.len() {
        return Err("после Derive(...) остался мусор");
    }
    Ok(Drv { outputs, input_drvs, input_srcs, system, builder, args, env })
}

/// Напечатать `.drv` ровно так, как печатает nix: одной строкой, без пробелов.
///
/// От этого зависит хэш пути, поэтому здесь нет ни одной вольности.
pub fn print(d: &Drv) -> String {
    let mut s = String::new();
    s.push_str("Derive([");
    for (i, o) in d.outputs.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push('(');
        quote(&mut s, &o.name);
        s.push(',');
        quote(&mut s, &o.path);
        s.push(',');
        quote(&mut s, &o.hash_algo);
        s.push(',');
        quote(&mut s, &o.hash);
        s.push(')');
    }
    s.push_str("],[");
    for (i, (path, outs)) in d.input_drvs.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push('(');
        quote(&mut s, path);
        s.push_str(",[");
        for (j, o) in outs.iter().enumerate() {
            if j > 0 {
                s.push(',');
            }
            quote(&mut s, o);
        }
        s.push_str("])");
    }
    s.push_str("],[");
    for (i, p) in d.input_srcs.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        quote(&mut s, p);
    }
    s.push_str("],");
    quote(&mut s, &d.system);
    s.push(',');
    quote(&mut s, &d.builder);
    s.push_str(",[");
    for (i, a) in d.args.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        quote(&mut s, a);
    }
    s.push_str("],[");
    for (i, (k, v)) in d.env.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push('(');
        quote(&mut s, k);
        s.push(',');
        quote(&mut s, v);
        s.push(')');
    }
    s.push_str("])");
    s
}

/// Экранирование ровно как `printString` в nix — пять случаев и ни одним больше.
fn quote(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
}

// ─── разбор ────────────────────────────────────────────────────────────────────

struct P<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> P<'a> {
    fn skip_ws(&mut self) {
        while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.b.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> Result<(), &'static str> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            Err("не тот символ в деривации")
        }
    }

    fn word(&mut self, w: &[u8]) -> Result<(), &'static str> {
        self.skip_ws();
        if self.b[self.i..].starts_with(w) {
            self.i += w.len();
            Ok(())
        } else {
            Err("деривация начинается не с Derive")
        }
    }

    fn string(&mut self) -> Result<String, &'static str> {
        self.eat(b'"')?;
        // Копим БАЙТАМИ, а строкой делаем в конце. Посимвольно тут нельзя: `byte as char` режет
        // UTF-8 по одному байту и превращает кириллицу в набор закорючек — ошибка тихая,
        // вылезающая через десять шагов в имени пакета.
        let mut out: Vec<u8> = Vec::new();
        loop {
            let c = *self.b.get(self.i).ok_or("строка не закрыта")?;
            self.i += 1;
            match c {
                b'"' => return String::from_utf8(out).map_err(|_| "строка не в UTF-8"),
                b'\\' => {
                    let e = *self.b.get(self.i).ok_or("строка не закрыта")?;
                    self.i += 1;
                    out.push(match e {
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b't' => b'\t',
                        // `\"` и `\\` — сами собой; всё прочее nix не экранирует, и встреченное
                        // здесь честнее пропустить как есть, чем выдумать ему смысл.
                        other => other,
                    });
                }
                other => out.push(other),
            }
        }
    }

    fn list<T, F>(&mut self, mut f: F) -> Result<Vec<T>, &'static str>
    where
        F: FnMut(&mut P<'a>) -> Result<T, &'static str>,
    {
        self.eat(b'[')?;
        let mut v = Vec::new();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(v);
        }
        loop {
            v.push(f(self)?);
            match self.peek() {
                Some(b',') => self.i += 1,
                _ => break,
            }
        }
        self.eat(b']')?;
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Настоящий `.drv`, какой печатает nix: одной строкой и без пробелов.
    const REAL: &str = concat!(
        r#"Derive([("out","/nix/store/00000000000000000000000000000000-hi","","")],"#,
        r#"[],[],"x86_64-linux","/bin/busybox",["sh","-c","echo hi > $out"],"#,
        r#"[("builder","/bin/busybox"),("name","hi"),"#,
        r#"("out","/nix/store/00000000000000000000000000000000-hi"),("system","x86_64-linux")])"#
    );

    #[test]
    fn разбор_настоящей_деривации() {
        let d = parse(REAL.as_bytes()).expect("разобралась");
        assert_eq!(d.system, "x86_64-linux");
        assert_eq!(d.builder, "/bin/busybox");
        assert_eq!(d.args, ["sh", "-c", "echo hi > $out"]);
        assert_eq!(d.output("out"), Some("/nix/store/00000000000000000000000000000000-hi"));
        assert_eq!(d.env_get("name"), Some("hi"));
        assert!(d.input_drvs.is_empty() && d.input_srcs.is_empty());
    }

    /// Печать обязана быть БАЙТ В БАЙТ: из неё считается хэш пути.
    #[test]
    fn печать_возвращает_тот_же_текст() {
        let d = parse(REAL.as_bytes()).unwrap();
        assert_eq!(print(&d), REAL);
    }

    #[test]
    fn экранирование_переживает_круг() {
        let d = Drv {
            outputs: vec![Output {
                name: String::from("out"),
                path: String::from("/nix/store/x"),
                hash_algo: String::new(),
                hash: String::new(),
            }],
            input_drvs: vec![(String::from("/nix/store/a.drv"), vec![String::from("out")])],
            input_srcs: vec![String::from("/nix/store/src")],
            system: String::from("riscv64-linux"),
            builder: String::from("/bin/sh"),
            args: vec![String::from("-c"), String::from("echo \"a\\b\"\n\tи кириллица")],
            env: vec![(String::from("k"), String::from("v\r\n"))],
        };
        assert_eq!(parse(print(&d).as_bytes()).unwrap(), d);
    }

    /// Написанную руками деривацию с переносами и отступами прочесть надо — первые проверки
    /// шага 4 именно такие.
    #[test]
    fn пробелы_между_лексемами_допустимы() {
        let text = r#"Derive(
            [ ("out", "/nix/store/x", "", "") ],
            [], [],
            "x86_64-linux",
            "/bin/busybox",
            [ "sh", "-c", "true" ],
            [ ("out", "/nix/store/x") ]
        )"#;
        let d = parse(text.as_bytes()).expect("разобралась");
        assert_eq!(d.args.len(), 3);
        assert_eq!(d.output("out"), Some("/nix/store/x"));
    }

    #[test]
    fn мусор_после_деривации_это_ошибка() {
        let mut bad = String::from(REAL);
        bad.push_str("хвост");
        assert!(parse(bad.as_bytes()).is_err());
    }
}
