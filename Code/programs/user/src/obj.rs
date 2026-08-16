//! Объект store целиком — простой или блоб (Веха 139).
//!
//! Программе, которой нужен ФАЙЛ, всё равно, положили его одним куском или разрезали на части:
//! `fetch` режет всё длиннее 16 КиБ и связывает куски узлом, а мост с хоста кладёт одним
//! объектом. Разница видна только по манифесту, и разбирать её каждому потребителю по-своему
//! значит копировать один и тот же десяток строк — с разными ошибками в каждой копии.
//!
//! Память берётся `try_reserve`: файл выбирает человек, и «не хватило кучи» здесь такой же
//! нормальный ответ, как «нет такого корня», а не повод свалиться в аллокаторе.
//!
//! **Подключается модулем ПО ПУТИ** (`#[path = "../obj.rs"] mod obj;`) — по той же причине, что
//! [`archive`](../archive.rs) и [`roots`](../roots.rs): здесь нужен `alloc`, а объявить его в
//! библиотеке `void_user` значит потребовать глобальный аллокатор от всех программ, включая
//! живущие вовсе без кучи.

use alloc::vec::Vec;

use void_user as sys;

/// Прочитать объект целиком. `spec` — либо корень store (`f/etc/wall.png`), либо content-id
/// шестьюдесятью четырьмя шестнадцатеричными знаками.
///
/// Ошибка — строкой, годной для показа человеку: у всех отказов тут одна и та же судьба —
/// сообщение и выход, а разбирать их по видам некому.
pub fn read(store: usize, spec: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut id = [0u8; 32];
    if spec.len() == 64 && spec.iter().all(|b| b.is_ascii_hexdigit()) {
        let hex = |c: u8| match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => c - b'A' + 10,
        };
        for (i, pair) in spec.chunks(2).enumerate() {
            id[i] = hex(pair[0]) << 4 | hex(pair[1]);
        }
    } else if sys::obj_get_root(store, spec, &mut id) != 32 {
        return Err("нет такого корня в store");
    }

    // Первое чтение — с длиной: `obj_get_ex` говорит, сколько всего байт в объекте, даже если
    // в буфер влезло меньше. Манифест блоба заведомо короче 512 байт.
    let mut head = [0u8; 512];
    let (n, total) = sys::obj_get_ex(store, &id, &mut head);
    if n == 0 || n == usize::MAX {
        return Err("объект не читается");
    }
    let Some((size, nchunks, csize)) = sys::http::blob_info(&head[..n]) else {
        // Простой объект: перечитать целиком в буфер нужной длины.
        let mut out = room(total)?;
        out.resize(total, 0);
        if sys::obj_get(store, &id, &mut out) != total {
            return Err("объект прочитался не целиком");
        }
        return Ok(out);
    };

    let mut kids = alloc::vec![[0u8; 32]; nchunks];
    if sys::obj_children(store, &id, &mut kids) != nchunks {
        return Err("список кусков блоба не сошёлся");
    }
    let mut out = room(size)?;
    let mut buf = alloc::vec![0u8; csize];
    for kid in &kids {
        let n = sys::obj_get(store, kid, &mut buf);
        if n == 0 || n == usize::MAX {
            return Err("кусок блоба не читается");
        }
        out.extend_from_slice(&buf[..n]);
    }
    if out.len() != size {
        return Err("куски блоба не сложились в объявленную длину");
    }
    Ok(out)
}

/// Место под файл — с отказом вместо паники.
fn room(len: usize) -> Result<Vec<u8>, &'static str> {
    let mut v = Vec::new();
    v.try_reserve_exact(len).map_err(|_| "не хватило кучи под файл")?;
    Ok(v)
}
