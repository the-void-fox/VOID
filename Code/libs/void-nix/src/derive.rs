//! `derivation` — граница между языком и store.
//!
//! Здесь кончается вычисление и начинается адрес: множество атрибутов превращается в ЗАДАНИЕ,
//! у задания считается хэш, из хэша — пути выходов, из полного текста — путь самого задания.
//! Арифметика живёт в [`void_drv::paths`], сюда приезжает готовой.
//!
//! ## Что становится окружением, а что нет
//!
//! Правило nix простое и неочевидное: **в окружение попадает ВСЁ**, что дали, кроме `args` —
//! он особый, потому что это аргументы сборщика, а не переменные. `outputs`, если он задан,
//! попадает как строка через пробел (`"out dev"`), а каждый выход дополнительно кладётся
//! переменной со своим путём. Проверено против живого nix, тесты — в `void-drv`.
//!
//! Значения приводятся к тексту ШИРОКО (как `toString`): `true` → `1`, `false` → пусто, число →
//! десятичной записью, список — через пробел. Это тоже часть адреса: другой текст — другой путь.
//!
//! ## Контекст строки — это и есть граф зависимостей (Веха 190)
//!
//! Строка, полученная из `outPath` чужой деривации, несёт КОНТЕКСТ — след того, от чего она
//! зависит. `"${hello}/bin/x"` выглядит обычным текстом, но помнит, что за ним стоит задание;
//! из этой памяти и строится список входов. Без неё зависимость исчезла бы бесследно, а сборка
//! полезла бы по адресу, которого никто не собирал.
//!
//! Хэш такого задания считается с ПОДМЕНОЙ: путь входа заменяется его собственным хэшем
//! (`hashDerivationModulo`, рекурсивно). Отсюда свойство, ради которого всё и затевалось: два
//! задания, отличающиеся лишь тем, каким путём пришла та же зависимость, дают ОДИН адрес.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use core::cell::RefCell;

use void_drv::{paths, Drv, Output};

use crate::eval::{Attrs, EResult, Eval, NixStr, Thunk, Value};
use crate::DrvSink;

/// Куда положить текст задания. Вычислителю знать про файлы нечего — он отдаёт готовый путь и
/// готовый текст, а кладёт их тот, у кого есть store.
pub trait StoreText {
    fn add(&self, path: &str, text: &str) -> Result<(), String>;
    /// Прочитать положенное раньше. Нужно графу: чтобы посчитать хэш задания, надо знать хэши
    /// его входов, а входом может оказаться `.drv` из прошлого запуска.
    fn read(&self, path: &str) -> Option<String>;
}

/// Приставка контекста для выхода чужой деривации: `!<имя выхода>!<путь задания>` — запись nix.
const CTX_OUT: char = '!';
/// Приставка контекста для самого задания (`drvPath`).
const CTX_DRV: char = '=';

pub struct Deriver {
    pub store: Rc<dyn StoreText>,
    /// Задания, построенные в ЭТОМ вычислении. Держим их при себе, чтобы не перечитывать
    /// только что написанный файл — и чтобы граф считался, даже если store читать не умеет.
    made: RefCell<BTreeMap<String, Drv>>,
}

impl Deriver {
    pub fn new(store: Rc<dyn StoreText>) -> Self {
        Deriver { store, made: RefCell::new(BTreeMap::new()) }
    }

    /// Найти задание по пути: сперва среди своих, потом в store.
    fn resolve(&self, path: &str) -> Option<Drv> {
        if let Some(d) = self.made.borrow().get(path) {
            return Some(d.clone());
        }
        let text = self.store.read(path)?;
        void_drv::parse(text.as_bytes()).ok()
    }
}

impl DrvSink for Deriver {
    fn derivation(&self, ev: &mut Eval, args: &Attrs) -> EResult<Value> {
        // ── обязательное ───────────────────────────────────────────────────────
        let name = want_str(ev, args, "name")?;
        let system = want_str(ev, args, "system")?;
        let mut ctx: BTreeSet<String> = BTreeSet::new();
        let builder = {
            let t = args.get("builder").ok_or("деривации нужен 'builder'")?.clone();
            let v = ev.force(&t)?;
            let s = ev.coerce_str(&v, true)?;
            ctx.extend(s.ctx.iter().cloned());
            s.s
        };

        // ── аргументы сборщика ─────────────────────────────────────────────────
        let mut argv: Vec<String> = Vec::new();
        if let Some(t) = args.get("args") {
            let v = ev.force(t)?;
            let Value::List(items) = v else {
                return Err(String::from("'args' деривации — список"));
            };
            for it in items.iter() {
                let x = ev.force(it)?;
                let s = ev.coerce_str(&x, true)?;
                ctx.extend(s.ctx.iter().cloned());
                argv.push(s.s);
            }
        }

        // ── выходы ─────────────────────────────────────────────────────────────
        // Порядок, данный человеком, значим: первый выход — главный, и он же попадает в
        // `outPath`. А вот в списке выходов задания они идут по алфавиту — так печатает nix.
        let mut out_names: Vec<String> = Vec::new();
        if let Some(t) = args.get("outputs") {
            let v = ev.force(t)?;
            let Value::List(items) = v else {
                return Err(String::from("'outputs' деривации — список"));
            };
            for it in items.iter() {
                let x = ev.force(it)?;
                out_names.push(ev.coerce_str(&x, false)?.s);
            }
        }
        if out_names.is_empty() {
            out_names.push(String::from("out"));
        }

        // ── окружение ──────────────────────────────────────────────────────────
        let mut env: Vec<(String, String)> = Vec::new();
        for (k, t) in args.iter() {
            if k == "args" {
                continue; // аргументы сборщика переменной не становятся
            }
            let v = ev.force(t)?;
            let s = ev.coerce_str(&v, true)?;
            ctx.extend(s.ctx.iter().cloned());
            env.push((k.clone(), s.s));
        }
        for o in &out_names {
            env.push((o.clone(), String::new())); // заполнится, когда узнаем путь
        }
        env.sort_by(|a, b| a.0.cmp(&b.0));
        env.dedup_by(|a, b| a.0 == b.0);

        // ── контекст: что это за зависимости ───────────────────────────────────
        // `!<выход>!<задание>` — нужен выход чужой деривации, `<путь>` — готовый исходник.
        // `=<задание>` (сам `drvPath` строкой) отвергаем: в nix он значит «положи рядом ещё и
        // сам .drv», а у нас складывать некуда — отказ честнее молчаливого пропуска.
        let mut input_srcs: Vec<String> = Vec::new();
        let mut input_drvs: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for c in &ctx {
            match c.chars().next() {
                Some(CTX_OUT) => {
                    let rest = &c[CTX_OUT.len_utf8()..];
                    let Some(i) = rest.find(CTX_OUT) else {
                        return Err(alloc::format!("испорченный контекст строки: {}", c));
                    };
                    let (o, p) = (&rest[..i], &rest[i + CTX_OUT.len_utf8()..]);
                    input_drvs.entry(p.to_string()).or_default().push(o.to_string());
                }
                Some(CTX_DRV) => {
                    return Err(alloc::format!(
                        "деривация просит сам файл задания ({}) — складывать его некуда",
                        &c[CTX_DRV.len_utf8()..]
                    ))
                }
                _ => input_srcs.push(c.clone()),
            }
        }
        let input_drvs: Vec<(String, Vec<String>)> = input_drvs
            .into_iter()
            .map(|(p, mut outs)| {
                outs.sort();
                outs.dedup();
                (p, outs)
            })
            .collect();

        // ── задание ────────────────────────────────────────────────────────────
        let mut sorted_outs = out_names.clone();
        sorted_outs.sort();
        let mut d = Drv {
            outputs: sorted_outs
                .iter()
                .map(|o| Output {
                    name: o.clone(),
                    path: String::new(),
                    hash_algo: String::new(),
                    hash: String::new(),
                })
                .collect(),
            input_drvs,
            input_srcs,
            system,
            builder,
            args: argv,
            env,
        };

        let h = paths::drv_hash_modulo(&d, true, &mut |p| self.resolve(p))?;
        for i in 0..d.outputs.len() {
            let o = d.outputs[i].name.clone();
            let p = paths::out_path(&h, &o, &name);
            d.outputs[i].path = p.clone();
            for e in d.env.iter_mut() {
                if e.0 == o {
                    e.1 = p.clone();
                }
            }
        }

        let text = void_drv::print(&d);
        let refs = paths::drv_refs(&d);
        let drv_path = paths::text_path(text.as_bytes(), &refs, &alloc::format!("{}.drv", name));
        self.store.add(&drv_path, &text)?;
        self.made.borrow_mut().insert(drv_path.clone(), d.clone());

        // ── значение ───────────────────────────────────────────────────────────
        // Возвращаем исходные атрибуты плюс то, что о деривации теперь известно. `all` мы не
        // строим намеренно: у nix он ленив, а у нас множества считаются сразу, и список,
        // содержащий сам себя, пришлось бы обрывать — лучше не отдавать вовсе, чем отдать
        // усечённым.
        let mut out = (*args).clone();
        let dctx: BTreeSet<String> = [alloc::format!("{}{}", CTX_DRV, drv_path)].into_iter().collect();
        out.insert(String::from("type"), Thunk::done(Value::Str(NixStr::plain("derivation"))));
        out.insert(
            String::from("drvPath"),
            Thunk::done(Value::Str(NixStr { s: drv_path.clone(), ctx: dctx })),
        );
        out.insert(
            String::from("outputs"),
            Thunk::done(Value::List(Rc::new(
                out_names
                    .iter()
                    .map(|o| Thunk::done(Value::Str(NixStr::plain(o.clone()))))
                    .collect(),
            ))),
        );
        let main = out_names[0].clone();
        for o in &out_names {
            let p = d.output(o).unwrap_or_default().to_string();
            let octx: BTreeSet<String> =
                [alloc::format!("{}{}{}{}", CTX_OUT, o, CTX_OUT, drv_path)].into_iter().collect();
            let val = Value::Str(NixStr { s: p, ctx: octx });
            let mut sub = Attrs::new();
            sub.insert(String::from("type"), Thunk::done(Value::Str(NixStr::plain("derivation"))));
            sub.insert(String::from("outputName"), Thunk::done(Value::Str(NixStr::plain(o.clone()))));
            sub.insert(String::from("outPath"), Thunk::done(val.clone()));
            sub.insert(
                String::from("drvPath"),
                Thunk::done(Value::Str(NixStr::plain(drv_path.clone()))),
            );
            out.insert(o.clone(), Thunk::done(Value::Attrs(Rc::new(sub))));
            if *o == main {
                out.insert(String::from("outPath"), Thunk::done(val));
                out.insert(
                    String::from("outputName"),
                    Thunk::done(Value::Str(NixStr::plain(o.clone()))),
                );
            }
        }
        Ok(Value::Attrs(Rc::new(out)))
    }
}

fn want_str(ev: &mut Eval, args: &Attrs, key: &str) -> EResult<String> {
    let t = args.get(key).ok_or_else(|| alloc::format!("деривации нужен '{}'", key))?.clone();
    let v = ev.force(&t)?;
    Ok(ev.coerce_str(&v, false)?.s)
}

/// Помощник для тех, кому store не нужен: текст никуда не кладётся.
pub struct NoStore;

impl StoreText for NoStore {
    fn add(&self, _path: &str, _text: &str) -> Result<(), String> {
        Ok(())
    }
    fn read(&self, _path: &str) -> Option<String> {
        None
    }
}

