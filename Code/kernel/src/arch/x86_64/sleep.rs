//! Веха 241 — **СОН S3**: погасить машину, сохранив память, и вернуться к той же работе.
//!
//! ## Что в этом сне от ACPI, а что нет
//!
//! От ACPI — ровно два числа (`SLP_TYP` из объекта `_S3_`) и один адрес (куда прошивка вернёт
//! управление, поле в FACS). Всё остальное — работа с процессором: сохранить состояние,
//! пережить потерю ВСЕГО, что в нём было, и собрать себя заново. Поэтому интерпретатор AML тут
//! почти не нужен, и сон живёт в ядре, а не в сервере.
//!
//! Дорого не «уснуть», дорого «проснуться» — это было записано в [[laptop-base]] ещё до начала
//! работы, и оказалось правдой.
//!
//! ## Как возвращается управление
//!
//! Прошивка будит процессор **в реальном режиме** и прыгает по адресу, который мы заранее
//! положили в FACS. Это ровно та же дорога, по которой у нас поднимаются прикладные ядра после
//! SIPI, — поэтому трамплин здесь ТОТ ЖЕ (`ap.s`, см. [`super::smp::install_tramp`]), только с
//! другой точкой входа. Двух копий такому коду быть не должно.
//!
//! ## Чего этот сон ещё НЕ умеет — названо вслух
//!
//! 1. **Только на одном ядре.** Прикладные ядра засыпают вместе с машиной, а их состояние в
//!    планировщике остаётся «работает». Парковка ядер перед сном — отдельная работа, и пока
//!    она не сделана, сон на SMP ОТКАЗЫВАЕТ, а не рискует.
//! 2. **Устройства поднимаются не все.** После S3 прошивка возвращает железо в состояние,
//!    близкое к включению: контроллеры USB, диск и видео надо поднимать заново. Здесь сделано
//!    то, без чего не обойтись (прерывания, таймер, клавиатура, экран); сеть и звук живут в
//!    процессах-драйверах, и их правильное лечение — перезапуск процесса, а не починка изнутри.
//! 3. **Счётчик тактов обнуляется.** На живой машине `rdtsc` после сна идёт с нуля, и этого
//!    достаточно, чтобы все сроки в системе уехали в далёкое будущее. Лечится сдвигом
//!    ([`super::tsc_add_offset`]), который считается по часам RTC.

use core::sync::atomic::{AtomicU64, Ordering};

use super::{acpi, gdt, lapic, paging, smp, trap};

unsafe extern "C" {
    fn s3_save(ctx: *mut u64) -> u64;
    fn s3_restore(ctx: *const u64) -> !;
    // Веха 243.4 — трамплин пробуждения (`s3tramp.s`). Смещения полей считаем ПО СИМВОЛАМ:
    // держать их числами значило бы завести вторую истину о раскладке.
    static s3_tramp_start: u8;
    static s3_tramp_end: u8;
    static s3_cr3: u8;
    static s3_stack: u8;
    static s3_entry: u8;
    static s3_mode_w: u8;
    static s3_mode_h: u8;
    static s3_mode_bpp: u8;
    static s3_mode_w2: u8;
    static s3_mode_h2: u8;
    static s3_mode_flags: u8;
    static s3_mode_result: u8;
    static s3_mode_lfb: u8;
    static s3_mode_pitch: u8;
}

/// Куда кладётся трамплин пробуждения. Отдельная страница от трамплина прикладных ядер
/// (`0x8000`): у них разные прологи, а жить в низкой памяти обязаны оба.
const S3_BASE: usize = 0x9000;

/// Звать ли POST видео-BIOS на следующем пробуждении. Включается сама, если режим вернуть не
/// вышло: средство тяжёлое, и начинать с него незачем.
static USE_POST: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Положить трамплин пробуждения в низкую память и назвать ему всё, что он должен знать.
///
/// # Safety
/// Пишет в физическую страницу [`S3_BASE`]. Она ниже кучи ядра, и аллокатор фреймов туда не
/// ходит (`frame::alloc_floor` начинается за образом ядра).
unsafe fn install_tramp(w: u16, h: u16, bpp: u16, w2: u16, h2: u16) -> bool {
    let Some(cr3) = smp::boot_cr3() else {
        crate::println!("  [acpi] загрузочные таблицы выше 4 ГиБ — трамплин пробуждения не поставить");
        return false;
    };
    let src = &raw const s3_tramp_start as usize;
    let len = &raw const s3_tramp_end as usize - src;
    let dst = crate::arch::phys_to_virt(S3_BASE) as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(src as *const u8, dst, len);
        let at = |sym: usize| dst.add(sym - src);
        (at(&raw const s3_cr3 as usize) as *mut u32).write_volatile(cr3);
        (at(&raw const s3_stack as usize) as *mut u64)
            .write_volatile(crate::cpu::idle_top(0) as u64);
        (at(&raw const s3_entry as usize) as *mut u64)
            .write_volatile(resume_entry as *const () as u64);
        (at(&raw const s3_mode_w as usize) as *mut u16).write_volatile(w);
        (at(&raw const s3_mode_h as usize) as *mut u16).write_volatile(h);
        (at(&raw const s3_mode_bpp as usize) as *mut u16).write_volatile(bpp);
        // Запасной режим — тот, что ставил загрузчик. Нужен, потому что режим, в котором
        // система работает, видео-BIOS может не знать вовсе: на панель её родного размера
        // ядро переходит САМО (Веха 232), и в списке VBE такого режима нет.
        (at(&raw const s3_mode_w2 as usize) as *mut u16).write_volatile(w2);
        (at(&raw const s3_mode_h2 as usize) as *mut u16).write_volatile(h2);
        // Бит 1 — ставить режим; бит 0 — звать POST (только если в прошлый раз не вышло).
        let flags = 2 | u16::from(USE_POST.load(Ordering::Relaxed));
        (at(&raw const s3_mode_flags as usize) as *mut u16).write_volatile(flags);
        (at(&raw const s3_mode_result as usize) as *mut u16).write_volatile(0);
    }
    true
}

/// Что трамплин сделал с видеорежимом: `(результат, адрес кадра, шаг строки)`.
unsafe fn tramp_video() -> (u16, u32, u16) {
    let src = &raw const s3_tramp_start as usize;
    let dst = crate::arch::phys_to_virt(S3_BASE) as *const u8;
    unsafe {
        let at = |sym: usize| dst.add(sym - src);
        (
            (at(&raw const s3_mode_result as usize) as *const u16).read_volatile(),
            (at(&raw const s3_mode_lfb as usize) as *const u32).read_volatile(),
            (at(&raw const s3_mode_pitch as usize) as *const u16).read_volatile(),
        )
    }
}

/// Время по ЧАСАМ МАШИНЫ, секунды Unix. Именно у железных часов, а не у системных: системные
/// считаются как «база плюс монотонный счётчик», а он сон не пережил. 0 — часов нет.
fn rtc_secs() -> u64 {
    super::wall_clock_unix_ns().unwrap_or(0) / 1_000_000_000
}

/// Сохранённое состояние: rsp, rbp, rbx, r12–r15 и адрес возврата (см. `s3.s`).
static mut CTX: [u64; 8] = [0; 8];

/// Сколько тиков натикало до засыпания — чтобы монотонное время не пошло назад после сна.
static TICKS_BEFORE: AtomicU64 = AtomicU64::new(0);
/// Настенное время (секунды Unix) в момент засыпания — им меряется длительность сна.
static WALL_BEFORE: AtomicU64 = AtomicU64::new(0);
/// Яркость панели до сна, проценты (`u32::MAX` — не знали). Регистр ШИМ сон не переживает, и
/// без этого экран может вернуться живым, но ЧЁРНЫМ — худший вид «не проснулось».
static BACKLIGHT_BEFORE: AtomicU64 = AtomicU64::new(u64::MAX);

/// Точка входа ПОСЛЕ пробуждения. Сюда прыгает трамплин, уже в длинном режиме, на загрузочных
/// таблицах и на стеке простоя загрузочного ядра.
extern "C" fn resume_entry() -> ! {
    unsafe {
        // Порядок тот же, что у прикладного ядра (`ap_main`), и по тем же причинам: сперва
        // настоящие таблицы ядра, потом своя GDT с TSS, потом вектор прерываний.
        super::mm_enable(paging::kernel_root());
        // Строка PAT (write-combining) живёт в MSR, а MSR после сна чист. Без неё консоль и
        // композитор писали бы в кадр через UC — то есть в двадцать раз медленнее (Веха 239.1).
        paging::pat_init();
        gdt::init_ap();
        trap::init_ap();
        lapic::init();
        // РЕЖИМ ACPI. После сна чипсет на живой машине возвращается в legacy: прошивка
        // восстановила свой блок питания и свои SMI, а бит SCI_EN оставила сброшенным. Без
        // режима ACPI прерывания прошивки не приходят вовсе — ни кнопка, ни крышка, ни
        // контроллер, — и кнопку питания обрабатывает прошивка, то есть гасит машину мимо
        // синхронизации store. Возвращаем режим себе.
        if !acpi::enter_acpi_mode() {
            crate::println!("  [acpi] режим ACPI после сна вернуть не вышло — событий прошивки не будет");
        }
        // Снять состояние событий PM1 ДО того, как разрешим прерывания: прошивка просыпается
        // с выставленными битами, и «нажатая» кнопка питания выключила бы машину вместо
        // пробуждения.
        acpi::clear_pm1_status();
        // Заново завести линии прерываний: IOAPIC после сна пуст, а с ним молчат и клавиатура,
        // и мышь, и консоль. Линия прошивки (SCI) заводится отдельно — теми же числами, какими
        // её заводили на загрузке.
        super::init_device_interrupts();
        super::reroute_sci();
        // Время. `rdtsc` на живой машине пошёл с нуля, и без сдвига все сроки системы
        // оказались бы в далёком будущем: процессы, спящие по сроку, не проснулись бы никогда.
        // Длительность сна берём у RTC — других часов, переживших сон, у нас нет.
        let slept = rtc_secs().saturating_sub(WALL_BEFORE.load(Ordering::Relaxed));
        let hz = crate::clock::tick_hz();
        super::tsc_set_offset(TICKS_BEFORE.load(Ordering::Relaxed) + slept * hz);
        // Часы подвести заново: настенное время у нас считается как «база + монотонное», а
        // база осталась от прошлой жизни счётчика. `clock::init` читает RTC и пересчитывает её
        // — ровно то, что нужно, и ровно тем же кодом, что на загрузке.
        crate::clock::init();
        // Конфигурация устройств шины: окна памяти и bus master после сна обнулены, и
        // восстанавливать их — работа системы, а не прошивки (см. `pci::save_state`).
        super::pci::restore_state();
        // Диск. Контроллер просыпается «как при включении»: командных списков не помнит, порт
        // не запущен. Без этого store отвечает списком корней (он в памяти), а СОДЕРЖИМОЕ
        // объектов прочитать нечем — и система молча перестаёт запускать программы.
        // БЛОК ПИТАНИЯ ЧИПСЕТА. Его адрес и бит «декодировать» живут в конфигурации южного
        // моста, а не в BAR, и прошивка возвращает их не всегда: в QEMU после пробуждения
        // регистр PM1 читается как `0xffff` — то есть портов нет вовсе. Без них машина
        // просыпается без кнопки питания, без событий прошивки и без возможности уснуть
        // второй раз (поймано ровно так: второй `suspend` отвечал «запись в PM1 ничего не
        // сделала»).
        //
        // Проверяем, а не чиним вслепую: на машине, где прошивка восстановила чипсет сама,
        // писать в его регистры незачем.
        if let Some((pm1a, _)) = acpi::pm1_control() {
            if super::io_read(pm1a, 2) == 0xffff && super::pci::restore_bridge() {
                crate::println!(
                    "  [acpi] блок питания чипсета не ответил после сна — вернули настройку моста ({:#06x})",
                    super::io_read(pm1a, 2),
                );
            }
        }
        if crate::object::store_on_ahci() {
            crate::ahci::resume();
        }
        // USB. Контроллер после сна тоже «как при включении», а прошивка за это время снова
        // взяла его себе (эмуляция клавиатуры через 8042). Без этого на машине, где клавиатура
        // única и живёт по USB, пробуждение означало бы «проснулись и остались без ввода».
        crate::ehci::resume();
        // ЭКРАН. Первым делом — что сделал трамплин: он пытался вернуть режим через
        // видео-BIOS, пока мы были в реальном режиме (единственное место, где это возможно).
        let (res, lfb, pitch) = tramp_video();
        let (w, h, our_pitch, _, _) = crate::fb::info();
        match res {
            1 => {
                crate::println!(
                    "  [video] видео-BIOS вернул режим {w}×{h} (кадр {lfb:#x}, шаг {pitch})",
                );
                // Кадр обязан лечь туда же, где он был: композитор и консоль пишут по старому
                // адресу. Разойдись они — картинка ушла бы в никуда, и молчать об этом нельзя.
                if let Some((base, _)) = crate::fb::window() {
                    if lfb != 0 && lfb as usize != base {
                        crate::println!(
                            "  [video] но кадр переехал: был {base:#x}, стал {lfb:#x} — картинки не будет",
                        );
                    }
                }
                if pitch != 0 && pitch as usize != our_pitch {
                    crate::println!(
                        "  [video] шаг строки сменился: был {our_pitch}, стал {pitch}",
                    );
                }
            }
            3 => {
                // Видео-BIOS знает только режим загрузчика — он его и поставил. Наш (родной
                // размер панели) ставили мы сами, значит и вернуть его должны сами: той же
                // дорогой, что на загрузке.
                crate::println!("  [video] видео-BIOS вернул режим загрузчика (кадр {lfb:#x})");
                if let Some((base, _)) = crate::fb::window() {
                    let (bw, bh, bbpp) = super::video_mode().unwrap_or((0, 0, 32));
                    let bpitch = bw * bbpp / 8;
                    if crate::intel_fb::init(base, bpitch, bw, bh) {
                        match crate::intel_fb::new_mode() {
                            Some((nw, nh, np)) if nw == w && nh == h && np == our_pitch => {
                                crate::println!("  [intel] родной размер панели вернули ({nw}×{nh})");
                            }
                            other => crate::println!(
                                "  [intel] после сна размер панели вышел другим: {other:?}, \
                                 а система работает в {w}×{h} (шаг {our_pitch})",
                            ),
                        }
                    }
                }
            }
            2 => {
                USE_POST.store(true, Ordering::Relaxed);
                crate::println!(
                    "  [video] видео-BIOS не нашёл режима {w}×{h} — в следующий раз позовём его POST",
                );
            }
            _ => {}
        }
        if crate::intel_fb::present() {
            let saved = match BACKLIGHT_BEFORE.load(Ordering::Relaxed) {
                u64::MAX => None,
                v => Some(v as u32),
            };
            crate::intel_fb::resume(saved);
        } else if res != 1 {
            // Запасной путь для эмуляторов: интерфейс Bochs VBE (DISPI). Зовётся, только если
            // видео-BIOS режима не вернул, — делать ту же работу дважды незачем.
            let (_, _, pitch, bpp, _) = crate::fb::info();
            if super::vbe::restore(w, h, pitch, bpp) {
                crate::println!("  [video] режим экрана поставлен заново через Bochs VBE ({w}×{h})");
            }
        }
        s3_restore(&raw const CTX as *const u64);
    }
}

/// Уснуть в S3. Возвращается ПОСЛЕ пробуждения — `Ok(())`; `Err` — не уснули и почему.
///
/// # Safety
/// Зовётся из ядра с запрещёнными прерываниями и при одном работающем ядре.
pub unsafe fn suspend() -> Result<(), &'static str> {
    let Some((slp_a, slp_b)) = acpi::s3_slp_typ() else {
        return Err("прошивка не объявляет сон S3 (нет объекта _S3_)");
    };
    let Some((pm1a, pm1b)) = acpi::pm1_control() else {
        return Err("в FADT нет блока управления PM1");
    };
    // Трамплин пробуждения — СВОЙ, не тот, которым поднимаются прикладные ядра: у него есть
    // пролог, возвращающий видеорежим через видео-BIOS (`s3tramp.s`), а звать видео-BIOS при
    // подъёме ядра посреди работы системы нельзя — экраном в этот момент владеет композитор.
    let (w, h, _pitch, bpp, _) = crate::fb::info();
    // Второй кандидат — режим ЗАГРУЗЧИКА: его видео-BIOS знает наверняка, а наш текущий может
    // не знать (на родной размер панели ядро переходит само).
    let (w2, h2) = super::video_mode().map_or((0, 0), |(w, h, _)| (w as u16, h as u16));
    if !unsafe { install_tramp(w as u16, h as u16, bpp as u16, w2, h2) } {
        return Err("трамплин возврата не поставить");
    }
    if !unsafe { acpi::set_waking_vector(S3_BASE as u32) } {
        return Err("FACS не найдена — некуда положить адрес возврата");
    }

    // Яркость панели: регистр ШИМ сон не переживёт, а вернуть её надо той же, какой была.
    BACKLIGHT_BEFORE.store(
        crate::intel_fb::backlight().map_or(u64::MAX, |v| v as u64),
        Ordering::Relaxed,
    );
    // Запомнить конфигурацию шины ДО сна: после него у устройств обнулены и окна памяти, и
    // bus master, а восстанавливать это обязана система.
    super::pci::save_state();
    TICKS_BEFORE.store(super::now_ticks(), Ordering::Relaxed);
    WALL_BEFORE.store(rtc_secs(), Ordering::Relaxed);

    // С этого места — две жизни одного вызова. `s3_save` вернёт 0 сейчас и 1 после пробуждения.
    if unsafe { s3_save(&raw mut CTX as *mut u64) } == 0 {
        // Записать `SLP_TYP | SLP_EN` — и машина гаснет прямо на этой инструкции.
        const SLP_EN: u32 = 1 << 13;
        super::io_write(pm1a, 2, ((slp_a as u32) & 0x7) << 10 | SLP_EN);
        if pm1b != 0 {
            super::io_write(pm1b, 2, ((slp_b as u32) & 0x7) << 10 | SLP_EN);
        }
        // Сюда попадаем, только если машина НЕ уснула: прошивка отказала молча. Подождём
        // немного (запись могла сработать не мгновенно) и честно скажем, что не вышло.
        for _ in 0..100_000 {
            core::hint::spin_loop();
        }
        return Err("прошивка не усыпила машину (запись в PM1 ничего не сделала)");
    }
    Ok(())
}
