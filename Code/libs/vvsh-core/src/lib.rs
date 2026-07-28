//! vvsh-core — маленький гомоиконный Lisp VOID (ADR 0006): reader + значения + вычислитель +
//! нормализатор системного конфига. `no_std`+`alloc`, без зависимостей.
//!
//! Конвейер: текст `.vv` → парсинг → вычисление НА VOID → нормализованный конфиг (те же строки
//! `service …`/`shell …`, что сегодня даёт `nix/system.nix`). Потребитель — программа
//! `bin/<arch>/vvsh` (A2); ядро крейт не линкует. Host-тестируем (`no_std` только вне `test`) —
//! парсер/eval/import гоняем `cargo test` без QEMU.
//!
//! - M1a: [`build_config`] — один файл, без импортов.
//! - M1b: [`build_config_with`] + [`ModuleLoader`] — `(import "модуль.vv")` со слиянием (`append`).
//!
//! Раскладка «FS-источник + поколение-сборка» и вехи — в [[vvsh-config-layout]].

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod config;
pub mod eval;
pub mod reader;
pub mod value;

pub use config::normalize_config;
pub use eval::{eval_program, root_env, Interp, ModuleLoader, NoLoader};
pub use reader::{read_all, ReadError};
pub use value::{Env, EvalError, Value};

use alloc::string::String;

/// Полный конвейер с загрузчиком модулей: текст `.vv` → нормализованный конфиг (или текст ошибки).
/// `import` внутри резолвится через `loader` (бинарь — posixfs, тесты — карта в памяти).
pub fn build_config_with(src: &str, loader: &dyn ModuleLoader) -> Result<String, String> {
    let interp = Interp::new(loader);
    let forms = read_all(src).map_err(|e| e.0)?;
    let val = interp.eval_program(&forms).map_err(|e| e.0)?;
    normalize_config(&val).map_err(|e| e.0)
}

/// Как [`build_config_with`], но без загрузчика: любой `import` — ошибка. Для конфига из одного файла.
pub fn build_config(src: &str) -> Result<String, String> {
    build_config_with(src, &NoLoader)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::cell::Cell;

    fn eval_str(src: &str) -> Value {
        let forms = read_all(src).expect("парсинг");
        eval_program(&forms).expect("вычисление")
    }

    #[test]
    fn arithmetic() {
        assert_eq!(eval_str("(+ 1 2 3)"), Value::Int(6));
        assert_eq!(eval_str("(- 10 3 2)"), Value::Int(5));
        assert_eq!(eval_str("(- 5)"), Value::Int(-5));
        assert_eq!(eval_str("(* 2 3 4)"), Value::Int(24));
        assert_eq!(eval_str("(+ (* 2 3) (- 10 4))"), Value::Int(12));
    }

    #[test]
    fn let_if_cond() {
        assert_eq!(eval_str("(let ((x 2) (y 3)) (+ x y))"), Value::Int(5));
        assert_eq!(eval_str("(if #t 1 2)"), Value::Int(1));
        assert_eq!(eval_str("(if #f 1 2)"), Value::Int(2));
        assert_eq!(eval_str("(cond (#f 1) (#t 2) (else 3))"), Value::Int(2));
        assert_eq!(eval_str("(cond (#f 1) (else 3))"), Value::Int(3));
    }

    #[test]
    fn closures_and_define() {
        assert_eq!(eval_str("(define (sq x) (* x x)) (sq 7)"), Value::Int(49));
        assert_eq!(
            eval_str("(define add (lambda (a b) (+ a b))) (add 4 5)"),
            Value::Int(9)
        );
        assert_eq!(
            eval_str("(define (adder n) (lambda (x) (+ x n))) (define inc (adder 1)) (inc 41)"),
            Value::Int(42)
        );
    }

    #[test]
    fn list_ops() {
        assert_eq!(
            eval_str("(append (list 1 2) (list 3))"),
            Value::list(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
        );
        assert_eq!(eval_str("(car (list 1 2 3))"), Value::Int(1));
        assert_eq!(
            eval_str("(cdr (list 1 2 3))"),
            Value::list(vec![Value::Int(2), Value::Int(3)])
        );
        assert_eq!(eval_str("(null? (list))"), Value::Bool(true));
        assert_eq!(eval_str("(null? (list 1))"), Value::Bool(false));
        assert_eq!(eval_str("(= 2 (+ 1 1))"), Value::Bool(true));
    }

    #[test]
    fn quote_and_atoms() {
        assert_eq!(eval_str("'foo"), Value::sym("foo"));
        assert_eq!(eval_str("\"hi\\nthere\""), Value::str("hi\nthere"));
        assert_eq!(eval_str("; коммент\n42"), Value::Int(42));
    }

    /// Конфиг из одного файла (M1a): с `net #t` — те же строки, что нынешний gen1.
    const DEFAULT_VV: &str = r#"
;; default.vv — конфиг ВЫЧИСЛЯЕТСЯ в те же service/shell-строки, что nix/system.nix
(define net #t)
(system
  (append
    (list (service "posixfs" "store:rw"))
    (if net (list (service "net-srv" "dev:net:rw")) (list))
    (list (shell "vsh"
                 "endpoint:posixfs" "store:xw"
                 (if net (list "endpoint:net-srv") (list))
                 "env"))))
"#;

    const GEN1: &str = "service posixfs store:rw\n\
                        service net-srv dev:net:rw\n\
                        shell vsh endpoint:posixfs store:xw endpoint:net-srv env\n";
    const GEN2: &str = "service posixfs store:rw\n\
                        shell vsh endpoint:posixfs store:xw env\n";

    #[test]
    fn normalizes_full_config() {
        assert_eq!(build_config(DEFAULT_VV).expect("сборка"), GEN1);
    }

    #[test]
    fn net_off_matches_gen2() {
        let src = DEFAULT_VV.replace("(define net #t)", "(define net #f)");
        assert_eq!(build_config(&src).expect("сборка"), GEN2);
    }

    // ── M1b: import + слияние ────────────────────────────────────────────────

    /// Загрузчик модулей из карты в памяти + счётчик загрузок (для проверки кэша).
    struct MapLoader {
        mods: Vec<(&'static str, &'static str)>,
        loads: Cell<usize>,
    }
    impl MapLoader {
        fn new(mods: Vec<(&'static str, &'static str)>) -> Self {
            MapLoader { mods, loads: Cell::new(0) }
        }
    }
    impl ModuleLoader for MapLoader {
        fn load(&self, name: &str) -> Result<String, String> {
            self.loads.set(self.loads.get() + 1);
            self.mods
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, s)| String::from(*s))
                .ok_or_else(|| alloc::format!("нет модуля '{}'", name))
        }
    }

    /// Модульный конфиг из четырёх `.vv` воспроизводит gen1 ТОЧНО (import + append-слияние).
    #[test]
    fn imports_and_merges_to_gen1() {
        let loader = MapLoader::new(vec![
            ("services.vv", r#"(list (service "posixfs" "store:rw"))"#),
            ("networking.vv", r#"(list (service "net-srv" "dev:net:rw"))"#),
            (
                "shell.vv",
                r#"(list (shell "vsh" "endpoint:posixfs" "store:xw" "endpoint:net-srv" "env"))"#,
            ),
        ]);
        let default = r#"(system (append (import "services.vv")
                                         (import "networking.vv")
                                         (import "shell.vv")))"#;
        assert_eq!(build_config_with(default, &loader).expect("сборка"), GEN1);
    }

    /// Модуль сам вычисляет свой вклад (define/if внутри модуля).
    #[test]
    fn module_can_compute_its_contribution() {
        let loader = MapLoader::new(vec![(
            "net.vv",
            r#"(define on #t) (if on (list (service "net-srv" "dev:net:rw")) (list))"#,
        )]);
        let out = build_config_with(r#"(system (import "net.vv"))"#, &loader).expect("сборка");
        assert_eq!(out, "service net-srv dev:net:rw\n");
    }

    /// Один и тот же модуль, импортированный дважды, грузится РАЗ (кэш по имени).
    #[test]
    fn import_is_cached() {
        let loader = MapLoader::new(vec![(
            "svc.vv",
            r#"(list (service "posixfs" "store:rw"))"#,
        )]);
        let default = r#"(system (append (import "svc.vv") (import "svc.vv")))"#;
        let out = build_config_with(default, &loader).expect("сборка");
        assert_eq!(out, "service posixfs store:rw\nservice posixfs store:rw\n");
        assert_eq!(loader.loads.get(), 1, "модуль должен грузиться один раз");
    }

    /// Циклический импорт детектируется, а не зависает.
    #[test]
    fn import_cycle_detected() {
        let loader = MapLoader::new(vec![
            ("a.vv", r#"(import "b.vv")"#),
            ("b.vv", r#"(import "a.vv")"#),
        ]);
        let err = build_config_with(r#"(import "a.vv")"#, &loader).unwrap_err();
        assert!(err.contains("цикл"), "ожидали ошибку цикла, получили: {}", err);
    }

    /// Импорт отсутствующего модуля — ошибка от загрузчика.
    #[test]
    fn import_missing_errors() {
        let loader = MapLoader::new(vec![]);
        let err = build_config_with(r#"(system (import "нет.vv"))"#, &loader).unwrap_err();
        assert!(err.contains("нет модуля"), "получили: {}", err);
    }

    /// Без загрузчика `import` — честная ошибка (не паника).
    #[test]
    fn import_without_loader_errors() {
        let err = build_config(r#"(system (import "x.vv"))"#).unwrap_err();
        assert!(err.contains("загрузчик"), "получили: {}", err);
    }
}
