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
//! На riscv ничего этого не нужно: там выключение — вызов SBI (`sbi::shutdown`).

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

/// Порты выключения гипервизоров — запасной путь, когда ACPI не сложился (например PVH-загрузка,
/// где таблиц BIOS нет вовсе). На реальной машине эти порты просто ничего не делают.
pub fn try_hypervisor_ports() {
    unsafe {
        outw(0x604, 0x2000); // QEMU (ACPI PM1a в q35/i440fx современных версий)
        outw(0xB004, 0x2000); // Bochs и старые QEMU
        outw(0x4004, 0x3400); // VirtualBox
    }
}
