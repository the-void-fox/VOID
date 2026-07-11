//! Драйвер блоков как userspace-сервер (Веха 12/17): принимает номер сектора, читает/пишет его
//! через шлюз ядра (предъявляя `dev_cap` — свой cap на устройство, `a0`) и отвечает клиенту по
//! IPC. В `op` запроса младшие биты = сектор, бит 40 = флаг записи.
#![no_std]
#![no_main]

use void_user as sys;

const BLK_WRITE_FLAG: usize = 1 << 40;

#[no_mangle]
pub extern "C" fn _start(dev_cap: usize, _a1: usize) -> ! {
    let mut req = [0u8; 512]; // данные запроса (для записи)
    let mut buf = [0u8; 512]; // буфер чтения/ответа
    loop {
        let m = sys::recv(&mut req);
        let sector = m.op & 0xffff_ffff; // младшие биты op = номер сектора
        if m.op & BLK_WRITE_FLAG == 0 {
            sys::blk_read(dev_cap, sector, &mut buf);
            sys::reply(m.reply_cap, &buf);
        } else {
            sys::blk_write(dev_cap, sector, &req[..m.len.min(512)]);
            sys::reply(m.reply_cap, &[]); // ack без нагрузки
        }
    }
}
