//! void-store-import — мост host→store (Веха 29): кладёт объекты и корни прямо в образ
//! диска VOID **тем же кодом**, что и ядро (`libs/void-store`); здесь только носитель
//! другой — файл-образ вместо virtio-blk. Это эмбрион void-pkg ([[0004-void-pkg]]):
//! путь «nix build → store» становится одной командой ещё до порта std.
//!
//! ```text
//! void-store-import <disk.img> ls                       # корни, поколение, объекты
//! void-store-import <disk.img> cat <корень>             # содержимое объекта корня → stdout
//! void-store-import <disk.img> put <файл> <корень>      # файл → объект + корень
//! void-store-import <disk.img> nar <архив.nar> <префикс># NAR → объект и корень на каждый файл
//! ```
//!
//! Импортированные ФАЙЛЫ заносятся и в индекс каталога posixfs (спец-корень `.dir`,
//! формат `count(1) | [nlen(1) | name]*`) — иначе `ls` в vsh их не покажет (open находит
//! корень и без индекса). Корни `bin/*` — программы, в каталог файлов не заносятся.
//!
//! ВАЖНО: не запускать, пока образ занят QEMU, — у store нет журнала совместного доступа.
//!
//! **Раздел ищется сам (Веха 132).** До неё мост писал store всегда с НУЛЕВОГО сектора — это
//! верно для «сырого» образа store (каким его подключают вторым диском), но ЗАГРУЗОЧНЫЙ образ
//! (`boot/mkdisk.sh`) устроен иначе: там store лежит вторым разделом, а в начале — загрузочная
//! запись и раздел с ядром. Мост их молча затирал и рапортовал успех; образ после этого не
//! грузился вовсе, а на настоящем носителе это было бы уничтожением данных без вопроса.
//! Теперь: есть MBR с разделом типа 0x9f (store VOID) — работаем внутри него; нет — как раньше,
//! с нуля.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};

use void_abi::ContentId;
use void_store::{BlockIo, Store, SECTOR};

// Пределы posixfs (см. programs/user/src/bin/posixfs.rs) — длиннее имя не откроется,
// больше DATA_MAX каталог не вырастет.
const DIR_ROOT: &str = ".dir";
const DIR_MAX: usize = 4096;
const DIR_NAME_MAX: usize = 32;

// ─── носитель: файл-образ ─────────────────────────────────────────────────────

struct FileIo {
    file: std::fs::File,
    /// Сектор, с которого начинается store. 0 — «сырой» образ; иначе начало раздела 0x9f.
    base: u64,
    /// Длина store в секторах (от `base`).
    sectors: u64,
}

/// Тип раздела store VOID в таблице MBR (его же ставит `boot/mkdisk.sh`).
const PART_TYPE_VOID: u8 = 0x9f;

impl FileIo {
    fn open(path: &str) -> Result<Self, String> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("не открыть образ {path}: {e} (создаётся: truncate -s 16M {path})"))?;
        let len = file.metadata().map_err(|e| format!("{path}: {e}"))?.len();
        if len == 0 || len % SECTOR as u64 != 0 {
            return Err(format!("{path}: длина {len} не кратна сектору {SECTOR}"));
        }
        let total = len / SECTOR as u64;

        // Заголовок: ищем таблицу MBR и в ней раздел store.
        let mut mbr = [0u8; SECTOR];
        file.seek(SeekFrom::Start(0)).map_err(|e| format!("{path}: {e}"))?;
        file.read_exact(&mut mbr).map_err(|e| format!("{path}: {e}"))?;
        let (base, sectors) = match find_void_partition(&mbr) {
            Some((start, count)) => {
                if start + count > total {
                    return Err(format!(
                        "{path}: раздел store (сектора {start}..{}) выходит за край образа",
                        start + count
                    ));
                }
                eprintln!("  (раздел store VOID: сектор {start}, {count} секторов)");
                (start, count)
            }
            None => {
                // Подпись MBR есть, а нашего раздела нет — это чужой размеченный носитель.
                // Писать в него с нуля значит стереть чужую таблицу разделов молча.
                if mbr[510] == 0x55 && mbr[511] == 0xaa {
                    return Err(format!(
                        "{path}: размечен, но раздела store VOID (тип {PART_TYPE_VOID:#x}) в нём нет — \
                         отказываюсь писать с нулевого сектора"
                    ));
                }
                (0, total)
            }
        };
        Ok(FileIo { file, base, sectors })
    }
}

/// Найти в таблице MBR раздел store VOID: возвращает (первый сектор, число секторов).
/// Четыре записи по 16 байт с 0x1BE; тип — байт 4, LBA-начало и длина — по 4 байта с 8 и 12.
fn find_void_partition(mbr: &[u8; SECTOR]) -> Option<(u64, u64)> {
    if mbr[510] != 0x55 || mbr[511] != 0xaa {
        return None; // подписи нет — образ не размечен
    }
    for i in 0..4 {
        let e = 0x1be + i * 16;
        if mbr[e + 4] != PART_TYPE_VOID {
            continue;
        }
        let start = u32::from_le_bytes([mbr[e + 8], mbr[e + 9], mbr[e + 10], mbr[e + 11]]) as u64;
        let count = u32::from_le_bytes([mbr[e + 12], mbr[e + 13], mbr[e + 14], mbr[e + 15]]) as u64;
        if start != 0 && count != 0 {
            return Some((start, count));
        }
    }
    None
}

impl BlockIo for FileIo {
    fn read(&mut self, sector: u64, buf: &mut [u8; SECTOR]) -> bool {
        sector < self.sectors
            && self.file.seek(SeekFrom::Start((self.base + sector) * SECTOR as u64)).is_ok()
            && self.file.read_exact(buf).is_ok()
    }
    fn write(&mut self, sector: u64, buf: &[u8; SECTOR]) -> bool {
        sector < self.sectors // за край РАЗДЕЛА не пишем — честный «диск кончился»
            && self.file.seek(SeekFrom::Start((self.base + sector) * SECTOR as u64)).is_ok()
            && self.file.write_all(buf).is_ok()
    }
    /// Веха 89 — ёмкость образа: store теперь считает место ДО записи и не начинает коммит,
    /// который не влезет (раньше упирались в отказ `write` уже посреди записи).
    fn capacity(&mut self) -> u64 {
        self.sectors
    }
}

// ─── NAR: разбор вынесен в общий крейт (Веха 105) ─────────────────────────────
// Формат живёт в `libs/void-nar` — по той же причине, что и сам store: одна реализация на
// всех потребителей. Мост здесь только СОБИРАЕТ результат обхода: на хосте держать дерево
// в памяти безобидно, а на VOID тот же обход кладёт файлы в store по одному.

/// Файл из NAR: путь внутри архива, содержимое, исполняемость.
struct NarFile {
    path: String,
    data: Vec<u8>,
    exec: bool,
}

fn parse_nar(bytes: &[u8]) -> Result<Vec<NarFile>, String> {
    let mut out = Vec::new();
    void_nar::walk(bytes, |e| {
        match e {
            void_nar::Entry::File { path, data, exec } => out.push(NarFile {
                path: path.to_owned(),
                data: data.to_vec(),
                exec,
            }),
            void_nar::Entry::Symlink { path, target } => {
                eprintln!("  ! симлинк {path} → {target} пропущен (симлинков в store пока нет)");
            }
            void_nar::Entry::Dir { .. } => {}
        }
        Ok(())
    })
    .map_err(|e| e.0)?;
    Ok(out)
}

// ─── индекс каталога posixfs ──────────────────────────────────────────────────

/// Поднять `.dir` из store (или пустой: count=0).
fn dir_load(store: &mut Store, io: &mut FileIo) -> Vec<u8> {
    let mut dir = vec![0u8];
    if let Some(id) = store.root(DIR_ROOT) {
        store.with(io, &id, |p| {
            if let Some(p) = p {
                if !p.is_empty() {
                    dir = p.to_vec();
                }
            }
        });
    }
    dir
}

/// Занести имя файла в индекс каталога. `false` — уже есть / не файл / не влезло.
fn dir_add(dir: &mut Vec<u8>, name: &str) -> bool {
    if name.starts_with("bin/") || name == DIR_ROOT {
        return false; // программы и спец-корни — не файлы каталога
    }
    let n = name.as_bytes();
    let cnt = dir[0] as usize;
    let mut off = 1;
    for _ in 0..cnt {
        let l = dir[off] as usize;
        if &dir[off + 1..off + 1 + l] == n {
            return false;
        }
        off += 1 + l;
    }
    if n.len() > DIR_NAME_MAX {
        eprintln!("  ! {name}: имя длиннее {DIR_NAME_MAX} — posixfs не откроет, в каталог не заношу");
        return false;
    }
    if dir.len() + 1 + n.len() > DIR_MAX || dir[0] == u8::MAX {
        eprintln!("  ! индекс каталога полон — {name} не занесён");
        return false;
    }
    dir.push(n.len() as u8);
    dir.extend_from_slice(n);
    dir[0] += 1;
    true
}

/// Убрать имя из индекса каталога (сдвиг хвоста). `false` — имени не было.
fn dir_remove(dir: &mut Vec<u8>, name: &[u8]) -> bool {
    let cnt = dir[0] as usize;
    let mut off = 1;
    for _ in 0..cnt {
        let l = dir[off] as usize;
        if &dir[off + 1..off + 1 + l] == name {
            dir.drain(off..off + 1 + l);
            dir[0] -= 1;
            return true;
        }
        off += 1 + l;
    }
    false
}

// ─── команды ──────────────────────────────────────────────────────────────────

fn hex12(id: &ContentId) -> String {
    id.0[..6].iter().map(|b| format!("{b:02x}")).collect()
}

fn cmd_ls(store: &mut Store, io: &mut FileIo) {
    // Не println!: при `| head` труба закрывается, а падать из-за этого не надо.
    let mut out = std::io::stdout().lock();
    if writeln!(out, "поколение {} · объектов {}", store.generation(), store.len()).is_err() {
        return;
    }
    let roots: Vec<(String, ContentId)> =
        store.roots().map(|(n, id)| (n.to_owned(), *id)).collect();
    for (name, id) in roots {
        let size = store.with(io, &id, |p| p.map(|p| p.len()));
        let line = match size {
            Some(s) => format!("  {}  {:>8} Б  {}", hex12(&id), s, name),
            None => format!("  {}  {:>8}    {} (объекта нет!)", hex12(&id), "?", name),
        };
        if writeln!(out, "{line}").is_err() {
            return;
        }
    }
}

fn cmd_cat(store: &mut Store, io: &mut FileIo, name: &str) -> Result<(), String> {
    let id = store.root(name).ok_or_else(|| format!("нет корня «{name}»"))?;
    store.with(io, &id, |p| match p {
        Some(p) => match std::io::stdout().write_all(p) {
            Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => Err(format!("stdout: {e}")),
            _ => Ok(()), // закрытая труба (`| head`) — не ошибка
        },
        None => Err(format!("корень «{name}» указывает на отсутствующий объект")),
    })
}

fn cmd_put(store: &mut Store, io: &mut FileIo, file: &str, root: &str) -> Result<(), String> {
    let data = std::fs::read(file).map_err(|e| format!("не прочитать {file}: {e}"))?;
    let id = store.put(&data);
    store.set_root(root, id);
    let mut dir = dir_load(store, io);
    if dir_add(&mut dir, root) {
        let did = store.put(&dir);
        store.set_root(DIR_ROOT, did);
    }
    store.commit(io);
    println!("  {root} ← {file} ({} Б) · id {} · поколение {}", data.len(), hex12(&id), store.generation());
    Ok(())
}

fn cmd_nar(store: &mut Store, io: &mut FileIo, archive: &str, prefix: &str) -> Result<(), String> {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return Err("пустой префикс корней".into());
    }
    let bytes = std::fs::read(archive).map_err(|e| format!("не прочитать {archive}: {e}"))?;
    let files = parse_nar(&bytes)?;
    if files.is_empty() {
        return Err("в NAR нет ни одного обычного файла".into());
    }
    let mut dir = dir_load(store, io);
    let mut dir_changed = false;
    for f in &files {
        let root = if f.path.is_empty() { prefix.to_owned() } else { format!("{prefix}/{}", f.path) };
        let id = store.put(&f.data);
        store.set_root(&root, id);
        dir_changed |= dir_add(&mut dir, &root);
        println!("  {root}{} ({} Б) · id {}", if f.exec { " *" } else { "" }, f.data.len(), hex12(&id));
    }
    if dir_changed {
        let did = store.put(&dir);
        store.set_root(DIR_ROOT, did);
    }
    store.commit(io);
    println!("  итого файлов: {} · поколение {}", files.len(), store.generation());
    Ok(())
}

const USAGE: &str = "мост host→store: импорт в образ диска VOID (не запускать при работающем QEMU!)

  void-store-import <disk.img> ls                        корни, поколение, объекты
  void-store-import <disk.img> cat <корень>              содержимое объекта → stdout
  void-store-import <disk.img> put <файл> <корень>       файл → объект + корень
  void-store-import <disk.img> nar <архив.nar> <префикс> NAR → корень на каждый файл
  void-store-import <disk.img> del <корень>              снять корень (объект уйдёт в gc)
  void-store-import <disk.img> gc [--compact]            уборка: mark-sweep; уплотнение
                                                         по порогу мусора (или форсом)

NAR делается nix'ом: nix-store --dump <путь> > a.nar (работает на любом пути,
в т.ч. $(nix-build ...) — это и есть поток «nix build → VOID»).";

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let (img, cmd) = match (args.get(1), args.get(2)) {
        (Some(i), Some(c)) => (i.as_str(), c.as_str()),
        _ => return Err(format!("нужны образ и команда\n\n{USAGE}")),
    };
    let mut io = FileIo::open(img)?;
    let mut store = Store::new();
    if !store.load(&mut io) {
        match cmd {
            // Писать можно и в чистый образ: первый commit инициализирует store,
            // ядро при загрузке увидит готовые данные (и досеет свои программы).
            "put" | "nar" => println!("  (образ пуст — инициализирую store)"),
            _ => {
                println!("(пустой образ — store не инициализирован)");
                return Ok(());
            }
        }
    }
    match (cmd, args.get(3), args.get(4)) {
        ("ls", None, None) => {
            cmd_ls(&mut store, &mut io);
            Ok(())
        }
        ("cat", Some(root), None) => cmd_cat(&mut store, &mut io, root),
        ("put", Some(file), Some(root)) => cmd_put(&mut store, &mut io, file, root),
        ("nar", Some(archive), Some(prefix)) => cmd_nar(&mut store, &mut io, archive, prefix),
        ("del", Some(root), None) => cmd_del(&mut store, &mut io, root),
        ("gc", None, None) => cmd_gc(&mut store, &mut io, false),
        ("gc", Some(flag), None) if flag == "--compact" => cmd_gc(&mut store, &mut io, true),
        _ => Err(format!("неверные аргументы\n\n{USAGE}")),
    }
}

/// Снять корень: объект становится недостижимым, уберёт его `gc`.
/// Поддерживает и запись каталога posixfs (как put — наоборот).
fn cmd_del(store: &mut Store, io: &mut FileIo, root: &str) -> Result<(), String> {
    if !store.del_root(root) {
        return Err(format!("корня '{root}' нет"));
    }
    let mut dir = dir_load(store, io);
    dir_remove(&mut dir, root.as_bytes());
    let id = store.put(&dir);
    store.set_root(DIR_ROOT, id);
    store.commit(io);
    println!("  корень '{root}' снят (объект уйдёт ближайшим gc) · поколение {}", store.generation());
    Ok(())
}

/// Уборка образа: mark-sweep + уплотнение по порогу мусора (Веха 33) или форсом.
fn cmd_gc(store: &mut Store, io: &mut FileIo, force_compact: bool) -> Result<(), String> {
    let (kept, collected) = store.gc(io);
    let (garbage, area) = (store.garbage_bytes(), store.area_bytes());
    println!(
        "  gc: живых {kept}, собрано {collected} · мусора {} КиБ из {} КиБ",
        garbage / 1024,
        area / 1024,
    );
    if force_compact || (garbage > 0 && garbage * 2 > area) {
        store.compact(io);
        println!(
            "  уплотнено (двухфазно): область {} КиБ, поколение {}",
            store.area_bytes() / 1024,
            store.generation(),
        );
    } else {
        store.commit(io);
        println!("  порог уплотнения (1/2) не достигнут — надгробия зафиксированы");
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("void-store-import: {e}");
        std::process::exit(1);
    }
}
