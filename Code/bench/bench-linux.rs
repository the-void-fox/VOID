//! Зеркало микробенчей VOID для гостевого Linux в ТОМ ЖЕ QEMU (q35, TCG, -m 128M) —
//! источник Linux-колонки таблицы в корневом README. Время — rdtsc, как и у VOID/x86:
//! относительное сравнение точное независимо от калибровки TSC. Работает как /init
//! в initramfs (PID 1): настраивает консоль, меряет, печатает, выключает машину.
//!
//! Роли: без аргументов — бенч; `echo` — пинг-понг-партнёр (байт из stdin → в stdout);
//! `noop` — немедленный выход (цель exec-бенча).
//!
//! Сборка и запуск (хост NixOS; ядро гостя — хостовый bzImage):
//! ```sh
//! rustup target add x86_64-unknown-linux-musl
//! rustc -O --target x86_64-unknown-linux-musl -C target-feature=+crt-static \
//!       -C linker=rust-lld -o bench-linux bench-linux.rs
//! mkdir -p initrd/dev initrd/proc && cp bench-linux initrd/init
//! (cd initrd && find . | cpio -o -H newc) > init.cpio
//! qemu-system-x86_64 -machine q35 -m 128M -nographic \
//!     -kernel /run/current-system/kernel -initrd init.cpio \
//!     -append 'console=ttyS0 rdinit=/init quiet loglevel=1' -no-reboot
//! ```

use std::arch::asm;
use std::fs;
use std::io::{Read, Write};
use std::process::{Command, Stdio};

fn rdtsc() -> u64 {
    let (lo, hi): (u32, u32);
    unsafe { asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack)) };
    (hi as u64) << 32 | lo as u64
}

unsafe fn sc(n: usize, a: usize, b: usize, c: usize, d: usize, e: usize, f: usize) -> isize {
    let r: isize;
    asm!(
        "syscall",
        inlateout("rax") n => r,
        in("rdi") a, in("rsi") b, in("rdx") c, in("r10") d, in("r8") e, in("r9") f,
        lateout("rcx") _, lateout("r11") _,
    );
    r
}

fn getpid() -> isize {
    unsafe { sc(39, 0, 0, 0, 0, 0, 0) }
}

/// PID 1: devtmpfs → /dev, консоль → fd 0/1/2, procfs → /proc.
fn setup_console() {
    unsafe {
        let devtmpfs = b"devtmpfs\0".as_ptr() as usize;
        let dev = b"/dev\0".as_ptr() as usize;
        sc(165, devtmpfs, dev, devtmpfs, 0, 0, 0); // mount
        let console = b"/dev/console\0".as_ptr() as usize;
        let fd = sc(2, console, 2 /*O_RDWR*/, 0, 0, 0, 0); // open → наименьший fd = 0
        sc(33, fd as usize, 1, 0, 0, 0, 0); // dup2 → stdout
        sc(33, fd as usize, 2, 0, 0, 0, 0); // dup2 → stderr
        let proc_ = b"proc\0".as_ptr() as usize;
        let procdir = b"/proc\0".as_ptr() as usize;
        sc(165, proc_, procdir, proc_, 0, 0, 0);
    }
}

fn report(name: &str, iters: u64, ticks: u64) {
    println!("    {}: {} итер · {} µs всего · ~{} ns/op", name, iters, ticks / 1000, ticks / iters);
}

fn bench() {
    setup_console();
    println!("  [bench-linux] гость Linux в том же QEMU (TCG, q35, 128M):");

    // 1. Null syscall: getpid (не vDSO — честный syscall).
    let n = 1000;
    let t0 = rdtsc();
    for _ in 0..n {
        std::hint::black_box(getpid());
    }
    report("null syscall (getpid)", n, rdtsc() - t0);

    // 2. IPC-пинг: процесс-партнёр, байт туда (pipe stdin) — байт обратно (pipe stdout).
    let mut child = Command::new("/init")
        .arg("echo")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn echo");
    let mut ci = child.stdin.take().unwrap();
    let mut co = child.stdout.take().unwrap();
    let mut b = [0u8; 1];
    let n = 300;
    let t0 = rdtsc();
    for _ in 0..n {
        ci.write_all(&b).unwrap();
        co.read_exact(&mut b).unwrap();
    }
    report("IPC pipe туда-обратно", n, rdtsc() - t0);
    drop(ci);
    let _ = child.wait();

    // 3. Page fault: mmap MAP_ANONYMOUS|MAP_PRIVATE, касание страниц.
    let pages = 256usize;
    let addr = unsafe { sc(9, 0, pages * 4096, 3 /*RW*/, 0x22 /*PRIVATE|ANON*/, usize::MAX, 0) };
    let t0 = rdtsc();
    for i in 0..pages {
        unsafe { std::ptr::write_volatile((addr as usize + i * 4096) as *mut u8, 1) };
    }
    report("page fault (anon mmap)", pages as u64, rdtsc() - t0);

    // 4. «obj_put»-аналог: создать файл в ramfs и записать 32 байта (без хэширования —
    //    у Linux нет дедупа; это ЕГО цена появления именованного 32-байтного объекта).
    let n = 100;
    let data = [7u8; 32];
    let t0 = rdtsc();
    for i in 0..n {
        fs::write(format!("/f{}", i), data).unwrap();
    }
    report("файл 32 Б (create+write, ramfs)", n, rdtsc() - t0);

    // 5. «obj_get»-аналог: прочитать файл обратно.
    let n = 100;
    let t0 = rdtsc();
    for _ in 0..n {
        std::hint::black_box(fs::read("/f0").unwrap());
    }
    report("чтение файла (open+read+close)", n, rdtsc() - t0);

    // 6. exec: полный цикл fork+execve+wait той же программы в роли noop.
    let n = 5;
    let t0 = rdtsc();
    for _ in 0..n {
        Command::new("/init").arg("noop").status().unwrap();
    }
    report("exec /init noop (полный цикл)", n, rdtsc() - t0);

    // Память: сколько RAM занято после загрузки ядра+init.
    if let Ok(mi) = fs::read_to_string("/proc/meminfo") {
        let get = |k: &str| {
            mi.lines()
                .find(|l| l.starts_with(k))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0)
        };
        let (total, free, avail) = (get("MemTotal"), get("MemFree"), get("MemAvailable"));
        println!(
            "  [mem-linux] MemTotal {} КиБ · занято {} КиБ (Available {} КиБ) · машина 128 МиБ",
            total, total - free, avail,
        );
        println!("  [mem-linux] (128 МиБ − MemTotal = {} КиБ съедено самим образом ядра до учёта)",
            128 * 1024 - total);
    }

    // Выключить машину: reboot(RB_POWER_OFF).
    let _ = std::io::stdout().flush();
    unsafe { sc(169, 0xfee1dead, 672274793, 0x4321fedc, 0, 0, 0) };
}

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_default();
    match arg.as_str() {
        "echo" => {
            let (mut si, mut so) = (std::io::stdin(), std::io::stdout());
            let mut b = [0u8; 1];
            while si.read_exact(&mut b).is_ok() {
                so.write_all(&b).unwrap();
                so.flush().unwrap();
            }
        }
        "noop" => {}
        _ => bench(),
    }
}
