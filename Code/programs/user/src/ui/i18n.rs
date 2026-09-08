//! Подстановки в переведённые строки (Веха 178).
//!
//! Сам перевод и таблица живут в библиотеке (`void_user::i18n`): переводить текст должны уметь и
//! программы БЕЗ тулкита — шелл, например, — а тулкит требует кучи, которой у половины программ
//! системы нет. Здесь остаётся ровно то, чему куча и правда нужна: сборка строки.

use alloc::string::String;

use void_user as sys;

#[allow(unused_imports)] // не всякой программе нужны все три
pub use sys::i18n::{is_en, set_from_config, t};

/// Подставить одно значение в переведённый шаблон (место подстановки — `{}`).
pub fn f1(tmpl: &str, a: &str) -> String {
    fill(tmpl, &[a])
}

/// Подставить два значения.
pub fn f2(tmpl: &str, a: &str, b: &str) -> String {
    fill(tmpl, &[a, b])
}

/// Подставить три значения.
pub fn f3(tmpl: &str, a: &str, b: &str, c: &str) -> String {
    fill(tmpl, &[a, b, c])
}

/// Замена `{}` по порядку. Лишние места подстановки остаются как есть — это видно на экране, и
/// это лучше, чем молча потерять число.
fn fill(tmpl: &str, args: &[&str]) -> String {
    let mut out = String::with_capacity(tmpl.len() + 16);
    let mut rest = tmpl;
    let mut i = 0usize;
    while let Some(p) = rest.find("{}") {
        out.push_str(&rest[..p]);
        match args.get(i) {
            Some(a) => out.push_str(a),
            None => out.push_str("{}"),
        }
        i += 1;
        rest = &rest[p + 2..];
    }
    out.push_str(rest);
    out
}
