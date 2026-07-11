//! vsh — интерактивный shell VOID (Веха 20.4): НАСТОЯЩИЙ ввод с консоли. Написан на POSIX-shim:
//! stdin — это `posix::read(fd=0)` (блокируется до нажатий), файлы — open/read/write через
//! персоналию, запуск программ — `posix::spawn` (как `posix_spawn`+`wait`). Аргументы:
//! `a0` = дескриптор персоналии, `a1` = дескриптор права запускать программы из store (EXEC).
//!
//! Line-discipline на стороне программы: эхо набранного, backspace (`\x7f`/`\x08`), Enter =
//! `\r` (терминал) или `\n` (pipe). Команды: `ls`, `cat F`, `echo TEXT > F` (или просто печать),
//! `run NAME` (например `run bin/hello`), `help`, `exit` — последняя завершает сессию VOID.
#![no_std]
#![no_main]

use void_user as sys;
use void_user::posix as px;

static HELP: &[u8] = b"commands: ls | cat FILE | echo TEXT > FILE | run NAME | help | exit\n";

#[no_mangle]
pub extern "C" fn _start(ep: usize, xcap: usize) -> ! {
    let mut line = [0u8; 128]; // собираемая строка команды
    let mut inb = [0u8; 16]; // порция сырого ввода
    let mut out = [0u8; 512]; // ответы персоналии (ls)
    px::write(ep, px::STDOUT, HELP);
    loop {
        px::write(ep, px::STDOUT, b"vsh> ");
        // ── собрать строку: читать порциями, эхо, backspace, до Enter ──
        let mut llen = 0usize;
        'line: loop {
            let n = px::read(ep, px::STDIN, &mut inb); // блокируется, пока нет ввода
            for &b in &inb[..n] {
                if b == b'\r' || b == b'\n' {
                    px::write(ep, px::STDOUT, b"\n");
                    break 'line;
                } else if b == 0x7f || b == 0x08 {
                    if llen > 0 {
                        llen -= 1;
                        px::write(ep, px::STDOUT, b"\x08 \x08"); // затереть символ на терминале
                    }
                } else if b >= 0x20 && llen < line.len() - 1 {
                    line[llen] = b;
                    llen += 1;
                    px::write(ep, px::STDOUT, &line[llen - 1..llen]); // эхо
                }
            }
        }
        let cmd = &line[..llen];
        if cmd.is_empty() {
            continue;
        }
        if cmd == b"exit" {
            sys::exit(0);
        }
        if cmd == b"help" {
            px::write(ep, px::STDOUT, HELP);
            continue;
        }
        if cmd == b"ls" {
            let n = px::readdir(ep, &mut out);
            px::write(ep, px::STDOUT, &out[..n]);
            continue;
        }
        if let Some(path) = cmd.strip_prefix(b"cat ") {
            px::cat(ep, path);
            continue;
        }
        if let Some(body) = cmd.strip_prefix(b"echo ") {
            // `echo TEXT > FILE` — записать; без ` > ` — просто напечатать TEXT.
            let mut sep = usize::MAX; // позиция последнего " > "
            let mut k = 0usize;
            while k + 3 <= body.len() {
                if &body[k..k + 3] == b" > " {
                    sep = k;
                }
                k += 1;
            }
            if sep != usize::MAX && sep > 0 && sep + 3 < body.len() {
                px::echo_to(ep, &body[sep + 3..], &body[..sep]);
            } else {
                px::write(ep, px::STDOUT, body);
                px::write(ep, px::STDOUT, b"\n");
            }
            continue;
        }
        if let Some(name) = cmd.strip_prefix(b"run ") {
            let code = px::spawn(xcap, name);
            if code == usize::MAX {
                px::write(ep, px::STDOUT, b"vsh: run failed (no such program in store?)\n");
            } else {
                px::write(ep, px::STDOUT, b"vsh: program exited, code ");
                px::write(ep, px::STDOUT, &[b'0' + (code % 10) as u8, b'\n']);
            }
            continue;
        }
        px::write(ep, px::STDOUT, b"vsh: unknown command (try 'help')\n");
    }
}
