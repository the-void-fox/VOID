//! Веха 38 — linux-abi (бэкенд B [[void-pkg]], в ядре): тонкий трансля́тор Linux-syscall'ов,
//! дающий неизменённым **static-PIE musl**-бинарям из бинарного кэша nixpkgs работать поверх
//! механизмов VOID. Путь FreeBSD linuxulator: процесс помечен «linux» ([`crate::proc`]), его
//! `ecall`/`syscall` уходит не в ABI VOID, а в таблицу трансляции здесь.
//!
//! Почему это вообще ложится: замыкания nix самодостаточны и НЕ зависят от FHS — их
//! /nix/store-пути отображаются в наш store 1:1, а Linux-ABI для статического CLI — это
//! ~десятки syscall'ов (write/writev/brk/mmap/openat/exit…), без страшных clone/futex, пока
//! бинарь однопоточный. Здесь — разбор номеров (у x86-64 и riscv64 они РАЗНЫЕ) и раскладка
//! стартового стека Linux (argc/argv/envp/**auxv**), по которому musl находит себя и TLS.
//!
//! Диспетчер syscall'ов живёт в [`crate::proc`] (ему нужны таблица процессов, ленивая куча и
//! консоль); здесь — только чистые части: коды ошибок, декодер номеров и строитель стека.

use alloc::vec::Vec;

use crate::elf::PieImage;

// ─── коды ошибок (возврат как -errno, по соглашению Linux) ─────────────────────
pub const ENOENT: isize = 2;
pub const EBADF: isize = 9;
pub const ENOMEM: isize = 12;
pub const EFAULT: isize = 14;
pub const EINVAL: isize = 22;
pub const ENOSYS: isize = 38;
pub const ENOTTY: isize = 25;
pub const ESPIPE: isize = 29;
/// Веха 108.3 — файловая система только для чтения: пакет неизменяем, и врать «нет файла» на
/// попытку записи было бы хуже, чем сказать правду.
pub const EROFS: isize = 30;
/// Веха 181 — места на носителе не хватило (или каталога, куда класть, нет). Отдельный код, а
/// не «успех»: сборка, чей `close` молча потерял вывод компилятора, — это испорченный пакет,
/// который выяснится через полчаса и в другом месте.
pub const ENOSPC: isize = 28;
/// Каталог не пуст — `rmdir` на непустом.
pub const ENOTEMPTY: isize = 39;
/// Уже существует.
pub const EEXIST: isize = 17;
/// Веха 183 — писать в трубу, у которой не осталось читателей. На Linux сюда же приходит
/// `SIGPIPE`; сигналов у нас нет, поэтому остаётся только честный код.
pub const EPIPE: isize = 32;
/// Ждать нечего: у процесса нет детей.
pub const ECHILD: isize = 10;

/// Обёрнутый в usize код ошибки (`-errno` в дополнительном коде — как возвращает ядро Linux).
pub fn err(e: isize) -> usize {
    (-e) as usize
}

// ─── номера syscall'ов → смысл ─────────────────────────────────────────────────
/// Смысл системного вызова Linux, общий для обеих архитектур. Номера у x86-64 (свои
/// исторические) и riscv64 (generic `asm-generic/unistd.h`) РАЗНЫЕ — разводит [`decode`].
/// Часть вариантов арх-специфична (`ArchPrctl` — только x86), отсюда `allow(dead_code)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Lx {
    Read,
    Write,
    Readv,
    Writev,
    Close,
    Lseek,
    Openat,
    /// Веха 181 (ADR 0019) — создание и снятие имён. Без них сборка не начнётся: `configure`
    /// первым делом заводит рабочий каталог и складывает в него пробные файлы.
    Mkdirat,
    Unlinkat,
    Renameat,
    /// Веха 182 (ADR 0019, шаг 2) — заменить образ ТЕКУЩЕГО процесса.
    Execve,
    /// Веха 183 — труба: два дескриптора, читающий и пишущий.
    Pipe2,
    /// Веха 183 — дождаться ребёнка и забрать его код выхода.
    Wait4,
    /// Веха 184 — ВЕТВЛЕНИЕ: `fork`, `vfork` и `clone` без разделения памяти.
    Fork,
    Fstat,
    Newfstatat,
    Getdents64,
    Ioctl,
    Fcntl,
    Brk,
    Mmap,
    Munmap,
    Mprotect,
    Mremap,
    Madvise,
    ArchPrctl,
    SetTidAddress,
    SetRobustList,
    RtSigprocmask,
    RtSigaction,
    Rseq,
    Prlimit64,
    Setuid,
    Setgid,
    Setgroups,
    ClockGettime,
    Gettimeofday,
    Nanosleep,
    ClockNanosleep,
    SchedYield,
    Getpid,
    Gettid,
    Getppid,
    Getuid,
    Geteuid,
    Getgid,
    Getegid,
    Uname,
    Sysinfo,
    Getrandom,
    Getcwd,
    Ppoll,
    Faccessat,
    Readlinkat,
    /// Веха 108.3 — ЛЕГАСИ-варианты x86-64 без `dirfd`: путь лежит в ПЕРВОМ аргументе. На riscv
    /// их не существует вовсе (generic ABI знает только `*at`), а musl на x86 предпочитает
    /// именно их — из-за чего файловый мост там сперва отвечал ENOSYS на всё подряд.
    Futex,
    Open,
    Pread64,
    Stat,
    Lstat,
    Access,
    Readlink,
    Dup,
    Dup3,
    ExitGroup,
    Exit,
}

/// Декодировать номер syscall'а x86-64 (arch/x86/entry/syscalls/syscall_64.tbl).
#[cfg(target_arch = "x86_64")]
pub fn decode(nr: usize) -> Option<Lx> {
    Some(match nr {
        0 => Lx::Read,
        1 => Lx::Write,
        2 => Lx::Open,   // legacy: musl на x86-64 предпочитает его openat'у
        3 => Lx::Close,
        4 => Lx::Stat,   // legacy: (path, buf)
        6 => Lx::Lstat,  // legacy: как stat — ссылки мы и так не разыменовываем
        17 => Lx::Pread64,
        21 => Lx::Access,
        5 => Lx::Fstat,
        8 => Lx::Lseek,
        9 => Lx::Mmap,
        10 => Lx::Mprotect,
        11 => Lx::Munmap,
        12 => Lx::Brk,
        13 => Lx::RtSigaction,
        14 => Lx::RtSigprocmask,
        16 => Lx::Ioctl,
        19 => Lx::Readv,
        20 => Lx::Writev,
        24 => Lx::SchedYield,
        25 => Lx::Mremap,
        28 => Lx::Madvise,
        32 => Lx::Dup,
        35 => Lx::Nanosleep,
        39 => Lx::Getpid,
        63 => Lx::Uname,
        72 => Lx::Fcntl,
        79 => Lx::Getcwd,
        89 => Lx::Readlink, // legacy: (path, buf, size) — БЕЗ dirfd
        96 => Lx::Gettimeofday,
        99 => Lx::Sysinfo,
        102 => Lx::Getuid,
        104 => Lx::Getgid,
        105 => Lx::Setuid,
        106 => Lx::Setgid,
        107 => Lx::Geteuid,
        108 => Lx::Getegid,
        116 => Lx::Setgroups,
        158 => Lx::ArchPrctl,
        202 => Lx::Futex,
        186 => Lx::Gettid,
        217 => Lx::Getdents64,
        218 => Lx::SetTidAddress,
        228 => Lx::ClockGettime,
        230 => Lx::ClockNanosleep,
        231 => Lx::ExitGroup,
        257 => Lx::Openat,
        83 => Lx::Mkdirat,   // legacy mkdir(path, mode)
        258 => Lx::Mkdirat,
        87 => Lx::Unlinkat,  // legacy unlink(path)
        84 => Lx::Unlinkat,  // legacy rmdir(path) — каталог отличаем по тому, что он каталог
        263 => Lx::Unlinkat,
        82 => Lx::Renameat,  // legacy rename(old, new)
        264 => Lx::Renameat,
        316 => Lx::Renameat, // renameat2 — флаги мы не поддерживаем, разбор ниже
        59 => Lx::Execve,
        22 => Lx::Pipe2,  // legacy pipe(fds) — без флагов
        293 => Lx::Pipe2,
        61 => Lx::Wait4,
        57 => Lx::Fork,
        58 => Lx::Fork,  // vfork — тем же ветвлением: обещание «родитель подождёт» мы
                         // не даём, а программы, которым нужен только exec, это устраивает
        56 => Lx::Fork,  // clone(flags, stack, …) — разбор флагов в обработчике
        262 => Lx::Newfstatat,
        269 => Lx::Faccessat,
        271 => Lx::Ppoll,
        273 => Lx::SetRobustList,
        292 => Lx::Dup3,
        302 => Lx::Prlimit64,
        318 => Lx::Getrandom,
        334 => Lx::Rseq,
        60 => Lx::Exit,
        _ => return None,
    })
}

/// Декодировать номер syscall'а riscv64 (generic ABI `include/uapi/asm-generic/unistd.h`).
#[cfg(target_arch = "riscv64")]
pub fn decode(nr: usize) -> Option<Lx> {
    Some(match nr {
        17 => Lx::Getcwd,
        23 => Lx::Dup,
        24 => Lx::Dup3,
        25 => Lx::Fcntl,
        29 => Lx::Ioctl,
        48 => Lx::Faccessat,
        56 => Lx::Openat,
        34 => Lx::Mkdirat,
        35 => Lx::Unlinkat,
        276 => Lx::Renameat, // renameat2 — legacy на riscv нет
        221 => Lx::Execve,
        59 => Lx::Pipe2, // на riscv legacy `pipe` отсутствует
        260 => Lx::Wait4,
        220 => Lx::Fork, // на riscv есть только `clone`
        57 => Lx::Close,
        61 => Lx::Getdents64,
        62 => Lx::Lseek,
        63 => Lx::Read,
        64 => Lx::Write,
        67 => Lx::Pread64,
        65 => Lx::Readv,
        66 => Lx::Writev,
        73 => Lx::Ppoll,
        78 => Lx::Readlinkat,
        79 => Lx::Newfstatat,
        80 => Lx::Fstat,
        93 => Lx::Exit,
        94 => Lx::ExitGroup,
        96 => Lx::SetTidAddress,
        99 => Lx::SetRobustList,
        101 => Lx::Nanosleep,
        113 => Lx::ClockGettime,
        115 => Lx::ClockNanosleep,
        124 => Lx::SchedYield,
        134 => Lx::RtSigaction,
        135 => Lx::RtSigprocmask,
        160 => Lx::Uname,
        169 => Lx::Gettimeofday,
        172 => Lx::Getpid,
        173 => Lx::Getppid,
        174 => Lx::Getuid,
        175 => Lx::Geteuid,
        176 => Lx::Getgid,
        177 => Lx::Getegid,
        144 => Lx::Setgid,
        146 => Lx::Setuid,
        159 => Lx::Setgroups,
        178 => Lx::Gettid,
        179 => Lx::Sysinfo,
        98 => Lx::Futex,
        214 => Lx::Brk,
        215 => Lx::Munmap,
        216 => Lx::Mremap,
        222 => Lx::Mmap,
        226 => Lx::Mprotect,
        233 => Lx::Madvise,
        261 => Lx::Prlimit64,
        278 => Lx::Getrandom,
        293 => Lx::Rseq,
        _ => return None,
    })
}

// ─── стартовый стек Linux (System V: argc/argv/envp/auxv) ──────────────────────
// Значения типов auxv (elf.h).
const AT_NULL: u64 = 0;
const AT_PHDR: u64 = 3;
const AT_PHENT: u64 = 4;
const AT_PHNUM: u64 = 5;
const AT_PAGESZ: u64 = 6;
const AT_BASE: u64 = 7;
const AT_FLAGS: u64 = 8;
const AT_ENTRY: u64 = 9;
const AT_UID: u64 = 11;
const AT_EUID: u64 = 12;
const AT_GID: u64 = 13;
const AT_EGID: u64 = 14;
const AT_HWCAP: u64 = 16;
const AT_CLKTCK: u64 = 17;
const AT_SECURE: u64 = 23;
const AT_RANDOM: u64 = 25;
const AT_EXECFN: u64 = 31;

const PAGE: usize = 4096;

/// Разбить NUL-разделённый блоб (`args`/`env` процесса) на записи, отбросив пустой хвост.
fn split_blob(blob: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, &b) in blob.iter().enumerate() {
        if b == 0 {
            if i > start {
                out.push(&blob[start..i]);
            }
            start = i + 1;
        }
    }
    if start < blob.len() {
        out.push(&blob[start..]);
    }
    out
}

/// Веха 38 — построить стартовый стек Linux-процесса: блок памяти, который кладётся по `sp`
/// (возвращается вместе с ним). Раскладка System V (одинакова на x86-64 и riscv64): по `sp` —
/// `argc`, дальше указатели `argv`, `NULL`, указатели `envp`, `NULL`, пары `auxv`, `AT_NULL`;
/// выше — строки argv/env, 16 байт `AT_RANDOM` (канарейка/ГПСЧ musl) и имя программы (`AT_EXECFN`).
/// `sp` выровнен на 16 (требование ABI на входе `_start`). Указатели — абсолютные VA в стеке.
pub fn build_init_stack(
    stack_top: usize,
    args: &[u8],
    env: &[u8],
    pie: &PieImage,
    random: [u8; 16],
    interp_base: usize,
) -> (Vec<u8>, usize) {
    let argv = split_blob(args);
    let envp = split_blob(env);

    // 1) Строки: argv, env, 16 байт AT_RANDOM. Запоминаем смещения внутри строкового блока.
    let mut strings = Vec::new();
    let mut argv_off = Vec::with_capacity(argv.len());
    for a in &argv {
        argv_off.push(strings.len());
        strings.extend_from_slice(a);
        strings.push(0);
    }
    let mut env_off = Vec::with_capacity(envp.len());
    for e in &envp {
        env_off.push(strings.len());
        strings.extend_from_slice(e);
        strings.push(0);
    }
    let random_off = strings.len();
    strings.extend_from_slice(&random);

    // 2) auxv (пары type,val). AT_EXECFN — имя программы (первая строка argv).
    let auxv: &[(u64, u64)] = &[
        (AT_PHDR, pie.phdr_va as u64),
        (AT_PHENT, pie.phentsize as u64),
        (AT_PHNUM, pie.phnum as u64),
        (AT_PAGESZ, PAGE as u64),
        // Веха 108.4: база, по которой лёг ИНТЕРПРЕТАТОР (`ld.so`). Он находит по ней сам себя
        // — без неё динамический бинарь не стартует вовсе. 0 — интерпретатора нет (static-PIE).
        (AT_BASE, interp_base as u64),
        (AT_FLAGS, 0),
        (AT_ENTRY, pie.entry as u64),
        (AT_UID, 0),
        (AT_EUID, 0),
        (AT_GID, 0),
        (AT_EGID, 0),
        (AT_HWCAP, 0),
        (AT_CLKTCK, 100),
        (AT_SECURE, 0),
        (AT_RANDOM, 0),  // val проставим ниже (нужен strings_base)
        (AT_EXECFN, 0),  // то же
        (AT_NULL, 0),
    ];

    // 3) Размер вектора: argc + argv-ptrs + NULL + envp-ptrs + NULL + auxv-пары.
    let vector_len = 8
        + argv.len() * 8
        + 8
        + envp.len() * 8
        + 8
        + auxv.len() * 16;
    let total = vector_len + strings.len();

    // sp выровнен вниз на 16; строки — сразу за вектором.
    let sp = (stack_top - total) & !0xf;
    let strings_base = sp + vector_len;

    // 4) Собрать блок [sp, sp+total): вектор, затем строки.
    let mut block = Vec::with_capacity(total);
    let push_u64 = |block: &mut Vec<u8>, v: u64| block.extend_from_slice(&v.to_le_bytes());

    push_u64(&mut block, argv.len() as u64); // argc
    for &off in &argv_off {
        push_u64(&mut block, (strings_base + off) as u64);
    }
    push_u64(&mut block, 0); // конец argv
    for &off in &env_off {
        push_u64(&mut block, (strings_base + off) as u64);
    }
    push_u64(&mut block, 0); // конец envp
    for &(ty, val) in auxv {
        let val = match ty {
            AT_RANDOM => (strings_base + random_off) as u64,
            AT_EXECFN => (strings_base + argv_off.first().copied().unwrap_or(random_off)) as u64,
            _ => val,
        };
        push_u64(&mut block, ty);
        push_u64(&mut block, val);
    }
    debug_assert_eq!(block.len(), vector_len);
    block.extend_from_slice(&strings);

    (block, sp)
}

// ─── struct stat (только для символьных устройств fd 0/1/2 — Веха 38) ───────────
/// Заполнить linux `struct stat` для СИМВОЛЬНОГО устройства (терминал/консоль) в `buf`.
/// Раскладка ядра различается по архам; заполняем лишь нужное musl `isatty`/`fstat`:
/// `st_mode = S_IFCHR|0620`, `st_rdev`≈tty, ненулевой `st_blksize`. Всё прочее — нули.
pub fn fill_stat_chr(buf: &mut [u8]) {
    const S_IFCHR: u32 = 0o0020000;
    let mode: u32 = S_IFCHR | 0o620;
    for b in buf.iter_mut() {
        *b = 0;
    }
    // Смещение st_mode в linux struct stat: x86-64 — 24, riscv64 (generic) — 16.
    #[cfg(target_arch = "x86_64")]
    let mode_off = 24usize;
    #[cfg(target_arch = "riscv64")]
    let mode_off = 16usize;
    if buf.len() >= mode_off + 4 {
        buf[mode_off..mode_off + 4].copy_from_slice(&mode.to_le_bytes());
    }
    // st_blksize (long) — x86-64 off 56, riscv64 off 56 (обе: после nlink/uid/gid/rdev/size).
    let blksz_off = 56usize;
    if buf.len() >= blksz_off + 8 {
        buf[blksz_off..blksz_off + 8].copy_from_slice(&4096u64.to_le_bytes());
    }
}

/// Веха 108.3 — заполнить linux `struct stat` для ФАЙЛА из объектного store: тип, права и
/// размер. Смещения те же, что у [`fill_stat_chr`]; заполняем ровно то, на что смотрят musl и
/// `ld.so`: `st_mode` (регулярный/каталог/ссылка + исполняемый бит), `st_size`, `st_blksize`.
///
/// `ty` — тип записи дерева пакета ([`void_tree`]), `ino` — **номер инода**.
///
/// Инод здесь не формальность, и это выяснилось дорого (Веха 108.4). `ld.so` считает объект уже
/// загруженным, если пара `(st_dev, st_ino)` совпала с чем-то в списке загруженного, — а мы
/// отдавали нули ВСЕМ файлам. Значит `libc.so.6` совпадала с самим бинарём: загрузчик закрывал
/// её, не отобразив, и падал на «undefined symbol: __libc_start_main». Инод берётся из
/// content-id: у одинакового содержимого он один и тот же — что как раз ВЕРНО, это один объект.
///
/// Времена остаются нулевыми намеренно: у пакета их нет, и врать про `mtime` хуже, чем показать
/// эпоху.
pub fn fill_stat_file(buf: &mut [u8], size: u64, ty: u8, ino: u64) {
    const S_IFREG: u32 = 0o0100000;
    const S_IFDIR: u32 = 0o0040000;
    const S_IFLNK: u32 = 0o0120000;
    let kind = if void_tree::is_dir(ty) {
        S_IFDIR
    } else if void_tree::is_link(ty) {
        S_IFLNK
    } else {
        S_IFREG
    };
    // Права: читать всем; исполнять — если бит стоял в NAR (у `ld.so` и `libc.so.6` он стоит,
    // и `ld.so` на него смотрит, решая, можно ли отобразить сегмент исполняемым).
    let perm: u32 = if void_tree::is_exec(ty) || void_tree::is_dir(ty) { 0o555 } else { 0o444 };
    let mode = kind | perm;
    for b in buf.iter_mut() {
        *b = 0;
    }
    // Раскладка `struct stat` у арх разная: у x86-64 после ino идёт nlink(8), у riscv64
    // (generic) — mode(4)+nlink(4).
    #[cfg(target_arch = "x86_64")]
    let (mode_off, size_off, nlink_off, nlink_len) = (24usize, 48usize, 16usize, 8usize);
    #[cfg(target_arch = "riscv64")]
    let (mode_off, size_off, nlink_off, nlink_len) = (16usize, 48usize, 20usize, 4usize);
    // st_dev — любой ненулевой: важна лишь пара (dev, ino) как признак «тот же файл».
    if buf.len() >= 16 {
        buf[0..8].copy_from_slice(&1u64.to_le_bytes());
        buf[8..16].copy_from_slice(&ino.to_le_bytes());
    }
    if buf.len() >= nlink_off + nlink_len {
        buf[nlink_off..nlink_off + nlink_len].copy_from_slice(&1u64.to_le_bytes()[..nlink_len]);
    }
    if buf.len() >= mode_off + 4 {
        buf[mode_off..mode_off + 4].copy_from_slice(&mode.to_le_bytes());
    }
    if buf.len() >= size_off + 8 {
        buf[size_off..size_off + 8].copy_from_slice(&size.to_le_bytes());
    }
    let blksz_off = 56usize;
    if buf.len() >= blksz_off + 8 {
        buf[blksz_off..blksz_off + 8].copy_from_slice(&4096u64.to_le_bytes());
    }
    // st_blocks (512-байтные блоки) — сразу за st_blksize; `du` и часть проверок смотрят туда.
    let blocks_off = 64usize;
    if buf.len() >= blocks_off + 8 {
        buf[blocks_off..blocks_off + 8].copy_from_slice(&size.div_ceil(512).to_le_bytes());
    }
}

/// Размер linux `struct stat` для этой архитектуры (x86-64 — 144, riscv64 generic — 128).
#[cfg(target_arch = "x86_64")]
pub const STAT_SIZE: usize = 144;
#[cfg(target_arch = "riscv64")]
pub const STAT_SIZE: usize = 128;

/// Заполнить `struct utsname` (6 полей по 65 байт) для `uname`.
pub fn fill_utsname(buf: &mut [u8]) {
    for b in buf.iter_mut() {
        *b = 0;
    }
    let fields: [&[u8]; 6] = [
        b"Linux",              // sysname (притворяемся Linux — того ждут бинари)
        b"void",               // nodename
        b"6.18.0-void",        // release (≥ную версию, которую проверяет glibc/musl)
        b"VOID linux-abi",     // version
        crate::arch::ARCH_NAME.as_bytes(), // machine
        b"",                   // domainname
    ];
    for (i, f) in fields.iter().enumerate() {
        let off = i * 65;
        if buf.len() >= off + f.len() {
            buf[off..off + f.len()].copy_from_slice(f);
        }
    }
}

/// Размер `struct utsname` (6 × 65).
pub const UTSNAME_SIZE: usize = 6 * 65;

// Веха 86: `tick_ns` отсюда убрана — таймбаза живёт в одном месте, [`crate::clock::TICK_NS`],
// а время для `clock_gettime`/`gettimeofday` берётся у `clock::realtime_ns`/`uptime_ns`
// (раньше константа была продублирована здесь и разошлась бы при калибровке под железо).
