//! Local APIC (xAPIC, MMIO 0xfee0_0000) — таймер вытеснения x86_64 (Веха 25).
//!
//! Аналог пары SBI TIME + sie.STIE на RISC-V: срабатывание программируется каждый раз заново.
//! Страница LAPIC отображается UC в таблицах ядра ([`super::paging::init`]); работаем до
//! IOAPIC — линии устройств придут с virtio-pci (Веха 27+).
//!
//! # Веха 145.4 — срок, а не квант
//!
//! Здесь был ровно один способ взвести таймер: «через 20 миллисекунд». Он же обслуживал и сон
//! по сроку — процесс, попросивший поспать 6 мс, просыпался на ближайшем кванте, то есть
//! когда придётся. Замер Вехи 145.3: просили 6.6 мс, спали 10.6. Для анимации это разница
//! между 60 кадрами в секунду и 50, и никаким подбором паузы в пользователе она не лечится —
//! пользователь не знает, когда его разбудят.
//!
//! Теперь у таймера есть [`arm_at`] — «разбуди в этот момент», и момент задаётся в тех же
//! тиках, в которых живут сроки ядра (`rdtsc`). Способов два:
//!
//! - **TSC-deadline** (CPUID.01H:ECX[24], Sandy Bridge и новее): срок пишется прямо в
//!   `IA32_TSC_DEADLINE`, сравнивает его процессор — ни пересчёта, ни калибровки, ни дрейфа.
//! - **one-shot** на машинах постарше: счёт в тиках шины таймера, поэтому шину приходится
//!   МЕРИТЬ (см. [`calibrate`]).
//!
//! Мерить пришлось и ради кванта вытеснения. Здесь стояло `INTERVAL = 20_000_000` тиков с
//! припиской «шина в QEMU ходит на ~1 ГГц, это 20 мс». На живом железе шина этого класса —
//! 100 МГц, и тот же счёт означал бы квант в **200 мс**: вытеснение раз в пятую долю секунды
//! вместо двадцати миллисекунд. Ошибка тихая ровно как все прочие ошибки времени — работает,
//! просто не то.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use core::ptr::{read_volatile, write_volatile};

use super::trap::{VEC_SPURIOUS, VEC_TIMER};

pub const LAPIC_BASE: usize = 0xfee0_0000;

// Смещения регистров (Intel SDM, том 3A, глава 13).
const REG_EOI: usize = 0x0b0;
const REG_SVR: usize = 0x0f0; // Spurious Interrupt Vector Register
const REG_LVT_TIMER: usize = 0x320;
const REG_TIMER_INIT: usize = 0x380; // initial count (запись = старт отсчёта)
const REG_TIMER_CUR: usize = 0x390; // current count (в режиме срока всегда 0)
const REG_TIMER_DIV: usize = 0x3e0;

/// Биты 18:17 LVT — режим таймера (SDM, таблица 13-2): 00 one-shot, 01 периодический,
/// 10 TSC-deadline.
const LVT_MODE_DEADLINE: u32 = 0b10 << 17;
const LVT_MASKED: u32 = 1 << 16;

/// Срок в тиках `rdtsc`; запись ненулевого значения взводит таймер, срабатывание его обнуляет.
const IA32_TSC_DEADLINE: u32 = 0x6e0;

/// Квант вытеснения — тот же, что на RISC-V. В НАНОСЕКУНДАХ, а не в тиках чего бы то ни было:
/// тики у каждой таймбазы свои, а двадцать миллисекунд — это двадцать миллисекунд.
const QUANTUM_NS: u64 = 20_000_000;

/// Запасной счёт one-shot, если шину измерить не вышло: прежняя константа Вехи 25 (≈20 мс при
/// шине 1 ГГц). Плохо, но лучше, чем остановленный таймер.
const FALLBACK_COUNT: u32 = 20_000_000;

/// Умеет ли процессор TSC-deadline. Проверяется, а не предполагается: под TCG QEMU по умолчанию
/// показывает `qemu64`, где этой возможности нет вовсе.
static DEADLINE_MODE: AtomicBool = AtomicBool::new(false);

/// Тиков шины таймера на 65536 тиков TSC — для запасного пути. Ноль = не измерено.
static LAPIC_PER_TSC_Q16: AtomicU64 = AtomicU64::new(0);

#[inline]
fn w(reg: usize, v: u32) {
    unsafe { write_volatile((LAPIC_BASE + reg) as *mut u32, v) }
}

#[inline]
fn r(reg: usize) -> u32 {
    unsafe { read_volatile((LAPIC_BASE + reg) as *const u32) }
}

/// Есть ли CPUID.01H:ECX[24] — TSC-deadline (SDM 13.5.4.1, шаг 1).
fn has_tsc_deadline() -> bool {
    let ecx: u32;
    unsafe {
        // rbx сохраняем руками: LLVM держит его занятым и в `out` его не отдаёт.
        core::arch::asm!(
            "push rbx", "cpuid", "pop rbx",
            inout("eax") 1u32 => _,
            out("ecx") ecx,
            out("edx") _,
            options(nostack),
        );
    }
    ecx & (1 << 24) != 0
}

/// Измерить шину таймера LAPIC по TSC (частота которого уже измерена по PIT, Веха 136).
///
/// Возвращает «тиков шины на 65536 тиков TSC». Отдельная калибровка нужна ТОЛЬКО запасному
/// пути: в режиме срока шину знать не надо вовсе — процессор сравнивает TSC сам.
fn calibrate() -> u64 {
    // Окно 2 мс: длиннее — точнее, но это чистая задержка загрузки, а от запасного пути и не
    // требуется аптечная точность — требуется не промахнуться в разы.
    let window = crate::clock::ns_to_ticks(2_000_000);
    if window == 0 {
        return 0;
    }
    w(REG_LVT_TIMER, VEC_TIMER as u32 | LVT_MASKED); // на время замера прерывание не нужно
    w(REG_TIMER_INIT, u32::MAX);
    let t0 = super::now_ticks();
    while super::now_ticks().wrapping_sub(t0) < window {}
    let left = r(REG_TIMER_CUR);
    w(REG_TIMER_INIT, 0); // запись нуля останавливает счёт
    let spent = (u32::MAX - left) as u64;
    // Счётчик успел добежать до нуля — окно длиннее, чем он считает: измерение не состоялось.
    if left == 0 || spent == 0 {
        return 0;
    }
    (spent << 16) / window
}

/// Включить LAPIC (SVR: enable + spurious-вектор), делитель 1, LVT-таймер на вектор
/// [`VEC_TIMER`]. Режим — TSC-deadline, если процессор умеет; иначе one-shot с измеренной шиной.
pub fn init() {
    w(REG_SVR, (1 << 8) | VEC_SPURIOUS as u32); // APIC enable + spurious vector
    w(REG_TIMER_DIV, 0b1011); // делитель 1
    let ratio = calibrate();
    LAPIC_PER_TSC_Q16.store(ratio, Ordering::Relaxed);
    let deadline = has_tsc_deadline();
    DEADLINE_MODE.store(deadline, Ordering::Relaxed);
    let mode = if deadline { LVT_MODE_DEADLINE } else { 0 };
    w(REG_LVT_TIMER, VEC_TIMER as u32 | mode); // не маскирован
    let _ = r(REG_SVR); // сериализовать записи

    // Говорим вслух, чем мерим время. «Предполагаем» и «измерили» обязаны отличаться на экране:
    // прошлая ошибка времени (Веха 136, TSC втрое быстрее заявленного) была тихой ровно потому,
    // что предположение нигде не печаталось.
    if deadline {
        crate::println!("  таймер: TSC-deadline — пробуждение по сроку, без пересчёта");
    } else if ratio != 0 {
        let hz = (ratio * crate::clock::ns_to_ticks(1_000_000_000)) >> 16;
        crate::println!("  таймер: one-shot, шина {} МГц (измерена по TSC)", hz / 1_000_000);
    } else {
        crate::println!("  таймер: one-shot, шину измерить НЕ вышло — кванты приблизительные");
    }

    // Взвести здесь, а не полагаться на `timer::init` — он зовёт `arm()` ДО нас, а калибровка
    // выше останавливает счётчик записью нуля. Без этой строки система осталась бы вовсе без
    // вытеснения, и заметно это стало бы не сразу.
    arm();
}

/// Перевзвести на квант вытеснения: «сейчас + 20 мс».
pub fn arm() {
    arm_at(super::now_ticks() + crate::clock::ns_to_ticks(QUANTUM_NS));
}

/// Веха 145.4 — разбудить В ЭТОТ момент (тики `rdtsc`, та же шкала, что у сроков ядра).
///
/// Срок в прошлом — не ошибка, а «прямо сейчас»: так зовущему не приходится проверять время
/// второй раз, а гонка «срок истёк между проверкой и взводом» перестаёт существовать.
pub fn arm_at(deadline: u64) {
    if DEADLINE_MODE.load(Ordering::Relaxed) {
        // SDM 13.5.4.1: в xAPIC-режиме запись LVT идёт в память, а срок — в MSR, и процессор
        // сам их не упорядочивает. Между ними обязан стоять MFENCE — иначе таймер может
        // остаться со СТАРЫМ режимом и новым сроком.
        unsafe { core::arch::asm!("mfence", options(nostack, nomem)) };
        // Ноль в этом MSR означает «разоружить», поэтому нулевой срок подменяем единицей:
        // он всё равно в прошлом, то есть сработает немедленно, чего и просили.
        unsafe { super::wrmsr(IA32_TSC_DEADLINE, deadline.max(1)) };
        return;
    }
    // Запасной путь: one-shot считает в тиках ШИНЫ, значит остаток срока надо перевести.
    let left = deadline.saturating_sub(super::now_ticks());
    let q = LAPIC_PER_TSC_Q16.load(Ordering::Relaxed);
    let count = if q == 0 {
        FALLBACK_COUNT
    } else {
        // Ноль в initial count ОСТАНАВЛИВАЕТ таймер (SDM 13.5.4) — то есть «разбуди сейчас»
        // превратилось бы в «не буди никогда». Поэтому снизу единица.
        ((left * q) >> 16).clamp(1, u32::MAX as u64) as u32
    };
    w(REG_TIMER_INIT, count);
}

/// Веха 170 — **включить LAPIC ПРИКЛАДНОГО ядра**: только разрешение и spurious-вектор.
///
/// Ни калибровки, ни таймера: запаркованное ядро ничего не отсчитывает, а `calibrate` ещё и
/// пишет в общие статики — двум ядрам там делать нечего. Таймер маскируем явно: прошивка могла
/// оставить LVT в любом состоянии, а прерывание на ядре без обработчика убивает машину.
pub fn init_ap() {
    w(REG_SVR, (1 << 8) | VEC_SPURIOUS as u32);
    w(REG_LVT_TIMER, VEC_TIMER as u32 | LVT_MASKED);
    let _ = r(REG_SVR);
}

/// Идентификатор ЭТОГО локального APIC (xAPIC — старший байт регистра 0x020).
pub fn id() -> u8 {
    (r(0x020) >> 24) as u8
}

// ── межпроцессорные прерывания (SDM 3A, 9.6) ───────────────────────────────────────────────
//
// ICR — два регистра: в старший пишется адресат (биты 24–31), в младший — всё остальное.
// Запись МЛАДШЕГО и отправляет сообщение, поэтому порядок обязателен.
const REG_ICR_LO: usize = 0x300;
const REG_ICR_HI: usize = 0x310;
/// Бит 12 младшего ICR: сообщение ещё не принято. Ждать его до записи следующего — не
/// вежливость: второе сообщение затёрло бы первое.
const ICR_PENDING: u32 = 1 << 12;

fn icr_wait() {
    for _ in 0..1_000_000 {
        if r(REG_ICR_LO) & ICR_PENDING == 0 {
            return;
        }
        core::hint::spin_loop();
    }
}

fn icr_send(dest: u8, low: u32) {
    icr_wait();
    w(REG_ICR_HI, (dest as u32) << 24);
    w(REG_ICR_LO, low);
    icr_wait();
}

/// INIT — сбросить ядро в состояние «ждёт SIPI».
///
/// # Safety
/// Адресат обязан быть ЧУЖИМ ядром: INIT самому себе перезапускает текущий процессор.
pub unsafe fn send_init(dest: u8) {
    // 0b101 << 8 — режим доставки INIT; 1 << 14 — level assert.
    icr_send(dest, (0b101 << 8) | (1 << 14));
}

/// SIPI — «стартуй с физического адреса `vector << 12`».
///
/// # Safety
/// Вектор обязан указывать на подготовленный трамплин (см. [`super::smp`]).
pub unsafe fn send_sipi(dest: u8, vector: u8) {
    // 0b110 << 8 — режим доставки Startup; level assert так же, как у INIT.
    icr_send(dest, (0b110 << 8) | (1 << 14) | vector as u32);
}

/// End-of-interrupt — сообщить LAPIC, что вектор обслужен (иначе следующий не придёт).
pub fn eoi() {
    w(REG_EOI, 0);
}

/// Замаскирован ли LVT-таймер (бит 16) — снимок для масок сессий (Веха 26).
pub fn timer_masked() -> bool {
    r(REG_LVT_TIMER) & LVT_MASKED != 0
}

/// Маскировать/размаскировать LVT-таймер, не трогая вектор и РЕЖИМ.
///
/// Режим трогать нельзя не из аккуратности: переход между режимами разоружает таймер (SDM
/// 13.5.4.1), то есть маскирование на сессию стирало бы уже назначенный срок.
pub fn set_timer_masked(masked: bool) {
    let v = r(REG_LVT_TIMER);
    w(REG_LVT_TIMER, if masked { v | LVT_MASKED } else { v & !LVT_MASKED });
}
