//! Установка VOID на диск (Веха 48) — «live-USB → install → загрузка с диска».
//!
//! Идея: система, загруженная с USB (GRUB-ISO), сама раскладывает себя на SATA-диск через
//! [`crate::ahci`]. Загрузочный образ (MBR+GRUB+ядро в FAT-разделе p1, см. `Code/boot/mkdisk.sh`)
//! приезжает с USB как **модуль multiboot2** (`module2` в grub.cfg) — GRUB кладёт его в RAM,
//! ядро находит через [`crate::arch::boot_module`]. Так установленное ядро == работающему
//! (один и тот же бинарь в p1 образа), без встраивания образа в само ядро (это было бы циклично).
//!
//! Шаги [`run`]:
//! 1. записать загрузочный префикс образа (сектора 0..начало p2: MBR + зазор с core.img + p1 FAT)
//!    на диск АБСОЛЮТНО ([`crate::ahci::write_abs`]);
//! 2. поправить в MBR диска раздел p2 (store) — растянуть на весь реальный диск;
//! 3. обнулить начало p2 → на ребуте store увидит «пусто» и засеет программы заново;
//! 4. ЗАМОРОЗИТЬ текущий store ([`crate::object::freeze`]) — его кэш больше не должен писать на
//!    диск, иначе group-commit затрёт свежий образ до перезагрузки.
//!
//! После этого пользователь вынимает USB и грузится с диска. **Диск стирается целиком.**

/// Выполнить установку на AHCI-диск. `Ok(p2_start)` — префикс записан, диск размечен, store
/// заморожен (нужен ребут). `Err(причина)` — не сложилось (диск/образ не годны), система цела.
pub fn run() -> Result<u64, &'static str> {
    let (mbase, mlen) =
        crate::arch::boot_module().ok_or("нет образа установки (модуль multiboot2 с USB)")?;
    let total = crate::ahci::total_sectors();
    if total == 0 {
        return Err("нет AHCI-диска (установка только на SATA)");
    }
    // Веха 87: GRUB кладёт модуль в RAM и сообщает ФИЗИЧЕСКИЙ адрес — читаем через direct-map.
    let img = unsafe { core::slice::from_raw_parts(crate::frame::ptr(mbase), mlen) };
    if img.len() < 512 || img[510] != 0x55 || img[511] != 0xaa {
        return Err("образ без MBR-подписи 0x55AA");
    }
    // Начало раздела p2 (store) — из таблицы разделов образа (вторая запись, поле lba_start@+8).
    let p2 = 446 + 16;
    let le32 = |b: &[u8], o: usize| {
        b[o] as u64 | (b[o + 1] as u64) << 8 | (b[o + 2] as u64) << 16 | (b[o + 3] as u64) << 24
    };
    let p2_start = le32(img, p2 + 8);
    let need = (p2_start as usize).saturating_mul(512);
    if p2_start == 0 || img.len() < need {
        return Err("образ короче своего загрузочного префикса");
    }
    if total <= p2_start {
        return Err("диск меньше загрузочного раздела");
    }

    // 1) Загрузочный префикс (MBR + зазор core.img + p1 FAT с GRUB и ядром) → диск, сектор в сектор.
    let mut sec = [0u8; 512];
    for s in 0..p2_start {
        let o = (s as usize) * 512;
        sec.copy_from_slice(&img[o..o + 512]);
        if !crate::ahci::write_abs(s, &sec) {
            return Err("сбой записи загрузочного префикса на диск");
        }
    }

    // 2) MBR диска: растянуть p2 (store) на весь диск — num_sectors@+12 = total − p2_start.
    if !crate::ahci::read_abs(0, &mut sec) {
        return Err("сбой чтения MBR после записи");
    }
    let cnt = ((total - p2_start) as u32).to_le_bytes();
    sec[p2 + 12..p2 + 16].copy_from_slice(&cnt);
    if !crate::ahci::write_abs(0, &sec) {
        return Err("сбой записи MBR");
    }

    // 3) Обнулить начало store (p2) — суперблок + область индекса: на ребуте store решит «пусто»
    //    и засеет программы (как первый запуск), не подхватив мусор старого содержимого диска.
    let zero = [0u8; 512];
    for s in 0..64u64 {
        if !crate::ahci::write_abs(p2_start + s, &zero) {
            return Err("сбой очистки области store");
        }
    }

    // 4) Заморозить текущий store: раскладку диска мы уже сменили, его кэш писать туда нельзя.
    crate::object::freeze();
    Ok(p2_start)
}
