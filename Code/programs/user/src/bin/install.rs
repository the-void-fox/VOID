//! install — установщик VOID на SATA-диск как ОТДЕЛЬНАЯ программа (`run install`).
//!
//! Раньше `install` был встроенной командой vsh — а значит присутствовал в КАЖДОЙ системе, даже
//! уже установленной. Теперь это программа `bin/<arch>/install`, которую init сеет ТОЛЬКО при
//! загрузке с install-носителя (есть multiboot-модуль с образом диска — [`boot_module`]). После
//! установки система грузится с диска БЕЗ модуля → `install` не сеется → команды установки на
//! рабочей системе просто нет (её и не должно быть). Запуск на носителе: `run install`.
//!
//! Права наследуются от vsh через `run` (`cap::endow`, Веха 30): у SYS_EXEC-ребёнка a0/a1 НЕ
//! несут права (в отличие от init-спавна) — они в ТАБЛИЦЕ стартовых прав, читаем `SYS_STARTCAP`.
//! Порядок как у vsh (config `endpoint:posixfs store:xw endpoint:net-srv`): slot 0 = posixfs-ep,
//! **slot 1 = store-cap** (у shell'а `store:xw` — есть WRITE, которого требует `SYS_INSTALL`),
//! slot 2 = net-srv-ep. ДИСК СТИРАЕТСЯ ЦЕЛИКОМ.
#![no_std]
#![no_main]

use void_user as sys;

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let store_cap = sys::start_cap(1); // унаследованный от vsh store-cap (store:xw, есть WRITE)
    sys::write("\x1b[1;31mУстановка VOID на диск — диск будет СТЁРТ ЦЕЛИКОМ…\x1b[0m\n".as_bytes());
    match sys::install(store_cap) {
        Some(_p2) => {
            sys::write(
                "\x1b[1;32mГотово.\x1b[0m VOID установлен на диск — вынь USB и перезагрузись.\n"
                    .as_bytes(),
            );
            sys::exit(0)
        }
        None => {
            sys::write(
                "\x1b[1;31mНе удалось.\x1b[0m Нет AHCI-диска, образа установки (модуль с USB) или прав на store.\n"
                    .as_bytes(),
            );
            sys::exit(1)
        }
    }
}
