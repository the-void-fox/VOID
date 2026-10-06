//! `lx-threads` — ПРОБНИК НИТЕЙ личности Linux (Веха 223.5).
//!
//! Настоящий Linux-бинарь (musl, static-PIE), который дёргает `clone`, `futex` и `gettid`
//! НАПРЯМУЮ и печатает, что вернул каждый. Нужен затем же, зачем [`lx-probe`]: доказать, что
//! заявленное работает, — и поймать, когда перестанет. До Вехи 223.5 первый же `clone` с
//! `CLONE_VM` отвечал `ENOSYS`.
//!
//! ## Почему СЫРЫЕ вызовы, а не `std::thread`
//!
//! Проверять нити программой на Rust-`std` было бы честнее и короче — но она на VOID НЕ
//! ЗАПУСКАЕТСЯ, и не из-за нитей: обычный `println!` без единой нити падает так же. Программа
//! зовёт `abort()`, та делает `raise(SIGABRT)`, сигналов в системе нет, вызов возвращается, и
//! следом исполняется `hlt` — #GP. Причина самого `abort` не разобрана (см. [[known-gaps]]).
//! Пробник на сырых вызовах от этого не зависит: он не трогает ничего, кроме того, что проверяет.
//!
//! ## Что именно проверяется
//!
//! | шаг | что за ним стоит в ядре |
//! |---|---|
//! | `getpid` против `gettid` | номер ПРОЦЕССА (лидер группы) против номера НИТИ — до вехи одно число |
//! | нить исполняется и видит общую память | `CLONE_VM`: нить — запись таблицы с тем же `space` |
//! | `%fs:0` в нити отдаёт свой блок | `CLONE_SETTLS` → `set_thread_ptr` (fsbase) |
//! | родитель получил номер нити | `CLONE_PARENT_SETTID` |
//! | слово `ctid` обнулилось само | `CLONE_CHILD_CLEARTID` — на нём стоит `pthread_join` |
//! | `futex` будит соседку | WAIT/WAKE между нитями одной группы |
//! | `futex` со сроком возвращает ETIMEDOUT | срок раньше игнорировался («ждём бессрочно») |
//! | нить пишет в дескриптор процесса | `CLONE_FILES`: стол дескрипторов у ГРУППЫ |
//!
//! ## Сборка (в репозиторий кладётся ИСХОДНИК, не бинарь — как у `lx-probe`)
//!
//! ```sh
//! nix-shell --run 'rustc --edition 2021 -O --target x86_64-unknown-linux-musl \
//!   -C panic=abort -C relocation-model=pie -C link-arg=-static-pie \
//!   -C default-linker-libraries=yes -o /tmp/lx-threads Code/programs/lx-threads/main.rs'
//! Code/tools/void-store-import <образ> put /tmp/lx-threads bin/x86_64/lx-threads
//! # в системе:  run lx-threads
//! ```
#![no_std]
#![no_main]

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicUsize, Ordering};

// ── сырые системные вызовы ───────────────────────────────────────────────────────────────────

#[inline(always)]
unsafe fn sys3(n: usize, a: usize, b: usize, c: usize) -> isize {
    let r: isize;
    asm!("syscall", inlateout("rax") n as isize => r, in("rdi") a, in("rsi") b, in("rdx") c,
         lateout("rcx") _, lateout("r11") _, options(nostack));
    r
}

#[inline(always)]
unsafe fn sys6(n: usize, a: usize, b: usize, c: usize, d: usize, e: usize, f: usize) -> isize {
    let r: isize;
    asm!("syscall", inlateout("rax") n as isize => r, in("rdi") a, in("rsi") b, in("rdx") c,
         in("r10") d, in("r8") e, in("r9") f, lateout("rcx") _, lateout("r11") _,
         options(nostack));
    r
}

const SYS_WRITE: usize = 1;
const SYS_MMAP: usize = 9;
const SYS_NANOSLEEP: usize = 35;
const SYS_GETPID: usize = 39;
const SYS_EXIT: usize = 60;
const SYS_GETTID: usize = 186;
const SYS_FUTEX: usize = 202;

const FUTEX_WAIT: usize = 0;
const FUTEX_WAKE: usize = 1;

// ── печать без libc ──────────────────────────────────────────────────────────────────────────

fn out(s: &str) {
    unsafe { sys3(SYS_WRITE, 1, s.as_ptr() as usize, s.len()) };
}

/// Число в строку. Своё, потому что `format!` тянет аллокатор, а аллокатора здесь нет.
fn num(mut v: i64, buf: &mut [u8; 24]) -> usize {
    let neg = v < 0;
    if neg {
        v = -v;
    }
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    if neg {
        i -= 1;
        buf[i] = b'-';
    }
    buf.copy_within(i.., 0);
    buf.len() - i
}

fn out_num(v: i64) {
    let mut b = [0u8; 24];
    let n = num(v, &mut b);
    unsafe { sys3(SYS_WRITE, 1, b.as_ptr() as usize, n) };
}

/// Строка отчёта: «ОК» либо «ПЛОХО», название, значение.
fn step(pass: bool, what: &str, v: i64) {
    out(if pass { "  ОК    " } else { "  ПЛОХО " });
    out(what);
    out(" = ");
    out_num(v);
    out("\n");
}

// ── обёртка clone: вход нити пишется на ассемблере ───────────────────────────────────────────
//
// Иначе никак: `clone` возвращается ДВАЖДЫ, и ребёнок продолжает с чужим стеком — скомпилированный
// вокруг код этого не переживёт (он рассчитывает на свой кадр). Тот же приём, что у `__clone`
// в musl и glibc.
//
// `r9` (адрес функции нити) переживает `syscall` нетронутым: тот портит только `rcx` и `r11`.
// Стек ребёнка 16-выровнен, поэтому `call` оставляет на входе в функцию rsp ≡ 8 (mod 16) — как
// требует ABI.
global_asm!(
    r#"
    .globl  do_clone
do_clone:
    mov     r10, rcx          // ctid → четвёртый аргумент syscall'а
    mov     eax, 56           // SYS_clone
    syscall
    test    eax, eax
    jnz     2f                // родитель: вернуть номер нити
    xor     ebp, ebp          // ребёнок: конец цепочки кадров
    call    r9
    xor     edi, edi
    mov     eax, 60           // SYS_exit — ТОЛЬКО эта нить
    syscall
    hlt
2:  ret
"#
);

extern "C" {
    /// `do_clone(flags, stack_top, ptid, ctid, tls, fn) -> tid | -errno`
    fn do_clone(
        flags: usize,
        stack: usize,
        ptid: usize,
        ctid: usize,
        tls: usize,
        f: extern "C" fn(),
    ) -> isize;
}

const CLONE_VM: usize = 0x0000_0100;
const CLONE_FS: usize = 0x0000_0200;
const CLONE_FILES: usize = 0x0000_0400;
const CLONE_SIGHAND: usize = 0x0000_0800;
const CLONE_THREAD: usize = 0x0001_0000;
const CLONE_SETTLS: usize = 0x0008_0000;
const CLONE_PARENT_SETTID: usize = 0x0010_0000;
const CLONE_CHILD_CLEARTID: usize = 0x0020_0000;
const CLONE_CHILD_SETTID: usize = 0x0100_0000;

// ── общая память: её видно обеим нитям только при CLONE_VM ───────────────────────────────────

static COUNTER: AtomicUsize = AtomicUsize::new(0);
static CHILD_TID: AtomicUsize = AtomicUsize::new(0);
static TLS_SEEN: AtomicUsize = AtomicUsize::new(0);
static WROTE_FD: AtomicUsize = AtomicUsize::new(0);
/// Слово, на котором ребёнок спит, а родитель его будит.
static mut HANDSHAKE: u32 = 0;
/// Блок «TLS» нити: по уговору x86-64 первое слово TCB — указатель на себя.
static mut TLS_BLOCK: [usize; 4] = [0; 4];

const BUMPS: usize = 50_000;

/// Тело нити. Всё, что она трогает, — общая память процесса; если бы `CLONE_VM` не сработал,
/// родитель не увидел бы ни одного из этих следов.
extern "C" fn thread_body() {
    CHILD_TID.store(unsafe { sys3(SYS_GETTID, 0, 0, 0) } as usize, Ordering::SeqCst);

    // `%fs:0` обязан отдать адрес нашего блока — это и есть доехавший `CLONE_SETTLS`.
    let tp: usize;
    unsafe { asm!("mov {}, fs:0", out(reg) tp, options(nostack, readonly)) };
    TLS_SEEN.store(tp, Ordering::SeqCst);

    for _ in 0..BUMPS {
        COUNTER.fetch_add(1, Ordering::Relaxed);
    }

    // Пишем в дескриптор 1 — он принадлежит ГРУППЕ, а не нити (`CLONE_FILES`).
    let msg = "  (строка напечатана из НИТИ — дескриптор общий)\n";
    let n = unsafe { sys3(SYS_WRITE, 1, msg.as_ptr() as usize, msg.len()) };
    WROTE_FD.store((n > 0) as usize, Ordering::SeqCst);

    // Ждём, пока родитель разбудит: проверка futex между нитями одной группы.
    unsafe {
        let p = &raw mut HANDSHAKE;
        while core::ptr::read_volatile(p) == 0 {
            sys6(SYS_FUTEX, p as usize, FUTEX_WAIT, 0, 0, 0, 0);
        }
    }
}

fn sleep_ms(ms: i64) {
    let ts = [ms / 1000, (ms % 1000) * 1_000_000];
    unsafe { sys3(SYS_NANOSLEEP, ts.as_ptr() as usize, 0, 0) };
}

#[no_mangle]
pub extern "C" fn main(_argc: i32, _argv: *const *const u8) -> i32 {
    out("lx-threads: пробник нитей личности Linux\n");
    let mut bad = 0;

    // ── 1. номера процесса и нити ────────────────────────────────────────────────────────
    let pid = unsafe { sys3(SYS_GETPID, 0, 0, 0) } as i64;
    let tid = unsafe { sys3(SYS_GETTID, 0, 0, 0) } as i64;
    if pid != tid {
        bad += 1;
    }
    step(pid == tid, "главная нить: getpid == gettid, pid", pid);

    // ── 2. стек для нити ─────────────────────────────────────────────────────────────────
    const STACK: usize = 128 * 1024;
    let mem = unsafe { sys6(SYS_MMAP, 0, STACK, 0x3 /*RW*/, 0x22 /*PRIVATE|ANON*/, usize::MAX, 0) };
    if mem <= 0 {
        step(false, "mmap стека нити", mem as i64);
        out("lx-threads: ЕСТЬ ОТКАЗЫ\n");
        unsafe { sys3(SYS_EXIT, 1, 0, 0) };
    }
    let stack_top = (mem as usize + STACK) & !0xF; // 16 — как требует ABI

    // ── 3. clone ─────────────────────────────────────────────────────────────────────────
    let mut ptid: u32 = 0;
    let mut ctid: u32 = 0;
    let tls = unsafe {
        let p = &raw mut TLS_BLOCK;
        (*p)[0] = p as usize; // TCB указывает на себя — так устроен Variant II
        p as usize
    };
    let flags = CLONE_VM
        | CLONE_FS
        | CLONE_FILES
        | CLONE_SIGHAND
        | CLONE_THREAD
        | CLONE_SETTLS
        | CLONE_PARENT_SETTID
        | CLONE_CHILD_SETTID
        | CLONE_CHILD_CLEARTID;
    let child = unsafe {
        do_clone(
            flags,
            stack_top,
            &mut ptid as *mut u32 as usize,
            &mut ctid as *mut u32 as usize,
            tls,
            thread_body,
        )
    };
    if child <= 0 {
        step(false, "clone(CLONE_VM) вернул", child as i64);
        out("lx-threads: ЕСТЬ ОТКАЗЫ — нитей нет\n");
        unsafe { sys3(SYS_EXIT, 1, 0, 0) };
    }
    step(true, "clone(CLONE_VM) завёл нить", child as i64);

    // Нить должна добежать до своего ожидания. Ждём щедро: это не замер, а проверка.
    for _ in 0..200 {
        if CHILD_TID.load(Ordering::SeqCst) != 0 && WROTE_FD.load(Ordering::SeqCst) != 0 {
            break;
        }
        sleep_ms(10);
    }

    let ctid_seen = unsafe { core::ptr::read_volatile(&raw const ctid) };
    let ptid_seen = unsafe { core::ptr::read_volatile(&raw const ptid) };
    let ch_tid = CHILD_TID.load(Ordering::SeqCst) as i64;

    if ch_tid == 0 {
        bad += 1;
    }
    step(ch_tid != 0, "нить исполнилась, её gettid", ch_tid);
    if ch_tid == tid {
        bad += 1;
    }
    step(ch_tid != tid, "у нити gettid ОТЛИЧАЕТСЯ от главной", ch_tid - tid);
    if ptid_seen as i64 != ch_tid {
        bad += 1;
    }
    step(ptid_seen as i64 == ch_tid, "CLONE_PARENT_SETTID: родителю записан tid", ptid_seen as i64);
    if ctid_seen as i64 != ch_tid {
        bad += 1;
    }
    step(ctid_seen as i64 == ch_tid, "CLONE_CHILD_SETTID: слово ctid держит tid", ctid_seen as i64);

    let tls_seen = TLS_SEEN.load(Ordering::SeqCst);
    if tls_seen != tls {
        bad += 1;
    }
    step(tls_seen == tls, "CLONE_SETTLS: %fs:0 в нити отдал свой блок", (tls_seen == tls) as i64);

    let c = COUNTER.load(Ordering::SeqCst);
    if c != BUMPS {
        bad += 1;
    }
    step(c == BUMPS, "CLONE_VM: нить насчитала в общей памяти", c as i64);

    if WROTE_FD.load(Ordering::SeqCst) == 0 {
        bad += 1;
    }
    step(WROTE_FD.load(Ordering::SeqCst) != 0, "CLONE_FILES: нить написала в stdout процесса", 1);

    // ── 4. futex: разбудить спящую нить ──────────────────────────────────────────────────
    let woke = unsafe {
        let p = &raw mut HANDSHAKE;
        core::ptr::write_volatile(p, 1);
        sys6(SYS_FUTEX, p as usize, FUTEX_WAKE, 1, 0, 0, 0)
    };
    step(woke >= 0, "futex WAKE соседней нити вернул", woke as i64);

    // ── 5. futex СО СРОКОМ: обязан вернуть ETIMEDOUT (-110), а не висеть ────────────────
    // До Вехи 223.5 срок игнорировался («ждём бессрочно»), и этот вызов не вернулся бы никогда.
    let mut never: u32 = 7;
    let ts: [i64; 2] = [0, 150_000_000]; // 150 мс
    let r = unsafe {
        sys6(SYS_FUTEX, &raw mut never as usize, FUTEX_WAIT, 7, ts.as_ptr() as usize, 0, 0)
    };
    let timed_out = r == -110;
    if !timed_out {
        bad += 1;
    }
    step(timed_out, "futex WAIT со сроком 150 мс вернул", r as i64);

    // ── 6. CLONE_CHILD_CLEARTID: нить легла — ядро обнулило слово ───────────────────────
    // Это и есть механика `pthread_join`, и больше у join'а ничего нет.
    let mut cleared = false;
    for _ in 0..200 {
        if unsafe { core::ptr::read_volatile(&raw const ctid) } == 0 {
            cleared = true;
            break;
        }
        sleep_ms(10);
    }
    if !cleared {
        bad += 1;
    }
    step(cleared, "CLONE_CHILD_CLEARTID: ядро обнулило ctid (join)", cleared as i64);

    if bad == 0 {
        out("lx-threads: ВСЁ ПРОШЛО\n");
    } else {
        out("lx-threads: ОТКАЗОВ ");
        out_num(bad);
        out("\n");
    }
    unsafe { sys3(SYS_EXIT, (bad != 0) as usize, 0, 0) };
    0
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    out("lx-threads: ПАНИКА\n");
    unsafe { sys3(SYS_EXIT, 2, 0, 0) };
    loop {}
}
