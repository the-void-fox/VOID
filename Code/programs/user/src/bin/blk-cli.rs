//! Клиент драйвера блоков (Веха 12/15/17): `a0` = cap на эндпоинт сервера, `a1` = сектор,
//! отданный под опыты. Читает сектор 0 (магия VOIDFS), ЗАПИСЫВАЕТ туда паттерн через сервер и
//! читает его обратно (доказывая запись). В конце пытается обратиться к диску НАПРЯМУЮ и
//! подделать REPLY — оба раза получает отказ ядра: у него нет cap на устройство, а эндпоинт-cap
//! — не reply-cap.
//!
//! **Сектор приходит извне, и это не мелочь** (Веха 111). Раньше здесь стояло `20000` с
//! комментарием «заведомо свободный: store использует низкие». Store дорос до него и демо стала
//! затирать живой кадр — 512 байт посреди объекта, всплывшие как «кадр с диска не сошёлся со
//! своим content-id» в совсем другом месте и через сутки. Кто владеет носителем, тот и говорит,
//! куда можно писать: ядро берёт сектор у `void_store::scratch_sector`. Не сказали — не пишем.
#![no_std]
#![no_main]

use void_user as sys;

const BLK_WRITE_FLAG: usize = 1 << 40;
static PATTERN: &[u8] = b"VOID block-write via server works";

#[no_mangle]
pub extern "C" fn _start(ep_cap: usize, scratch: usize) -> ! {
    let mut buf = [0u8; 512];

    // Прочитать сектор 0 через сервер и напечатать магию.
    sys::call(ep_cap, 0, &[], &mut buf);
    sys::write(b"[blk-cli] sector 0 via driver-server: ");
    sys::write(&buf[..6]);
    sys::write(b"\n");

    // Веха 17: ЗАПИСЬ отведённого сектора через сервер, затем чтение ОБРАТНО — доказательство
    // записи. Без отведённого сектора демо записи просто не проводится: писать «куда-нибудь» на
    // носитель, которым владеет store, — ровно та ошибка, что стоила Вехе 111 испорченного кадра.
    if scratch == 0 {
        sys::write(b"[blk-cli] sector for experiments not given - write demo skipped\n");
    } else {
        sys::call(ep_cap, scratch | BLK_WRITE_FLAG, PATTERN, &mut []);
        sys::call(ep_cap, scratch, &[], &mut buf);
        sys::write(b"[blk-cli] scratch sector read back after write: ");
        sys::write(&buf[..PATTERN.len()]);
        sys::write(b"\n");
    }

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
