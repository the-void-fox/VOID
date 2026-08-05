//! Список корней store: прочитать ЦЕЛИКОМ и разобрать по префиксу имени (Веха 107).
//!
//! Появился, когда на список корней встал второй потребитель: `vvsh` нумерует по нему поколения
//! системы (`system/gen<N>`), а `pkg` — поколения профиля (`pkg/profile/*/gen<N>`) и заводит по
//! два корня на каждый путь замыкания. Общего кода тут немного, но цена расхождения высокая:
//! **недосчитаться поколения — значит затереть существующее**, и обе копии ошиблись бы молча.
//!
//! Подключается модулем ПО ПУТИ (`#[path = "../roots.rs"] mod roots;`) по той же причине, что и
//! [`archive`](../archive.rs): здесь нужен `alloc`, а в библиотеке `void_user` это потребовало бы
//! глобальный аллокатор от всех программ, включая живущие без кучи.

use alloc::vec;
use alloc::vec::Vec;

use void_user as sys;

/// Ширина префикса строки списка: 12 hex короткого content-id + два пробела. Имя корня — дальше.
const NAME_OFF: usize = 14;

/// Весь список корней. `None` — прочитать целиком не вышло (нет прав на store либо список растёт
/// быстрее, чем мы просим): **частичный список не отдаётся** — на нём считают номера поколений, и
/// «корней не видно» должно быть отказом, а не тихо неверным ответом.
pub fn text(scap: usize) -> Option<Vec<u8>> {
    let mut cap = 8 * 1024;
    // Ядро называет полную длину, так что обычно хватает одного повтора; цикл — на случай, когда
    // между двумя вызовами кто-то завёл ещё корни.
    for _ in 0..8 {
        let mut buf = vec![0u8; cap];
        let (got, want) = sys::obj_list_roots_ex(scap, &mut buf)?;
        if want <= got {
            buf.truncate(got);
            return Some(buf);
        }
        cap = want;
    }
    None
}

/// Имена корней, начинающихся с `prefix`, — уже без самого префикса (`system/gen` → `3`).
pub fn suffixes<'a>(text: &'a [u8], prefix: &[u8]) -> Vec<&'a [u8]> {
    let mut out = Vec::new();
    for line in text.split(|&b| b == b'\n') {
        if line.len() <= NAME_OFF {
            continue;
        }
        if let Some(rest) = line[NAME_OFF..].strip_prefix(prefix) {
            out.push(rest);
        }
    }
    out
}

/// Разобрать десятичное число целиком (иначе `None`): номер поколения из суффикса корня.
pub fn number(s: &[u8]) -> Option<u32> {
    if s.is_empty() {
        return None;
    }
    let mut n: u32 = 0;
    for &b in s {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add((b - b'0') as u32)?;
    }
    Some(n)
}

/// Отсортированные номера поколений с именами вида `<prefix><N>`.
pub fn gen_numbers(text: &[u8], prefix: &[u8]) -> Vec<u32> {
    let mut nums: Vec<u32> = suffixes(text, prefix).into_iter().filter_map(number).collect();
    nums.sort_unstable();
    nums.dedup();
    nums
}
