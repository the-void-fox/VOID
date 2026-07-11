//! Демо гибели процесса (Веха 22.2): честно предупреждает и лезет по немапленному адресу вне
//! кучи. Ядро убивает ЕГО, а не паникует само — остальная система живёт дальше. `write_volatile`,
//! чтобы оптимизатор не выкинул запись по «мусорному» адресу (для него это UB-паттерн, для нас —
//! суть демо).
#![no_std]
#![no_main]

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    void_user::write(b"[crash] dereferencing 0x75000000 (unmapped, outside heap)...\n");
    unsafe { core::ptr::write_volatile(0x7500_0000 as *mut u8, 1) }; // ← store page fault, процесс убит
    void_user::exit(0); // не достигается
}
