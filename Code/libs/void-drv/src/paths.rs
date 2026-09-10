//! Арифметика адресов store: путь ВЫЧИСЛЯЕТСЯ, а не назначается.
//!
//! Это самая неочевидная часть nix и одновременно самая жёсткая: имя пакета не выбирают, его
//! считают из того, ЧЕМ пакет будет собран. Отсюда всё остальное — и то, что одинаковые входы
//! дают один путь на любой машине, и то, что изменение хоть одного байта задания даёт другой
//! адрес, а значит другую сборку рядом со старой.
//!
//! ## Ни одной вольности
//!
//! Каждая строка ниже — перевод конкретного места из nix (`makeStorePath`, `compressHash`,
//! `printHash32`, `hashDerivationModulo`, `Derivation::unparse`). Свобода здесь ровно нулевая:
//! лишний пробел, другой порядок, другой алфавит — и путь разойдётся с настоящим, то есть
//! собранное здесь никогда не совпадёт с собранным где-либо ещё. А вся ценность nix ровно в
//! обратном.
//!
//! Проверяется это единственным осмысленным способом — совпадением с живым `nix-instantiate`:
//! тесты внизу держат пути, снятые с хоста.
//!
//! ## Порядок вычисления
//!
//! ```text
//! задание с ПУСТЫМИ путями выходов ──sha256──→ хэш деривации
//!                                                 │
//!            "output:out:sha256:<хэш>:/nix/store:<имя>" ──sha256──→ сжать в 20 байт ──base32──→ ПУТЬ ВЫХОДА
//!                                                 │
//! задание с ЗАПОЛНЕННЫМИ путями ──sha256──→ "text:...:sha256:<хэш>:/nix/store:<имя>.drv" ──→ ПУТЬ ЗАДАНИЯ
//! ```
//!
//! Порядок именно такой и другим быть не может: путь выхода входит в текст задания, а хэш
//! задания считается по тексту — поэтому выход считается по тексту БЕЗ выходов.

use alloc::string::String;
use alloc::vec::Vec;

use sha2::{Digest, Sha256};

use crate::Drv;

/// Каталог store. Участвует в отпечатке, поэтому это не настройка: другой каталог — другие пути.
pub const STORE_DIR: &str = "/nix/store";

/// Алфавит nix-base32 — СВОЙ, не RFC 4648: выброшены `e`, `o`, `u`, `t`.
const NIX32: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

pub fn hex(bytes: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(D[(b >> 4) as usize] as char);
        s.push(D[(b & 15) as usize] as char);
    }
    s
}

/// Сжать хэш до 20 байт: `out[i % 20] ^= in[i]`. Именно XOR-свёртка, а не обрезание — так в nix
/// (`compressHash`), и от выбора зависит адрес.
pub fn compress20(h: &[u8; 32]) -> [u8; 20] {
    let mut out = [0u8; 20];
    for (i, b) in h.iter().enumerate() {
        out[i % 20] ^= b;
    }
    out
}

/// nix-base32: разряды читаются С КОНЦА, по 5 бит, и старший символ идёт первым.
pub fn base32(h: &[u8]) -> String {
    let len = (h.len() * 8 - 1) / 5 + 1;
    let mut s = String::with_capacity(len);
    for n in (0..len).rev() {
        let b = n * 5;
        let i = b / 8;
        let j = b % 8;
        let lo = h[i] >> j;
        // При `j == 0` сдвиг был бы на все 8 разрядов: в C значение расширяется до int и старшие
        // биты всё равно отсекает `& 0x1f`, а u8 на этом переполняется. Значит верхней части
        // просто нет.
        let hi = if j == 0 || i + 1 >= h.len() { 0 } else { h[i + 1] << (8 - j) };
        s.push(NIX32[((lo | hi) & 0x1f) as usize] as char);
    }
    s
}

/// `makeStorePath`: отпечаток `<тип>:sha256:<hex>:<каталог store>:<имя>` → сжатие → base32.
pub fn store_path(ty: &str, hash_hex: &str, name: &str) -> String {
    let mut s = String::new();
    s.push_str(ty);
    s.push_str(":sha256:");
    s.push_str(hash_hex);
    s.push(':');
    s.push_str(STORE_DIR);
    s.push(':');
    s.push_str(name);
    let h = compress20(&sha256(s.as_bytes()));
    let mut out = String::from(STORE_DIR);
    out.push('/');
    out.push_str(&base32(&h));
    out.push('-');
    out.push_str(name);
    out
}

/// Путь ВЫХОДА деривации. Имя выхода входит и в тип (`output:dev`), и в имя пути (`p-dev`);
/// у главного выхода `out` имя пути остаётся именем пакета.
pub fn out_path(drv_hash: &[u8; 32], output: &str, drv_name: &str) -> String {
    let mut ty = String::from("output:");
    ty.push_str(output);
    let mut name = String::from(drv_name);
    if output != "out" {
        name.push('-');
        name.push_str(output);
    }
    store_path(&ty, &hex(drv_hash), &name)
}

/// Путь ТЕКСТОВОГО объекта (у нас это сам `.drv`). Ссылки входят в ТИП: `text:<путь>:<путь>`.
/// Не в содержимое и не в имя — именно в тип, и порядок их тот, в котором они даны.
pub fn text_path(text: &[u8], refs: &[String], name: &str) -> String {
    let mut ty = String::from("text");
    for r in refs {
        ty.push(':');
        ty.push_str(r);
    }
    store_path(&ty, &hex(&sha256(text)), name)
}

/// Хэш ДЕРИВАЦИИ: sha256 её текста, напечатанного с ПУСТЫМИ путями выходов.
///
/// Пустыми — потому что путь выхода считается из этого хэша, и включи мы его в исходные данные,
/// получилось бы уравнение с самим собой. Маскируются два места сразу: поля путей в списке
/// выходов и значения тех переменных окружения, чьё ИМЯ совпадает с именем выхода.
///
/// `Err` — у деривации есть входы-деривации. Их пути в тексте nix заменяет на их собственные
/// хэши (`hashDerivationModulo` рекурсивна), а мы графа сборок пока не строим вовсе; отвечать
/// здесь числом значило бы отвечать НЕ ТЕМ числом.
pub fn drv_hash(d: &Drv) -> Result<[u8; 32], &'static str> {
    if !d.input_drvs.is_empty() {
        return Err("хэш деривации со входами-деривациями требует обхода графа");
    }
    Ok(sha256(crate::print_masked(d).as_bytes()))
}

/// Хэш деривации С УЧЁТОМ ГРАФА (Веха 190) — `hashDerivationModulo` целиком.
///
/// Каждый вход-деривация заменяется в тексте на СВОЙ хэш, посчитанный этой же функцией. Отсюда
/// главное свойство nix: два задания, отличающиеся только тем, ЧЕРЕЗ КАКОЙ путь пришла та же
/// самая зависимость, дают один адрес. Список входов после подмены упорядочен по хэшу — так его
/// печатает `std::map` в nix, и порядок здесь часть отпечатка.
///
/// `mask` различает два случая, и путать их нельзя: у задания, которое СОЗДАЁТСЯ, путей выходов
/// ещё нет (маскируем), а у входа они уже есть и в хэш входят (не маскируем). Ровно так в nix:
/// `derivationStrict` зовёт с маской, `pathDerivationModulo` — без.
pub fn drv_hash_modulo(
    d: &Drv,
    mask: bool,
    resolve: &mut dyn FnMut(&str) -> Option<Drv>,
) -> Result<[u8; 32], String> {
    let mut inputs: Vec<(String, Vec<String>)> = Vec::new();
    for (path, outs) in &d.input_drvs {
        let inner = resolve(path)
            .ok_or_else(|| alloc::format!("задания-входа нет в store: {}", path))?;
        let h = drv_hash_modulo(&inner, false, resolve)?;
        inputs.push((hex(&h), outs.clone()));
    }
    inputs.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(sha256(crate::print_modulo(d, mask, &inputs).as_bytes()))
}

/// Ссылки, которые несёт текст `.drv`: исходники и задания-входы.
pub fn drv_refs(d: &Drv) -> Vec<String> {
    let mut refs: Vec<String> = Vec::new();
    for (p, _) in &d.input_drvs {
        refs.push(p.clone());
    }
    refs.extend(d.input_srcs.iter().cloned());
    refs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Output;
    use alloc::string::ToString;
    use alloc::vec;

    /// Собрать задание так, как его собрал бы вычислитель из выражения nix.
    fn drv(name: &str, args: &[&str], extra: &[(&str, &str)], outs: &[&str]) -> Drv {
        let mut d = Drv {
            outputs: Vec::new(),
            input_drvs: Vec::new(),
            input_srcs: Vec::new(),
            system: "x86_64-linux".to_string(),
            builder: "/bin/sh".to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            env: Vec::new(),
        };
        d.env.push(("builder".to_string(), d.builder.clone()));
        d.env.push(("name".to_string(), name.to_string()));
        for (k, v) in extra {
            d.env.push((k.to_string(), v.to_string()));
        }
        d.env.push(("system".to_string(), d.system.clone()));
        for o in outs {
            d.outputs.push(Output {
                name: o.to_string(),
                path: String::new(),
                hash_algo: String::new(),
                hash: String::new(),
            });
            d.env.push((o.to_string(), String::new()));
        }
        d.outputs.sort_by(|a, b| a.name.cmp(&b.name));
        d.env.sort_by(|a, b| a.0.cmp(&b.0));
        d
    }

    /// Достроить пути выходов и вернуть путь самого задания — то же, что делает `derivation`.
    fn finish(mut d: Drv, name: &str) -> (Drv, String) {
        let h = drv_hash(&d).expect("входов-дериваций нет");
        for i in 0..d.outputs.len() {
            let p = out_path(&h, &d.outputs[i].name, name);
            d.outputs[i].path = p.clone();
            let key = d.outputs[i].name.clone();
            for e in d.env.iter_mut() {
                if e.0 == key {
                    e.1 = p.clone();
                }
            }
        }
        let text = crate::print(&d);
        let refs = drv_refs(&d);
        let mut fname = String::from(name);
        fname.push_str(".drv");
        let path = text_path(text.as_bytes(), &refs, &fname);
        (d, path)
    }

    /// Пути сняты с ЖИВОГО `nix-instantiate` — придумывать их здесь нельзя, тогда тест проверял
    /// бы наше представление о nix, а не совпадение с ним.
    #[test]
    fn простая_деривация_совпадает_с_хостом() {
        let d = drv(
            "privet",
            &["sh", "-c", "echo privet-iz-pesochnicy > $out"],
            &[],
            &["out"],
        );
        let mut d = d;
        d.builder = "/bin/busybox".to_string();
        for e in d.env.iter_mut() {
            if e.0 == "builder" {
                e.1 = "/bin/busybox".to_string();
            }
        }
        let (d, path) = finish(d, "privet");
        assert_eq!(
            d.output("out"),
            Some("/nix/store/r1dlm00ran3afxw95w3v9mygvf76adk4-privet")
        );
        assert_eq!(path, "/nix/store/4y7xf0g9p4zwnspck4ym59vd5jn4l078-privet.drv");
    }

    #[test]
    fn два_выхода_совпадают_с_хостом() {
        let mut d = drv("p", &[], &[("outputs", "out dev")], &["out", "dev"]);
        d.env.sort_by(|a, b| a.0.cmp(&b.0));
        let (d, path) = finish(d, "p");
        assert_eq!(d.output("out"), Some("/nix/store/jrvcyq6pj1kb1r7563lbyri1rwvqqv5i-p"));
        assert_eq!(d.output("dev"), Some("/nix/store/392kyryc17rf9cjcwb4jxpg0gg4kklff-p-dev"));
        assert_eq!(path, "/nix/store/5di10lhi7p1schncwdl36fvfnbiybcw4-p.drv");
    }

    /// Приведение значений к тексту (`true` → `1`, `false` → пусто, список — через пробел) —
    /// тоже часть адреса: другой текст даёт другой путь.
    #[test]
    fn приведение_значений_совпадает_с_хостом() {
        let d = drv(
            "p",
            &[],
            &[("flag", "1"), ("l", "a b"), ("n", "7"), ("off", "")],
            &["out"],
        );
        let (d, path) = finish(d, "p");
        assert_eq!(d.output("out"), Some("/nix/store/ffa5vm6cn5klzp4d7i1ihi92lvdifg7h-p"));
        assert_eq!(path, "/nix/store/p10ipzypd0l8vv95f8szf5ggd6mrpbrb-p.drv");
    }

    #[test]
    fn base32_читается_с_конца() {
        // 20 нулей дают 32 нуля алфавита — простейшая проверка длины и порядка.
        assert_eq!(base32(&[0u8; 20]).len(), 32);
        assert_eq!(base32(&[0u8; 20]), "00000000000000000000000000000000");
    }
}
