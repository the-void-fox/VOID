//! vsh — интерактивный shell VOID (Веха 20.4): НАСТОЯЩИЙ ввод с консоли. Написан на POSIX-shim:
//! stdin — это `posix::read(fd=0)` (блокируется до нажатий), файлы — open/read/write через
//! персоналию, запуск программ — `posix::spawn` (как `posix_spawn`+`wait`). Аргументы:
//! `a0` = дескриптор персоналии, `a1` = дескриптор права запускать программы из store (EXEC).
//!
//! Line-discipline на стороне программы: эхо набранного, backspace (`\x7f`/`\x08`), Enter =
//! `\r` (терминал) или `\n` (pipe). Команды: `ls`, `cat F`, `echo TEXT > F` (или просто печать),
//! `run NAME [ARGS…]` (Веха 30: слова после имени становятся argv ребёнка; например
//! `run bin/hello мир`), `thaw NAME` (Веха 37: разморозить процесс из образа
//! `proc/<arch>/NAME`), `switch GEN` / `sysdef GEN FILE` (Веха 40: выбрать/задать
//! поколение системы — декларативный init грузит `system/current` на следующей
//! загрузке), `mv OLD NEW` (rename персоналии), `clear` (Веха 43: ANSI-очистка экрана),
//! цветной `help`, `exit` — последняя завершает сессию VOID. Вывод цветной (ANSI-коды
//! понимают и терминал, и VGA-ядро на реальном железе, Веха 43).
#![no_std]
#![no_main]

use void_user as sys;
use void_user::posix as px;

/// ANSI-коды (работают и в терминале QEMU, и на VGA — ядро их толкует, Веха 43).
const RESET: &[u8] = b"\x1b[0m";
const C_CMD: &[u8] = b"\x1b[1;33m"; // жёлтый жирный — имя команды
const C_HEAD: &[u8] = b"\x1b[1;36m"; // голубой жирный — заголовки
const C_PROMPT: &[u8] = b"\x1b[1;32m"; // зелёный жирный — приглашение

/// Одна строка справки: имя команды (в цвете) + выравнивание + описание (кириллица — UTF-8).
fn help_row(ep: usize, cmd: &[u8], desc: &str) {
    px::write(ep, px::STDOUT, b"  ");
    px::write(ep, px::STDOUT, C_CMD);
    px::write(ep, px::STDOUT, cmd);
    px::write(ep, px::STDOUT, RESET);
    let pad = 18usize.saturating_sub(cmd.len());
    for _ in 0..pad {
        px::write(ep, px::STDOUT, b" ");
    }
    px::write(ep, px::STDOUT, desc.as_bytes());
    px::write(ep, px::STDOUT, b"\n");
}

/// Веха 43 — читаемая цветная справка (вместо одной длинной строки).
fn print_help(ep: usize) {
    px::write(ep, px::STDOUT, C_HEAD);
    px::write(ep, px::STDOUT, "  VOID vsh — команды:".as_bytes());
    px::write(ep, px::STDOUT, RESET);
    px::write(ep, px::STDOUT, b"\n");
    help_row(ep, b"ls", "список файлов");
    help_row(ep, b"cat FILE", "показать содержимое файла");
    help_row(ep, b"tail FILE", "последние ~32 байта файла");
    help_row(ep, b"echo TEXT > FILE", "записать текст в файл (без > — печать)");
    help_row(ep, b"run NAME [ARGS]", "запустить программу из store");
    help_row(ep, b"thaw NAME", "разморозить процесс из образа");
    help_row(ep, b"switch GEN", "выбрать поколение системы (после ребута)");
    help_row(ep, b"sysdef GEN FILE", "задать поколение из файла-конфига");
    help_row(ep, b"mv OLD NEW", "переименовать файл");
    help_row(ep, b"ping IP", "ICMP-пинг адреса A.B.C.D");
    help_row(ep, b"clear", "очистить экран");
    help_row(ep, b"help", "эта справка");
    help_row(ep, b"exit", "завершить сессию VOID");
}

/// Разобрать IPv4 в точечной записи «A.B.C.D» в 4 байта. `None` — не разобрать.
fn parse_ipv4(s: &[u8]) -> Option<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut idx = 0usize;
    let mut val: u32 = 0;
    let mut digits = 0;
    for &b in s {
        if b == b'.' {
            if digits == 0 || idx >= 3 {
                return None;
            }
            octets[idx] = val as u8;
            idx += 1;
            val = 0;
            digits = 0;
        } else if b.is_ascii_digit() {
            val = val * 10 + (b - b'0') as u32;
            if val > 255 {
                return None;
            }
            digits += 1;
        } else {
            return None;
        }
    }
    if idx != 3 || digits == 0 {
        return None;
    }
    octets[3] = val as u8;
    Some(octets)
}

/// Напечатать usize десятично (форматтера в no_std-бинаре нет).
fn put_dec(ep: usize, mut v: usize) {
    let mut nb = [0u8; 20];
    let mut n = 0;
    loop {
        nb[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
        if v == 0 {
            break;
        }
    }
    while n > 0 {
        n -= 1;
        px::write(ep, px::STDOUT, &nb[n..n + 1]);
    }
}

#[no_mangle]
pub extern "C" fn _start(ep: usize, xcap: usize) -> ! {
    let mut line = [0u8; 128]; // собираемая строка команды
    let mut inb = [0u8; 16]; // порция сырого ввода
    let mut out = [0u8; 512]; // ответы персоналии (ls)
    print_help(ep);
    loop {
        // Веха 43: цветное приглашение (зелёный `vsh>`). ANSI толкует и терминал, и VGA-ядро.
        px::write(ep, px::STDOUT, C_PROMPT);
        px::write(ep, px::STDOUT, b"vsh>");
        px::write(ep, px::STDOUT, RESET);
        px::write(ep, px::STDOUT, b" ");
        // ── собрать строку: читать порциями, эхо, backspace, до Enter ──
        // Эхо — ПАЧКОЙ на порцию ввода, не по байту: SYS_WRITE валидирует UTF-8, и
        // разрезанный посередине двухбайтный символ (кириллица) печатался бы как «<?>».
        let mut llen = 0usize;
        'line: loop {
            let n = px::read(ep, px::STDIN, &mut inb); // блокируется, пока нет ввода
            let mut from = llen; // начало ещё не показанного хвоста line[from..llen]
            for &b in &inb[..n] {
                if b == b'\r' || b == b'\n' {
                    if llen > from {
                        px::write(ep, px::STDOUT, &line[from..llen]);
                    }
                    px::write(ep, px::STDOUT, b"\n");
                    break 'line;
                } else if b == 0x7f || b == 0x08 {
                    if llen > from {
                        llen -= 1; // байт ещё не показан — просто забыть
                    } else if llen > 0 {
                        llen -= 1;
                        from = llen;
                        px::write(ep, px::STDOUT, b"\x08 \x08"); // затереть символ на терминале
                    }
                } else if b >= 0x20 && llen < line.len() - 1 {
                    line[llen] = b;
                    llen += 1;
                }
            }
            if llen > from {
                px::write(ep, px::STDOUT, &line[from..llen]); // эхо принятого целиком
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
            print_help(ep);
            continue;
        }
        if cmd == b"clear" {
            // Веха 43: ANSI-очистка экрана + курсор в начало (VGA и терминал понимают одинаково).
            px::write(ep, px::STDOUT, b"\x1b[2J\x1b[H");
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
        if let Some(path) = cmd.strip_prefix(b"tail ") {
            // Веха 30: последние 32 байта файла — витрина lseek (SEEK_END со знаковым минусом).
            let fd = px::open(ep, path, 0);
            if fd == usize::MAX {
                px::write(ep, px::STDOUT, b"vsh: no such file\n");
            } else {
                px::seek(ep, fd, -32, px::SEEK_END);
                let mut tb = [0u8; 64];
                let n = px::read(ep, fd, &mut tb);
                px::write(ep, px::STDOUT, &tb[..n]);
                if n == 0 || tb[n - 1] != b'\n' {
                    px::write(ep, px::STDOUT, b"\n");
                }
                px::close(ep, fd);
            }
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
        if let Some(rest) = cmd.strip_prefix(b"run ") {
            // Веха 30: `run NAME [ARGS…]` — имя до первого пробела, остальные слова
            // становятся argv ребёнка (NUL-разделённый блоб для SYS_EXEC).
            let sp = rest.iter().position(|&b| b == b' ').unwrap_or(rest.len());
            let (name, tail) = (&rest[..sp], &rest[sp..]);
            let mut ab = [0u8; 128];
            let mut alen = 0usize;
            let mut in_word = false;
            for &b in tail {
                if b == b' ' {
                    if in_word {
                        ab[alen] = 0;
                        alen += 1;
                        in_word = false;
                    }
                } else if alen < ab.len() - 1 {
                    ab[alen] = b;
                    alen += 1;
                    in_word = true;
                }
            }
            if in_word {
                ab[alen] = 0;
                alen += 1;
            }
            let code = px::spawn_args(xcap, name, &ab[..alen]);
            if code == usize::MAX {
                px::write(ep, px::STDOUT, b"vsh: run failed (no such program in store?)\n");
            } else {
                px::write(ep, px::STDOUT, b"vsh: program exited, code ");
                put_dec(ep, code);
                px::write(ep, px::STDOUT, b"\n");
            }
            continue;
        }
        if let Some(rest) = cmd.strip_prefix(b"thaw ") {
            // Веха 37: `thaw ИМЯ` — разморозить процесс из образа `proc/<arch>/ИМЯ`
            // (снятого его же `SYS_CHECKPOINT`); ждём завершения, как run.
            let code = sys::restore(xcap, rest);
            if code == usize::MAX {
                px::write(ep, px::STDOUT, b"vsh: thaw failed (no such image?)\n");
            } else {
                px::write(ep, px::STDOUT, b"vsh: thawed program exited, code ");
                put_dec(ep, code);
                px::write(ep, px::STDOUT, b"\n");
            }
            continue;
        }
        if let Some(name) = cmd.strip_prefix(b"switch ") {
            // Веха 40: выбрать поколение системы — записать его имя в корень-указатель
            // `system/current`. Право WRITE на store у vsh есть (a1 = store:xw из конфига).
            // Вступает в силу на следующей загрузке (декларативный init читает корень).
            let mut id = [0u8; 32];
            if sys::obj_put(xcap, name, &mut id) == 0
                && sys::obj_set_root(xcap, b"system/current", &id) == 0
            {
                px::write(ep, px::STDOUT, "vsh: поколение выбрано, перезагрузи QEMU: ".as_bytes());
                px::write(ep, px::STDOUT, name);
                px::write(ep, px::STDOUT, b"\n");
            } else {
                px::write(ep, px::STDOUT, "vsh: switch failed (нет права WRITE на store?)\n".as_bytes());
            }
            continue;
        }
        if let Some(rest) = cmd.strip_prefix(b"sysdef ") {
            // Веха 40: `sysdef ИМЯ ФАЙЛ` — зарегистрировать содержимое файла персоналии как
            // конфиг поколения `system/ИМЯ` (объект store + корень). Так конфиг, написанный/
            // доставленный как файл (в т.ч. сгенерированный `nix/system.nix`), становится
            // поколением, на которое можно `switch`. Читаем файл через персоналию, кладём в store.
            let sp = rest.iter().position(|&b| b == b' ').unwrap_or(rest.len());
            let (gname, fname) = (&rest[..sp], rest.get(sp + 1..).unwrap_or(&[]));
            if gname.is_empty() || fname.is_empty() {
                px::write(ep, px::STDOUT, "usage: sysdef ИМЯ ФАЙЛ\n".as_bytes());
                continue;
            }
            let fd = px::open(ep, fname, 0);
            if fd == usize::MAX {
                px::write(ep, px::STDOUT, b"vsh: sysdef: no such file\n");
                continue;
            }
            // Конфиг мал (несколько строк) — читаем одним буфером.
            let n = px::read(ep, fd, &mut out);
            px::close(ep, fd);
            let mut rootbuf = [0u8; 40]; // "system/" + имя ≤ 32
            let root = b"system/";
            rootbuf[..root.len()].copy_from_slice(root);
            let gl = gname.len().min(rootbuf.len() - root.len());
            rootbuf[root.len()..root.len() + gl].copy_from_slice(&gname[..gl]);
            let rlen = root.len() + gl;
            let mut id = [0u8; 32];
            if sys::obj_put(xcap, &out[..n], &mut id) == 0
                && sys::obj_set_root(xcap, &rootbuf[..rlen], &id) == 0
            {
                px::write(ep, px::STDOUT, "vsh: поколение записано: ".as_bytes());
                px::write(ep, px::STDOUT, gname);
                px::write(ep, px::STDOUT, b" (switch ");
                px::write(ep, px::STDOUT, gname);
                px::write(ep, px::STDOUT, ", затем перезагрузка)\n".as_bytes());
            } else {
                px::write(ep, px::STDOUT, b"vsh: sysdef failed\n");
            }
            continue;
        }
        if let Some(rest) = cmd.strip_prefix(b"mv ") {
            // Веха 30: `mv OLD NEW` — rename персоналии (корень + каталог атомарно для store).
            match rest.iter().position(|&b| b == b' ') {
                Some(sp) if sp > 0 && sp + 1 < rest.len() => {
                    if px::rename(ep, &rest[..sp], &rest[sp + 1..]) != 0 {
                        px::write(ep, px::STDOUT, b"vsh: mv failed (no such file?)\n");
                    }
                }
                _ => {
                    px::write(ep, px::STDOUT, b"usage: mv OLD NEW\n");
                }
            }
            continue;
        }
        if let Some(ipstr) = cmd.strip_prefix(b"ping ") {
            // Веха 34: `ping A.B.C.D` — вызвать сетевой сервер (эндпоинт из старт-cap слота 2),
            // тот делает ARP+ICMP и возвращает RTT. Стек живёт в userspace, не в ядре.
            match parse_ipv4(ipstr) {
                Some(ip) => {
                    let netep = sys::start_cap(2);
                    if netep == sys::NO_CAP {
                        px::write(ep, px::STDOUT, "vsh: сети нет\n".as_bytes());
                    } else {
                        let mut rep = [0u8; 5];
                        let n = sys::call(netep, 0 /* OP_PING */, &ip, &mut rep);
                        if n >= 5 && rep[0] == 0 {
                            let rtt = u32::from_le_bytes([rep[1], rep[2], rep[3], rep[4]]) as usize;
                            px::write(ep, px::STDOUT, "ответ от ".as_bytes());
                            px::write(ep, px::STDOUT, ipstr);
                            px::write(ep, px::STDOUT, b": ");
                            put_dec(ep, rtt);
                            px::write(ep, px::STDOUT, " мкс\n".as_bytes());
                        } else {
                            px::write(ep, px::STDOUT, "ping: нет ответа\n".as_bytes());
                        }
                    }
                }
                None => {
                    px::write(ep, px::STDOUT, b"usage: ping A.B.C.D\n");
                }
            }
            continue;
        }
        px::write(ep, px::STDOUT, b"vsh: unknown command (try 'help')\n");
    }
}
