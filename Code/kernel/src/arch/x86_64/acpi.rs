//! Веха 101 — **выключение машины по ACPI**. Раньше `power_off` на x86 честно останавливал
//! процессор (`cli; hlt`), и «выключить» приходилось кнопкой: команда была, эффекта не было.
//!
//! Путь стандартный и целиком читающий: `RSD PTR ` в области BIOS → RSDT/XSDT → таблица `FACP`
//! (FADT) → в ней порты PM1a/PM1b и указатель на DSDT → в DSDT ищется объект `_S5_`, откуда
//! берутся значения `SLP_TYP` для «мягкого выключения». Затем в порт PM1 пишется
//! `SLP_TYP | SLP_EN` — и питание снимается.
//!
//! Разбирать AML целиком мы, конечно, не будем: `_S5_` ищется поиском сигнатуры и читается
//! коротким разбором пакета — ровно так это делают маленькие ядра. Если не нашлось или ACPI
//! нет вовсе (PVH-загрузка), остаются порты гипервизоров и, последним, честная остановка.
//!
//! **Веха 197 — тем же путём идёт ПЕРЕЗАГРУЗКА** ([`try_reboot`]): в FADT есть регистр сброса
//! (`RESET_REG` + `RESET_VALUE`), и он ФИКСИРОВАННЫЙ — читается прямо из таблицы, без AML. Это
//! не роскошь: каждая пересборка системы кончается словами «перезагрузись», а до сих пор
//! перезагрузка означала выключить машину и включить её руками.
//!
//! На riscv ничего этого не нужно: там выключение и перезагрузка — вызовы SBI.

use super::{outb, phys_to_virt};

/// Прочитать `len` байт физической памяти через direct-map (ниже 4 ГиБ он сплошной — см.
/// `paging.rs`, там дыры карты закрыты намеренно, и ACPI-таблицы попадают в него как есть).
unsafe fn phys(pa: usize, len: usize) -> &'static [u8] {
    core::slice::from_raw_parts(phys_to_virt(pa) as *const u8, len)
}

fn rd32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn rd64(b: &[u8], at: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(v)
}

/// Сумма байт == 0 — так ACPI проверяет свои структуры.
fn checksum_ok(b: &[u8]) -> bool {
    b.iter().fold(0u8, |a, &x| a.wrapping_add(x)) == 0
}

/// Найти `RSD PTR ` в области BIOS: сперва EBDA (её сегмент лежит словом по 0x40E), затем
/// 0xE0000..0x100000. Так делает любой загрузчик, и это не зависит от того, чем нас грузили.
fn find_rsdp() -> Option<&'static [u8]> {
    let mut ranges = [(0usize, 0usize); 2];
    let ebda = unsafe { (rd16(phys(0x400, 0x100), 0x0E) as usize) << 4 };
    ranges[0] = if (0x400..0xA_0000).contains(&ebda) { (ebda, ebda + 1024) } else { (0, 0) };
    ranges[1] = (0xE_0000, 0x10_0000);
    for (start, end) in ranges {
        if start == 0 {
            continue;
        }
        let mut pa = start;
        while pa + 20 <= end {
            let b = unsafe { phys(pa, 36) };
            if &b[..8] == b"RSD PTR " && checksum_ok(&b[..20]) {
                return Some(b);
            }
            pa += 16; // сигнатура выровнена на 16 байт (так требует спецификация)
        }
    }
    None
}

fn rd16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

/// Найти таблицу с сигнатурой `sig` через RSDT (ACPI 1.0) или XSDT (2.0+).
fn find_table(rsdp: &[u8], sig: &[u8; 4]) -> Option<&'static [u8]> {
    let revision = rsdp[15];
    let (root_pa, wide) = if revision >= 2 && rd64(rsdp, 24) != 0 {
        (rd64(rsdp, 24) as usize, true)
    } else {
        (rd32(rsdp, 16) as usize, false)
    };
    if root_pa == 0 {
        return None;
    }
    let head = unsafe { phys(root_pa, 36) };
    let len = rd32(head, 4) as usize;
    if len < 36 || len > 0x10000 {
        return None;
    }
    let root = unsafe { phys(root_pa, len) };
    let step = if wide { 8 } else { 4 };
    let mut at = 36;
    while at + step <= len {
        let pa = if wide { rd64(root, at) as usize } else { rd32(root, at) as usize };
        at += step;
        if pa == 0 {
            continue;
        }
        let t = unsafe { phys(pa, 36) };
        if &t[..4] == sig {
            let tlen = (rd32(t, 4) as usize).clamp(36, 0x10_0000);
            return Some(unsafe { phys(pa, tlen) });
        }
    }
    None
}

/// Веха 170 — **MADT** (сигнатура `APIC`): таблица, в которой прошивка перечисляет процессоры.
///
/// Отдаётся как есть, разбирает её [`super::smp`]: здесь живёт умение НАЙТИ таблицу ACPI, а что
/// в ней написано — дело того, кому она нужна. `None` — ACPI нет вовсе (так бывает при
/// PVH-загрузке) либо таблицы с таким именем в ней нет.
pub fn madt() -> Option<&'static [u8]> {
    find_table(find_rsdp()?, b"APIC")
}

/// Значение целого из AML: `ZeroOp`/`OneOp`/`BytePrefix` — больше в `_S5_` не встречается.
fn aml_int(aml: &[u8], p: &mut usize) -> u8 {
    let v = aml[*p];
    match v {
        0x0A => {
            *p += 2;
            aml[*p - 1]
        }
        _ => {
            *p += 1;
            if v <= 1 { v } else { 0 }
        }
    }
}

/// `SLP_TYPa`/`SLP_TYPb` из объекта `_S5_` в DSDT.
fn find_s5(dsdt: &[u8]) -> Option<(u8, u8)> {
    let at = dsdt.windows(4).position(|w| w == b"_S5_")?;
    let mut p = at + 4;
    if p >= dsdt.len() || dsdt[p] != 0x12 {
        return None; // ожидали PackageOp — разбирать что-то сложнее незачем
    }
    p += 1;
    p += ((dsdt[p] & 0xC0) >> 6) as usize + 1; // PkgLength
    p += 1; // NumElements
    if p + 2 >= dsdt.len() {
        return None;
    }
    let a = aml_int(dsdt, &mut p);
    let b = aml_int(dsdt, &mut p);
    Some((a, b))
}

unsafe fn outw(port: u16, v: u16) {
    core::arch::asm!("out dx, ax", in("dx") port, in("ax") v, options(nomem, nostack));
}

unsafe fn inw(port: u16) -> u16 {
    let v: u16;
    core::arch::asm!("in ax, dx", in("dx") port, out("ax") v, options(nomem, nostack));
    v
}

/// Выключить машину по ACPI. Возвращается только если не вышло.
pub fn try_power_off() {
    let Some(rsdp) = find_rsdp() else { return };
    let Some(fadt) = find_table(rsdp, b"FACP") else { return };
    if fadt.len() < 76 {
        return;
    }
    let smi_cmd = rd32(fadt, 48);
    let acpi_enable = fadt[52];
    let pm1a = rd32(fadt, 64) as u16;
    let pm1b = rd32(fadt, 68) as u16;
    // DSDT: 64-битный указатель у ACPI 2.0+, иначе 32-битный.
    let dsdt_pa = if fadt.len() >= 148 && rd64(fadt, 140) != 0 {
        rd64(fadt, 140) as usize
    } else {
        rd32(fadt, 40) as usize
    };
    if pm1a == 0 || dsdt_pa == 0 {
        return;
    }
    let head = unsafe { phys(dsdt_pa, 36) };
    if &head[..4] != b"DSDT" {
        return;
    }
    let dlen = (rd32(head, 4) as usize).clamp(36, 0x20_0000);
    let dsdt = unsafe { phys(dsdt_pa, dlen) };
    let Some((slp_a, slp_b)) = find_s5(dsdt) else { return };

    unsafe {
        // Перевести чипсет в ACPI-режим, если он ещё в legacy (SCI_EN == 0). На реальной машине
        // без этого запись в PM1 ничего не даст; в виртуалке чаще всего уже включено.
        if smi_cmd != 0 && acpi_enable != 0 && inw(pm1a) & 1 == 0 {
            outb(smi_cmd as u16, acpi_enable);
            for _ in 0..100_000 {
                if inw(pm1a) & 1 != 0 {
                    break;
                }
                core::hint::spin_loop();
            }
        }
        const SLP_EN: u16 = 1 << 13;
        outw(pm1a, ((slp_a as u16) & 0x7) << 10 | SLP_EN);
        if pm1b != 0 {
            outw(pm1b, ((slp_b as u16) & 0x7) << 10 | SLP_EN);
        }
    }
}

/// Веха 197 — КНОПКА ПИТАНИЯ, фиксированным регистром.
///
/// На ноутбуке это единственная кнопка, которой человек ждёт послушания: нажал — машина
/// выключилась сама, а не через пять секунд удержания (то есть жёстким снятием питания, мимо
/// синка store). Разбирать AML для этого не нужно — кнопка описана В САМОЙ FADT:
///
/// * `SCI_INT` (46) — номер прерывания, которым чипсет сообщает о событии ACPI;
/// * `PM1a_EVT_BLK`/`PM1b_EVT_BLK` (56/60) — блок событий: первая половина регистр состояния,
///   вторая — разрешений (длина блока в `PM1_EVT_LEN`, 88);
/// * бит 8 в обоих — `PWRBTN`: в состоянии «нажали», в разрешениях «сообщать об этом».
///
/// Возвращает `(gsi, active_low, level)` — всё, что нужно вызывающему, чтобы завести линию.
///
/// Полярность и тип линии НЕ УГАДЫВАЕМ: в MADT есть переопределения источников (тип 2), и на
/// q35 линия SCI объявлена именно там. Первая версия ставила «level, active-low» по букве
/// спецификации ACPI — и прерывание не приходило вовсе: QEMU объявляет её active-high, а
/// IOAPIC честно ждал противоположного уровня.
pub fn enable_power_button() -> Option<(u32, bool, bool)> {
    let rsdp = find_rsdp()?;
    let fadt = find_table(rsdp, b"FACP")?;
    if fadt.len() < 89 {
        return None;
    }
    let sci = rd16(fadt, 46) as u32;
    let pm1a = rd32(fadt, 56) as u16;
    let pm1b = rd32(fadt, 60) as u16;
    let len = fadt[88] as u16;
    if pm1a == 0 || len < 4 {
        return None; // блока событий нет — кнопку не поймать
    }
    let half = len / 2;
    unsafe {
        // ВКЛЮЧИТЬ ACPI-режим, если чипсет ещё в legacy. Это оказалось главным: пока `SCI_EN` в
        // регистре управления PM1 равен нулю, кнопка питания даёт не SCI, а SMI — то есть
        // обрабатывается прошивкой мимо нас, и ждать прерывания бессмысленно. Ровно поэтому
        // первая версия «включала кнопку» и молчала на нажатие.
        let smi_cmd = rd32(fadt, 48);
        let acpi_enable = fadt[52];
        let pm1a_cnt = rd32(fadt, 64) as u16;
        if smi_cmd != 0 && acpi_enable != 0 && pm1a_cnt != 0 && inw(pm1a_cnt) & 1 == 0 {
            outb(smi_cmd as u16, acpi_enable);
            for _ in 0..1_000_000 {
                if inw(pm1a_cnt) & 1 != 0 {
                    break;
                }
                core::hint::spin_loop();
            }
        }
        // Снять уже висящее состояние (RW1C) и разрешить только кнопку питания: остальные
        // события ACPI (таймер, GPE, шина) мы обслуживать не умеем, и просить о них значило бы
        // получать прерывания, на которые нечего ответить.
        outw(pm1a, PWRBTN_STS);
        outw(pm1a + half, PWRBTN_EN);
        if pm1b != 0 {
            outw(pm1b, PWRBTN_STS);
            outw(pm1b + half, PWRBTN_EN);
        }
    }
    PM1A_STS.store(pm1a as usize, core::sync::atomic::Ordering::Relaxed);
    PM1B_STS.store(pm1b as usize, core::sync::atomic::Ordering::Relaxed);
    Some(interrupt_override(sci))
}

/// Как на самом деле заведена линия `irq`: переопределение из MADT (тип 2) либо умолчание шины
/// ISA — фронт, активный высокий уровень.
///
/// Флаги переопределения: биты 0..1 — полярность (1 высокая, 3 низкая), биты 2..3 — тип
/// (1 фронт, 3 уровень); ноль в паре значит «как принято на этой шине».
fn interrupt_override(irq: u32) -> (u32, bool, bool) {
    let Some(madt) = madt() else { return (irq, false, false) };
    let mut p = 44; // за общей шапкой таблицы и полями MADT
    while p + 2 <= madt.len() {
        let (kind, len) = (madt[p], madt[p + 1] as usize);
        if len < 2 || p + len > madt.len() {
            break;
        }
        if kind == 2 && len >= 10 && madt[p + 3] as u32 == irq {
            let gsi = rd32(madt, p + 4);
            let flags = rd16(madt, p + 8);
            let active_low = flags & 0b11 == 3;
            let level = (flags >> 2) & 0b11 == 3;
            return (gsi, active_low, level);
        }
        p += len;
    }
    (irq, false, false)
}

/// Бит кнопки питания в регистре состояния PM1 (он же в регистре разрешений).
const PWRBTN_STS: u16 = 1 << 8;
const PWRBTN_EN: u16 = 1 << 8;

/// Порты состояния PM1a/PM1b — их читает обработчик SCI. Хранятся числами, потому что обработчик
/// прерывания не может позволить себе искать таблицы заново.
static PM1A_STS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static PM1B_STS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Обработать событие SCI. `true` — это была КНОПКА ПИТАНИЯ (состояние снято).
///
/// Прочие события ACPI нам приходить не должны (мы их не разрешали), но линия SCI разделяемая:
/// прочитать состояние и уйти, ничего не сняв, — правильное поведение для чужого события.
pub fn sci_power_button() -> bool {
    let mut hit = false;
    for port in [PM1A_STS.load(core::sync::atomic::Ordering::Relaxed),
                 PM1B_STS.load(core::sync::atomic::Ordering::Relaxed)] {
        if port == 0 {
            continue;
        }
        unsafe {
            let sts = inw(port as u16);
            if sts & PWRBTN_STS != 0 {
                outw(port as u16, PWRBTN_STS); // RW1C: снять состояние
                hit = true;
            }
        }
    }
    hit
}

/// Перезагрузить машину. Возвращается только если не вышло НИ ОДНИМ из способов.
///
/// Три пути по убыванию честности, и каждый следующий — потому что предыдущего может не быть:
///
/// 1. **`RESET_REG` из FADT** (ACPI 2.0+): описан обобщённым адресом (GAS) — порт ввода-вывода,
///    память или конфиг PCI, — и значением, которое туда пишут. Единственный способ, который
///    производитель объявил САМ;
/// 2. **импульс контроллера клавиатуры** (0xFE в порт 0x64): линия сброса процессора висит на
///    i8042 со времён IBM PC. На машине новее ~2015 контроллера может не быть вовсе — именно
///    поэтому он второй, а не первый;
/// 3. **тройная ошибка**: загрузить пустую таблицу прерываний и вызвать прерывание. Процессор,
///    не сумев обработать ошибку обработки ошибки, сбрасывается. Грубо, зато работает везде.
pub fn try_reboot() {
    if let Some(rsdp) = find_rsdp() {
        if let Some(fadt) = find_table(rsdp, b"FACP") {
            // GAS: [0] пространство адресов, [1] ширина в битах, [2] смещение бита,
            // [3] размер доступа, [4..12] адрес. Значение — байтом на 128.
            // Бит 10 флагов (offset 112) — «регистр сброса поддержан».
            if fadt.len() >= 129 && rd32(fadt, 112) & (1 << 10) != 0 {
                let space = fadt[116];
                let addr = rd64(fadt, 120);
                let value = fadt[128];
                unsafe {
                    match space {
                        1 if addr != 0 && addr <= u16::MAX as u64 => outb(addr as u16, value),
                        0 if addr != 0 => {
                            // Регистр в памяти: ниже 4 ГиБ прямая карта сплошная (paging.rs).
                            core::ptr::write_volatile(phys_to_virt(addr as usize) as *mut u8, value)
                        }
                        // Пространство конфигурации PCI (2) не трогаем: адрес там — это
                        // шина/устройство/функция/смещение в одном числе, и писать туда наугад
                        // значит писать в случайное устройство.
                        _ => {}
                    }
                }
                for _ in 0..1_000_000 {
                    core::hint::spin_loop();
                }
            }
        }
    }
    unsafe {
        // Импульс сброса через контроллер клавиатуры. Ждём, пока опустеет входной буфер, —
        // иначе команда потеряется.
        for _ in 0..100_000 {
            if super::inb(0x64) & 2 == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        outb(0x64, 0xFE);
        for _ in 0..1_000_000 {
            core::hint::spin_loop();
        }
        // Тройная ошибка: пустой IDTR и прерывание. Отсюда не возвращаются нигде, кроме
        // эмулятора, который решит нас пожалеть.
        let null_idt: [u16; 5] = [0; 5];
        core::arch::asm!(
            "lidt [{0}]",
            "int3",
            in(reg) null_idt.as_ptr(),
            options(nostack),
        );
    }
}

/// Порты выключения гипервизоров — запасной путь, когда ACPI не сложился (например PVH-загрузка,
/// где таблиц BIOS нет вовсе). На реальной машине эти порты просто ничего не делают.
pub fn try_hypervisor_ports() {
    unsafe {
        outw(0x604, 0x2000); // QEMU (ACPI PM1a в q35/i440fx современных версий)
        outw(0xB004, 0x2000); // Bochs и старые QEMU
        outw(0x4004, 0x3400); // VirtualBox
    }
}
