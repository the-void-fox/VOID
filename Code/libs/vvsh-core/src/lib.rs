//! vvsh-core — маленький гомоиконный Lisp VOID (ADR 0006): reader + значения + вычислитель +
//! нормализатор системного конфига. `no_std`+`alloc`, без зависимостей.
//!
//! M1a: конвейер [`build_config`] — текст `.vv` → парсинг → вычисление НА VOID → нормализованный
//! конфиг (те же строки `service …`/`shell …`, что сегодня даёт `nix/system.nix`). Потребитель —
//! программа `bin/<arch>/vvsh` (A2); ядро крейт не линкует. Host-тестируем (см. ниже `no_std`
//! только вне `test`) — парсер/eval гоняем `cargo test` без QEMU.
//!
//! Раскладка «FS-источник + поколение-сборка» и вехи — в [[vvsh-config-layout]].

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod config;
pub mod eval;
pub mod reader;
pub mod value;

pub use config::normalize_config;
pub use eval::{eval, eval_program, root_env};
pub use reader::{read_all, ReadError};
pub use value::{Env, EvalError, Value};

use alloc::string::String;

/// Полный конвейер M1a: текст `.vv` → нормализованный системный конфиг (или текст ошибки).
pub fn build_config(src: &str) -> Result<String, String> {
    let forms = read_all(src).map_err(|e| e.0)?;
    let val = eval_program(&forms).map_err(|e| e.0)?;
    normalize_config(&val).map_err(|e| e.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

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
        // Замыкание захватывает окружение определения.
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

    /// Конфиг M1a: с `net #t` даёт те же строки, что нынешний gen1 (полный, с сетью).
    const DEFAULT_VV: &str = r#"
;; default.vv — M1a: конфиг ВЫЧИСЛЯЕТСЯ в те же service/shell-строки, что nix/system.nix
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

    #[test]
    fn normalizes_full_config() {
        let out = build_config(DEFAULT_VV).expect("сборка");
        assert_eq!(
            out,
            "service posixfs store:rw\n\
             service net-srv dev:net:rw\n\
             shell vsh endpoint:posixfs store:xw endpoint:net-srv env\n"
        );
    }

    /// `net #f` → сеть выпадает и из сервиса, и из прав shell'а: ровно нынешний gen2 (без сети).
    #[test]
    fn net_off_matches_gen2() {
        let src = DEFAULT_VV.replace("(define net #t)", "(define net #f)");
        let out = build_config(&src).expect("сборка");
        assert_eq!(
            out,
            "service posixfs store:rw\n\
             shell vsh endpoint:posixfs store:xw env\n"
        );
    }
}
