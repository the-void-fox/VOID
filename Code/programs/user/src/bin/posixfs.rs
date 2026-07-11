//! POSIX-персоналия (Вехи 18.1–18.3): даёт клиентам файловый API
//! `open/read/write/close/stat/unlink/readdir` по IPC. Файл = значение в объектном store,
//! привязанное к корню-имени (`a0` = cap на store): `open(name)` = `get_root(name)` → есть?
//! загрузить : создать; `close` изменённого файла = `put` + `set_root` (атомарный чекпойнт).
//! Привязка к корню переживает и GC ядра, и перезагрузку.
//!
//! Данные файлов (16 × 4 КиБ) и скретч — в ленивой куче (`SYS_MAP`): страницы приходят по мере
//! реальной записи, по одной на файл. Метаданные — обычные массивы на стеке: с Вехи 23 это
//! настоящий ELF, memset/memcpy линкуются в сам бинарь, ухищрения времён секции `.user`
//! (сырые указатели, MaybeUninit) больше не нужны.
//!
//! Индекс каталога (для readdir/unlink) персистится под спец-корнем `.dir` — формат
//! count(1) | [nlen(1) | name]*. Корни-файлы ядро перечислять не даёт, поэтому список имён свой.
#![no_std]
#![no_main]

use void_user as sys;
use void_user::posix::{OP_CLOSE, OP_OPEN, OP_READ, OP_READDIR, OP_STAT, OP_UNLINK, OP_WRITE};
use void_user::posix::{O_APPEND, O_TRUNC};

const NFILES: usize = 16;
const NAME_MAX: usize = 32;
const DATA_MAX: usize = 4096;
static DIRROOT: &[u8] = b".dir";

/// Есть ли имя в индексе каталога.
fn dir_contains(dir: &[u8], name: &[u8]) -> bool {
    let cnt = dir[0] as usize;
    let mut off = 1usize;
    for _ in 0..cnt {
        let l = dir[off] as usize;
        off += 1;
        if &dir[off..off + l] == name {
            return true;
        }
        off += l;
    }
    false
}

/// Добавить имя в индекс, если его ещё нет. Возвращает `true`, если индекс изменился.
fn dir_add(dir: &mut [u8], dir_len: &mut usize, name: &[u8]) -> bool {
    if dir_contains(&dir[..*dir_len], name) {
        return false;
    }
    if *dir_len + 1 + name.len() > DATA_MAX {
        return false; // нет места — упрощение (без ENOSPC)
    }
    dir[*dir_len] = name.len() as u8;
    dir[*dir_len + 1..*dir_len + 1 + name.len()].copy_from_slice(name);
    *dir_len += 1 + name.len();
    dir[0] += 1; // count++
    true
}

/// Убрать имя из индекса (сдвиг хвоста). Возвращает `true`, если что-то удалили.
fn dir_remove(dir: &mut [u8], dir_len: &mut usize, name: &[u8]) -> bool {
    let cnt = dir[0] as usize;
    let mut off = 1usize;
    for _ in 0..cnt {
        let l = dir[off] as usize;
        let entry = 1 + l;
        if &dir[off + 1..off + 1 + l] == name {
            dir.copy_within(off + entry..*dir_len, off);
            *dir_len -= entry;
            dir[0] = (cnt - 1) as u8; // count--
            return true;
        }
        off += entry;
    }
    false
}

/// Записать индекс каталога в store и привязать к спец-корню ".dir" (переживёт перезагрузку).
fn dir_persist(store_cap: usize, dir: &[u8], id: &mut [u8; 32]) {
    sys::obj_put(store_cap, dir, id);
    sys::obj_set_root(store_cap, DIRROOT, id);
}

#[no_mangle]
pub extern "C" fn _start(store_cap: usize, _a1: usize) -> ! {
    // Данные файлов + скретч для stat — в ленивой куче: 17 страничных диапазонов адресов,
    // а физические страницы приходят по одной на реально записанный файл.
    let heap = sys::heap_map((NFILES + 1) * DATA_MAX);
    if heap == usize::MAX {
        sys::exit(1);
    }
    let all = unsafe { core::slice::from_raw_parts_mut(heap as *mut u8, (NFILES + 1) * DATA_MAX) };
    let (files, scratch) = all.split_at_mut(NFILES * DATA_MAX);

    // Метаданные: namespace (имена → данные) и таблица дескрипторов.
    let mut names = [[0u8; NAME_MAX]; NFILES];
    let mut name_len = [0usize; NFILES];
    let mut size = [0usize; NFILES];
    let mut fused = [false; NFILES]; // слот файла занят
    let mut dirty = [false; NFILES]; // изменён с последней записи в store
    let mut fd_file = [0usize; NFILES]; // fd → файл
    let mut fd_off = [0usize; NFILES]; // fd → смещение (курсор)
    let mut fd_used = [false; NFILES]; // дескриптор занят

    let mut dir = [0u8; DATA_MAX];
    let mut dir_len = 1usize; // count(0) — пустой каталог

    let mut req = [0u8; 512];
    let mut rep = [0u8; 512];
    let mut idb = [0u8; 32]; // content-id для OBJ_PUT/GET

    // Поднять индекс каталога с прошлого запуска (get_root(".dir") → get), иначе — пустой.
    if sys::obj_get_root(store_cap, DIRROOT, &mut idb) == 32 {
        let n = sys::obj_get(store_cap, &idb, &mut dir);
        if n > 0 {
            dir_len = n;
        }
    }

    loop {
        // RECV → op (opcode | fd<<8 | mode<<16), reply-cap, нагрузка в req.
        let m = sys::recv(&mut req);
        let opcode = m.op & 0xff;
        let fd = (m.op >> 8) & 0xff;
        let mode = (m.op >> 16) & 0xff;
        let len = m.len.min(512);
        let mut reply_len = 0usize;

        match opcode {
            OP_OPEN => {
                // req[..len] — имя. Найти файл или создать; выделить fd; вернуть [fd] (0xff — ошибка).
                let nl = len.min(NAME_MAX);
                let mut fidx = usize::MAX;
                for i in 0..NFILES {
                    if fused[i] && &names[i][..name_len[i]] == &req[..len] {
                        fidx = i;
                        break;
                    }
                }
                if fidx == usize::MAX {
                    // Не в RAM — занять свободный слот и попробовать поднять из store, иначе создать.
                    if let Some(j) = (0..NFILES).find(|&j| !fused[j]) {
                        fidx = j;
                        fused[j] = true;
                        dirty[j] = false;
                        name_len[j] = nl;
                        names[j][..nl].copy_from_slice(&req[..nl]);
                        // Есть ли корень с этим именем? (get_root)
                        if sys::obj_get_root(store_cap, &req[..nl], &mut idb) == 32 {
                            // Загрузить содержимое по content-id в буфер данных файла.
                            let dbuf = &mut files[j * DATA_MAX..(j + 1) * DATA_MAX];
                            size[j] = sys::obj_get(store_cap, &idb, dbuf);
                            // Персистентный файл, привязанный к корню ДО появления индекса
                            // каталога, — занести в индекс, чтобы его видел readdir.
                            if dir_add(&mut dir, &mut dir_len, &req[..nl]) {
                                dir_persist(store_cap, &dir[..dir_len], &mut idb);
                            }
                        } else {
                            size[j] = 0; // новый файл
                        }
                    }
                }
                let mut nfd = usize::MAX;
                if fidx != usize::MAX {
                    // O_TRUNC обнуляет содержимое (пометив изменённым — перезапишется на close).
                    if mode & O_TRUNC != 0 {
                        size[fidx] = 0;
                        dirty[fidx] = true;
                    }
                    if let Some(d) = (0..NFILES).find(|&d| !fd_used[d]) {
                        nfd = d;
                        fd_used[d] = true;
                        fd_file[d] = fidx;
                        // O_APPEND ставит курсор в конец, иначе — в начало.
                        fd_off[d] = if mode & O_APPEND != 0 { size[fidx] } else { 0 };
                    }
                }
                rep[0] = if nfd == usize::MAX { 0xff } else { nfd as u8 };
                reply_len = 1;
            }
            OP_WRITE => {
                // req[..len] — данные; дописать в файл дескриптора со смещения fd_off.
                if fd < NFILES && fd_used[fd] {
                    let fi = fd_file[fd];
                    let w = fd_off[fd];
                    let n = len.min(DATA_MAX - w);
                    files[fi * DATA_MAX + w..fi * DATA_MAX + w + n].copy_from_slice(&req[..n]);
                    fd_off[fd] = w + n;
                    if w + n > size[fi] {
                        size[fi] = w + n;
                    }
                    dirty[fi] = true; // пометить для записи в store при close
                }
            }
            OP_READ => {
                // Прочитать из файла со смещения до конца (клиент ограничит своим recv-буфером).
                if fd < NFILES && fd_used[fd] {
                    let fi = fd_file[fd];
                    let r = fd_off[fd];
                    let n = size[fi].saturating_sub(r).min(rep.len());
                    rep[..n].copy_from_slice(&files[fi * DATA_MAX + r..fi * DATA_MAX + r + n]);
                    fd_off[fd] = r + n;
                    reply_len = n;
                }
            }
            OP_STAT => {
                // req[..len] — имя. Вернуть [exists:1 | size:4 LE]. Ищем в RAM, затем среди корней.
                let mut sz = usize::MAX;
                for i in 0..NFILES {
                    if fused[i] && &names[i][..name_len[i]] == &req[..len] {
                        sz = size[i];
                        break;
                    }
                }
                if sz == usize::MAX && sys::obj_get_root(store_cap, &req[..len], &mut idb) == 32 {
                    // Не в RAM, но корень есть — подгрузить ради длины в СКРЕТЧ кучи,
                    // не в 512-байтный буфер ответа (файл до 4 КиБ переполнил бы его).
                    sz = sys::obj_get(store_cap, &idb, scratch);
                }
                let exists = (sz != usize::MAX) as u8;
                let szv = if sz == usize::MAX { 0 } else { sz } as u32;
                rep[0] = exists;
                rep[1..5].copy_from_slice(&szv.to_le_bytes());
                reply_len = 5;
            }
            OP_UNLINK => {
                // req[..len] — имя. Убрать из RAM namespace, снять корень и убрать из каталога.
                for i in 0..NFILES {
                    if fused[i] && &names[i][..name_len[i]] == &req[..len] {
                        fused[i] = false;
                        // закрыть висящие дескрипторы на удаляемый файл
                        for d in 0..NFILES {
                            if fd_used[d] && fd_file[d] == i {
                                fd_used[d] = false;
                            }
                        }
                        break;
                    }
                }
                // снять персистентный корень — объект уйдёт в GC (честный unlink)
                sys::obj_del_root(store_cap, &req[..len]);
                if dir_remove(&mut dir, &mut dir_len, &req[..len]) {
                    dir_persist(store_cap, &dir[..dir_len], &mut idb);
                }
                rep[0] = 0;
                reply_len = 1;
            }
            OP_READDIR => {
                // Вернуть имена файлов из индекса каталога, разделённые '\n'.
                let cnt = dir[0] as usize;
                let mut off = 1usize;
                for _ in 0..cnt {
                    let l = dir[off] as usize;
                    off += 1;
                    for k in 0..l {
                        if reply_len < 511 {
                            rep[reply_len] = dir[off + k];
                            reply_len += 1;
                        }
                    }
                    off += l;
                    if reply_len < 511 {
                        rep[reply_len] = b'\n';
                        reply_len += 1;
                    }
                }
            }
            _ => {
                // OP_CLOSE — если файл менялся, записать содержимое в store, привязать к
                // корню-имени и занести имя в индекс каталога (переживёт GC/перезагрузку,
                // виден в readdir). Затем освободить дескриптор.
                let _ = OP_CLOSE;
                if fd < NFILES && fd_used[fd] {
                    let fi = fd_file[fd];
                    if dirty[fi] {
                        sys::obj_put(store_cap, &files[fi * DATA_MAX..fi * DATA_MAX + size[fi]], &mut idb);
                        sys::obj_set_root(store_cap, &names[fi][..name_len[fi]], &idb);
                        let nm = names[fi];
                        if dir_add(&mut dir, &mut dir_len, &nm[..name_len[fi]]) {
                            dir_persist(store_cap, &dir[..dir_len], &mut idb);
                        }
                        dirty[fi] = false;
                    }
                    fd_used[fd] = false;
                }
            }
        }

        sys::reply(m.reply_cap, &rep[..reply_len]);
    }
}
