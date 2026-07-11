//! Раздатчик прав (Веха 21.1): держит cap на store `[rw-g-]` (`a0`) и на любой запрос отвечает
//! УРЕЗАННОЙ копией своего права `[r-g--]`: `CAP_DERIVE` (аттенуация у себя) + `REPLY` с правом
//! в сообщении (grant-в-ответе). Данные он не передаёт вовсе — только право; клиент дальше
//! ходит в store сам. Это паттерн KeyKOS: сервер как источник полномочий.
#![no_std]
#![no_main]

use void_user as sys;

const RO_MASK: usize = 0b101; // READ | GRANT: выдаваемое право (без WRITE — аттенуация)

#[no_mangle]
pub extern "C" fn _start(store_cap: usize, _a1: usize) -> ! {
    let mut req = [0u8; 64];
    loop {
        // op/нагрузка/приложенное право не важны — любой запрос = «дай почитать».
        let m = sys::recv(&mut req);
        let ro = sys::cap_derive(store_cap, RO_MASK);
        sys::reply_full(m.reply_cap, &[], ro); // пустое сообщение, только право
    }
}
