//! Клиент store-сервера (Веха 13/14): `a0` = cap на эндпоинт сервера. Кладёт сообщение (put),
//! получает его content-id, привязывает к именованному корню `greeting` и на СЛЕДУЮЩЕМ запуске
//! читает прежнее значение обратно по этому корню — персистентность через userspace-сервер.
//! Затем пытается `OBJ_PUT` НАПРЯМУЮ (эндпоинт-cap, не store) → отказ ядра.
#![no_std]
#![no_main]

use void_user as sys;

const OP_PUT: usize = 0;
const OP_GET: usize = 1;
const OP_SET_ROOT: usize = 2;
const OP_GET_ROOT: usize = 3;

static MSG: &[u8] = b"hello from VOID object-store client";
static GREETING: &[u8] = b"greeting";

#[no_mangle]
pub extern "C" fn _start(ep_cap: usize, _a1: usize) -> ! {
    let mut id = [0u8; 32];
    let mut val = [0u8; 512];

    // 1. GET_ROOT("greeting") → content-id (32 байта), либо пусто (корня ещё нет).
    let n = sys::call(ep_cap, OP_GET_ROOT, GREETING, &mut id);
    if n == 32 {
        // Корень есть с прошлого запуска: прочитать по нему значение и напечатать.
        let vlen = sys::call(ep_cap, OP_GET, &id, &mut val);
        sys::write(b"[store-cli] root 'greeting' from previous boot: ");
        sys::write(&val[..vlen]);
        sys::write(b"\n");
    } else {
        sys::write(b"[store-cli] root 'greeting' not set yet (first boot)\n");
    }

    // 2. PUT(MSG) → content-id значения.
    sys::call(ep_cap, OP_PUT, MSG, &mut id);

    // 3–4. SET_ROOT: запрос = [id(32) | "greeting"] — привязать корень (переживёт перезагрузку).
    let mut rq = [0u8; 32 + 8];
    rq[..32].copy_from_slice(&id);
    rq[32..].copy_from_slice(GREETING);
    sys::call(ep_cap, OP_SET_ROOT, &rq, &mut []);
    sys::write(b"[store-cli] value stored and bound to root 'greeting' (survives reboot)\n");

    // 5. Попытка прямого доступа: OBJ_PUT с эндпоинт-cap (у клиента НЕТ cap на store).
    if sys::obj_put(ep_cap, MSG, &mut id) != 0 {
        sys::write(b"[store-cli] direct OBJ_PUT DENIED by kernel (no store capability)\n");
    }
    sys::exit(0);
}
