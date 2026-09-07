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
    OP_CLOSE, OP_MKDIR, OP_OPEN, OP_READ, OP_READDIR, OP_READLINK, OP_RENAME, OP_SEEK, OP_STAT,
    OP_UNLINK, OP_WRITE,
};
use void_user::posix::{O_APPEND, O_TRUNC};

// Формат дерева пакета — тот же крейт, которым его ПИШЕТ `pkg` (Веха 108.1). Он намеренно без
// `alloc`: здесь кучи нет вовсе, поэтому индекс читается итератором по чужому буферу.
use void_tree as tree;

const NFILES: usize = 16;
/// Путь (Веха 108.2: было 128 — не хватало даже на `/nix/store/<хэш>-<имя>/lib/...`).
const PATH_MAX: usize = 512;
/// Веха 39: файл ≤ 128 КиБ (wasm-модули проходят через персоналию). Буферы — в ленивой куче.
/// Для файлов ПАКЕТА этот потолок не действует: они читаются прямо из дерева store, кусками.
const DATA_MAX: usize = 128 * 1024;
/// Индекс каталога — на СТЕКЕ, свой скромный потолок (не DATA_MAX): count(u16) + записи.
const DIR_MAX: usize = 4096;
/// Имя корня = префикс `f`/`d` + абсолютный путь.
const ROOT_MAX: usize = 1 + PATH_MAX;

/// Точка монтирования дерева пакетов (Веха 108.2). Всё под ней — ЧТЕНИЕ: пакет неизменяем, и
/// «записать в /nix/store» означало бы завести вторую правду о его содержимом.
const MOUNT: &[u8] = b"/nix/store";

/// Префикс корня store, под которым `pkg` держит распакованные деревья.
const TREE_ROOT: &[u8] = b"pkg/tree/";

/// Длина хэша пути nix.
const HASH_LEN: usize = 32;

/// Сколько исходящих ссылок узла мы готовы выписать за раз. Потолок осмысленный: столько записей
/// в каталоге и столько кусков у файла (при куске 16 КиБ — файл до 64 МиБ).
const KIDS_MAX: usize = 4096;

/// Путь относительно точки монтирования — либо `None`, если он не под ней. Пустой ломоть значит
/// сам каталог `/nix/store`. Проверка на `/` обязательна: `/nix/storage` — не наш путь.
fn under_mount(path: &[u8]) -> Option<&[u8]> {
    if path == MOUNT {
        return Some(b"");
    }
    let rest = path.strip_prefix(MOUNT)?;
    if rest.first() == Some(&b'/') {
        Some(&rest[1..])
    } else {
        None
    }
}

/// Найденный узел дерева пакета.
#[derive(Clone, Copy)]
struct Node {
    ty: u8,
    size: u64,
    id: [u8; 32],
}

/// Спуститься по пути внутри `/nix/store` до узла. `ibuf` — рабочий буфер под индекс каталога,
/// `kbuf` — под исходящие ссылки узла.
fn tree_find(
    scap: usize,
    rel: &[u8],
    ibuf: &mut [u8],
    kbuf: &mut [[u8; 32]],
) -> Option<Node> {
    let mut parts = rel.split(|&b| b == b'/').filter(|c| !c.is_empty());
    let base = parts.next()?;
    if base.len() < HASH_LEN {
        return None;
    }
    let mut rn = [0u8; 64];
    let rl = TREE_ROOT.len() + HASH_LEN;
    rn[..TREE_ROOT.len()].copy_from_slice(TREE_ROOT);
    rn[TREE_ROOT.len()..rl].copy_from_slice(&base[..HASH_LEN]);
    let mut id = [0u8; 32];
    if sys::obj_get_root(scap, &rn[..rl], &mut id) != 32 {
        return None;
    }

    // Корень пакета — индекс из одной записи с полным именем пути: имя сверяется целиком, а не
    // по хэшу, иначе `/nix/store/<хэш>-что-угодно` открывал бы чужой пакет.
    let mut cur = Node { ty: tree::K_DIR, size: 0, id };
    let mut name: &[u8] = base;
    loop {
        let n = sys::obj_get(scap, &cur.id, ibuf);
        if n == 0 || n > ibuf.len() {
            return None;
        }
        let (i, ty, size) = {
            let (i, r) = tree::find(&ibuf[..n], name)?;
            (i, r.ty, r.size)
        };
        if i >= kbuf.len() || sys::obj_children(scap, &cur.id, &mut kbuf[..i + 1]) < i + 1 {
            return None;
        }
        cur = Node { ty, size, id: kbuf[i] };
        match parts.next() {
            Some(next) => {
                if !tree::is_dir(cur.ty) {
                    return None; // спуск сквозь файл или симлинк
                }
                name = next;
            }
            None => return Some(cur),
        }
    }
}

/// Прочитать кусок файла из дерева пакета: с `off` и сколько влезет в `out` (но не дальше конца
/// текущего куска блоба — короткое чтение законно, клиент дочитает следующим вызовом).
fn tree_read(
    scap: usize,
    node: &Node,
    off: usize,
    out: &mut [u8],
    ibuf: &mut [u8],
    kbuf: &mut [[u8; 32]],
) -> usize {
    if !tree::is_blob(node.ty) {
        let n = sys::obj_get(scap, &node.id, ibuf);
        if n == 0 || n > ibuf.len() || off >= n {
            return 0;
        }
        let k = (n - off).min(out.len());
        out[..k].copy_from_slice(&ibuf[off..off + k]);
        return k;
    }
    let m = sys::obj_get(scap, &node.id, ibuf);
    if m == 0 || m > ibuf.len() {
        return 0;
    }
    let Some((total, nchunks, csize)) = sys::http::blob_info(&ibuf[..m]) else {
        return 0;
    };
    if off >= total || csize == 0 {
        return 0;
    }
    let ci = off / csize;
    if ci >= nchunks || ci >= kbuf.len() {
        return 0;
    }
    if sys::obj_children(scap, &node.id, &mut kbuf[..ci + 1]) < ci + 1 {
        return 0;
    }
    let n = sys::obj_get(scap, &kbuf[ci], ibuf);
    let within = off % csize;
    if n == 0 || n > ibuf.len() || within >= n {
        return 0;
    }
    let k = (n - within).min(out.len());
    out[..k].copy_from_slice(&ibuf[within..within + k]);
    k
}

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

/// Сколько записей поддерева помещается в один обход. Потолок, а не вежливость: обход держит
/// список путей, и без границы каталог с миллионом имён съел бы всю кучу персоналии молча.
const WALK_MAX: usize = 512;

/// Веха 175 — **обойти ПОДДЕРЕВО каталога** и вернуть все его записи списком.
///
/// Зачем это вообще нужно. В store нет каталогов как объектов: файл — корень `f<путь>`, каталог —
/// корень-индекс `d<путь>`, и путь входит в ИМЯ каждого корня. Поэтому переименовать каталог
/// значит перевесить корень КАЖДОГО потомка вглубь, а удалить непустой — снять их все. Обе
/// операции — один и тот же обход, и он поэтому здесь один.
///
/// Список работает и очередью: вынутый каталог дописывает в него своих детей, поэтому обход
/// идёт вширь и без рекурсии. Рекурсия здесь была бы не стилем, а риском: глубину дерева задаёт
/// человек, а стек персоналии кончается молча.
///
/// Нулевой записью идёт сам `root`. `None` — дерево не влезло в `WALK_MAX`; тогда вызывающий
/// обязан отказать, а не сделать половину.
fn subtree(
    store: usize,
    root: &[u8],
    paths: &mut [[u8; PATH_MAX]],
    lens: &mut [usize],
    dirs: &mut [bool],
) -> Option<usize> {
    let mut dir = [0u8; DIR_MAX];
    let mut idb = [0u8; 32];
    let mut n = 0usize;
    let put = |paths: &mut [[u8; PATH_MAX]], lens: &mut [usize], dirs: &mut [bool],
               n: &mut usize, p: &[u8], d: bool| -> bool {
        if *n == WALK_MAX || p.len() > PATH_MAX {
            return false;
        }
        paths[*n][..p.len()].copy_from_slice(p);
        lens[*n] = p.len();
        dirs[*n] = d;
        *n += 1;
        true
    };
    if !put(paths, lens, dirs, &mut n, root, true) {
        return None;
    }
    let mut i = 0usize;
    while i < n {
        if !dirs[i] {
            i += 1;
            continue;
        }
        let (plen, mut base) = (lens[i], [0u8; PATH_MAX]);
        base[..plen].copy_from_slice(&paths[i][..plen]);
        let mut rn = [0u8; ROOT_MAX];
        let rl = root_name(b'd', &base[..plen], &mut rn);
        if sys::obj_get_root(store, &rn[..rl], &mut idb) == 32 {
            let dlen = sys::obj_get(store, &idb, &mut dir);
            let cnt = if dlen >= 2 { u16::from_le_bytes([dir[0], dir[1]]) as usize } else { 0 };
            let mut off = 2usize;
            for _ in 0..cnt {
                if off + 2 > dlen {
                    break;
                }
                let (ty, nl) = (dir[off], dir[off + 1] as usize);
                if off + 2 + nl > dlen {
                    break;
                }
                // Полный путь ребёнка: у корня разделитель уже есть, у прочих его надо дописать.
                let mut child = [0u8; PATH_MAX];
                let mut w = plen;
                child[..w].copy_from_slice(&base[..w]);
                if w > 0 && child[w - 1] != b'/' {
                    child[w] = b'/';
                    w += 1;
                }
                if w + nl > PATH_MAX {
                    return None;
                }
                child[w..w + nl].copy_from_slice(&dir[off + 2..off + 2 + nl]);
                if !put(paths, lens, dirs, &mut n, &child[..w + nl], ty == 1) {
                    return None;
                }
                off += 2 + nl;
            }
        }
        i += 1;
    }
    Some(n)
}

#[no_mangle]
pub extern "C" fn _start(store_cap: usize, _a1: usize) -> ! {
    // Данные открытых файлов — в ленивой куче: NFILES страничных диапазонов, физпамять по факту.
    // Веха 108.2 — ещё три области под дерево пакетов: индекс каталога/кусок файла (`ibuf`),
    // исходящие ссылки узла (`kids`) и ОТВЕТ. Ответ переехал со стека, потому что каталог пакета
    // бывает в сотни имён (у glibc `lib/gconv` — 255), и на 4 КиБ список снова начал бы упираться.
    const AREAS: usize = NFILES + 3;
    let kids_bytes = KIDS_MAX * 32;
    // Веха 175 — плюс область под ОБХОД ПОДДЕРЕВА: список путей, он же очередь. Куча ленивая
    // (`SYS_MAP` резервирует диапазон, страницы приходят по обращению), поэтому четверть
    // мегабайта здесь ничего не стоит, пока каталоги не переименовывают.
    let walk_bytes = WALK_MAX * PATH_MAX;
    let total = AREAS * DATA_MAX + kids_bytes + walk_bytes;
    let heap = sys::heap_map(total);
    if heap == usize::MAX {
        sys::exit(1);
    }
    let all = unsafe { core::slice::from_raw_parts_mut(heap as *mut u8, total) };
    let (files, rest) = all.split_at_mut(NFILES * DATA_MAX);
    let (scratch, rest) = rest.split_at_mut(DATA_MAX);
    let (ibuf, rest) = rest.split_at_mut(DATA_MAX);
    let (repbuf, rest) = rest.split_at_mut(DATA_MAX);
    let (kidsb, walkb) = rest.split_at_mut(kids_bytes);
    let kids = unsafe {
        core::slice::from_raw_parts_mut(kidsb.as_mut_ptr() as *mut [u8; 32], KIDS_MAX)
    };
    let walk = unsafe {
        core::slice::from_raw_parts_mut(walkb.as_mut_ptr() as *mut [u8; PATH_MAX], WALK_MAX)
    };
    // Длины и признак «каталог» — рядом со списком, но на стеке: пять килобайт, зато без ещё
    // одного куска ленивой кучи и без арифметики смещений в двух местах.
    let mut walk_len = [0usize; WALK_MAX];
    let mut walk_dir = [false; WALK_MAX];

    // Метаданные слотов файлов (кэш открытых) и таблица дескрипторов.
    let mut paths = [[0u8; PATH_MAX]; NFILES]; // абсолютный путь файла в слоте
    let mut path_len = [0usize; NFILES];
    let mut size = [0usize; NFILES];
    let mut fused = [false; NFILES];
    let mut dirty = [false; NFILES];
    let mut fd_file = [0usize; NFILES];
    let mut fd_off = [0usize; NFILES];
    let mut fd_used = [false; NFILES];
    // Слот файла ПАКЕТА: данные не копируются в `files` вовсе (libc.so.6 — 2.4 МБ), читается он
    // прямо из дерева store по узлу.
    let mut tnode = [Node { ty: 0, size: 0, id: [0u8; 32] }; NFILES];
    let mut is_tree = [false; NFILES];

    let mut req = [0u8; 512];
    // Ответ — размером с область в куче (Веха 108.2). Был 512 байт, и список из полусотни имён
    // обрывался на 510-м байте МОЛЧА: `ls` каталога с распакованным пакетом показывал первые 18
    // файлов из 57 и ничем не выдавал, что показал не всё (Веха 105 подняла его до индекса
    // каталога). С пакетами и этого мало: у glibc в `lib/gconv` 255 записей.
    let rep = repbuf;
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
                // Пакет неизменяем: под точкой монтирования любая запись — отказ (Веха 108.2).
                if under_mount(path).is_some() {
                    sys::reply(m.reply_cap, &rep[..1]);
                    continue;
                }
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
                // Файл ПАКЕТА (Веха 108.2): в слот кладётся узел дерева, а не содержимое —
                // копировать libc.so.6 в буфер на 128 КиБ и незачем, и некуда.
                if let Some(rel) = under_mount(path) {
                    let mut nfd = usize::MAX;
                    if let Some(node) = tree_find(store_cap, rel, ibuf, kids) {
                        if !tree::is_dir(node.ty) && !tree::is_link(node.ty) {
                            let free = (0..NFILES).find(|&j| !fused[j]).or_else(|| {
                                (0..NFILES).find(|&j| {
                                    !dirty[j] && !(0..NFILES).any(|d| fd_used[d] && fd_file[d] == j)
                                })
                            });
                            if let Some(j) = free {
                                fused[j] = true;
                                dirty[j] = false;
                                is_tree[j] = true;
                                tnode[j] = node;
                                size[j] = node.size as usize;
                                let n = pl.min(PATH_MAX);
                                path_len[j] = n;
                                paths[j][..n].copy_from_slice(&path[..n]);
                                if let Some(d) = (0..NFILES).find(|&d| !fd_used[d]) {
                                    nfd = d;
                                    fd_used[d] = true;
                                    fd_file[d] = j;
                                    fd_off[d] = 0;
                                }
                            }
                        }
                    }
                    rep[0] = if nfd == usize::MAX { 0xff } else { nfd as u8 };
                    sys::reply(m.reply_cap, &rep[..1]);
                    continue;
                }
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
                        // Слот занимается НАВСЕГДА, если его не вытеснять: `close` освобождает
                        // дескриптор, но не слот. До Вехи 105 это значило потолок в 16 РАЗНЫХ
                        // файлов за сессию — распаковка настоящего пакета спотыкалась на
                        // семнадцатом (и говорила лишь «не записался», без причины).
                        //
                        // Вытесняем чистый слот, который никем не открыт: его содержимое уже в
                        // store (там его оставил `close`), и следующий `open` прочитает файл
                        // обратно по корню `f<путь>`. Грязный не трогаем — это потеря данных.
                        let free = (0..NFILES).find(|&j| !fused[j]);
                        let victim = free.or_else(|| {
                            (0..NFILES).find(|&j| {
                                !dirty[j] && !(0..NFILES).any(|d| fd_used[d] && fd_file[d] == j)
                            })
                        });
                        if let Some(j) = victim {
                            fidx = j;
                            fused[j] = true;
                            dirty[j] = false;
                            is_tree[j] = false;
                            path_len[j] = pl;
                            paths[j][..pl].copy_from_slice(path);
                            let mut rn = [0u8; ROOT_MAX];
                            let rl = root_name(b'f', path, &mut rn);
                            if sys::obj_get_root(store_cap, &rn[..rl], &mut idb) == 32 {
                                let dbuf = &mut files[j * DATA_MAX..(j + 1) * DATA_MAX];
                                // Веха 114: ядро называет НАСТОЯЩУЮ длину объекта, и файл больше
                                // слота теперь виден. Раньше он молча приезжал обрезанным до
                                // 128 КиБ — то же самое семейство тихих усечений, что съедало
                                // хвост `.vv`-модулей. Отдавать половину файла нельзя: половина
                                // шрифта не шрифт, половина архива не архив.
                                let (got, whole) = sys::obj_get_ex(store_cap, &idb, dbuf);
                                if whole > dbuf.len() {
                                    sys::write_console("[posixfs] файл больше 128 КиБ — открыть нельзя: ".as_bytes());
                                    sys::write_console(path);
                                    sys::write_console(b"\n");
                                    fused[j] = false;
                                    fidx = usize::MAX;
                                } else {
                                    size[j] = got;
                                }
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
                if fd < NFILES && fd_used[fd] && !is_tree[fd_file[fd]] {
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
                    // Ёмкость ПРИЁМНИКА (Веха 112): её называет клиент, потому что знает её
                    // только он. Отдать больше нельзя — лишнее ядро отрежет по дороге, а курсор
                    // файла уедет на всю длину ответа, и хвост пропадёт молча. Пустой запрос —
                    // клиент старого образца: ведём себя как раньше.
                    let room = if len >= 4 {
                        u32::from_le_bytes([req[0], req[1], req[2], req[3]]) as usize
                    } else {
                        rep.len()
                    }
                    .min(rep.len());
                    if is_tree[fi] {
                        // Чтение файла пакета — прямо из дерева store; ответ не длиннее куска
                        // блоба (короткое чтение законно, клиент дочитает следующим вызовом).
                        let n = tree_read(store_cap, &tnode[fi], r, &mut rep[..room], ibuf, kids);
                        fd_off[fd] = r + n;
                        reply_len = n;
                    } else {
                        let n = size[fi].saturating_sub(r).min(room);
                        rep[..n].copy_from_slice(&files[fi * DATA_MAX + r..fi * DATA_MAX + r + n]);
                        fd_off[fd] = r + n;
                        reply_len = n;
                    }
                }
            }
            OP_STAT => {
                // req — путь. rep = [есть:1 | размер:4 LE | каталог:1 | тип записи:1].
                // Седьмой байт (Веха 108.2) несёт ТИП дерева пакета — по нему видно симлинк и
                // исполняемый бит; старые клиенты читают первые шесть и не замечают разницы.
                let pl = normalize(&req[..len], &mut pbuf);
                let path = &pbuf[..pl];
                let mut sz = usize::MAX;
                let mut is_dir = false;
                let mut ty = 0u8;
                if path == b"/nix" && read_index(store_cap, path, &mut dir, &mut idb).is_none() {
                    rep[0] = 1;
                    rep[1..5].copy_from_slice(&0u32.to_le_bytes());
                    rep[5] = 1;
                    rep[6] = tree::K_DIR;
                    sys::reply(m.reply_cap, &rep[..7]);
                    continue;
                }
                if let Some(rel) = under_mount(path) {
                    if rel.is_empty() {
                        // Сам /nix/store — каталог, который есть всегда: он состоит из корней.
                        is_dir = true;
                        sz = 0;
                        ty = tree::K_DIR;
                    } else if let Some(node) = tree_find(store_cap, rel, ibuf, kids) {
                        is_dir = tree::is_dir(node.ty);
                        sz = node.size as usize;
                        ty = node.ty;
                    }
                    rep[0] = (sz != usize::MAX) as u8;
                    let szv = if sz == usize::MAX { 0 } else { sz } as u32;
                    rep[1..5].copy_from_slice(&szv.to_le_bytes());
                    rep[5] = is_dir as u8;
                    rep[6] = ty;
                    sys::reply(m.reply_cap, &rep[..7]);
                    continue;
                }
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
                rep[6] = if is_dir { tree::K_DIR } else { tree::K_FILE };
                reply_len = 7;
            }
            OP_UNLINK => {
                // req — путь. Файл: снять корень `f<path>` + убрать из родителя. Каталог: только
                // пустой — снять `d<path>` + убрать из родителя. rep[0]: 0 ок / 1 ошибка.
                //
                // Веха 175 — режим 1 значит РЕКУРСИВНО: снять и всё, что внутри. Отдельным
                // режимом, а не отдельной операцией: спрашивают то же самое («убери вот это»),
                // разница только в согласии человека на потерю содержимого. И спрашивать его
                // обязан тот, кто разговаривает с человеком, — файловый менеджер или шелл.
                let pl = normalize(&req[..len], &mut pbuf);
                let path = &pbuf[..pl];
                rep[0] = 1;
                if under_mount(path).is_some() {
                    sys::reply(m.reply_cap, &rep[..1]);
                    continue;
                }
                if pl > 1 && mode == 1 && read_index(store_cap, path, &mut dir, &mut idb).is_some()
                {
                    // Рекурсивное удаление: обойти поддерево и снять корень каждой записи.
                    // Порядок не важен — пути собраны заранее, и родитель, снятый раньше
                    // ребёнка, ничего о нём не забывает: связь тут через ИМЯ корня, а не ссылку.
                    match subtree(store_cap, path, walk, &mut walk_len, &mut walk_dir) {
                        None => {
                            // Дерево не влезло в обход. Отказываем ЦЕЛИКОМ: половина удалённого
                            // каталога хуже, чем неудалённый.
                            sys::reply(m.reply_cap, &rep[..1]);
                            continue;
                        }
                        Some(n) => {
                            for i in 0..n {
                                let p = &walk[i][..walk_len[i]];
                                // Открытые слоты этого файла закрываем: иначе `close` вернул бы
                                // содержимое на корень, который мы только что сняли.
                                for k in 0..NFILES {
                                    if fused[k] && &paths[k][..path_len[k]] == p {
                                        fused[k] = false;
                                        for d in 0..NFILES {
                                            if fd_used[d] && fd_file[d] == k {
                                                fd_used[d] = false;
                                            }
                                        }
                                    }
                                }
                                let mut rn = [0u8; ROOT_MAX];
                                let rl = root_name(
                                    if walk_dir[i] { b'd' } else { b'f' },
                                    p,
                                    &mut rn,
                                );
                                sys::obj_del_root(store_cap, &rn[..rl]);
                            }
                            let par = parent(path);
                            let plen =
                                read_index(store_cap, par, &mut dir, &mut idb).unwrap_or(2);
                            let nlen = idx_remove(&mut dir, plen, leaf(path));
                            write_index(store_cap, par, &dir[..nlen], &mut idb);
                            rep[0] = 0;
                        }
                    }
                    sys::reply(m.reply_cap, &rep[..1]);
                    continue;
                }
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
                    if under_mount(&oldp[..onl]).is_some() || under_mount(&newp[..nnl]).is_some() {
                        sys::reply(m.reply_cap, &rep[..1]);
                        continue;
                    }
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

                    // Веха 175 — КАТАЛОГ переименовывается и переезжает. В store каталогов как
                    // объектов нет: путь входит в ИМЯ корня каждого потомка (`f<путь>`,
                    // `d<путь>`), поэтому «переименовать каталог» — это перевесить корень
                    // каждого потомка вглубь. Раньше файловый менеджер честно говорил, что не
                    // умеет; теперь умеет — тем же обходом, что и рекурсивное удаление.
                    if !same && onl > 1 && read_index(store_cap, old, &mut dir, &mut idb).is_some()
                    {
                        // Внутрь самого себя каталог не переезжает. Проверка обязательна: без неё
                        // обход получил бы дерево, растущее по мере обхода, и не кончился бы.
                        let inside = nnl > onl && newp[..onl] == oldp[..onl] && newp[onl] == b'/';
                        let mut rd = [0u8; ROOT_MAX];
                        let rf = root_name(b'd', new, &mut rd);
                        let taken_d = sys::obj_get_root(store_cap, &rd[..rf], &mut idb) == 32;
                        let rf = root_name(b'f', new, &mut rd);
                        let taken_f = sys::obj_get_root(store_cap, &rd[..rf], &mut idb) == 32;
                        if inside || taken_d || taken_f {
                            sys::reply(m.reply_cap, &rep[..1]); // rep[0] уже 0xff — отказ
                            continue;
                        }
                        let Some(n) = subtree(store_cap, old, walk, &mut walk_len, &mut walk_dir)
                        else {
                            // Дерево не влезло в обход. Отказываем ЦЕЛИКОМ: каталог, переехавший
                            // наполовину, — это потерянные данные под правдоподобным именем.
                            sys::reply(m.reply_cap, &rep[..1]);
                            continue;
                        };
                        for i in 0..n {
                            let plen = walk_len[i];
                            let mut src = [0u8; PATH_MAX];
                            src[..plen].copy_from_slice(&walk[i][..plen]);
                            // Новый путь: НОВЫЙ корень плюс хвост старого пути после старого.
                            let tail = &src[onl..plen];
                            let mut dst = [0u8; PATH_MAX];
                            if nnl + tail.len() > PATH_MAX {
                                continue; // не поместилось — оставляем как было, скажем отказом
                            }
                            dst[..nnl].copy_from_slice(new);
                            dst[nnl..nnl + tail.len()].copy_from_slice(tail);
                            let (dl, sp) = (nnl + tail.len(), &src[..plen]);
                            let dp = &dst[..dl];
                            let kind = if walk_dir[i] { b'd' } else { b'f' };
                            let mut ro = [0u8; ROOT_MAX];
                            let mut rn = [0u8; ROOT_MAX];
                            let rlo = root_name(kind, sp, &mut ro);
                            let rln = root_name(kind, dp, &mut rn);
                            if sys::obj_get_root(store_cap, &ro[..rlo], &mut idb) == 32 {
                                sys::obj_set_root(store_cap, &rn[..rln], &idb);
                                sys::obj_del_root(store_cap, &ro[..rlo]);
                            }
                            // Открытый файл под старым путём должен уехать вместе с ним: иначе
                            // `close` вернул бы его содержимое на корень, которого больше нет.
                            for k in 0..NFILES {
                                if fused[k] && &paths[k][..path_len[k]] == sp {
                                    paths[k][..dl].copy_from_slice(dp);
                                    path_len[k] = dl;
                                }
                            }
                        }
                        // Индексы родителей: из старого имя убрать, в новый добавить каталогом.
                        let pold = parent(old);
                        let plen = read_index(store_cap, pold, &mut dir, &mut idb).unwrap_or(2);
                        let nlen = idx_remove(&mut dir, plen, leaf(old));
                        write_index(store_cap, pold, &dir[..nlen], &mut idb);
                        let pnew = parent(new);
                        let plen = read_index(store_cap, pnew, &mut dir, &mut idb).unwrap_or(2);
                        let nlen = idx_add(&mut dir, plen, leaf(new), true);
                        write_index(store_cap, pnew, &dir[..nlen], &mut idb);
                        rep[0] = 0;
                        sys::reply(m.reply_cap, &rep[..1]);
                        continue;
                    }
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
            OP_READLINK => {
                // req — путь. Ответ: цель ссылки (пусто — не ссылка либо нет такой).
                // Симлинки есть только в дереве пакета: своих в персоналии по-прежнему нет.
                let pl = normalize(&req[..len], &mut pbuf);
                let path = &pbuf[..pl];
                if let Some(rel) = under_mount(path) {
                    if let Some(node) = tree_find(store_cap, rel, ibuf, kids) {
                        if tree::is_link(node.ty) {
                            reply_len = sys::obj_get(store_cap, &node.id, rep).min(rep.len());
                        }
                    }
                }
            }
            OP_READDIR => {
                // req — путь каталога (пусто/`.`/`/` → корень). Ответ: имена через '\n',
                // у каталогов — с хвостовым '/'.
                let pl = normalize(&req[..len], &mut pbuf);
                let path = &pbuf[..pl];
                // Дерево пакетов (Веха 108.2). Сам `/nix/store` — не индекс, а СПИСОК КОРНЕЙ
                // `pkg/tree/*`: пакет виден ровно тогда, когда он в сторе, и отдельного каталога
                // для этого заводить не нужно.
                if let Some(rel) = under_mount(path) {
                    if rel.is_empty() {
                        let (got, want) = sys::obj_list_roots_ex(store_cap, ibuf).unwrap_or((0, 0));
                        if want > got {
                            // Молча показать часть — ровно та беда, которую в этой системе ловят
                            // поимённо; лучше отдать пусто, чем «пакетов нет, наверное».
                            reply_len = 0;
                        } else {
                            let mut off = 0usize;
                            // Каждая строка списка: 12 hex короткого id + два пробела + имя.
                            for line in ibuf[..got].split(|&b| b == b'\n') {
                                if line.len() <= 14 {
                                    continue;
                                }
                                let Some(h) = line[14..].strip_prefix(TREE_ROOT) else { continue };
                                if h.len() < HASH_LEN {
                                    continue;
                                }
                                // Имя пакета лежит в его же корневом индексе — там оно полное.
                                let mut rn = [0u8; 64];
                                let rl = TREE_ROOT.len() + HASH_LEN;
                                rn[..TREE_ROOT.len()].copy_from_slice(TREE_ROOT);
                                rn[TREE_ROOT.len()..rl].copy_from_slice(&h[..HASH_LEN]);
                                let mut tid = [0u8; 32];
                                if sys::obj_get_root(store_cap, &rn[..rl], &mut tid) != 32 {
                                    continue;
                                }
                                let n = sys::obj_get(store_cap, &tid, scratch);
                                let Some(mut it) = tree::iter(&scratch[..n.min(scratch.len())])
                                else {
                                    continue;
                                };
                                let Some(r) = it.next() else { continue };
                                for &b in r.name {
                                    if off + 2 < rep.len() {
                                        rep[off] = b;
                                        off += 1;
                                    }
                                }
                                if r.is_dir() && off + 1 < rep.len() {
                                    rep[off] = b'/';
                                    off += 1;
                                }
                                if off < rep.len() {
                                    rep[off] = b'\n';
                                    off += 1;
                                }
                            }
                            reply_len = off;
                        }
                    } else if let Some(node) = tree_find(store_cap, rel, ibuf, kids) {
                        if tree::is_dir(node.ty) {
                            let n = sys::obj_get(store_cap, &node.id, ibuf);
                            if let Some(it) = tree::iter(&ibuf[..n.min(ibuf.len())]) {
                                let mut off = 0usize;
                                for r in it {
                                    for &b in r.name {
                                        if off + 2 < rep.len() {
                                            rep[off] = b;
                                            off += 1;
                                        }
                                    }
                                    if r.is_dir() && off + 1 < rep.len() {
                                        rep[off] = b'/';
                                        off += 1;
                                    }
                                    if off < rep.len() {
                                        rep[off] = b'\n';
                                        off += 1;
                                    }
                                }
                                reply_len = off;
                            }
                        }
                    }
                    sys::reply(m.reply_cap, &rep[..reply_len]);
                    continue;
                }
                // Родитель точки монтирования: `/nix` показывает `store/`, даже если своего
                // каталога `/nix` в персоналии нет.
                if path == b"/nix" && read_index(store_cap, path, &mut dir, &mut idb).is_none() {
                    rep[..6].copy_from_slice(b"store/");
                    rep[6] = b'\n';
                    sys::reply(m.reply_cap, &rep[..7]);
                    continue;
                }
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
                        // Границы считаются от буфера, а не зашиты числом: зашитые 510/511
                        // пережили увеличение `rep` бы молча, и обрыв остался бы на месте.
                        for k in 0..nl {
                            if reply_len + 2 < rep.len() {
                                rep[reply_len] = dir[off + 2 + k];
                                reply_len += 1;
                            }
                        }
                        if ty == 1 && reply_len + 1 < rep.len() {
                            rep[reply_len] = b'/';
                            reply_len += 1;
                        }
                        if reply_len < rep.len() {
                            rep[reply_len] = b'\n';
                            reply_len += 1;
                        }
                        off += 2 + nl;
                    }
                }
                // Веха 166 — КОРЕНЬ показывает `nix/`, даже если своего каталога `/nix` в
                // персоналии нет. Ровно тот же случай, что строкой выше у `/nix`: точка
                // монтирования существует всегда, а в индексе родителя её нет — её туда никто
                // не клал. На свежей системе (в персоналии не создано ещё ничего) из-за этого
                // корень выглядел ПУСТЫМ, хотя весь store лежал под ним: `ls /` не показывал
                // ничего, а `ls /nix/store` — десятки пакетов.
                //
                // Дописываем, только если индекс сам её не назвал: свой `/nix` человек создать
                // может, и второй строкой он бы задвоился.
                let named_nix =
                    rep[..reply_len].split(|&b| b == b'\n').any(|l| l == b"nix/" || l == b"nix");
                if (path == b"/" || path.is_empty()) && !named_nix && reply_len + 5 <= rep.len()
                {
                    rep[reply_len..reply_len + 5].copy_from_slice(b"nix/\n");
                    reply_len += 5;
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
