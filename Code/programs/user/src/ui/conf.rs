//! Откуда тулкит берёт настройки: активное поколение конфига (Веха 144).
//!
//! Тем же способом его читают терминал и композитор — `system/current` называет имя, `system/<имя>`
//! хранит текст. Никакого «файла настроек интерфейса» рядом нет намеренно: вид системы обязан
//! откатываться вместе с системой ([[0017-settings-from-config]]), а второй источник правды
//! разошёлся бы с первым на первом же откате.
//!
//! Здесь только «ОТКУДА взять текст»: право на store, активное поколение, корень. На «КАК читается
//! строка» отвечает крейт `void-conf` (Веха 148.8) — тот же самый, каким разбирает конфиг ядро.

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use void_user as sys;

/// Текст активного поколения. `None` — права на store нет либо поколения ещё нет.
pub fn generation() -> Option<String> {
    let cap = store_cap()?;
    let name = generation_name()?;
    let mut root = b"system/".to_vec();
    root.extend_from_slice(name.as_bytes());
    String::from_utf8(read_root(cap, &root)?).ok()
}

/// ИМЯ активного поколения (`gen4`). Веха 145 — его показывает меню и им же подписана кнопка,
/// которая меню открывает: система, которой человек пользуется, обязана уметь назвать себя.
pub fn generation_name() -> Option<String> {
    let cap = store_cap()?;
    let name = read_root(cap, b"system/current")?;
    Some(core::str::from_utf8(&name).ok()?.trim().to_string())
}

/// Веха 145.1 — «кто эта машина»: `device name` и `device avatar`.
///
/// Пользователей в VOID нет вовсе, поэтому на вопрос «чьё это устройство» отвечает имя
/// УСТРОЙСТВА. Показывает его меню оболочки ([[menu]]).
pub fn device(text: &str, key: &str) -> Option<String> {
    void_conf::get(text, "device", key).map(|v| v.to_string())
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

/// Содержимое именованного корня store. Публично с Вехи 146.1: тем же путём строка запуска читает
/// ЯРЛЫКИ (`app/<арх>/<имя>`) — данные о программе, лежащие рядом с ней.
pub fn read_root(cap: usize, name: &[u8]) -> Option<Vec<u8>> {
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
