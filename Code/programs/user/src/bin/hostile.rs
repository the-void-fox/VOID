//! Веха 126.5 — **враждебный процесс**: бьёт по границе «пользователь → ядро».
//!
//! Зачем. Веха 126.4 стоила дня поисков из-за ОДНОГО бита: процессор при входе в trap не
//! сбрасывает флаг направления, и он приезжал в ядро взведённым из userspace. Дальше `memcpy`
//! ядра (это `rep movsb`) шёл назад и затирал собственный адрес возврата — машина вставала
//! намертво, но позже и в другом месте ([[direction-flag]]).
//!
//! Ошибка была не в `memcpy`. Класс такой: **ядро что-то предполагает о своём окружении, а
//! устанавливать это никто не обязан.** Защита от класса — не список флагов, а процесс, который
//! приходит на границу с нарочно испорченным состоянием и требует, чтобы ядро выжило.
//!
//! Что проверяется:
//!
//! 1. **Ядро переживает чужое состояние.** Флаг направления (x86), флаг выравнивания, мусор в
//!    регистрах — и всё это на входе в syscall, много раз подряд. Провал здесь виден не строкой
//!    «не прошёл», а гибелью машины: именно так эта ошибка и выглядела.
//! 2. **Ядро не портит состояние процесса.** Обратная сторона: `cld` ядра не имеет права
//!    просочиться в процесс — `iretq` обязан вернуть флаги владельцу. Без этой проверки
//!    «исправление», сбрасывающее DF в КАДРЕ, выглядело бы удачным и молча ломало бы чужой
//!    `memmove`.
//!
//! Бьём по `SYS_TIME` (36) — тому самому вызову, что стоял в аварийном дампе: он дешёвый, и
//! каждый его вызов проводит ядро через `resume`, где и лежало то самое копирование кадра.
//!
//! Запуск: `hostile` в vvsh. Код возврата 0 — все проверки прошли.

#![no_std]
#![no_main]

const ROUNDS: usize = 4096;

/// Магия для регистров: не 0 и не маленькое число — такие ядро могло бы «случайно» совпасть.
const JUNK: usize = 0x5641_4C49_4E41_4359; // "VALINACY"

fn ok(name: &str) {
    void_user::write("  ".as_bytes());
    void_user::write(name.as_bytes());
    void_user::write(" … ok\n".as_bytes());
}

fn fail(name: &str, got: usize, want: usize) -> ! {
    void_user::write("  ".as_bytes());
    void_user::write(name.as_bytes());
    void_user::write(" … НЕ ПРОШЁЛ: получено ".as_bytes());
    hex(got);
    void_user::write(", ожидалось ".as_bytes());
    hex(want);
    void_user::write("\n[hostile] ПРОВАЛ\n".as_bytes());
    void_user::exit(1);
}

/// Небольшое десятичное число (счётчик проверок).
fn dec(v: usize) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    let mut v = v;
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    void_user::write(&buf[i..]);
}

fn hex(v: usize) {
    let mut buf = [0u8; 18];
    buf[0] = b'0';
    buf[1] = b'x';
    for i in 0..16 {
        let nib = (v >> (60 - i * 4)) & 0xf;
        buf[2 + i] = if nib < 10 { b'0' + nib as u8 } else { b'a' + (nib - 10) as u8 };
    }
    void_user::write(&buf);
}

// ─── x86_64 ──────────────────────────────────────────────────────────────────

/// `SYS_TIME(1)` со ВЗВЕДЁННЫМ флагом направления ровно на время вызова. Возвращает результат
/// и флаги, какими они стали ПОСЛЕ возврата из ядра (второе — проверка, что ядро их не отняло).
///
/// `std` стоит вплотную к `int 0x80`, а `cld` — сразу за ним: испорченный флаг видит только
/// ядро, свой же код между этими инструкциями ничего не копирует.
#[cfg(target_arch = "x86_64")]
fn time_with_df() -> (usize, usize) {
    let (ret, flags): (usize, usize);
    unsafe {
        core::arch::asm!(
            "std",
            "int 0x80",
            "pushfq",
            "pop {flags}",
            "cld",
            flags = out(reg) flags,
            inout("rax") 36usize => ret,
            inout("rdi") 1usize => _,
            out("rsi") _, out("rdx") _, out("r10") _, out("r8") _, out("r9") _,
            out("rcx") _, out("r11") _,
        );
    }
    (ret, flags)
}

/// То же со взведённым флагом ВЫРАВНИВАНИЯ (`AC`, бит 18). Сегодня он безобиден: SMAP у нас не
/// включён. Проверка стоит здесь заранее — в тот день, когда SMAP включат, `AC` из userspace
/// начнёт отключать саму защиту, и этот тест обязан заметить это сразу, а не через полгода.
#[cfg(target_arch = "x86_64")]
fn time_with_ac() -> usize {
    let ret: usize;
    unsafe {
        core::arch::asm!(
            "pushfq",
            "pop {tmp}",
            "or {tmp}, {ac}",
            "push {tmp}",
            "popfq",
            "int 0x80",
            "pushfq",
            "pop {tmp}",
            "and {tmp}, {nac}",
            "push {tmp}",
            "popfq",
            tmp = out(reg) _,
            ac = in(reg) 1usize << 18,
            nac = in(reg) !(1usize << 18),
            inout("rax") 36usize => ret,
            inout("rdi") 1usize => _,
            out("rsi") _, out("rdx") _, out("r10") _, out("r8") _, out("r9") _,
            out("rcx") _, out("r11") _,
        );
    }
    ret
}

/// Мусор в регистрах, которые ядро обязано вернуть нетронутыми (оно снимает их с кадра trap'а).
/// Возвращает то, что осталось в `r12`..`r15` после syscall'а.
#[cfg(target_arch = "x86_64")]
fn time_with_junk_regs() -> [usize; 4] {
    let (a, b, c, d): (usize, usize, usize, usize);
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inout("rax") 36usize => _,
            inout("rdi") 1usize => _,
            inout("r12") JUNK => a,
            inout("r13") JUNK ^ 1 => b,
            inout("r14") JUNK ^ 2 => c,
            inout("r15") JUNK ^ 3 => d,
            out("rsi") _, out("rdx") _, out("r10") _, out("r8") _, out("r9") _,
            out("rcx") _, out("r11") _,
        );
    }
    [a, b, c, d]
}

/// Управляющее слово SSE процесса: ядро собрано с soft-float и XMM не трогает, но состояние
/// носит в кадре — значит обязано вернуть его в точности.
#[cfg(target_arch = "x86_64")]
fn mxcsr_roundtrip() -> (u32, u32) {
    let mut before: u32 = 0;
    let mut after: u32 = 0;
    unsafe {
        core::arch::asm!("stmxcsr [{0}]", in(reg) &mut before);
        // Округление к −∞ (биты 13-14 = 01) поверх обычной маски исключений — значение
        // заведомо «не по умолчанию», подмену такого не пропустишь.
        let poisoned: u32 = (before & !(0b11 << 13)) | (0b01 << 13);
        core::arch::asm!("ldmxcsr [{0}]", in(reg) &poisoned);
        core::arch::asm!(
            "int 0x80",
            inout("rax") 36usize => _, inout("rdi") 1usize => _,
            out("rsi") _, out("rdx") _, out("r10") _, out("r8") _, out("r9") _,
            out("rcx") _, out("r11") _,
        );
        core::arch::asm!("stmxcsr [{0}]", in(reg) &mut after);
        core::arch::asm!("ldmxcsr [{0}]", in(reg) &before); // вернуть себе нормальный режим
        (poisoned, after)
    }
}

#[cfg(target_arch = "x86_64")]
fn arch_checks() -> usize {
    const DF: usize = 1 << 10;

    // 1. Флаг направления на входе в syscall. Ошибка Вехи 126.4 убивала машину здесь — не
    //    строкой «не прошёл», а тем, что до следующей строки дело не доходило.
    let mut last = 0usize;
    for _ in 0..ROUNDS {
        let (t, flags) = time_with_df();
        if flags & DF == 0 {
            fail("флаг направления пережил syscall", flags & DF, DF);
        }
        if t < last {
            fail("монотонное время не пошло назад", t, last);
        }
        last = t;
    }
    ok("флаг направления взведён на входе в syscall");
    ok("флаг направления процесса пережил syscall");

    // 2. Флаг выравнивания — задел под SMAP (см. `time_with_ac`).
    for _ in 0..ROUNDS {
        let t = time_with_ac();
        if t < last {
            fail("время под AC=1", t, last);
        }
        last = t;
    }
    ok("флаг выравнивания взведён на входе в syscall");

    // 3. Регистры возвращаются нетронутыми.
    for _ in 0..64 {
        let r = time_with_junk_regs();
        let want = [JUNK, JUNK ^ 1, JUNK ^ 2, JUNK ^ 3];
        for i in 0..4 {
            if r[i] != want[i] {
                fail("мусор в r12..r15 пережил syscall", r[i], want[i]);
            }
        }
    }
    ok("мусор в r12..r15 пережил syscall");

    // 4. Состояние SSE процесса.
    for _ in 0..64 {
        let (want, got) = mxcsr_roundtrip();
        if got != want {
            fail("MXCSR процесса пережил syscall", got as usize, want as usize);
        }
    }
    ok("MXCSR процесса пережил syscall");

    5
}

// ─── riscv64 ─────────────────────────────────────────────────────────────────

/// Мусор в регистрах-сохраняемых (`s2`..`s5`) вокруг `ecall`.
#[cfg(target_arch = "riscv64")]
fn time_with_junk_regs() -> [usize; 4] {
    let (a, b, c, d): (usize, usize, usize, usize);
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") 36usize,
            inout("a0") 1usize => _,
            inout("s2") JUNK => a,
            inout("s3") JUNK ^ 1 => b,
            inout("s4") JUNK ^ 2 => c,
            inout("s5") JUNK ^ 3 => d,
            out("a1") _, out("a2") _, out("a3") _,
            options(nostack),
        );
    }
    [a, b, c, d]
}

/// На riscv флага направления нет и строковых операций «назад» тоже — ошибка Вехи 126.4 сюда не
/// переносится. Но САМ ВОПРОС («что вход в trap предполагает и не устанавливает») здесь ещё не
/// задавали: разбор входа riscv — отдельный долг ([[known-gaps]]). Пока проверяем то, что общее
/// для обеих арх: ядро обязано вернуть регистры процесса в точности.
#[cfg(target_arch = "riscv64")]
fn arch_checks() -> usize {
    void_user::write("  (флагов направления и выравнивания на riscv нет — эти проверки x86)\n".as_bytes());
    for _ in 0..64 {
        let r = time_with_junk_regs();
        let want = [JUNK, JUNK ^ 1, JUNK ^ 2, JUNK ^ 3];
        for i in 0..4 {
            if r[i] != want[i] {
                fail("мусор в s2..s5 пережил syscall", r[i], want[i]);
            }
        }
    }
    ok("мусор в s2..s5 пережил syscall");
    1
}

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    void_user::write("[hostile] бью по границе «пользователь → ядро»\n".as_bytes());
    let n = arch_checks();
    void_user::write("[hostile] ПРОШЁЛ, проверок: ".as_bytes());
    dec(n);
    void_user::write("\n".as_bytes());
    void_user::exit(0)
}
