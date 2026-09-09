//! `lx-hello` — второй образ для проверки `execve` (Веха 182).
//!
//! Нужен ровно затем, чтобы `lx-probe` было НА ЧТО себя заменить: удачный `execve` не
//! возвращается, поэтому доказать его можно только тем, что заговорил другой бинарь.
//!
//! Сборка — та же, что у `lx-probe` (см. его шапку), и кладётся он в ИЕРАРХИЮ, а не корнем
//! программы: `execve` берёт ПУТЬ, а не имя корня store.
//!
//! ```sh
//! Code/tools/void-store-import <образ> put /tmp/lx-hello 'f/etc/lx-hello'
//! ```
#![no_std]
#![no_main]

use core::arch::asm;

unsafe fn sys3(n: usize, a: usize, b: usize, c: usize) -> isize {
    let r: isize;
    asm!("syscall", inlateout("rax") n as isize => r, in("rdi") a, in("rsi") b, in("rdx") c,
         lateout("rcx") _, lateout("r11") _, options(nostack));
    r
}

#[no_mangle]
pub extern "C" fn main(argc: i32, _argv: *const *const u8) -> i32 {
    let msg = "[lx-hello] это ДРУГОЙ образ — execve сработал, argc=".as_bytes();
    unsafe { sys3(1, 1, msg.as_ptr() as usize, msg.len()) };
    let d = [b'0' + (argc.clamp(0, 9) as u8), b'\n'];
    unsafe { sys3(1, 1, d.as_ptr() as usize, d.len()) };
    0
}

#[no_mangle]
pub unsafe extern "C" fn memset(d: *mut u8, c: i32, n: usize) -> *mut u8 {
    let mut i = 0;
    while i < n { *d.add(i) = c as u8; i += 1; }
    d
}

#[panic_handler]
fn ph(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
