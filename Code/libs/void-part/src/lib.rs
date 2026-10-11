#![no_std]

//! **Таблица разделов** — где на диске лежит store VOID (Веха 226.6).
//!
//! ## Зачем модуль общий
//!
//! Разбор MBR жил в ЧЕТЫРЁХ местах сразу: `ahci`, `nvme`, `xhci` и `ramdisk`. Четыре копии
//! одного цикла по четырём записям — и добавить к ним GPT значило бы сделать восемь. Это ровно
//! тот случай, которым в этом проекте уже обожглись не раз: две меры расходятся молча, и хуже
//! всего расходятся они в редком пути, куда никто не смотрит.
//!
//! Теперь разбор один, а разным остаётся только ЧТЕНИЕ СЕКТОРА: у AHCI это DMA, у NVMe своё
//! кольцо, у USB — транспорт BOT, а у живого носителя сектор уже в памяти. Поэтому наружу
//! модуль смотрит одной функцией [`find_store`], которой передают способ прочитать сектор.
//!
//! ## Почему у нас СВОЙ тип раздела
//!
//! Носителем становится только диск, на котором есть раздел нашего типа. Это не придирка, а
//! единственный след ЯВНОГО СОГЛАСИЯ человека отдать диск: такой раздел делает `install`.
//! Иначе система, загруженная с живого ISO у постороннего, первым же коммитом store легла бы
//! поверх чужой таблицы разделов. В MBR это байт [`VOID_STORE_MBR`], в GPT — [`VOID_STORE_GPT`].
//!
//! GUID выбран читаемым в шестнадцатеричном дампе: `564F4944-0000-4000-A000-564F49445354` —
//! «VOID» в начале и «VOIDST» в хвосте. На экране `hexdump` это видно глазом, и когда диск
//! однажды придётся разбирать руками, такая мелочь стоит дороже красивого случайного числа.
//!
//! ## Порядок: сперва GPT, потом MBR
//!
//! На GPT-диске в нулевом секторе лежит ЗАЩИТНЫЙ MBR — таблица с одной записью типа `0xEE`,
//! закрывающей весь диск. Она существует ровно для того, чтобы старый инструмент не счёл диск
//! пустым и не разметил его заново. Значит читать MBR первым нельзя: на гибридном или
//! неаккуратно размеченном диске мы нашли бы в нём чужую запись и поверили ей.

/// Размер сектора. Диски с 4096 нам пока не попадались, и притворяться, что поддержка есть,
/// хуже, чем честно написать константу. Публична: читалка обязана отдавать ровно столько, и
/// пусть это будет видно в её типе, а не в договорённости.
pub const SECTOR: usize = 512;

/// Тип MBR-раздела под store VOID (Веха 48). Произвольный незанятый байт.
pub const VOID_STORE_MBR: u8 = 0x9f;

/// Тип GPT-раздела под store VOID, байтами КАК НА ДИСКЕ (Веха 226.6).
///
/// В GPT первые три поля GUID пишутся little-endian, а два последних — как есть; из-за этого
/// «тот же» GUID в строке и в дампе выглядит по-разному, и перепутать их легко. Поэтому здесь
/// лежит именно дисковый вид, а каноническая запись названа в шапке модуля.
pub const VOID_STORE_GPT: [u8; 16] = [
    0x44, 0x49, 0x4F, 0x56, // 564F4944 → «VOID», little-endian
    0x00, 0x00, // 0000
    0x00, 0x40, // 4000 — версия 4
    0xA0, 0x00, // A000 — вариант RFC 4122
    0x56, 0x4F, 0x49, 0x44, 0x53, 0x54, // «VOIDST» как есть
];

/// Тип раздела ESP — системный раздел EFI. Нужен установке (она его создаёт) и поиску
/// («куда шить `BOOTX64.EFI`»). Канонически `C12A7328-F81F-11D2-BA4B-00A0C93EC93B`.
pub const ESP_GPT: [u8; 16] = [
    0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B,
];

/// Найденный раздел: с какого сектора и сколько секторов.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Part {
    pub start: u64,
    pub sectors: u64,
}

fn le32(b: &[u8], at: usize) -> u64 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]) as u64
}

fn le64(b: &[u8], at: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(v)
}

/// Найти раздел store на устройстве. `read(lba, buf)` обязана прочитать ровно один сектор и
/// вернуть `false`, если не смогла; `None` — раздела нет (диск не наш).
///
/// Порядок — GPT, потом MBR: см. шапку модуля про защитный MBR.
pub fn find_store(mut read: impl FnMut(u64, &mut [u8; SECTOR]) -> bool) -> Option<Part> {
    let mut sec = [0u8; SECTOR];
    if let Some(p) = find_gpt(&mut read, &mut sec, &VOID_STORE_GPT) {
        return Some(p);
    }
    if !read(0, &mut sec) {
        return None;
    }
    find_mbr(&sec)
}

/// Найти раздел ЗАДАННОГО типа GPT. Отдельно от [`find_store`], потому что установке нужен ещё
/// и ESP — а «найти раздел по типу» это одно умение, а не два.
pub fn find_gpt_type(
    mut read: impl FnMut(u64, &mut [u8; SECTOR]) -> bool,
    kind: &[u8; 16],
) -> Option<Part> {
    let mut sec = [0u8; SECTOR];
    find_gpt(&mut read, &mut sec, kind)
}

fn find_gpt(
    read: &mut impl FnMut(u64, &mut [u8; SECTOR]) -> bool,
    sec: &mut [u8; SECTOR],
    kind: &[u8; 16],
) -> Option<Part> {
    if !read(1, sec) || &sec[..8] != b"EFI PART" {
        return None;
    }
    let entries_lba = le64(sec, 72);
    let count = le32(sec, 80) as usize;
    let size = le32(sec, 84) as usize;
    // Здравый смысл вместо доверия: запись GPT не меньше 128 байт по спецификации, а число
    // записей у живых дисков — 128. Мусор в этих полях увёл бы цикл в чтение всего диска.
    if size < 128 || size > SECTOR || count == 0 || count > 1024 || entries_lba == 0 {
        return None;
    }
    let per_sector = SECTOR / size;
    let mut seen = 0usize;
    let mut lba = entries_lba;
    while seen < count {
        if !read(lba, sec) {
            return None;
        }
        for i in 0..per_sector {
            if seen >= count {
                break;
            }
            seen += 1;
            let e = &sec[i * size..];
            if &e[..16] != kind {
                continue;
            }
            let (first, last) = (le64(e, 32), le64(e, 40));
            if first == 0 || last < first {
                continue; // пустая или бессмысленная запись — не наш раздел
            }
            return Some(Part { start: first, sectors: last - first + 1 });
        }
        lba += 1;
    }
    None
}

/// Разобрать MBR, уже лежащий в памяти. Публична ради живого носителя: у него образ целиком в
/// оперативной памяти, и городить ради одного сектора чтение по LBA незачем.
pub fn find_mbr(sec: &[u8]) -> Option<Part> {
    if sec.len() < SECTOR || sec[510] != 0x55 || sec[511] != 0xaa {
        return None;
    }
    for i in 0..4 {
        let e = 446 + i * 16;
        if sec[e + 4] != VOID_STORE_MBR {
            continue;
        }
        let (start, sectors) = (le32(sec, e + 8), le32(sec, e + 12));
        if start == 0 || sectors == 0 {
            continue;
        }
        return Some(Part { start, sectors });
    }
    None
}

#[cfg(test)]
extern crate alloc;

#[cfg(test)]
mod tests {
    use alloc::vec;
    use alloc::vec::Vec;

    use super::*;

    /// Собрать диск в памяти и отдать его как читалку секторов.
    fn disk(sectors: usize) -> Vec<u8> {
        vec![0u8; sectors * SECTOR]
    }

    fn reader(d: &[u8]) -> impl FnMut(u64, &mut [u8; SECTOR]) -> bool + '_ {
        move |lba, buf| {
            let at = lba as usize * SECTOR;
            match d.get(at..at + SECTOR) {
                Some(s) => {
                    buf.copy_from_slice(s);
                    true
                }
                None => false,
            }
        }
    }

    fn put_mbr(d: &mut [u8], idx: usize, kind: u8, start: u32, count: u32) {
        d[510] = 0x55;
        d[511] = 0xaa;
        let e = 446 + idx * 16;
        d[e + 4] = kind;
        d[e + 8..e + 12].copy_from_slice(&start.to_le_bytes());
        d[e + 12..e + 16].copy_from_slice(&count.to_le_bytes());
    }

    fn put_gpt(d: &mut [u8], kind: &[u8; 16], first: u64, last: u64) {
        let h = SECTOR; // заголовок в LBA 1
        d[h..h + 8].copy_from_slice(b"EFI PART");
        d[h + 72..h + 80].copy_from_slice(&2u64.to_le_bytes()); // записи с LBA 2
        d[h + 80..h + 84].copy_from_slice(&128u32.to_le_bytes());
        d[h + 84..h + 88].copy_from_slice(&128u32.to_le_bytes());
        let e = 2 * SECTOR;
        d[e..e + 16].copy_from_slice(kind);
        d[e + 32..e + 40].copy_from_slice(&first.to_le_bytes());
        d[e + 40..e + 48].copy_from_slice(&last.to_le_bytes());
    }

    #[test]
    fn mbr_нашёлся() {
        let mut d = disk(4);
        put_mbr(&mut d, 1, VOID_STORE_MBR, 2048, 100);
        assert_eq!(find_store(reader(&d)), Some(Part { start: 2048, sectors: 100 }));
    }

    #[test]
    fn чужой_mbr_не_наш() {
        let mut d = disk(4);
        put_mbr(&mut d, 0, 0x83, 2048, 100); // Linux
        assert_eq!(find_store(reader(&d)), None);
    }

    #[test]
    fn gpt_нашёлся() {
        let mut d = disk(8);
        put_gpt(&mut d, &VOID_STORE_GPT, 2048, 4095);
        assert_eq!(find_store(reader(&d)), Some(Part { start: 2048, sectors: 2048 }));
    }

    /// Главная проверка порядка: на GPT-диске в нулевом секторе лежит ЗАЩИТНЫЙ MBR. Если
    /// разбирать его первым, можно поверить чужой записи вместо настоящей таблицы.
    #[test]
    fn защитный_mbr_не_уводит_от_gpt() {
        let mut d = disk(8);
        put_mbr(&mut d, 0, 0xEE, 1, 0xFFFF_FFFF); // защитный
        put_mbr(&mut d, 1, VOID_STORE_MBR, 9999, 1); // враньё, которому верить нельзя
        put_gpt(&mut d, &VOID_STORE_GPT, 2048, 4095);
        assert_eq!(find_store(reader(&d)), Some(Part { start: 2048, sectors: 2048 }));
    }

    #[test]
    fn esp_ищется_своим_типом() {
        let mut d = disk(8);
        put_gpt(&mut d, &ESP_GPT, 2048, 2048 + 1023);
        assert_eq!(find_gpt_type(reader(&d), &ESP_GPT), Some(Part { start: 2048, sectors: 1024 }));
        assert_eq!(find_store(reader(&d)), None); // ESP — не store
    }

    /// Мусор в полях заголовка не должен увести обход в чтение всего диска.
    #[test]
    fn нелепый_заголовок_отвергнут() {
        let mut d = disk(8);
        put_gpt(&mut d, &VOID_STORE_GPT, 2048, 4095);
        d[SECTOR + 80..SECTOR + 84].copy_from_slice(&u32::MAX.to_le_bytes()); // число записей
        assert_eq!(find_store(reader(&d)), None);
    }

    #[test]
    fn пустая_запись_пропускается() {
        let mut d = disk(8);
        put_gpt(&mut d, &VOID_STORE_GPT, 0, 0);
        assert_eq!(find_store(reader(&d)), None);
    }
}
