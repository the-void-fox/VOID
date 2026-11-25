//! Веха 244 — **свой модсет панели Intel**: поднять изображение, когда видео-BIOS отказал.
//!
//! ## Зачем
//!
//! После сна S3 графика теряет питание. Обычно экран возвращает прошивка: трамплин
//! пробуждения просит видео-BIOS поставить режим заново (`s3tramp.s`). На ASUS X54C владельца
//! это не работает ни одним способом: ПЗУ режима не находит, а его POST (`C000:0003`) вешает
//! машину наглухо — проверено дважды, выключать приходилось удержанием кнопки
//! ([[laptop-base]]). Значит панель поднимаем сами.
//!
//! ## Как — и почему это не «драйвер Intel»
//!
//! Модсет целиком — это вычисление: делители PLL из частоты пикселей, тайминги из EDID,
//! M/N из ширины канала. Мы НИЧЕГО НЕ ВЫЧИСЛЯЕМ. При загрузке, когда панель заведомо
//! работает, снимается снимок полусотни регистров — и пробуждение записывает ровно их,
//! в правильном порядке и с правильными ожиданиями. От настоящего модсета остаётся только
//! ПОСЛЕДОВАТЕЛЬНОСТЬ, а она одна на всё поколение.
//!
//! Это сознательно узко: мы умеем вернуть ТОТ режим, который уже стоял, и не умеем поставить
//! другой. Для сна этого довольно, а для смены разрешения понадобится счёт — но тогда и
//! снимок будет не нужен.
//!
//! ## Откуда порядок
//!
//! Из драйвера Linux i915 (`Reference/i915/`, ядро 6.12) — `ilk_crtc_enable`,
//! `ilk_pch_enable`, `gen6_fdi_link_train`, `ibx_pch_dpll_enable`, `intel_enable_lvds`,
//! `pch_enable_backlight`. Числа задержек и биты — оттуда же; в комментариях к шагам названы
//! функции, чтобы сверка была дешёвой.
//!
//! ## Что это за железо
//!
//! Sandy Bridge (шестое поколение) + микросхема PCH Cougar Point. Картинка идёт так:
//! **конвейер процессора → линия FDI → транскодер PCH → порт LVDS → панель**. Линия FDI —
//! это настоящий последовательный канал между кристаллом процессора и микросхемой PCH, и он
//! ТРЕБУЕТ ОБУЧЕНИЯ: уровень сигнала подбирается перебором, пока приёмник не защёлкнет
//! синхронизацию. Записью сохранённого значения такое не чинится — отсюда весь `train_fdi`.
//!
//! Проверить это в QEMU нельзя ничем: эмулятора Sandy Bridge-дисплея не существует. Поэтому
//! каждый шаг говорит о себе в журнал, все ожидания ограничены по времени, и ни одно
//! невыполненное условие не останавливает цепочку молча.

#![cfg(target_arch = "x86_64")]

use crate::arch;
use crate::sync::SpinLock;

/// Смещения регистров от начала BAR0. Имена — как в `i915_reg.h`, чтобы сверять глазами.
mod reg {
    // ---- кристалл процессора: конвейер A и его транскодер ----
    pub const HTOTAL: usize = 0x60000;
    pub const HBLANK: usize = 0x60004;
    pub const HSYNC: usize = 0x60008;
    pub const VTOTAL: usize = 0x6000c;
    pub const VBLANK: usize = 0x60010;
    pub const VSYNC: usize = 0x60014;
    pub const PIPESRC: usize = 0x6001c;
    pub const VSYNCSHIFT: usize = 0x60028;
    pub const DATA_M1: usize = 0x60030;
    pub const DATA_N1: usize = 0x60034;
    pub const LINK_M1: usize = 0x60040;
    pub const LINK_N1: usize = 0x60044;
    pub const FDI_TX_CTL: usize = 0x60100;
    pub const PF_WIN_POS: usize = 0x68070;
    pub const PF_WIN_SZ: usize = 0x68074;
    pub const PF_CTL: usize = 0x68080;
    /// Где сейчас строка развёртки. Меняется — значит конвейер ДЕЙСТВИТЕЛЬНО работает; это
    /// надёжнее бита состояния, которому нужны часы с микросхемы PCH.
    pub const PIPEDSL: usize = 0x70000;
    pub const PIPECONF: usize = 0x70008;
    pub const DSPCNTR: usize = 0x70180;
    pub const DSPLINOFF: usize = 0x70184;
    pub const DSPSTRIDE: usize = 0x70188;
    pub const DSPSURF: usize = 0x7019c;
    pub const DSPTILEOFF: usize = 0x701a4;
    /// Счётчик кадров конвейера A: по его изменению видно, что развёртка живая и что
    /// выключение плоскости УЖЕ защёлкнулось.
    pub const FRMCOUNT: usize = 0x70040;
    /// Плоскость КУРСОРА и плоскость наложения (спрайт). Это тоже плоскости конвейера, и
    /// перед его выключением они обязаны молчать — ровно это проверяет `assert_planes_disabled`
    /// в i915, а у нас курсор аппаратный и работал.
    pub const CURCNTR: usize = 0x70080;
    pub const CURBASE: usize = 0x70084;
    pub const CURPOS: usize = 0x70088;
    pub const DVSCNTR: usize = 0x72180;
    pub const BLC_CPU_CTL2: usize = 0x48250;
    pub const BLC_CPU_CTL: usize = 0x48254;
    /// Водяные знаки: сколько конвейер успевает набрать в очередь, прежде чем начнёт показывать.
    /// Без них картинка рвётся полосами, хотя режим стоит верно.
    pub const WM_PIPE_A: usize = 0x45100;
    pub const WM_LP1: usize = 0x45108;
    pub const WM_LP2: usize = 0x4510c;
    pub const WM_LP3: usize = 0x45110;
    pub const WM1S_LP: usize = 0x45120;
    /// ОБЩЕЕ ХОЗЯЙСТВО ДИСПЛЕЯ: тактирование, арбитраж памяти и «chicken»-биты — обходы
    /// ошибок кремния. Прошивка выставляет их при включении, сон сбрасывает, а мы до сих пор
    /// не трогали вовсе. Без них картинка собирается с мусором: выборка пикселей не успевает
    /// за развёрткой. В i915 это `ilk_init_clock_gating` / `gen6_init_clock_gating` /
    /// `cpt_init_clock_gating`, и зовутся они в том числе ПРИ ПРОБУЖДЕНИИ.
    pub const CHICKEN1: usize = 0x42000;
    pub const CHICKEN2: usize = 0x42004;
    pub const DSPCLK_GATE_D: usize = 0x42020;
    pub const DISP_ARB_CTL: usize = 0x45000;
    pub const SOUTH_CHICKEN1: usize = 0xc2000;
    pub const SOUTH_CHICKEN2: usize = 0xc2004;
    pub const SOUTH_DSPCLK_GATE_D: usize = 0xc2020;
    pub const TRANS_CHICKEN1: usize = 0xf0060;

    // ---- микросхема PCH ----
    pub const DREF_CTL: usize = 0xc6200;
    pub const RAWCLK: usize = 0xc6204;
    pub const PCH_DPLL_A: usize = 0xc6014;
    pub const PCH_FPA0: usize = 0xc6040;
    pub const PCH_FPA1: usize = 0xc6044;
    pub const PCH_DPLL_SEL: usize = 0xc7000;
    pub const PP_STATUS: usize = 0xc7200;
    pub const PP_CONTROL: usize = 0xc7204;
    pub const PP_ON_DELAYS: usize = 0xc7208;
    pub const PP_OFF_DELAYS: usize = 0xc720c;
    pub const PP_DIVISOR: usize = 0xc7210;
    pub const BLC_PCH_CTL1: usize = 0xc8250;
    pub const BLC_PCH_CTL2: usize = 0xc8254;
    pub const T_HTOTAL: usize = 0xe0000;
    pub const T_HBLANK: usize = 0xe0004;
    pub const T_HSYNC: usize = 0xe0008;
    pub const T_VTOTAL: usize = 0xe000c;
    pub const T_VBLANK: usize = 0xe0010;
    pub const T_VSYNC: usize = 0xe0014;
    pub const T_VSYNCSHIFT: usize = 0xe0028;
    pub const T_DATA_M1: usize = 0xe0030;
    pub const T_DATA_N1: usize = 0xe0034;
    pub const T_LINK_M1: usize = 0xe0040;
    pub const T_LINK_N1: usize = 0xe0044;
    pub const PCH_LVDS: usize = 0xe1180;
    pub const TRANSCONF: usize = 0xf0008;
    pub const FDI_RX_CTL: usize = 0xf000c;
    pub const FDI_RX_MISC: usize = 0xf0010;
    pub const FDI_RX_IIR: usize = 0xf0014;
    /// Состояние прерываний дисплея: там же лежит отметка о срыве выборки конвейера.
    pub const DEIIR: usize = 0x44008;
    pub const FDI_RX_IMR: usize = 0xf0018;
    pub const FDI_RX_TUSIZE1: usize = 0xf0030;
    pub const TRANS_CHICKEN2: usize = 0xf0064;
}

/// Биты, которые мы ставим и ждём сами (остальные приходят из снимка как есть).
mod bit {
    /// Конвейер процессора: включение и подтверждение «работаю».
    pub const PIPE_ENABLE: u32 = 1 << 31;
    pub const PIPE_STATE: u32 = 1 << 30;
    /// Плоскость (то, что читает пиксели из памяти).
    pub const PLANE_ENABLE: u32 = 1 << 31;
    /// Плоскость наложения (спрайт) — у неё свой бит включения.
    pub const DVS_ENABLE: u32 = 1 << 31;
    /// Вид курсора: нули в младших битах и есть «курсора нет» (`MCURSOR_MODE_DISABLE`).
    pub const CURSOR_MODE_MASK: u32 = 0x27;
    /// Транскодер PCH.
    pub const TRANS_ENABLE: u32 = 1 << 31;
    pub const TRANS_STATE: u32 = 1 << 30;
    pub const TRANS_TIMING_OVERRIDE: u32 = 1 << 31;
    /// PLL микросхемы PCH.
    pub const DPLL_VCO_ENABLE: u32 = 1 << 31;
    /// Линия FDI, передатчик (процессор).
    pub const FDI_TX_ENABLE: u32 = 1 << 31;
    pub const FDI_TX_PLL_ENABLE: u32 = 1 << 14;
    pub const FDI_TRAIN_MASK: u32 = 3 << 28;
    pub const FDI_TRAIN_PATTERN_1: u32 = 0 << 28;
    pub const FDI_TRAIN_PATTERN_2: u32 = 1 << 28;
    pub const FDI_TRAIN_NONE: u32 = 3 << 28;
    pub const FDI_TX_ENHANCE_FRAME: u32 = 1 << 18;
    pub const FDI_VOL_EMP_MASK: u32 = 0x3f << 22;
    /// Линия FDI, приёмник (PCH Cougar Point — у него свои биты шаблона).
    pub const FDI_RX_ENABLE: u32 = 1 << 31;
    pub const FDI_RX_PLL_ENABLE: u32 = 1 << 13;
    pub const FDI_PCDCLK: u32 = 1 << 4;
    pub const FDI_RX_ENHANCE_FRAME: u32 = 1 << 6;
    pub const FDI_AUTO_TRAINING: u32 = 1 << 10;
    pub const FDI_TRAIN_MASK_CPT: u32 = 3 << 8;
    pub const FDI_TRAIN_PATTERN_1_CPT: u32 = 0 << 8;
    pub const FDI_TRAIN_PATTERN_2_CPT: u32 = 1 << 8;
    pub const FDI_TRAIN_NORMAL_CPT: u32 = 3 << 8;
    /// Что приёмник сообщает об обучении: защёлкнулись биты и символы.
    pub const FDI_RX_BIT_LOCK: u32 = 1 << 8;
    pub const FDI_RX_SYMBOL_LOCK: u32 = 1 << 9;
    /// Настройка приёмника на время обучения (`FDI_RX_TP1_TO_TP2_48 | FDI_RX_FDI_DELAY_90`).
    pub const FDI_RX_MISC_TRAIN: u32 = (2 << 20) | 0x90;
    /// Порт LVDS.
    pub const LVDS_PORT_EN: u32 = 1 << 31;
    /// Питание панели. Старшие шестнадцать бит `PP_CONTROL` — КЛЮЧ: без него запись не
    /// действует вовсе (`PANEL_UNLOCK_REGS`), и это первая ловушка этого регистра.
    pub const PP_UNLOCK: u32 = 0xabcd << 16;
    pub const PP_UNLOCK_MASK: u32 = 0xffff << 16;
    pub const PANEL_POWER_ON: u32 = 1 << 0;
    pub const PP_ON: u32 = 1 << 31;
    /// Подсветка.
    pub const BLM_PWM_ENABLE: u32 = 1 << 31;
    pub const BLM_PCH_PWM_ENABLE: u32 = 1 << 31;
}

/// Четыре уровня сигнала FDI для Sandy Bridge, вариант B. Обучение перебирает их по порядку,
/// пока приёмник не защёлкнется (`snb_b_fdi_train_param` в i915).
const TRAIN_PARAM: [u32; 4] = [0x0 << 22, 0x3a << 22, 0x39 << 22, 0x38 << 22];

/// Снимок рабочей настройки дисплея. Снимается на загрузке, применяется на пробуждении.
#[derive(Clone, Copy)]
struct Snap {
    htotal: u32,
    hblank: u32,
    hsync: u32,
    vtotal: u32,
    vblank: u32,
    vsync: u32,
    vsyncshift: u32,
    pipesrc: u32,
    data_m1: u32,
    data_n1: u32,
    link_m1: u32,
    link_n1: u32,
    fdi_tx: u32,
    pf_pos: u32,
    pf_sz: u32,
    pf_ctl: u32,
    pipeconf: u32,
    dspcntr: u32,
    dsplinoff: u32,
    dspstride: u32,
    dspsurf: u32,
    dsptileoff: u32,
    curcntr: u32,
    curbase: u32,
    curpos: u32,
    blc_cpu_ctl: u32,
    blc_cpu_ctl2: u32,
    wm_pipe_a: u32,
    wm_lp1: u32,
    wm_lp2: u32,
    wm_lp3: u32,
    wm1s_lp: u32,
    disp_chicken1: u32,
    disp_chicken2: u32,
    dspclk_gate: u32,
    arb_ctl: u32,
    s_chicken1: u32,
    s_chicken2: u32,
    s_dspclk_gate: u32,
    t_chicken1: u32,
    dref: u32,
    rawclk: u32,
    fpa0: u32,
    fpa1: u32,
    dpll_a: u32,
    dpll_sel: u32,
    t_htotal: u32,
    t_hblank: u32,
    t_hsync: u32,
    t_vtotal: u32,
    t_vblank: u32,
    t_vsync: u32,
    t_vsyncshift: u32,
    t_data_m1: u32,
    t_data_n1: u32,
    t_link_m1: u32,
    t_link_n1: u32,
    transconf: u32,
    chicken2: u32,
    fdi_rx: u32,
    fdi_rx_misc: u32,
    fdi_rx_tusize: u32,
    lvds: u32,
    pp_on: u32,
    pp_off: u32,
    pp_div: u32,
    pp_ctl: u32,
    blc_pch_ctl1: u32,
    blc_pch_ctl2: u32,
}

static SNAP: SpinLock<Option<Snap>> = SpinLock::new(None);

/// Говорить ли в журнал о КАЖДОМ обращении к регистру. Включается на время модсета.
///
/// Это не отладочный мусор, а единственный способ разбора на этой машине: экран во время
/// модсета тёмен, последовательного порта нет, а журнал переживает перезагрузку. Если машина
/// повиснет на обращении к регистру — в журнале останется ровно оно, последней строкой.
/// Первый же заход так и вышел: `disable()` не напечатал даже своей последней строки, и
/// место зависания пришлось угадывать.
static TRACE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Идёт ли прямо сейчас модсет. Пока идёт, экран не показывает ничего, и рисующему незачем
/// считать это поломкой: иначе композитор видит, что переключения страниц не защёлкиваются,
/// и уходит на один буфер — насовсем, до перезапуска.
static BUSY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn busy() -> bool {
    BUSY.load(core::sync::atomic::Ordering::Relaxed)
}

pub fn set_trace(on: bool) {
    TRACE.store(on, core::sync::atomic::Ordering::Relaxed);
}

fn tracing() -> bool {
    TRACE.load(core::sync::atomic::Ordering::Relaxed)
}

/// Чтение БЕЗ трассировки — для опросных циклов. С трассировкой они залили бы журнал
/// тысячами одинаковых строк и стёрли то, ради чего он ведётся.
fn rd_raw(off: usize) -> u32 {
    arch::hw_read32(crate::intel_fb::bar0() + off)
}

fn rd(off: usize) -> u32 {
    if tracing() {
        crate::println!("  [intel] ← {off:#07x}");
    }
    let v = arch::hw_read32(crate::intel_fb::bar0() + off);
    if tracing() {
        crate::println!("  [intel] ← {off:#07x} = {v:#010x}");
    }
    v
}

fn wr(off: usize, v: u32) {
    if tracing() {
        crate::println!("  [intel] → {off:#07x} = {v:#010x}");
    }
    arch::hw_write32(crate::intel_fb::bar0() + off, v);
}

/// Записать и тут же прочитать. Чтение здесь не проверка, а БАРЬЕР: оно выталкивает запись к
/// устройству, иначе следующая задержка отсчитывается от момента, когда запись ещё в пути
/// (`intel_de_posting_read` в i915 — ровно это).
fn wr_post(off: usize, v: u32) {
    wr(off, v);
    let _ = rd(off);
}

fn rmw(off: usize, clear: u32, set: u32) {
    let v = (rd(off) & !clear) | set;
    wr_post(off, v);
}

/// Подождать микросекунды занятым ожиданием: мы либо на загрузке, либо на пробуждении, где
/// ядро одно и спать не перед кем.
fn udelay(us: u64) {
    let until = arch::now_ticks() + crate::clock::ns_to_ticks(us * 1000);
    while arch::now_ticks() < until {
        core::hint::spin_loop();
    }
}

/// Дождаться, пока в регистре поднимутся биты `mask`. `false` — не дождались за `us`.
fn wait_set(off: usize, mask: u32, us: u64) -> bool {
    let until = arch::now_ticks() + crate::clock::ns_to_ticks(us * 1000);
    loop {
        if rd_raw(off) & mask == mask {
            return true;
        }
        if arch::now_ticks() >= until {
            return false;
        }
        core::hint::spin_loop();
    }
}

/// То же наоборот: дождаться, пока биты погаснут.
fn wait_clear(off: usize, mask: u32, us: u64) -> bool {
    let until = arch::now_ticks() + crate::clock::ns_to_ticks(us * 1000);
    loop {
        if rd_raw(off) & mask == 0 {
            return true;
        }
        if arch::now_ticks() >= until {
            return false;
        }
        core::hint::spin_loop();
    }
}

/// Дождаться смены кадра: выключение плоскости защёлкивается на обратном ходе луча, и пока
/// это не случилось, плоскость для железа ВСЁ ЕЩЁ ЧИТАЕТ память. Выключать под ней конвейер
/// нельзя — i915 про это говорит прямо: «иначе можем подвесить дисплей».
fn wait_frame() {
    let was = rd(reg::FRMCOUNT);
    let until = arch::now_ticks() + crate::clock::ns_to_ticks(50_000_000);
    while rd_raw(reg::FRMCOUNT) == was {
        if arch::now_ticks() >= until {
            crate::println!("  [intel] кадр не сменился за 50 мс — развёртка уже стоит");
            return;
        }
        core::hint::spin_loop();
    }
}

/// Снять снимок рабочей настройки. Зовётся на загрузке — ПОСЛЕ того, как ядро поставило
/// родной размер панели: восстанавливать надо то, в чём система живёт, а не то, что оставил
/// загрузчик.
pub fn snapshot() {
    if !crate::intel_fb::known() {
        return;
    }
    let s = Snap {
        htotal: rd(reg::HTOTAL),
        hblank: rd(reg::HBLANK),
        hsync: rd(reg::HSYNC),
        vtotal: rd(reg::VTOTAL),
        vblank: rd(reg::VBLANK),
        vsync: rd(reg::VSYNC),
        vsyncshift: rd(reg::VSYNCSHIFT),
        pipesrc: rd(reg::PIPESRC),
        data_m1: rd(reg::DATA_M1),
        data_n1: rd(reg::DATA_N1),
        link_m1: rd(reg::LINK_M1),
        link_n1: rd(reg::LINK_N1),
        fdi_tx: rd(reg::FDI_TX_CTL),
        pf_pos: rd(reg::PF_WIN_POS),
        pf_sz: rd(reg::PF_WIN_SZ),
        pf_ctl: rd(reg::PF_CTL),
        pipeconf: rd(reg::PIPECONF),
        dspcntr: rd(reg::DSPCNTR),
        dsplinoff: rd(reg::DSPLINOFF),
        dspstride: rd(reg::DSPSTRIDE),
        dspsurf: rd(reg::DSPSURF),
        dsptileoff: rd(reg::DSPTILEOFF),
        curcntr: rd(reg::CURCNTR),
        curbase: rd(reg::CURBASE),
        curpos: rd(reg::CURPOS),
        blc_cpu_ctl: rd(reg::BLC_CPU_CTL),
        blc_cpu_ctl2: rd(reg::BLC_CPU_CTL2),
        wm_pipe_a: rd(reg::WM_PIPE_A),
        wm_lp1: rd(reg::WM_LP1),
        wm_lp2: rd(reg::WM_LP2),
        wm_lp3: rd(reg::WM_LP3),
        wm1s_lp: rd(reg::WM1S_LP),
        disp_chicken1: rd(reg::CHICKEN1),
        disp_chicken2: rd(reg::CHICKEN2),
        dspclk_gate: rd(reg::DSPCLK_GATE_D),
        arb_ctl: rd(reg::DISP_ARB_CTL),
        s_chicken1: rd(reg::SOUTH_CHICKEN1),
        s_chicken2: rd(reg::SOUTH_CHICKEN2),
        s_dspclk_gate: rd(reg::SOUTH_DSPCLK_GATE_D),
        t_chicken1: rd(reg::TRANS_CHICKEN1),
        dref: rd(reg::DREF_CTL),
        rawclk: rd(reg::RAWCLK),
        fpa0: rd(reg::PCH_FPA0),
        fpa1: rd(reg::PCH_FPA1),
        dpll_a: rd(reg::PCH_DPLL_A),
        dpll_sel: rd(reg::PCH_DPLL_SEL),
        t_htotal: rd(reg::T_HTOTAL),
        t_hblank: rd(reg::T_HBLANK),
        t_hsync: rd(reg::T_HSYNC),
        t_vtotal: rd(reg::T_VTOTAL),
        t_vblank: rd(reg::T_VBLANK),
        t_vsync: rd(reg::T_VSYNC),
        t_vsyncshift: rd(reg::T_VSYNCSHIFT),
        t_data_m1: rd(reg::T_DATA_M1),
        t_data_n1: rd(reg::T_DATA_N1),
        t_link_m1: rd(reg::T_LINK_M1),
        t_link_n1: rd(reg::T_LINK_N1),
        transconf: rd(reg::TRANSCONF),
        chicken2: rd(reg::TRANS_CHICKEN2),
        fdi_rx: rd(reg::FDI_RX_CTL),
        fdi_rx_misc: rd(reg::FDI_RX_MISC),
        fdi_rx_tusize: rd(reg::FDI_RX_TUSIZE1),
        lvds: rd(reg::PCH_LVDS),
        pp_on: rd(reg::PP_ON_DELAYS),
        pp_off: rd(reg::PP_OFF_DELAYS),
        pp_div: rd(reg::PP_DIVISOR),
        pp_ctl: rd(reg::PP_CONTROL),
        blc_pch_ctl1: rd(reg::BLC_PCH_CTL1),
        blc_pch_ctl2: rd(reg::BLC_PCH_CTL2),
    };
    // Снимок с выключенным конвейером бесполезен и опасен: им потом «восстановят» темноту.
    // Лучше не иметь снимка вовсе — тогда пробуждение честно скажет, что поднимать нечем.
    if s.pipeconf & bit::PIPE_ENABLE == 0 {
        crate::println!("  [intel] снимок режима не снят: конвейер выключен");
        return;
    }
    *SNAP.lock() = Some(s);
    snapshot_wide();
    crate::println!(
        "  [intel] снимок режима снят (PLL {:#010x}, FDI {:#010x}/{:#010x}, LVDS {:#010x})",
        s.dpll_a,
        s.fdi_tx,
        s.fdi_rx,
        s.lvds,
    );
}


/// ШИРОКИЙ СНИМОК: все регистры дисплея, к которым ходит i915 на этом поколении. Нужен не
/// модсету, а РАЗБОРУ: сравнив его с тем, что в железе сейчас, видно разом всё, что сон
/// сбросил, а мы не вернули. Гадать по одному регистру за заход дорого — каждый заход это
/// перепрошивка.
///
/// Список курирован, а не выметен подряд: чтение неописанного регистра на этом железе может
/// не вернуться вовсе (мы это уже видели). Здесь только то, что трогает сам i915.
const WIDE: [usize; 75] = [
    // общее хозяйство и прерывания дисплея
    0x42000, 0x42004, 0x42020, 0x44004, 0x4400c, 0x45000, 0x45100, 0x45104, 0x45108, 0x4510c,
    0x45110, 0x45120, 0x46000, 0x46004, 0x46008, 0x46030,
    // подсветка со стороны процессора
    0x48250, 0x48254,
    // конвейер A: развёртка, M/N, линия FDI
    0x60000, 0x60004, 0x60008, 0x6000c, 0x60010, 0x60014, 0x6001c, 0x60028, 0x60030, 0x60034,
    0x60040, 0x60044, 0x60100,
    // масштабатор панели
    0x68070, 0x68074, 0x68080,
    // конвейер A: состояние, плоскости, курсор
    0x70008, 0x70080, 0x70084, 0x70088, 0x70180, 0x70184, 0x70188, 0x7019c, 0x701a4, 0x72180,
    // микросхема PCH: хозяйство, опорные часы, PLL
    0xc2000, 0xc2004, 0xc2020, 0xc6014, 0xc6018, 0xc6040, 0xc6044, 0xc6200, 0xc6204, 0xc7000,
    // питание панели и подсветка со стороны PCH
    0xc7200, 0xc7204, 0xc7208, 0xc720c, 0xc7210, 0xc8250, 0xc8254,
    // транскодер PCH, порт LVDS и приёмник линии
    0xe0000, 0xe0004, 0xe0008, 0xe000c, 0xe0010, 0xe0014, 0xe0028, 0xe1180, 0xf0008, 0xf000c,
    0xf0010, 0xf0030, 0xf0060, 0xf0064,
];

static WIDE_SAVED: SpinLock<Option<[u32; WIDE.len()]>> = SpinLock::new(None);

fn snapshot_wide() {
    let mut v = [0u32; WIDE.len()];
    for (i, off) in WIDE.iter().enumerate() {
        v[i] = rd_raw(*off);
    }
    *WIDE_SAVED.lock() = Some(v);
}

/// Сказать в журнал, чем железо СЕЙЧАС отличается от состояния при загрузке. Возвращает
/// число расхождений.
pub fn diff() -> usize {
    let Some(was) = *WIDE_SAVED.lock() else {
        crate::println!("  [intel] сравнивать не с чем: широкого снимка нет");
        return usize::MAX;
    };
    let mut n = 0;
    for (i, off) in WIDE.iter().enumerate() {
        let now = rd_raw(*off);
        // Эти меняются сами и всё время: положение луча, счётчик кадров, состояние
        // прерываний. Называть их расхождением значит топить настоящие в шуме.
        if matches!(*off, 0x44004 | 0x4400c | 0x70000 | 0x70040) {
            continue;
        }
        if now != was[i] {
            crate::println!("  [intel] расхождение {off:#07x}: было {:#010x}, стало {now:#010x}", was[i]);
            n += 1;
        }
    }
    if n == 0 {
        crate::println!("  [intel] расхождений с загрузкой нет ни одного");
    } else {
        crate::println!("  [intel] расхождений с загрузкой: {n}");
    }
    n
}

/// Есть ли чем поднимать панель.
pub fn have() -> bool {
    SNAP.lock().is_some()
}

/// Обучение линии FDI (Sandy Bridge, приёмник Cougar Point) — `gen6_fdi_link_train` в i915.
///
/// Два шаблона подряд: первым приёмник ловит биты, вторым — символы. На каждом перебираются
/// четыре уровня сигнала: какой подойдёт, зависит от разводки платы, и угадать его нельзя.
fn train_fdi(s: &Snap) -> bool {
    // Размер единицы передачи — до обучения, иначе приёмник не считает ошибки.
    wr(reg::FDI_RX_TUSIZE1, s.fdi_rx_tusize);
    // Открыть в маске прерываний биты защёлкивания: иначе приёмник не покажет их в IIR.
    rmw(reg::FDI_RX_IMR, bit::FDI_RX_BIT_LOCK | bit::FDI_RX_SYMBOL_LOCK, 0);
    udelay(150);

    // Передатчик: ширина канала из снимка, шаблон 1, начальный уровень сигнала.
    let tx_base = (s.fdi_tx & !bit::FDI_TRAIN_MASK & !bit::FDI_VOL_EMP_MASK) | TRAIN_PARAM[0];
    wr(reg::FDI_TX_CTL, tx_base | bit::FDI_TRAIN_PATTERN_1 | bit::FDI_TX_ENABLE);
    wr(reg::FDI_RX_MISC, bit::FDI_RX_MISC_TRAIN);
    // Приёмник: своё поле шаблона, автообучение выключено — ведём перебор сами.
    let rx_base = s.fdi_rx & !bit::FDI_TRAIN_MASK_CPT & !bit::FDI_AUTO_TRAINING;
    wr_post(reg::FDI_RX_CTL, rx_base | bit::FDI_TRAIN_PATTERN_1_CPT | bit::FDI_RX_ENABLE);
    udelay(150);

    if !train_step(bit::FDI_RX_BIT_LOCK) {
        crate::println!("  [intel] FDI: биты не защёлкнулись (шаблон 1)");
        return false;
    }

    // Шаблон 2 — символьная синхронизация.
    rmw(reg::FDI_TX_CTL, bit::FDI_TRAIN_MASK | bit::FDI_VOL_EMP_MASK,
        bit::FDI_TRAIN_PATTERN_2 | TRAIN_PARAM[0]);
    rmw(reg::FDI_RX_CTL, bit::FDI_TRAIN_MASK_CPT, bit::FDI_TRAIN_PATTERN_2_CPT);
    udelay(150);

    if !train_step(bit::FDI_RX_SYMBOL_LOCK) {
        crate::println!("  [intel] FDI: символы не защёлкнулись (шаблон 2)");
        return false;
    }
    true
}

/// Один шаблон обучения: перебрать четыре уровня сигнала, ожидая бит `lock` в `FDI_RX_IIR`.
fn train_step(lock: u32) -> bool {
    for param in TRAIN_PARAM {
        rmw(reg::FDI_TX_CTL, bit::FDI_VOL_EMP_MASK, param);
        udelay(500);
        for _ in 0..5 {
            let iir = rd_raw(reg::FDI_RX_IIR);
            if iir & lock != 0 {
                // Биты IIR сбрасываются ЗАПИСЬЮ ЕДИНИЦЫ — иначе следующий шаблон увидит
                // старую отметку и решит, что уже защёлкнулся.
                wr(reg::FDI_RX_IIR, iir | lock);
                return true;
            }
            udelay(50);
        }
    }
    false
}

/// Перевести линию FDI в рабочий режим после обучения (`intel_fdi_normal_train`).
fn fdi_normal(s: &Snap) {
    rmw(reg::FDI_TX_CTL, bit::FDI_TRAIN_MASK, bit::FDI_TRAIN_NONE | bit::FDI_TX_ENHANCE_FRAME);
    rmw(reg::FDI_RX_CTL, bit::FDI_TRAIN_MASK_CPT,
        bit::FDI_TRAIN_NORMAL_CPT | bit::FDI_RX_ENHANCE_FRAME);
    // Одно время холостого шаблона.
    udelay(1000);
    // И ВЕРНУТЬ НАСТРОЙКУ ВЫБОРКИ, какую ставила прошивка. На время обучения туда пишется
    // своё (`FDI_RX_TP1_TO_TP2_48 | FDI_RX_FDI_DELAY_90` — так в i915), и мы это значение
    // оставляли жить. А в нём ЗАДЕРЖКА ВЫБОРКИ приёмника: у прошивки 0x80, у нас 0x90. Чем
    // мимо середины глаза попадает защёлка, тем чаще искажается пиксель — и это ровно то, на
    // что похожи «цветные муравьи» по картинке.
    wr_post(reg::FDI_RX_MISC, s.fdi_rx_misc);
}

/// Поднять панель по снимку. `true` — конвейер и панель включились.
///
/// Порядок — `ilk_crtc_enable` + `ilk_pch_enable` из i915, с той разницей, что все значения
/// берутся из снимка, а не вычисляются.
pub fn enable() -> bool {
    BUSY.store(true, core::sync::atomic::Ordering::Relaxed);
    let r = enable_inner();
    BUSY.store(false, core::sync::atomic::Ordering::Relaxed);
    r
}

fn enable_inner() -> bool {
    let Some(s) = *SNAP.lock() else {
        crate::println!("  [intel] поднять панель нечем: снимка режима нет");
        return false;
    };

    // 0. ОБЩЕЕ ХОЗЯЙСТВО ДИСПЛЕЯ — раньше всего остального. Тактирование, арбитраж памяти и
    // обходы ошибок кремния: сон их сбрасывает, и без них конвейер запускается, панель
    // светится, а по картинке бегают цветные точки — выборка пикселей не успевает за
    // развёрткой. Нашлось сравнением двух трассировок: живой модсет и послесонный писали
    // ОДНО И ТО ЖЕ, значит дело было в том, чего мы не писали вовсе.
    wr(reg::CHICKEN1, s.disp_chicken1);
    wr(reg::CHICKEN2, s.disp_chicken2);
    wr(reg::DSPCLK_GATE_D, s.dspclk_gate);
    wr(reg::DISP_ARB_CTL, s.arb_ctl);
    wr(reg::SOUTH_CHICKEN1, s.s_chicken1);
    wr(reg::SOUTH_CHICKEN2, s.s_chicken2);
    wr(reg::SOUTH_DSPCLK_GATE_D, s.s_dspclk_gate);
    wr_post(reg::TRANS_CHICKEN1, s.t_chicken1);

    // 1. Опорные часы микросхемы PCH. Без них не заведётся ни PLL, ни линия; после сна они
    // сброшены вместе со всем остальным.
    wr_post(reg::DREF_CTL, s.dref);
    wr_post(reg::RAWCLK, s.rawclk);
    udelay(1000);

    // 2. Порт LVDS и задержки панели (`intel_pre_enable_lvds`). ДО включения всех PLL — в
    // i915 этот шаг прямо проверяет, что PLL линии и PLL микросхемы ещё ВЫКЛЮЧЕНЫ. Питание
    // панели здесь не включаем: оно последним, когда по линии уже идут пиксели.
    //
    // Ключ `PANEL_UNLOCK_REGS` отпирает регистры задержек — значит он первым, иначе запись
    // задержек уходит в пустоту молча.
    let pp_unlocked = (s.pp_ctl & !bit::PP_UNLOCK_MASK & !bit::PANEL_POWER_ON) | bit::PP_UNLOCK;
    wr_post(reg::PP_CONTROL, pp_unlocked);
    wr(reg::PP_ON_DELAYS, s.pp_on);
    wr(reg::PP_OFF_DELAYS, s.pp_off);
    wr(reg::PP_DIVISOR, s.pp_div);
    wr_post(reg::PCH_LVDS, s.lvds | bit::LVDS_PORT_EN);

    // 3. PLL линии FDI (`ilk_fdi_pll_enable`) — ДО включения конвейера, так в i915 и записано
    // отдельным шагом с объяснением «иначе нельзя». Порядок: приёмник, переход с сырых часов
    // на пиксельные, передатчик.
    //
    // СНИМОК ЗДЕСЬ ОПАСЕН, и это стоило зависания живой машины. В сохранённом значении
    // взведены биты РАБОТАЮЩЕЙ линии: переход на пиксельные часы (`FDI_PCDCLK`) и рабочий
    // шаблон. Записать их вместе с включением PLL — значит перевести блок регистров PCH на
    // часы, которых ещё нет: PLL включился миллисекунду назад и не захватился. Машина встала
    // намертво на ЧТЕНИИ того же регистра (журнал: `→ 0xf000c = 0x001a2350`, затем тишина).
    // Поэтому включаем ровно по одному биту за раз, как в i915, и с его же паузами.
    let rx_warm = (s.fdi_rx & !bit::FDI_RX_ENABLE & !bit::FDI_PCDCLK & !bit::FDI_TRAIN_MASK_CPT)
        | bit::FDI_TRAIN_PATTERN_1_CPT
        | bit::FDI_RX_PLL_ENABLE;
    wr_post(reg::FDI_RX_CTL, rx_warm);
    udelay(200);
    // Только теперь — с сырых часов на пиксельные.
    rmw(reg::FDI_RX_CTL, 0, bit::FDI_PCDCLK);
    udelay(200);
    let tx_warm = (s.fdi_tx & !bit::FDI_TX_ENABLE & !bit::FDI_TRAIN_MASK)
        | bit::FDI_TRAIN_PATTERN_1
        | bit::FDI_TX_PLL_ENABLE;
    wr_post(reg::FDI_TX_CTL, tx_warm);
    udelay(100);

    // 3. Тайминги конвейера процессора, соотношение M/N для линии, масштабатор панели и
    // водяные знаки (`ilk_configure_cpu_transcoder`, `ilk_pfit_enable`).
    wr(reg::HTOTAL, s.htotal);
    wr(reg::HBLANK, s.hblank);
    wr(reg::HSYNC, s.hsync);
    wr(reg::VTOTAL, s.vtotal);
    wr(reg::VBLANK, s.vblank);
    wr(reg::VSYNC, s.vsync);
    wr(reg::VSYNCSHIFT, s.vsyncshift);
    wr(reg::PIPESRC, s.pipesrc);
    wr(reg::DATA_M1, s.data_m1);
    wr(reg::DATA_N1, s.data_n1);
    wr(reg::LINK_M1, s.link_m1);
    wr(reg::LINK_N1, s.link_n1);
    wr(reg::PIPECONF, s.pipeconf & !bit::PIPE_ENABLE & !bit::PIPE_STATE);
    wr(reg::PF_WIN_POS, s.pf_pos);
    wr(reg::PF_WIN_SZ, s.pf_sz);
    wr_post(reg::PF_CTL, s.pf_ctl);
    wr(reg::WM_PIPE_A, s.wm_pipe_a);
    wr(reg::WM_LP1, s.wm_lp1);
    wr(reg::WM_LP2, s.wm_lp2);
    wr(reg::WM_LP3, s.wm_lp3);
    wr(reg::WM1S_LP, s.wm1s_lp);

    // 4. Включить конвейер процессора. Плоскость при этом ОБЯЗАНА быть выключена — иначе она
    // продолжит качать пиксели в ещё не готовую цепь, и это подвешивает дисплей
    // (`assert_planes_disabled` в i915 — проверка ровно на это).
    wr(reg::DSPCNTR, s.dspcntr & !bit::PLANE_ENABLE);
    wr_post(reg::PIPECONF, (s.pipeconf & !bit::PIPE_STATE) | bit::PIPE_ENABLE);
    // Бита «работаю» здесь ЖДАТЬ НЕЛЬЗЯ, и это стоило целого захода: ждал сто миллисекунд,
    // не дожидался и бросал всю цепочку — то есть сам гарантировал чёрный экран. В i915
    // после включения конвейера этот бит не ждут ВОВСЕ (ждут его только при выключении).
    // Смотрим коротко и только ради журнала; правду скажет движение строки развёртки в конце.
    if !wait_set(reg::PIPECONF, bit::PIPE_STATE, 20_000) {
        crate::println!("  [intel] конвейер включён, но «работаю» пока не подтвердил — идём дальше");
    }

    // 5. Обучить линию FDI — единственное место, где снимком не обойтись.
    if !train_fdi(&s) {
        return false;
    }

    // 6. PLL микросхемы PCH (`ibx_pch_dpll_enable`). Именно здесь, после обучения: так в
    // i915, и там же сказано, что раньше тоже можно, но позже транскодера — нельзя.
    // Делители, затем сам PLL — и ДВАЖДЫ с паузами: множитель пикселей принимается только
    // на уже работающих часах.
    wr_post(reg::PCH_DPLL_SEL, s.dpll_sel);
    wr(reg::PCH_FPA0, s.fpa0);
    wr(reg::PCH_FPA1, s.fpa1);
    wr_post(reg::PCH_DPLL_A, s.dpll_a | bit::DPLL_VCO_ENABLE);
    udelay(150);
    wr_post(reg::PCH_DPLL_A, s.dpll_a | bit::DPLL_VCO_ENABLE);
    udelay(200);

    // 7. Тайминги транскодера PCH и его M/N — те же числа, что у конвейера.
    wr(reg::T_HTOTAL, s.t_htotal);
    wr(reg::T_HBLANK, s.t_hblank);
    wr(reg::T_HSYNC, s.t_hsync);
    wr(reg::T_VTOTAL, s.t_vtotal);
    wr(reg::T_VBLANK, s.t_vblank);
    wr(reg::T_VSYNC, s.t_vsync);
    wr(reg::T_VSYNCSHIFT, s.t_vsyncshift);
    wr(reg::T_DATA_M1, s.t_data_m1);
    wr(reg::T_DATA_N1, s.t_data_n1);
    wr(reg::T_LINK_M1, s.t_link_m1);
    wr(reg::T_LINK_N1, s.t_link_n1);

    // 8. Линию — в рабочий режим, и только теперь включать транскодер PCH. Перед ним
    // обязательная оговорка Cougar Point: бит перекрытия таймингов.
    fdi_normal(&s);
    wr_post(reg::TRANS_CHICKEN2, s.chicken2 | bit::TRANS_TIMING_OVERRIDE);
    wr_post(reg::TRANSCONF, (s.transconf & !bit::TRANS_STATE) | bit::TRANS_ENABLE);
    if !wait_set(reg::TRANSCONF, bit::TRANS_STATE, 100_000) {
        // Не отказ: дальше мы включаем питание панели и подсветку, и даже половина картинки
        // лучше гарантированной темноты. Правду скажет итоговая строка.
        crate::println!(
            "  [intel] транскодер PCH не подтвердил включение (TRANSCONF {:#010x})",
            rd(reg::TRANSCONF),
        );
    }

    // 9. Питание панели (`intel_enable_lvds`): порт включён ещё до PLL, а питание — только
    // теперь, когда по линии идут пиксели. Задержки включения панель диктует сама.
    wr_post(reg::PP_CONTROL, pp_unlocked | bit::PANEL_POWER_ON);
    if !wait_set(reg::PP_STATUS, bit::PP_ON, 5_000_000) {
        // Не отказ: конвейер уже работает, а питание панели могло просто задержаться.
        crate::println!("  [intel] панель не отчиталась о питании (PP_STATUS {:#010x})", rd(reg::PP_STATUS));
    }

    // 10. Подсветка (`pch_enable_backlight`): сперва выключатель снять, потом период, уровень
    // и только потом включение — иначе уровень «не прилипает».
    wr_post(reg::BLC_CPU_CTL2, s.blc_cpu_ctl2 & !bit::BLM_PWM_ENABLE);
    wr_post(reg::BLC_CPU_CTL2, s.blc_cpu_ctl2 | bit::BLM_PWM_ENABLE);
    wr(reg::BLC_CPU_CTL, s.blc_cpu_ctl);
    wr(reg::BLC_PCH_CTL2, s.blc_pch_ctl2);
    wr_post(reg::BLC_PCH_CTL1, s.blc_pch_ctl1 & !bit::BLM_PCH_PWM_ENABLE);
    wr_post(reg::BLC_PCH_CTL1, s.blc_pch_ctl1 | bit::BLM_PCH_PWM_ENABLE);

    // 11. И последней — плоскость: откуда читать пиксели и сколько их в строке. Запись
    // поверхности защёлкивает всю плоскость целиком.
    wr(reg::DSPSTRIDE, s.dspstride);
    wr(reg::DSPLINOFF, s.dsplinoff);
    wr(reg::DSPTILEOFF, s.dsptileoff);
    wr(reg::DSPCNTR, s.dspcntr | bit::PLANE_ENABLE);
    wr_post(reg::DSPSURF, s.dspsurf);
    // Курсор — отдельной плоскостью, как и гасили.
    wr(reg::CURPOS, s.curpos);
    wr(reg::CURCNTR, s.curcntr);
    wr_post(reg::CURBASE, s.curbase);

    // Главная проверка — не биты состояния, а ДВИЖЕНИЕ: бежит ли строка развёртки. Если
    // бежит, конвейер живой, что бы ни говорили остальные регистры.
    let dsl1 = rd_raw(reg::PIPEDSL);
    udelay(20_000);
    let dsl2 = rd_raw(reg::PIPEDSL);
    let running = dsl1 != dsl2;
    // Что приёмник линии и дисплейные прерывания думают о качестве картинки. Биты «липкие»:
    // раз взведясь, держатся — поэтому перед замером их гасят, а читают через кадр.
    wr(reg::FDI_RX_IIR, 0xffff);
    wr(reg::DEIIR, 0xffff_ffff);
    udelay(50_000);
    let iir = rd_raw(reg::FDI_RX_IIR);
    let deiir = rd_raw(reg::DEIIR);
    if iir & 0x77 != 0 || deiir != 0 {
        crate::println!(
            "  [intel] линия жалуется: FDI_RX_IIR {iir:#010x} ({}{}{}{}{}), прерывания дисплея {deiir:#010x}",
            if iir & (1 << 6) != 0 { "ошибка кода FS " } else { "" },
            if iir & (1 << 5) != 0 { "ошибка кода FE " } else { "" },
            if iir & (1 << 4) != 0 { "частых ошибок символов " } else { "" },
            if iir & (1 << 2) != 0 { "переполнение очереди пикселей " } else { "" },
            if iir & 3 != 0 { "переполнение символов" } else { "" },
        );
    }
    crate::println!(
        "  [intel] модсет окончен: развёртка {}, PIPECONF {:#010x}, TRANSCONF {:#010x}, \
         PP_STATUS {:#010x}, ШИМ {:#010x}/{:#010x}, строка {dsl1}→{dsl2}",
        if running { "БЕЖИТ" } else { "стоит" },
        rd(reg::PIPECONF),
        rd(reg::TRANSCONF),
        rd(reg::PP_STATUS),
        rd(reg::BLC_CPU_CTL2),
        rd(reg::BLC_PCH_CTL1),
    );
    running
}

/// Погасить панель и всю цепь — обратным порядком. Нужна для ПРОВЕРКИ модсета на живой машине
/// без сна: `intel off` гасит, `intel on` поднимает, и видно сразу, работает ли наша цепочка.
pub fn disable() {
    BUSY.store(true, core::sync::atomic::Ordering::Relaxed);
    disable_inner();
    BUSY.store(false, core::sync::atomic::Ordering::Relaxed);
}

fn disable_inner() {
    if !crate::intel_fb::known() {
        return;
    }
    // Порядок — `ilk_crtc_disable`, и он не зеркальный: конвейер процессора гасится РАНЬШЕ
    // линии FDI, а транскодер PCH — ПОСЛЕ неё. Перепутать значит оставить железо в
    // состоянии, из которого следующее включение уже не выйдет.

    // 1. Кодировщик: подсветка, питание панели, порт.
    rmw(reg::BLC_PCH_CTL1, bit::BLM_PCH_PWM_ENABLE, 0);
    rmw(reg::BLC_CPU_CTL2, bit::BLM_PWM_ENABLE, 0);
    let pp = (rd(reg::PP_CONTROL) & !bit::PP_UNLOCK_MASK & !bit::PANEL_POWER_ON) | bit::PP_UNLOCK;
    wr_post(reg::PP_CONTROL, pp);
    if !wait_clear(reg::PP_STATUS, bit::PP_ON, 1_000_000) {
        crate::println!("  [intel] панель не погасла вовремя (PP_STATUS {:#010x})", rd(reg::PP_STATUS));
    }
    rmw(reg::PCH_LVDS, bit::LVDS_PORT_EN, 0);

    // 2. ВСЕ плоскости — и только потом конвейер. Плоскостей у конвейера три: кадр, курсор и
    // наложение; i915 требует молчания от каждой, а первый заход на живой машине повис
    // именно здесь — курсор у нас аппаратный и продолжал читать память.
    rmw(reg::DSPCNTR, bit::PLANE_ENABLE, 0);
    wr(reg::DSPSURF, rd(reg::DSPSURF));
    rmw(reg::CURCNTR, bit::CURSOR_MODE_MASK, 0);
    wr(reg::CURBASE, rd(reg::CURBASE));
    if rd(reg::DVSCNTR) & bit::DVS_ENABLE != 0 {
        rmw(reg::DVSCNTR, bit::DVS_ENABLE, 0);
    }
    // И дождаться кадра: до обратного хода луча выключение плоскостей не действует.
    wait_frame();
    rmw(reg::PIPECONF, bit::PIPE_ENABLE, 0);
    if !wait_clear(reg::PIPECONF, bit::PIPE_STATE, 100_000) {
        crate::println!("  [intel] конвейер не выключился (PIPECONF {:#010x})", rd(reg::PIPECONF));
    }
    // 3. Масштабатор.
    wr(reg::PF_CTL, 0);
    wr(reg::PF_WIN_POS, 0);
    wr_post(reg::PF_WIN_SZ, 0);

    // 4. Линия FDI: снять передачу и приём, вернуть шаблон 1 (`ilk_fdi_disable`).
    rmw(reg::FDI_TX_CTL, bit::FDI_TX_ENABLE, 0);
    rmw(reg::FDI_RX_CTL, bit::FDI_RX_ENABLE, 0);
    udelay(100);
    rmw(reg::FDI_TX_CTL, bit::FDI_TRAIN_MASK, bit::FDI_TRAIN_PATTERN_1);
    rmw(reg::FDI_RX_CTL, bit::FDI_TRAIN_MASK_CPT, bit::FDI_TRAIN_PATTERN_1_CPT);
    udelay(100);

    // 5. Транскодер PCH — после линии, и бит перекрытия таймингов снять обратно.
    rmw(reg::TRANSCONF, bit::TRANS_ENABLE, 0);
    if !wait_clear(reg::TRANSCONF, bit::TRANS_STATE, 100_000) {
        crate::println!("  [intel] транскодер PCH не выключился (TRANSCONF {:#010x})", rd(reg::TRANSCONF));
    }
    rmw(reg::TRANS_CHICKEN2, bit::TRANS_TIMING_OVERRIDE, 0);

    // 6. PLL линии и PLL микросхемы PCH (`ilk_fdi_pll_disable`, `ibx_pch_dpll_disable`).
    rmw(reg::FDI_RX_CTL, bit::FDI_PCDCLK, 0);
    rmw(reg::FDI_TX_CTL, bit::FDI_TX_PLL_ENABLE, 0);
    udelay(100);
    rmw(reg::FDI_RX_CTL, bit::FDI_RX_PLL_ENABLE, 0);
    udelay(100);
    rmw(reg::PCH_DPLL_A, bit::DPLL_VCO_ENABLE, 0);
    crate::println!("  [intel] цепь панели погашена целиком (проба модсета)");
}
