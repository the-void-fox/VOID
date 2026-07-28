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
const C_DIR: &[u8] = b"\x1b[1;34m"; // синий жирный — текущий каталог/имена каталогов
const C_ERR: &[u8] = b"\x1b[1;31m"; // красный жирный — ошибки

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
    help_row(ep, b"ls [DIR]", "список файлов (каталога DIR или текущего)");
    help_row(ep, b"roots", "показать корни store (объекты: bin/*, system/*, …)");
    help_row(ep, b"cd DIR", "сменить каталог (.. — вверх, / — корень)");
    help_row(ep, b"pwd", "показать текущий каталог");
    help_row(ep, b"mkdir DIR", "создать каталог");
    help_row(ep, b"rm PATH", "удалить файл (или пустой каталог)");
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

/// Веха 44 — разрешить путь `arg` относительно `cwd` в абсолютный нормализованный путь `out`,
/// вернуть длину. Поддерживает ведущий `/` (абсолютный), `.`, `..`, пустые компоненты. Пустой
/// `arg` возвращает сам `cwd` (для `ls` без аргумента).
fn resolve(cwd: &[u8], arg: &[u8], out: &mut [u8; 128]) -> usize {
    let mut len;
    if arg.first() == Some(&b'/') {
        out[0] = b'/';
        len = 1;
    } else {
        len = cwd.len().min(128);
        out[..len].copy_from_slice(&cwd[..len]);
        if len == 0 {
            out[0] = b'/';
            len = 1;
        }
    }
    let mut i = 0usize;
    while i < arg.len() {
        while i < arg.len() && arg[i] == b'/' {
            i += 1;
        }
        let start = i;
        while i < arg.len() && arg[i] != b'/' {
            i += 1;
        }
        let comp = &arg[start..i];
        if comp.is_empty() || comp == b"." {
            continue;
        }
        if comp == b".." {
            if len > 1 {
                while len > 1 && out[len - 1] != b'/' {
                    len -= 1;
                }
                if len > 1 {
                    len -= 1; // убрать слэш (кроме корня)
                }
            }
            continue;
        }
        if out[len - 1] != b'/' && len < 128 {
            out[len] = b'/';
            len += 1;
        }
        for &b in comp {
            if len < 128 {
                out[len] = b;
                len += 1;
            }
        }
    }
    if len == 0 {
        out[0] = b'/';
        len = 1;
    }
    len
}

/// UTF-8-continuation байт (0x80..0xBF) — не начало символа. Для движения по границам символов.
fn is_cont(b: u8) -> bool {
    b & 0xC0 == 0x80
}

/// Сколько СИМВОЛОВ (не байт) в `bytes` — для сдвига курсора на реальные колонки (кириллица =
/// 2 байта, но 1 колонка). Невалидный UTF-8 → число байт (запасной путь).
fn char_count(bytes: &[u8]) -> usize {
    match core::str::from_utf8(bytes) {
        Ok(s) => s.chars().count(),
        Err(_) => bytes.len(),
    }
}

/// Напечатать `ESC[<n><fin>` (напр. `ESC[3D` — курсор влево на 3). Для редактора строки.
fn csi_num(ep: usize, mut n: usize, fin: u8) {
    let mut buf = [0u8; 24];
    buf[0] = 0x1b;
    buf[1] = b'[';
    let mut k = 2;
    let mut digs = [0u8; 20];
    let mut d = 0;
    if n == 0 {
        digs[0] = b'0';
        d = 1;
    } else {
        while n > 0 {
            digs[d] = b'0' + (n % 10) as u8;
            n /= 10;
            d += 1;
        }
    }
    while d > 0 {
        d -= 1;
        buf[k] = digs[d];
        k += 1;
    }
    buf[k] = fin;
    k += 1;
    px::write(ep, px::STDOUT, &buf[..k]);
}

/// Напечатать цветное приглашение (зелёный `vsh`, синий каталог `cwd`, `> `).
fn print_prompt(ep: usize, cwd: &[u8]) {
    px::write(ep, px::STDOUT, C_PROMPT);
    px::write(ep, px::STDOUT, b"vsh");
    px::write(ep, px::STDOUT, RESET);
    px::write(ep, px::STDOUT, C_DIR);
    px::write(ep, px::STDOUT, cwd);
    px::write(ep, px::STDOUT, RESET);
    px::write(ep, px::STDOUT, b"> ");
}

/// Веха 45 — перерисовать строку ввода целиком: в начало (`\r`), приглашение, содержимое,
/// стереть хвост (`ESC[K`), вернуть курсор на позицию `pos` (в КОЛОНКАХ). `line[..llen]` обязан
/// быть валидным UTF-8 (вызывающий гарантирует — иначе SYS_WRITE показал бы весь буфер как `<?>`).
fn redraw(ep: usize, cwd: &[u8], line: &[u8], llen: usize, pos: usize) {
    px::write(ep, px::STDOUT, b"\r");
    print_prompt(ep, cwd);
    px::write(ep, px::STDOUT, &line[..llen]);
    px::write(ep, px::STDOUT, b"\x1b[K");
    let back = char_count(&line[pos..llen]);
    if back > 0 {
        csi_num(ep, back, b'D');
    }
}

#[no_mangle]
pub extern "C" fn _start(ep: usize, xcap: usize) -> ! {
    let mut line = [0u8; 128]; // собираемая строка команды
    let mut inb = [0u8; 16]; // порция сырого ввода
    let mut out = [0u8; 512]; // ответы персоналии (ls)
    let mut cwd = [0u8; 128]; // Веха 44: текущий каталог (начинаем с корня)
    cwd[0] = b'/';
    let mut cwd_len = 1usize;
    let mut rp = [0u8; 128]; // буфер разрешённого пути
    // Веха 45: история команд — кольцо последних HISTN (для стрелок ↑/↓).
    const HISTN: usize = 8;
    let mut hist = [[0u8; 128]; HISTN];
    let mut hlen = [0usize; HISTN];
    let mut hhead = 0usize; // следующий слот записи
    let mut hcount = 0usize; // сколько сохранено (≤ HISTN)
    print_help(ep);
    loop {
        print_prompt(ep, &cwd[..cwd_len]);
        // ── редактор строки (Веха 45): курсор pos, вставка/удаление в позиции, стрелки, история ──
        // Стрелки/Home/End/Del приходят ANSI-последовательностями (`ESC[…`) — и с терминала QEMU,
        // и от PS/2-клавиатуры (ps2.rs шлёт те же коды). Движемся по границам символов (UTF-8).
        let mut llen = 0usize;
        let mut pos = 0usize;
        let mut esc = 0u8; // 0 обычный, 1 после ESC, 2 после ESC[ (копим до финального байта)
        let mut hb = 0usize; // индекс просмотра истории (0 — не просматриваем)
        'line: loop {
            let n = px::read(ep, px::STDIN, &mut inb);
            for &b in &inb[..n] {
                match esc {
                    1 => esc = if b == b'[' { 2 } else { 0 },
                    2 => {
                        if b.is_ascii_digit() || b == b';' {
                            // параметр (напр. '3' в ESC[3~) — остаёмся в состоянии, финал ниже
                        } else {
                            esc = 0;
                            match b {
                                b'C' => {
                                    if pos < llen {
                                        pos += 1;
                                        while pos < llen && is_cont(line[pos]) {
                                            pos += 1;
                                        }
                                        px::write(ep, px::STDOUT, b"\x1b[C");
                                    }
                                }
                                b'D' => {
                                    if pos > 0 {
                                        pos -= 1;
                                        while pos > 0 && is_cont(line[pos]) {
                                            pos -= 1;
                                        }
                                        px::write(ep, px::STDOUT, b"\x1b[D");
                                    }
                                }
                                b'H' => {
                                    if pos > 0 {
                                        csi_num(ep, char_count(&line[..pos]), b'D');
                                        pos = 0;
                                    }
                                }
                                b'F' => {
                                    if pos < llen {
                                        csi_num(ep, char_count(&line[pos..llen]), b'C');
                                        pos = llen;
                                    }
                                }
                                b'A' => {
                                    if hb < hcount {
                                        hb += 1;
                                        let slot = (hhead + HISTN - hb) % HISTN;
                                        llen = hlen[slot];
                                        line[..llen].copy_from_slice(&hist[slot][..llen]);
                                        pos = llen;
                                        redraw(ep, &cwd[..cwd_len], &line, llen, pos);
                                    }
                                }
                                b'B' => {
                                    if hb > 1 {
                                        hb -= 1;
                                        let slot = (hhead + HISTN - hb) % HISTN;
                                        llen = hlen[slot];
                                        line[..llen].copy_from_slice(&hist[slot][..llen]);
                                    } else {
                                        hb = 0;
                                        llen = 0;
                                    }
                                    pos = llen;
                                    redraw(ep, &cwd[..cwd_len], &line, llen, pos);
                                }
                                b'~' => {
                                    // Delete: удалить символ В позиции курсора (может быть многобайтным)
                                    if pos < llen {
                                        let mut end = pos + 1;
                                        while end < llen && is_cont(line[end]) {
                                            end += 1;
                                        }
                                        line.copy_within(end..llen, pos);
                                        llen -= end - pos;
                                        redraw(ep, &cwd[..cwd_len], &line, llen, pos);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {
                        if b == 0x1b {
                            esc = 1;
                        } else if b == b'\r' || b == b'\n' {
                            px::write(ep, px::STDOUT, b"\n");
                            break 'line;
                        } else if b == 0x7f || b == 0x08 {
                            // Backspace: удалить символ ПЕРЕД курсором.
                            if pos > 0 {
                                let mut start = pos - 1;
                                while start > 0 && is_cont(line[start]) {
                                    start -= 1;
                                }
                                line.copy_within(pos..llen, start);
                                llen -= pos - start;
                                pos = start;
                                redraw(ep, &cwd[..cwd_len], &line, llen, pos);
                            }
                        } else if b >= 0x20 && llen < line.len() - 1 {
                            // Вставить байт в позицию курсора.
                            line.copy_within(pos..llen, pos + 1);
                            line[pos] = b;
                            llen += 1;
                            pos += 1;
                            // Перерисовать лишь когда строка — валидный UTF-8 (не середина
                            // многобайтного символа): иначе SYS_WRITE показал бы весь буфер как «<?>».
                            if core::str::from_utf8(&line[..llen]).is_ok() {
                                redraw(ep, &cwd[..cwd_len], &line, llen, pos);
                            }
                        }
                    }
                }
            }
        }
        let cmd = &line[..llen];
        if cmd.is_empty() {
            continue;
        }
        // Веха 45: сохранить непустую команду в историю (кольцо).
        hist[hhead][..llen].copy_from_slice(cmd);
        hlen[hhead] = llen;
        hhead = (hhead + 1) % HISTN;
        if hcount < HISTN {
            hcount += 1;
        }
        let cmd = &line[..llen];
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
        // Установка — теперь ОТДЕЛЬНАЯ программа `bin/<arch>/install` (Веха 74): `run install`.
        // init сеет её только на install-носителе, поэтому на установленной системе её просто нет
        // (раньше `install` был встроен в vsh и присутствовал везде).
        if cmd == b"roots" {
            // Показать СЫРЫЕ корни store (как `ls`, но для объектов store, не файлов posixfs):
            // короткий content-id + имя на строку. Гейт — store-cap (xcap: store:xw, есть WRITE).
            let mut rbuf = [0u8; 8192];
            let n = sys::obj_list_roots(xcap, &mut rbuf);
            if n == 0 {
                px::write(ep, px::STDOUT, "нет корней (или нет прав на store)\n".as_bytes());
            } else {
                px::write(ep, px::STDOUT, &rbuf[..n]);
            }
            continue;
        }
        if cmd == b"ls" || cmd.strip_prefix(b"ls ").is_some() {
            // Веха 44: `ls` — текущий каталог; `ls DIR` — указанный (относительно cwd).
            let arg = cmd.strip_prefix(b"ls ").unwrap_or(b"");
            let n = resolve(&cwd[..cwd_len], arg, &mut rp);
            let k = px::readdir(ep, &rp[..n], &mut out);
            px::write(ep, px::STDOUT, &out[..k]);
            continue;
        }
        if cmd == b"pwd" {
            // Веха 44: показать текущий каталог.
            px::write(ep, px::STDOUT, &cwd[..cwd_len]);
            px::write(ep, px::STDOUT, b"\n");
            continue;
        }
        if let Some(arg) = cmd.strip_prefix(b"cd ") {
            // Веха 44: сменить каталог (проверив, что цель — существующий каталог).
            let n = resolve(&cwd[..cwd_len], arg, &mut rp);
            match px::stat(ep, &rp[..n]) {
                Some((true, _)) => {
                    cwd[..n].copy_from_slice(&rp[..n]);
                    cwd_len = n;
                }
                _ => {
                    px::write(ep, px::STDOUT, C_ERR);
                    px::write(ep, px::STDOUT, "vsh: нет такого каталога\n".as_bytes());
                    px::write(ep, px::STDOUT, RESET);
                }
            }
            continue;
        }
        if let Some(arg) = cmd.strip_prefix(b"mkdir ") {
            // Веха 44: создать каталог (относительно cwd).
            let n = resolve(&cwd[..cwd_len], arg, &mut rp);
            if px::mkdir(ep, &rp[..n]) != 0 {
                px::write(ep, px::STDOUT, C_ERR);
                px::write(ep, px::STDOUT, "vsh: mkdir не удался (уже есть? нет родителя?)\n".as_bytes());
                px::write(ep, px::STDOUT, RESET);
            }
            continue;
        }
        if let Some(arg) = cmd.strip_prefix(b"rm ") {
            // Веха 44: удалить файл (или пустой каталог) — относительно cwd.
            let n = resolve(&cwd[..cwd_len], arg, &mut rp);
            if px::unlink(ep, &rp[..n]) != 0 {
                px::write(ep, px::STDOUT, C_ERR);
                px::write(ep, px::STDOUT, "vsh: rm не удался (нет файла? каталог не пуст?)\n".as_bytes());
                px::write(ep, px::STDOUT, RESET);
            }
            continue;
        }
        if let Some(path) = cmd.strip_prefix(b"cat ") {
            let n = resolve(&cwd[..cwd_len], path, &mut rp);
            px::cat(ep, &rp[..n]);
            continue;
        }
        if let Some(path) = cmd.strip_prefix(b"tail ") {
            // Веха 30: последние 32 байта файла — витрина lseek (SEEK_END со знаковым минусом).
            let n = resolve(&cwd[..cwd_len], path, &mut rp);
            let fd = px::open(ep, &rp[..n], 0);
            if fd == usize::MAX {
                px::write(ep, px::STDOUT, b"vsh: no such file\n");
            } else {
                px::seek(ep, fd, -32, px::SEEK_END);
                let mut tb = [0u8; 64];
                let k = px::read(ep, fd, &mut tb);
                px::write(ep, px::STDOUT, &tb[..k]);
                if k == 0 || tb[k - 1] != b'\n' {
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
                let n = resolve(&cwd[..cwd_len], &body[sep + 3..], &mut rp);
                px::echo_to(ep, &rp[..n], &body[..sep]);
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
            // Веха 30/44: `mv OLD NEW` — оба пути резолвятся относительно cwd, затем rename.
            match rest.iter().position(|&b| b == b' ') {
                Some(sp) if sp > 0 && sp + 1 < rest.len() => {
                    let mut oldr = [0u8; 128];
                    let ol = resolve(&cwd[..cwd_len], &rest[..sp], &mut oldr);
                    let nl = resolve(&cwd[..cwd_len], &rest[sp + 1..], &mut rp);
                    if px::rename(ep, &oldr[..ol], &rp[..nl]) != 0 {
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
