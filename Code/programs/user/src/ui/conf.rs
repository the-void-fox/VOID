//! Откуда тулкит берёт настройки: активное поколение конфига (Веха 144).
//!
//! Тем же способом его читают терминал и композитор — `system/current` называет имя, `system/<имя>`
//! хранит текст. Никакого «файла настроек интерфейса» рядом нет намеренно: вид системы обязан
//! откатываться вместе с системой ([[0017-settings-from-config]]), а второй источник правды
//! разошёлся бы с первым на первом же откате.

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use void_user as sys;

/// Текст активного поколения. `None` — права на store нет либо поколения ещё нет.
pub fn generation() -> Option<String> {
    let cap = store_cap()?;
    let name = read_root(cap, b"system/current")?;
    let name = core::str::from_utf8(&name).ok()?.trim().to_string();
    let mut root = b"system/".to_vec();
    root.extend_from_slice(name.as_bytes());
    String::from_utf8(read_root(cap, &root)?).ok()
}

/// Право на store: по имени (Веха 99.1), иначе перебором. Проба безобидна — чтение корня ничего
/// не меняет, а без права клиент просто останется при умолчаниях.
pub fn store_cap() -> Option<usize> {
    let readable = |c: usize| {
        let mut id = [0u8; 32];
        c != sys::NO_CAP && sys::obj_get_root(c, b"system/current", &mut id) == 32
    };
    sys::cap_named("STORE")
        .filter(|&c| readable(c))
        .or_else(|| (0..8).map(sys::start_cap).find(|&c| readable(c)))
}

/// Содержимое именованного корня store.
fn read_root(cap: usize, name: &[u8]) -> Option<Vec<u8>> {
    let mut id = [0u8; 32];
    if sys::obj_get_root(cap, name, &mut id) != 32 {
        return None;
    }
    let mut buf = vec![0u8; 64 * 1024];
    let n = sys::obj_get(cap, &id, &mut buf);
    if n == 0 || n > buf.len() {
        return None;
    }
    buf.truncate(n);
    Some(buf)
}
