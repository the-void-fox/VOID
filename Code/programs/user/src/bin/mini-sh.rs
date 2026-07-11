//! mini-shell (Веха 18.4): `a0` = дескриптор персоналии. Написан ЦЕЛИКОМ на POSIX-shim — ни
//! одного `ecall`, op-кода или capability в теле; «не знает» ни про IPC, ни про VOID. Играет
//! маленькую сессию `cat; echo > ; cat; ls` над `motd.txt` (файл переживает перезагрузку).
#![no_std]
#![no_main]

use void_user::posix as px;

static MOTD: &[u8] = b"motd.txt";
static MOTDMSG: &[u8] = b"VOID says hi, written by mini-echo, kept by the personality\n";
static CATLBL: &[u8] = b"[mini-sh] $ cat motd.txt\n";

#[no_mangle]
pub extern "C" fn _start(ep: usize, _a1: usize) -> ! {
    let mut buf = [0u8; 512];
    // $ cat motd.txt   (покажет содержимое с прошлой загрузки — на первой пусто)
    px::write(ep, px::STDOUT, CATLBL);
    px::cat(ep, MOTD);
    // $ echo "..." > motd.txt
    px::write(ep, px::STDOUT, b"[mini-sh] $ echo \"...\" > motd.txt\n");
    px::echo_to(ep, MOTD, MOTDMSG);
    // $ cat motd.txt   (только что записанное)
    px::write(ep, px::STDOUT, CATLBL);
    px::cat(ep, MOTD);
    // $ ls
    px::write(ep, px::STDOUT, b"[mini-sh] $ ls\n");
    let n = px::readdir(ep, &mut buf);
    px::write(ep, px::STDOUT, &buf[..n]);
    void_user::exit(0);
}
