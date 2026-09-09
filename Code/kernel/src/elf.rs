//! Веха 19.2 — минимальный ELF-загрузчик userspace-программ.
//!
//! До Вехи 19 весь код процессов был **вшит в ядро** (секция `.user` — общая, read-only,
//! отображённая всем процессам сразу); Веха 19 добавила ВТОРОЙ путь — код приходит СНАРУЖИ,
//! как обычный статический ELF64/RISC-V (крейт `programs/user`, хранится как объект
//! [[object-model|store]] по content-id, а не в образе ядра), — а Веха 23 сделала его
//! ЕДИНСТВЕННЫМ: секции `.user` больше нет. Этот модуль — узкий и осознанно **минимальный**
//! мост между «байтами ELF» и «замапленным адресным пространством процесса»:
//! - разбирает заголовок ELF64 и program header'ы (никакой секционной информации — она не нужна
//!   для запуска, только PT_LOAD);
//! - для каждого `PT_LOAD` выделяет свежие фреймы, копирует файловую часть, оставляет bss
//!   нулевым (фреймы уже обнулены [`frame::alloc`]) и маппит страницы в адресное пространство
//!   процесса правами из `p_flags`, **никогда не давая одновременно `W` и `X`** (W^X — тот же
//!   инвариант, что и для ядра, см. [[sv39-paging]]);
//! - возвращает точку входа (`e_entry`) — дальше `proc::spawn_elf` заводит обычный процесс.
//!
//! Осознанно НЕ поддержано (см. [[exec-from-store]]): релокации, `PT_DYNAMIC`/`PT_INTERP`,
//! несколько сегментов, делящих одну страницу с разными правами (учтено в `programs/*/linker.ld`
//! через `ALIGN(4K)` перед каждым сегментом — иначе загрузчик перезаписал бы права страницы).

use crate::{arch, frame};

/// Почему загрузка ELF не удалась. Ядро не падает на плохом ELF — просто отказывает в exec'е.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    /// Файл короче заголовка ELF64 или обрезан на program header'ах/данных сегмента.
    Truncated,
    /// Нет магии `\x7fELF`.
    BadMagic,
    /// Не ELF64 (`EI_CLASS`) или не little-endian (`EI_DATA`) — на нашей платформе иначе нельзя.
    BadClass,
    /// `e_machine` — не архитектура этого ядра ([`arch::ELF_MACHINE`]).
    BadMachine,
    /// `e_type` — не `ET_EXEC`: поддерживаем только статические исполняемые файлы, не PIE/DYN
    /// (это и значит «без релокаций/динамики» — см. [[exec-from-store]]).
    BadType,
    /// Сегмент просит одновременно `W` и `X` — нарушение W^X, отказ (см. [[sv39-paging]]).
    WriteExec,
    /// Сегмент выходит за отведённый процессу регион (VPN[2]=1) или залезает в стек.
    OutOfRange,
    /// Кончились физические фреймы.
    OutOfMemory,
}

// ─── константы формата ELF64 (см. System V ABI) ───────────────────────────────
const EI_CLASS: usize = 4;
const EI_DATA: usize = 5;
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const PT_LOAD: u32 = 1;
/// Веха 108.4 — путь ИНТЕРПРЕТАТОРА (`ld.so`) у динамического бинаря.
const PT_INTERP: u32 = 3;
const PF_X: u32 = 1 << 0;
const PF_W: u32 = 1 << 1;
const PF_R: u32 = 1 << 2;

const EHDR_SIZE: usize = 64; // размер Elf64_Ehdr
const PAGE: usize = 4096;

fn u16_at(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(b[off..off + 2].try_into().unwrap())
}
fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

/// Разобрать статический ELF64/RISC-V, замапить его `PT_LOAD`-сегменты в адресное пространство
/// `root` (уже созданное — клон корня ядра + приватный стек, см. `proc::new_address_space`) и
/// вернуть точку входа (`e_entry`). `va_limit` — верхняя граница разрешённых виртуальных адресов
/// (низ стека процесса, см. `proc::spawn_elf`): сегменты выше нельзя, иначе загрузчик перезапишет
/// маппинг стека тем же кодом `map()`, которым мапит сегмент (последняя запись PTE побеждает).
pub fn load(root: usize, bytes: &[u8], va_limit: usize) -> Result<usize, ElfError> {
    if bytes.len() < EHDR_SIZE {
        return Err(ElfError::Truncated);
    }
    if &bytes[0..4] != b"\x7fELF" {
        return Err(ElfError::BadMagic);
    }
    if bytes[EI_CLASS] != ELFCLASS64 || bytes[EI_DATA] != ELFDATA2LSB {
        return Err(ElfError::BadClass);
    }

    let e_type = u16_at(bytes, 16);
    let e_machine = u16_at(bytes, 18);
    let e_entry = u64_at(bytes, 24) as usize;
    let e_phoff = u64_at(bytes, 32) as usize;
    let e_phentsize = u16_at(bytes, 54) as usize;
    let e_phnum = u16_at(bytes, 56) as usize;

    if e_machine != arch::ELF_MACHINE {
        return Err(ElfError::BadMachine);
    }
    if e_type != ET_EXEC {
        return Err(ElfError::BadType); // только статический — без PIE/DYN, без релокаций
    }
    if e_phentsize == 0 || e_phoff.checked_add(e_phnum * e_phentsize).map_or(true, |end| end > bytes.len()) {
        return Err(ElfError::Truncated);
    }

    for i in 0..e_phnum {
        let ph = e_phoff + i * e_phentsize;
        if u32_at(bytes, ph) != PT_LOAD {
            continue; // интересуют только загружаемые сегменты (PT_LOAD)
        }
        let p_flags = u32_at(bytes, ph + 4);
        let p_offset = u64_at(bytes, ph + 8) as usize;
        let p_vaddr = u64_at(bytes, ph + 16) as usize;
        let p_filesz = u64_at(bytes, ph + 32) as usize;
        let p_memsz = u64_at(bytes, ph + 40) as usize;

        if p_filesz > p_memsz {
            return Err(ElfError::Truncated); // файловая часть не может быть больше памяти сегмента
        }
        if p_offset.checked_add(p_filesz).map_or(true, |end| end > bytes.len()) {
            return Err(ElfError::Truncated);
        }

        // Права страницы из p_flags + обязательный `U` (страница процесса). W^X: если сошлись
        // оба бита — это либо кривой ELF, либо (что хуже) попытка получить W+X страницу для
        // инъекции кода через store — отказываем безусловно, не пытаясь «угадать» намерение.
        let mut flags = arch::MAP_U;
        if p_flags & PF_R != 0 {
            flags |= arch::MAP_R;
        }
        if p_flags & PF_W != 0 {
            flags |= arch::MAP_W;
        }
        if p_flags & PF_X != 0 {
            flags |= arch::MAP_X;
        }
        if flags & arch::MAP_W != 0 && flags & arch::MAP_X != 0 {
            return Err(ElfError::WriteExec);
        }

        let va_start = p_vaddr & !(PAGE - 1);
        let va_end = p_vaddr
            .checked_add(p_memsz)
            .map(|e| (e + PAGE - 1) & !(PAGE - 1))
            .ok_or(ElfError::OutOfRange)?;
        if va_start < crate::proc::USER_REGION_START || va_end > va_limit || va_end < va_start {
            return Err(ElfError::OutOfRange);
        }

        let seg_file_end = p_vaddr + p_filesz; // конец файловой (не-bss) части сегмента, в VA
        let mut va = va_start;
        while va < va_end {
            let pa = frame::alloc().ok_or(ElfError::OutOfMemory)?; // уже обнулён — bss бесплатен

            // Пересечение этой страницы [va, va+PAGE) с файловой частью сегмента [p_vaddr,
            // seg_file_end) в VA-координатах — то, что реально нужно скопировать из файла.
            // Хвост страницы за p_filesz (в т.ч. целые bss-страницы) остаётся нулём: фрейм из
            // `frame::alloc` уже обнулён, отдельная зачистка bss не нужна.
            let copy_start = va.max(p_vaddr);
            let copy_end = (va + PAGE).min(seg_file_end);
            if copy_start < copy_end {
                let file_off = p_offset + (copy_start - p_vaddr);
                let dst_off = copy_start - va;
                let n = copy_end - copy_start;
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        bytes.as_ptr().add(file_off),
                        frame::ptr(pa).add(dst_off),
                        n,
                    );
                }
            }

            // Веха 89: не хватило памяти под таблицы — программа не грузится, ядро цело.
            if !unsafe { arch::map(root, va, pa, flags) } {
                return Err(ElfError::OutOfMemory);
            }
            va += PAGE;
        }
    }

    Ok(e_entry)
}

/// Веха 38 — образ загруженного static-PIE (ET_DYN) для linux-abi: точка входа и адрес
/// program-header'ов в памяти процесса (нужен для `AT_PHDR` в auxv — musl по нему находит
/// `PT_TLS` и себя).
pub struct PieImage {
    /// Абсолютная точка входа: `base + e_entry`.
    pub entry: usize,
    /// VA program-header'ов в загруженном образе (для `AT_PHDR`).
    pub phdr_va: usize,
    /// Размер одной записи program-header (`AT_PHENT`).
    pub phentsize: usize,
    /// Число program-header'ов (`AT_PHNUM`).
    pub phnum: usize,
}

/// Веха 38 — загрузить статический **PIE** (ET_DYN) по базе `base` для linux-персоналии.
///
/// В отличие от [`load`] (наш ET_EXEC по фиксированным адресам): сегменты `PT_LOAD` кладутся по
/// `base + p_vaddr` — линукс-бинарь с nixpkgs слинкован от нуля и «плавает». Релокации ядро
/// **не применяет**: musl `rcrt1` само-релоцируется из `_DYNAMIC` (стандарт static-pie —
/// применить их и здесь означало бы удвоить их и всё испортить). W^X держится, как в [`load`].
///
/// `base` обязан быть кратен странице (адрес — прямо VA сегментов). `va_limit` — верхняя граница
/// (низ кучи), как у [`load`]. Возвращает точку входа и раскладку phdr для auxv.
pub fn load_pie(root: usize, bytes: &[u8], base: usize, va_limit: usize) -> Result<PieImage, ElfError> {
    if bytes.len() < EHDR_SIZE {
        return Err(ElfError::Truncated);
    }
    if &bytes[0..4] != b"\x7fELF" {
        return Err(ElfError::BadMagic);
    }
    if bytes[EI_CLASS] != ELFCLASS64 || bytes[EI_DATA] != ELFDATA2LSB {
        return Err(ElfError::BadClass);
    }

    let e_type = u16_at(bytes, 16);
    let e_machine = u16_at(bytes, 18);
    let e_entry = u64_at(bytes, 24) as usize;
    let e_phoff = u64_at(bytes, 32) as usize;
    let e_phentsize = u16_at(bytes, 54) as usize;
    let e_phnum = u16_at(bytes, 56) as usize;

    if e_machine != arch::ELF_MACHINE {
        return Err(ElfError::BadMachine);
    }
    if e_type != ET_DYN && e_type != ET_EXEC {
        return Err(ElfError::BadType);
    }
    // Веха 185 — статический `ET_EXEC` (а это половина того, что даёт nixpkgs) требует СВОИХ
    // адресов: сдвинуть его нельзя, релокаций в нём нет. Значит база у него нулевая, а
    // предложенная вызывающим относится только к `ET_DYN`, который для того и сделан подвижным.
    let base = if e_type == ET_DYN { base } else { 0 };
    if e_phentsize == 0 || e_phoff.checked_add(e_phnum * e_phentsize).map_or(true, |end| end > bytes.len()) {
        return Err(ElfError::Truncated);
    }

    let mut phdr_va = 0usize; // VA program-header'ов в образе (для AT_PHDR)
    for i in 0..e_phnum {
        let ph = e_phoff + i * e_phentsize;
        if u32_at(bytes, ph) != PT_LOAD {
            continue;
        }
        let p_flags = u32_at(bytes, ph + 4);
        let p_offset = u64_at(bytes, ph + 8) as usize;
        let p_vaddr = u64_at(bytes, ph + 16) as usize;
        let p_filesz = u64_at(bytes, ph + 32) as usize;
        let p_memsz = u64_at(bytes, ph + 40) as usize;

        if p_filesz > p_memsz {
            return Err(ElfError::Truncated);
        }
        if p_offset.checked_add(p_filesz).map_or(true, |end| end > bytes.len()) {
            return Err(ElfError::Truncated);
        }

        // Если program-header'ы попадают в этот сегмент — запомнить их адрес в памяти.
        if e_phoff >= p_offset && e_phoff < p_offset + p_filesz {
            phdr_va = base + p_vaddr + (e_phoff - p_offset);
        }

        let mut flags = arch::MAP_U;
        if p_flags & PF_R != 0 {
            flags |= arch::MAP_R;
        }
        if p_flags & PF_W != 0 {
            flags |= arch::MAP_W;
        }
        if p_flags & PF_X != 0 {
            flags |= arch::MAP_X;
        }
        if flags & arch::MAP_W != 0 && flags & arch::MAP_X != 0 {
            return Err(ElfError::WriteExec);
        }

        let seg_va = base.checked_add(p_vaddr).ok_or(ElfError::OutOfRange)?;
        let va_start = seg_va & !(PAGE - 1);
        let va_end = seg_va
            .checked_add(p_memsz)
            .map(|e| (e + PAGE - 1) & !(PAGE - 1))
            .ok_or(ElfError::OutOfRange)?;
        // Веха 185 — нижняя граница здесь НЕ [`crate::proc::USER_REGION_START`], в отличие от
        // родного загрузчика. Чужой статический `ET_EXEC` сам называет свои адреса, и Linux
        // кладёт такие по `0x400000` — ниже нашей базы. Запрещать ему это значило бы запретить
        // половину того, что даёт nixpkgs, ради границы, которая ничего не защищает: всё, что
        // ниже, — такое же пользовательское пространство.
        //
        // Что граница защищает на самом деле — НУЛЕВУЮ СТРАНИЦУ: она обязана остаться
        // неотображённой, иначе разыменование нулевого указателя перестанет быть ошибкой и
        // станет тихой порчей памяти.
        if va_start < PAGE || va_end > va_limit || va_end < va_start {
            return Err(ElfError::OutOfRange);
        }

        let seg_file_end = seg_va + p_filesz;
        let mut va = va_start;
        while va < va_end {
            let pa = frame::alloc().ok_or(ElfError::OutOfMemory)?;
            let copy_start = va.max(seg_va);
            let copy_end = (va + PAGE).min(seg_file_end);
            if copy_start < copy_end {
                let file_off = p_offset + (copy_start - seg_va);
                let dst_off = copy_start - va;
                let n = copy_end - copy_start;
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        bytes.as_ptr().add(file_off),
                        frame::ptr(pa).add(dst_off),
                        n,
                    );
                }
            }
            // Веха 89: не хватило памяти под таблицы — программа не грузится, ядро цело.
            if !unsafe { arch::map(root, va, pa, flags) } {
                return Err(ElfError::OutOfMemory);
            }
            va += PAGE;
        }
    }

    Ok(PieImage {
        entry: base + e_entry,
        phdr_va,
        phentsize: e_phentsize,
        phnum: e_phnum,
    })
}

/// Веха 108.4 — путь динамического загрузчика (`PT_INTERP`), если бинарь динамический.
///
/// У nixpkgs он абсолютный и указывает прямо в /nix/store — то есть в тот самый пакет, который
/// мы уже умеем разложить и прочитать. Это и делает связку возможной: ничего искать не нужно,
/// адрес загрузчика записан в самом бинаре.
pub fn interp_path(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.len() < EHDR_SIZE || &bytes[0..4] != b"\x7fELF" {
        return None;
    }
    let e_phoff = u64_at(bytes, 32) as usize;
    let e_phentsize = u16_at(bytes, 54) as usize;
    let e_phnum = u16_at(bytes, 56) as usize;
    if e_phentsize == 0 || e_phoff.checked_add(e_phnum * e_phentsize)? > bytes.len() {
        return None;
    }
    for i in 0..e_phnum {
        let ph = e_phoff + i * e_phentsize;
        if u32_at(bytes, ph) != PT_INTERP {
            continue;
        }
        let off = u64_at(bytes, ph + 8) as usize;
        let len = u64_at(bytes, ph + 32) as usize;
        let end = off.checked_add(len)?;
        if end > bytes.len() || len == 0 {
            return None;
        }
        // Строка с завершающим нулём — отдаём без него.
        let s = &bytes[off..end];
        return Some(s.strip_suffix(b"\0").unwrap_or(s));
    }
    None
}

/// Веха 38 — тип ELF-объекта для выбора пути запуска: наш ET_EXEC ([`load`]) или
/// чужой static-PIE ET_DYN ([`load_pie`], linux-abi). `None` — не разобрать заголовок.
pub fn elf_type(bytes: &[u8]) -> Option<u16> {
    if bytes.len() < EHDR_SIZE || &bytes[0..4] != b"\x7fELF" {
        return None;
    }
    Some(u16_at(bytes, 16))
}

/// Первый адрес, по которому образ просит себя разместить (`p_vaddr` первого `PT_LOAD`).
/// `None` — заголовок не разобрать либо загружаемых сегментов нет.
pub fn first_load_vaddr(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < EHDR_SIZE || &bytes[0..4] != b"\x7fELF" {
        return None;
    }
    let e_phoff = u64_at(bytes, 32) as usize;
    let e_phentsize = u16_at(bytes, 54) as usize;
    let e_phnum = u16_at(bytes, 56) as usize;
    if e_phentsize == 0 || e_phoff.checked_add(e_phnum * e_phentsize)? > bytes.len() {
        return None;
    }
    (0..e_phnum)
        .map(|i| e_phoff + i * e_phentsize)
        .filter(|&ph| u32_at(bytes, ph) == PT_LOAD)
        .map(|ph| u64_at(bytes, ph + 16) as usize)
        .min()
}

/// Веха 185 — **чей это образ: наш или чужой**.
///
/// До этой вехи ответ давал ТИП ELF: `ET_DYN` — линуксовый, иначе наш. Ответ был неполон, и это
/// стоило заметного времени: `pkgsStatic.busybox` из nixpkgs — статический `ET_EXEC`, он уезжал
/// в родной загрузчик и не запускался, а выглядело это как «личность Linux не работает».
///
/// Теперь вопрос задаётся правильно: **свой образ узнаётся по БАЗЕ ЛИНКОВКИ**. Родные программы
/// VOID собираются одним линкер-скриптом с фиксированной базой [`crate::proc::USER_REGION_START`]
/// — это свойство нашей сборки, а не догадка о чужой. Всё, что просит разместить себя в другом
/// месте, чужое: и `ET_DYN` (static-PIE), и статический `ET_EXEC` по адресу вроде `0x400000`.
///
/// Оговорка, которую надо признать: линуксовый бинарь, слинкованный РОВНО по нашей базе, был бы
/// принят за свой. Такой не встречается — 0x4000_0000 не принадлежит ни одному соглашению
/// Linux, — но если однажды встретится, лечится это не хитростью, а меткой в наших образах.
pub fn is_foreign(bytes: &[u8], native_base: usize) -> bool {
    match elf_type(bytes) {
        Some(ET_EXEC) => first_load_vaddr(bytes) != Some(native_base),
        Some(_) => true, // ET_DYN и всё прочее — точно не наше
        None => false,   // заголовок не разобрать: пусть отвечает родной загрузчик
    }
}
