//! Клиент раздатчика прав (Веха 21): стартует БЕЗ прав на store. Первая загрузка (`a1`=MAX):
//! просит право по IPC — сначала демонстративно пытается приложить к CALL свой эндпоинт-cap
//! (нет GRANT → отказ ядра ДО отправки), потом честно получает `[r-g--]` В ОТВЕТЕ сервера.
//! Следующая загрузка: ядро нашло право в ВОССТАНОВЛЕННОМ из `.cspace` домене и отдало его
//! дескриптор в `a1` — IPC не нужен, право пережило перезагрузку (тезис ADR 0002).
//! Полученным правом клиент читает корень 'system' напрямую; запись отклоняется (аттенуация).
#![no_std]
#![no_main]

use void_user as sys;

static SYSNAME: &[u8] = b"system";

#[no_mangle]
pub extern "C" fn _start(ep_cap: usize, restored: usize) -> ! {
    let mut id = [0u8; 32];
    let mut val = [0u8; 96];

    let cap = if restored != usize::MAX {
        sys::write(b"[cap-cli] store-cap RESTORED from previous boot (.cspace) - no re-grant needed\n");
        restored
    } else {
        // 1) Приложить к CALL свой ep-cap: на нём нет GRANT → ядро отклонит ДО отправки.
        let (r, _) = sys::call_full(ep_cap, 0, &[], &mut [], ep_cap);
        if r == usize::MAX {
            sys::write(b"[cap-cli] attaching ep-cap to CALL DENIED by kernel (no GRANT right)\n");
        }
        // 2) Честный запрос: право придёт вместе с ответом.
        sys::write(b"[cap-cli] asking cap-server for read access to the store...\n");
        let (_, got) = sys::call_full(ep_cap, 0, &[], &mut [], sys::NO_CAP);
        got
    };

    if cap != usize::MAX {
        // Читать СВОИМ правом, без сервера: GET_ROOT('system') → id, GET(id) → значение.
        if sys::obj_get_root(cap, SYSNAME, &mut id) == 32 {
            let vlen = sys::obj_get(cap, &id, &mut val);
            sys::write(b"[cap-cli] reading root 'system' with the cap: ");
            sys::write(&val[..vlen]);
            sys::write(b"\n");
        } else {
            sys::write(b"[cap-cli] root 'system' not set yet (fresh disk)\n");
        }
        // Запись тем же правом → отказ: у копии нет WRITE (аттенуация пережила и передачу,
        // и — на второй загрузке — перезагрузку).
        if sys::obj_put(cap, SYSNAME, &mut id) == usize::MAX {
            sys::write(b"[cap-cli] OBJ_PUT with the cap DENIED (attenuated: no WRITE)\n");
        }
    }
    sys::exit(0);
}
