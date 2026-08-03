//! POSIX-персоналия (Вехи 18–30, ИЕРАРХИЯ — Веха 44): файловый API `open/read/write/close/stat/
//! unlink/readdir/seek/rename/mkdir` по IPC, теперь с **каталогами и путями**. Файл = значение
//! в store под корнем `f<путь>`; каталог = индекс под корнем `d<путь>` (список имён + типов).
//! И то, и другое — обычные корни, поэтому GC их держит, а иерархия переживает перезагрузку —
//! БЕЗ изменений в ядре (стройматериал — произвольные имена корней со слэшами).
//!
//! Пути абсолютные (`/a/b/c`); голое имя без ведущего `/` (от std-программ) трактуется как
//! `/<имя>` — файлы таких программ живут в корне `/`. Клиент vsh резолвит относительные пути к
//! своему `cwd` до отправки, так что сюда приходят готовые абсолютные пути.
//!
//! Данные открытых файлов (16 слотов × 128 КиБ) — в ленивой куче; индекс каталога и скретч — на
//! стеке. Совместимость: формат ответа `stat` = `[есть:1 | размер:4 LE | каталог:1]` (старые
//! клиенты читают первые 5 байт, 6-й игнорируют); `readdir` теперь принимает путь каталога.
#![no_std]
#![no_main]

use void_user as sys;
use void_user::posix::{
    OP_CLOSE, OP_MKDIR, OP_OPEN, OP_READ, OP_READDIR, OP_RENAME, OP_SEEK, OP_STAT, OP_UNLINK,
    OP_WRITE,
};
use void_user::posix::{O_APPEND, O_TRUNC};

const NFILES: usize = 16;
const PATH_MAX: usize = 128;
/// Веха 39: файл ≤ 128 КиБ (wasm-модули проходят через персоналию). Буферы — в ленивой куче.
const DATA_MAX: usize = 128 * 1024;
/// Индекс каталога — на СТЕКЕ, свой скромный потолок (не DATA_MAX): count(u16) + записи.
const DIR_MAX: usize = 4096;
/// Имя корня = префикс `f`/`d` + абсолютный путь.
const ROOT_MAX: usize = 1 + PATH_MAX;

// ─── пути ──────────────────────────────────────────────────────────────────────
/// Нормализовать запрос в абсолютный путь в `out`, вернуть длину. Пусто/`.`/`/` → корень `/`;
/// голое имя → `/<имя>`; хвостовой `/` (кроме корня) убираем.
fn normalize(req: &[u8], out: &mut [u8; PATH_MAX]) -> usize {
    let mut n = 0usize;
    if req.is_empty() || req == b"." || req == b"/" {
        out[0] = b'/';
        return 1;
    }
    if req[0] != b'/' {
        out[0] = b'/';
        n = 1;
    }
    for &b in req {
        if n < PATH_MAX {
            out[n] = b;
            n += 1;
        }
    }
    if n > 1 && out[n - 1] == b'/' {
        n -= 1; // убрать хвостовой слэш
    }
    n
}

/// Родитель пути (`/a/b` → `/a`; `/a` → `/`; `/` → `/`).
fn parent(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&b| b == b'/') {
        Some(0) | None => b"/",
        Some(i) => &path[..i],
    }
}

/// Листовое имя (`/a/b` → `b`; `/` → пусто).
fn leaf(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&b| b == b'/') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

/// Собрать имя корня `<prefix><path>` в `out`, вернуть длину.
fn root_name(prefix: u8, path: &[u8], out: &mut [u8; ROOT_MAX]) -> usize {
    out[0] = prefix;
    let n = path.len().min(ROOT_MAX - 1);
    out[1..1 + n].copy_from_slice(&path[..n]);
    1 + n
}

// ─── индекс каталога (корень `d<путь>`): count(u16 LE) | [type(1) | nlen(1) | name]* ───
/// Есть ли `name` в индексе `dir[..len]`; возвращает тип (0=файл, 1=каталог).
fn idx_type(dir: &[u8], len: usize, name: &[u8]) -> Option<u8> {
    if len < 2 {
        return None;
    }
    let cnt = u16::from_le_bytes([dir[0], dir[1]]) as usize;
    let mut off = 2usize;
    for _ in 0..cnt {
        if off + 2 > len {
            break;
        }
        let ty = dir[off];
        let nl = dir[off + 1] as usize;
        if off + 2 + nl > len {
            break;
        }
        if &dir[off + 2..off + 2 + nl] == name {
            return Some(ty);
        }
        off += 2 + nl;
    }
    None
}

/// Добавить запись (если её ещё нет). Возвращает новую длину.
fn idx_add(dir: &mut [u8], mut len: usize, name: &[u8], is_dir: bool) -> usize {
    if len < 2 {
        dir[0] = 0;
        dir[1] = 0;
        len = 2;
    }
    if idx_type(dir, len, name).is_some() || len + 2 + name.len() > DIR_MAX {
        return len;
    }
    dir[len] = is_dir as u8;
    dir[len + 1] = name.len() as u8;
    dir[len + 2..len + 2 + name.len()].copy_from_slice(name);
    let cnt = u16::from_le_bytes([dir[0], dir[1]]) + 1;
    dir[0..2].copy_from_slice(&cnt.to_le_bytes());
    len + 2 + name.len()
}

/// Убрать запись (сдвиг хвоста). Возвращает новую длину (не меняется, если не было).
fn idx_remove(dir: &mut [u8], mut len: usize, name: &[u8]) -> usize {
    if len < 2 {
        return len;
    }
    let cnt = u16::from_le_bytes([dir[0], dir[1]]) as usize;
    let mut off = 2usize;
    for _ in 0..cnt {
        if off + 2 > len {
            break;
        }
        let nl = dir[off + 1] as usize;
        let entry = 2 + nl;
        if &dir[off + 2..off + 2 + nl] == name {
            dir.copy_within(off + entry..len, off);
            len -= entry;
            dir[0..2].copy_from_slice(&((cnt - 1) as u16).to_le_bytes());
            return len;
        }
        off += entry;
    }
    len
}

/// Пуст ли каталог (count == 0).
fn idx_empty(dir: &[u8], len: usize) -> bool {
    len < 2 || u16::from_le_bytes([dir[0], dir[1]]) == 0
}

#[no_mangle]
pub extern "C" fn _start(store_cap: usize, _a1: usize) -> ! {
    // Данные открытых файлов — в ленивой куче: NFILES страничных диапазонов, физпамять по факту.
    let heap = sys::heap_map((NFILES + 1) * DATA_MAX);
    if heap == usize::MAX {
        sys::exit(1);
    }
    let all = unsafe { core::slice::from_raw_parts_mut(heap as *mut u8, (NFILES + 1) * DATA_MAX) };
    let (files, scratch) = all.split_at_mut(NFILES * DATA_MAX);

    // Метаданные слотов файлов (кэш открытых) и таблица дескрипторов.
    let mut paths = [[0u8; PATH_MAX]; NFILES]; // абсолютный путь файла в слоте
    let mut path_len = [0usize; NFILES];
    let mut size = [0usize; NFILES];
    let mut fused = [false; NFILES];
    let mut dirty = [false; NFILES];
    let mut fd_file = [0usize; NFILES];
    let mut fd_off = [0usize; NFILES];
    let mut fd_used = [false; NFILES];

    let mut req = [0u8; 512];
    let mut rep = [0u8; 512];
    let mut idb = [0u8; 32];
    let mut dir = [0u8; DIR_MAX]; // рабочий буфер индекса каталога
    let mut pbuf = [0u8; PATH_MAX]; // нормализованный путь запроса

    // Убедиться, что корневой каталог `/` существует (первый запуск).
    {
        let mut rn = [0u8; ROOT_MAX];
        let rl = root_name(b'd', b"/", &mut rn);
        if sys::obj_get_root(store_cap, &rn[..rl], &mut idb) != 32 {
            dir[0] = 0;
            dir[1] = 0;
            sys::obj_put(store_cap, &dir[..2], &mut idb);
            sys::obj_set_root(store_cap, &rn[..rl], &idb);
        }
    }

    // Прочитать индекс каталога `path` в `dir`, вернуть длину (0 — каталога нет).
    let read_index = |store: usize,
                      path: &[u8],
                      dir: &mut [u8; DIR_MAX],
                      idb: &mut [u8; 32]|
     -> Option<usize> {
        let mut rn = [0u8; ROOT_MAX];
        let rl = root_name(b'd', path, &mut rn);
        if sys::obj_get_root(store, &rn[..rl], idb) != 32 {
            return None;
        }
        Some(sys::obj_get(store, idb, dir))
    };
    let write_index = |store: usize, path: &[u8], dir: &[u8], idb: &mut [u8; 32]| {
        let mut rn = [0u8; ROOT_MAX];
        let rl = root_name(b'd', path, &mut rn);
        sys::obj_put(store, dir, idb);
        sys::obj_set_root(store, &rn[..rl], idb);
    };

    loop {
        let m = sys::recv(&mut req);
        let opcode = m.op & 0xff;
        let fd = (m.op >> 8) & 0xff;
        let mode = (m.op >> 16) & 0xff;
        let len = m.len.min(512);
        let mut reply_len = 0usize;

        match opcode {
            OP_MKDIR => {
                // Создать каталог: пустой индекс `d<path>` + запись в родителе. rep[0]: 0 ок / 1 ошибка.
                let pl = normalize(&req[..len], &mut pbuf);
                let path = &pbuf[..pl];
                rep[0] = 1;
                if pl > 1 {
                    // родитель должен существовать (или это корень)
                    let par = parent(path);
                    let par_exists = par == b"/"
                        || read_index(store_cap, par, &mut dir, &mut idb).is_some();
                    let exists = read_index(store_cap, path, &mut dir, &mut idb).is_some();
                    if par_exists && !exists {
                        dir[0] = 0;
                        dir[1] = 0;
                        write_index(store_cap, path, &dir[..2], &mut idb);
                        // добавить в родителя
                        let plen = read_index(store_cap, par, &mut dir, &mut idb).unwrap_or(2);
                        let nlen = idx_add(&mut dir, plen, leaf(path), true);
                        write_index(store_cap, par, &dir[..nlen], &mut idb);
                        rep[0] = 0;
                    }
                }
                reply_len = 1;
            }
            OP_OPEN => {
                // req — путь; открыть/создать файл, вернуть [fd] (0xff — ошибка).
                let pl = normalize(&req[..len], &mut pbuf);
                let path = &pbuf[..pl];
                // нельзя открыть каталог как файл
                let is_dir = read_index(store_cap, path, &mut dir, &mut idb).is_some();
                let mut fidx = usize::MAX;
                if !is_dir {
                    // уже в кэше?
                    for i in 0..NFILES {
                        if fused[i] && &paths[i][..path_len[i]] == path {
                            fidx = i;
                            break;
                        }
                    }
                    if fidx == usize::MAX {
                        if let Some(j) = (0..NFILES).find(|&j| !fused[j]) {
                            fidx = j;
                            fused[j] = true;
                            dirty[j] = false;
                            path_len[j] = pl;
                            paths[j][..pl].copy_from_slice(path);
                            let mut rn = [0u8; ROOT_MAX];
                            let rl = root_name(b'f', path, &mut rn);
                            if sys::obj_get_root(store_cap, &rn[..rl], &mut idb) == 32 {
                                let dbuf = &mut files[j * DATA_MAX..(j + 1) * DATA_MAX];
                                size[j] = sys::obj_get(store_cap, &idb, dbuf);
                            } else {
                                size[j] = 0; // новый файл (создастся при close)
                                dirty[j] = true;
                            }
                        }
                    }
                }
                let mut nfd = usize::MAX;
                if fidx != usize::MAX {
                    if mode & O_TRUNC != 0 {
                        size[fidx] = 0;
                        dirty[fidx] = true;
                    }
                    if let Some(d) = (0..NFILES).find(|&d| !fd_used[d]) {
                        nfd = d;
                        fd_used[d] = true;
                        fd_file[d] = fidx;
                        fd_off[d] = if mode & O_APPEND != 0 { size[fidx] } else { 0 };
                    }
                }
                rep[0] = if nfd == usize::MAX { 0xff } else { nfd as u8 };
                reply_len = 1;
            }
            OP_WRITE => {
                if fd < NFILES && fd_used[fd] {
                    let fi = fd_file[fd];
                    let w = fd_off[fd];
                    let n = len.min(DATA_MAX - w);
                    files[fi * DATA_MAX + w..fi * DATA_MAX + w + n].copy_from_slice(&req[..n]);
                    fd_off[fd] = w + n;
                    if w + n > size[fi] {
                        size[fi] = w + n;
                    }
                    dirty[fi] = true;
                }
            }
            OP_READ => {
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
                // req — путь. rep = [есть:1 | размер:4 LE | каталог:1].
                let pl = normalize(&req[..len], &mut pbuf);
                let path = &pbuf[..pl];
                let mut sz = usize::MAX;
                let mut is_dir = false;
                if read_index(store_cap, path, &mut dir, &mut idb).is_some() {
                    is_dir = true;
                    sz = 0;
                } else {
                    for i in 0..NFILES {
                        if fused[i] && &paths[i][..path_len[i]] == path {
                            sz = size[i];
                            break;
                        }
                    }
                    if sz == usize::MAX {
                        let mut rn = [0u8; ROOT_MAX];
                        let rl = root_name(b'f', path, &mut rn);
                        if sys::obj_get_root(store_cap, &rn[..rl], &mut idb) == 32 {
                            sz = sys::obj_get(store_cap, &idb, scratch);
                        }
                    }
                }
                rep[0] = (sz != usize::MAX) as u8;
                let szv = if sz == usize::MAX { 0 } else { sz } as u32;
                rep[1..5].copy_from_slice(&szv.to_le_bytes());
                rep[5] = is_dir as u8;
                reply_len = 6;
            }
            OP_UNLINK => {
                // req — путь. Файл: снять корень `f<path>` + убрать из родителя. Каталог: только
                // пустой — снять `d<path>` + убрать из родителя. rep[0]: 0 ок / 1 ошибка.
                let pl = normalize(&req[..len], &mut pbuf);
                let path = &pbuf[..pl];
                rep[0] = 1;
                if pl > 1 {
                    let par = parent(path);
                    if let Some(dlen) = read_index(store_cap, path, &mut dir, &mut idb) {
                        // каталог — удаляем, только если пуст
                        if idx_empty(&dir, dlen) {
                            let mut rn = [0u8; ROOT_MAX];
                            let rl = root_name(b'd', path, &mut rn);
                            sys::obj_del_root(store_cap, &rn[..rl]);
                            let plen = read_index(store_cap, par, &mut dir, &mut idb).unwrap_or(2);
                            let nlen = idx_remove(&mut dir, plen, leaf(path));
                            write_index(store_cap, par, &dir[..nlen], &mut idb);
                            rep[0] = 0;
                        }
                    } else {
                        // файл
                        for i in 0..NFILES {
                            if fused[i] && &paths[i][..path_len[i]] == path {
                                fused[i] = false;
                                for d in 0..NFILES {
                                    if fd_used[d] && fd_file[d] == i {
                                        fd_used[d] = false;
                                    }
                                }
                                break;
                            }
                        }
                        let mut rn = [0u8; ROOT_MAX];
                        let rl = root_name(b'f', path, &mut rn);
                        sys::obj_del_root(store_cap, &rn[..rl]);
                        let plen = read_index(store_cap, par, &mut dir, &mut idb).unwrap_or(2);
                        let nlen = idx_remove(&mut dir, plen, leaf(path));
                        write_index(store_cap, par, &dir[..nlen], &mut idb);
                        rep[0] = 0;
                    }
                }
                reply_len = 1;
            }
            OP_SEEK => {
                let mut pos = u64::MAX;
                if fd < NFILES && fd_used[fd] && len >= 8 {
                    let off = i64::from_le_bytes(req[..8].try_into().unwrap());
                    let fi = fd_file[fd];
                    let base = match mode {
                        1 => fd_off[fd] as i64,
                        2 => size[fi] as i64,
                        _ => 0,
                    };
                    let p = (base + off).clamp(0, size[fi] as i64) as usize;
                    fd_off[fd] = p;
                    pos = p as u64;
                }
                rep[..8].copy_from_slice(&pos.to_le_bytes());
                reply_len = 8;
            }
            OP_RENAME => {
                // req: old_len(1) | old | new (абсолютные пути). Перевесить файл-корень + индексы.
                rep[0] = 0xff;
                let ol = if len >= 2 { req[0] as usize } else { usize::MAX };
                if ol != usize::MAX && 1 + ol < len {
                    let mut oldp = [0u8; PATH_MAX];
                    let mut newp = [0u8; PATH_MAX];
                    let onl = normalize(&req[1..1 + ol], &mut oldp);
                    let mut nnl = normalize(&req[1 + ol..len], &mut newp);
                    // Если цель — СУЩЕСТВУЮЩИЙ КАТАЛОГ, POSIX кладёт файл ВНУТРЬ него
                    // (`mv файл каталог` = `mv файл каталог/файл`). Без этого содержимое файла
                    // вешалось на корень `f<каталог>` рядом с живым `d<каталог>`: файл
                    // становился недостижим, то есть `mv` ТЕРЯЛ ДАННЫЕ. Найдено владельцем
                    // на X54C (Веха 97.1).
                    {
                        let mut rd = [0u8; ROOT_MAX];
                        let rdl = root_name(b'd', &newp[..nnl], &mut rd);
                        if sys::obj_get_root(store_cap, &rd[..rdl], &mut idb) == 32 {
                            let name = leaf(&oldp[..onl]);
                            // `/` уже оканчивается разделителем — второй не нужен.
                            let mut w = nnl;
                            if newp[w - 1] != b'/' && w < PATH_MAX {
                                newp[w] = b'/';
                                w += 1;
                            }
                            let n = name.len().min(PATH_MAX - w);
                            newp[w..w + n].copy_from_slice(&name[..n]);
                            nnl = w + n;
                        }
                    }
                    let (old, new) = (&oldp[..onl], &newp[..nnl]);
                    // Переименование в самого себя — успех и НИКАКОЙ работы: иначе ниже мы бы
                    // сняли корень сразу после того, как его же поставили, и потеряли файл.
                    let same = old == new;
                    let mut ok = same;
                    // перевесить файл-корень f<old> → f<new>
                    let mut ro = [0u8; ROOT_MAX];
                    let mut rnw = [0u8; ROOT_MAX];
                    let rlo = root_name(b'f', old, &mut ro);
                    let rln = root_name(b'f', new, &mut rnw);
                    if !same && sys::obj_get_root(store_cap, &ro[..rlo], &mut idb) == 32 {
                        sys::obj_set_root(store_cap, &rnw[..rln], &idb);
                        sys::obj_del_root(store_cap, &ro[..rlo]);
                        ok = true;
                    }
                    // слот в кэше
                    for i in 0..NFILES {
                        if !same && fused[i] && &paths[i][..path_len[i]] == old {
                            paths[i][..nnl].copy_from_slice(new);
                            path_len[i] = nnl;
                            ok = true;
                            break;
                        }
                    }
                    if same {
                        rep[0] = 0;
                    } else if ok {
                        // индексы родителей
                        let pold = parent(old);
                        let plen = read_index(store_cap, pold, &mut dir, &mut idb).unwrap_or(2);
                        let nlen = idx_remove(&mut dir, plen, leaf(old));
                        write_index(store_cap, pold, &dir[..nlen], &mut idb);
                        let pnew = parent(new);
                        let plen = read_index(store_cap, pnew, &mut dir, &mut idb).unwrap_or(2);
                        let nlen = idx_add(&mut dir, plen, leaf(new), false);
                        write_index(store_cap, pnew, &dir[..nlen], &mut idb);
                        rep[0] = 0;
                    }
                }
                reply_len = 1;
            }
            OP_READDIR => {
                // req — путь каталога (пусто/`.`/`/` → корень). Ответ: имена через '\n',
                // у каталогов — с хвостовым '/'.
                let pl = normalize(&req[..len], &mut pbuf);
                let path = &pbuf[..pl];
                if let Some(dlen) = read_index(store_cap, path, &mut dir, &mut idb) {
                    let cnt = if dlen >= 2 {
                        u16::from_le_bytes([dir[0], dir[1]]) as usize
                    } else {
                        0
                    };
                    let mut off = 2usize;
                    for _ in 0..cnt {
                        if off + 2 > dlen {
                            break;
                        }
                        let ty = dir[off];
                        let nl = dir[off + 1] as usize;
                        if off + 2 + nl > dlen {
                            break;
                        }
                        for k in 0..nl {
                            if reply_len < 510 {
                                rep[reply_len] = dir[off + 2 + k];
                                reply_len += 1;
                            }
                        }
                        if ty == 1 && reply_len < 511 {
                            rep[reply_len] = b'/';
                            reply_len += 1;
                        }
                        if reply_len < 511 {
                            rep[reply_len] = b'\n';
                            reply_len += 1;
                        }
                        off += 2 + nl;
                    }
                }
            }
            _ => {
                // OP_CLOSE — записать изменённый файл в store, привязать к корню `f<path>`,
                // занести имя в индекс РОДИТЕЛЬСКОГО каталога. Затем освободить дескриптор.
                let _ = OP_CLOSE;
                if fd < NFILES && fd_used[fd] {
                    let fi = fd_file[fd];
                    if dirty[fi] {
                        let path = &paths[fi][..path_len[fi]];
                        let mut rn = [0u8; ROOT_MAX];
                        let rl = root_name(b'f', path, &mut rn);
                        sys::obj_put(store_cap, &files[fi * DATA_MAX..fi * DATA_MAX + size[fi]], &mut idb);
                        sys::obj_set_root(store_cap, &rn[..rl], &idb);
                        // добавить имя в индекс родителя (скопировать путь — dir/idb переиспользуются)
                        let mut pcopy = [0u8; PATH_MAX];
                        let pl = path_len[fi];
                        pcopy[..pl].copy_from_slice(path);
                        let par = parent(&pcopy[..pl]);
                        let plen = read_index(store_cap, par, &mut dir, &mut idb).unwrap_or(2);
                        let nlen = idx_add(&mut dir, plen, leaf(&pcopy[..pl]), false);
                        write_index(store_cap, par, &dir[..nlen], &mut idb);
                        dirty[fi] = false;
                    }
                    fd_used[fd] = false;
                }
            }
        }

        sys::reply(m.reply_cap, &rep[..reply_len]);
    }
}
