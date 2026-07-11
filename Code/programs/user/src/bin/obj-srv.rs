//! Сервер объектного store в userspace (Веха 13/14): держит cap на store (`a0`) и обслуживает
//! put/get и set_root/get_root по IPC. Клиент store-syscall'ов не имеет — только этот сервер
//! их делает: персистентное пространство отдаётся как сервис под capability.
#![no_std]
#![no_main]

use void_user as sys;

const OP_PUT: usize = 0;
const OP_GET: usize = 1;
const OP_SET_ROOT: usize = 2;
// прочее = OP_GET_ROOT (3)

#[no_mangle]
pub extern "C" fn _start(store_cap: usize, _a1: usize) -> ! {
    let mut req = [0u8; 512];
    let mut val = [0u8; 512];
    let mut id = [0u8; 32];
    loop {
        let m = sys::recv(&mut req);
        match m.op {
            OP_PUT => {
                // сохранить нагрузку, вернуть её 32-байтный content-id
                sys::obj_put(store_cap, &req[..m.len], &mut id);
                sys::reply(m.reply_cap, &id);
            }
            OP_GET => {
                // req[..32] = id → прочитать значение
                id.copy_from_slice(&req[..32]);
                let n = sys::obj_get(store_cap, &id, &mut val);
                sys::reply(m.reply_cap, &val[..n]);
            }
            OP_SET_ROOT => {
                // req = [id(32) | name] → привязать корень (переживёт перезагрузку)
                id.copy_from_slice(&req[..32]);
                sys::obj_set_root(store_cap, &req[32..m.len], &id);
                sys::reply(m.reply_cap, &[]); // ack
            }
            _ => {
                // OP_GET_ROOT: req = name → content-id корня (32 байта) либо пусто
                let n = sys::obj_get_root(store_cap, &req[..m.len], &mut id);
                sys::reply(m.reply_cap, &id[..if n == 32 { 32 } else { 0 }]);
            }
        }
    }
}
