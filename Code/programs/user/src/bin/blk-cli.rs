//! Клиент драйвера блоков (Веха 12/15/17): `a0` = cap на эндпоинт сервера. Читает сектор 0
//! (магия VOIDFS), ЗАПИСЫВАЕТ паттерн в свободный сектор через сервер и читает его обратно
//! (доказывая запись). В конце пытается обратиться к диску НАПРЯМУЮ и подделать REPLY — оба
//! раза получает отказ ядра: у него нет cap на устройство, а эндпоинт-cap — не reply-cap.
#![no_std]
#![no_main]

use void_user as sys;

const BLK_WRITE_FLAG: usize = 1 << 40;
const TEST_SECTOR: usize = 20000; // заведомо свободный сектор (store использует низкие)
static PATTERN: &[u8] = b"VOID block-write via server works";

#[no_mangle]
pub extern "C" fn _start(ep_cap: usize, _a1: usize) -> ! {
    let mut buf = [0u8; 512];

    // Прочитать сектор 0 через сервер и напечатать магию.
    sys::call(ep_cap, 0, &[], &mut buf);
    sys::write(b"[blk-cli] sector 0 via driver-server: ");
    sys::write(&buf[..6]);
    sys::write(b"\n");

    // Веха 17: ЗАПИСЬ сектора TEST через сервер, затем чтение ОБРАТНО — доказательство записи.
    sys::call(ep_cap, TEST_SECTOR | BLK_WRITE_FLAG, PATTERN, &mut []);
    sys::call(ep_cap, TEST_SECTOR, &[], &mut buf);
    sys::write(b"[blk-cli] sector 20000 read back after write: ");
    sys::write(&buf[..PATTERN.len()]);
    sys::write(b"\n");

    // Попытка прямого доступа: BLK_READ с эндпоинт-cap (у клиента НЕТ cap на устройство).
    if sys::blk_read(ep_cap, 0, &mut buf) != 0 {
        sys::write(b"[blk-cli] direct disk read DENIED by kernel (no device capability)\n");
    }
    // Веха 15: попытка подделать REPLY эндпоинт-cap'ом (не reply-cap) → отказ ядра.
    if sys::reply(ep_cap, &buf[..8]) != 0 {
        sys::write(b"[blk-cli] forged REPLY DENIED by kernel (not a reply-capability)\n");
    }
    sys::exit(0);
}
