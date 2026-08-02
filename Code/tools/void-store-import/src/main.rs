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
    sectors: u64,
}

impl FileIo {
    fn open(path: &str) -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("не открыть образ {path}: {e} (создаётся: truncate -s 16M {path})"))?;
        let len = file.metadata().map_err(|e| format!("{path}: {e}"))?.len();
        if len == 0 || len % SECTOR as u64 != 0 {
            return Err(format!("{path}: длина {len} не кратна сектору {SECTOR}"));
        }
        Ok(FileIo { file, sectors: len / SECTOR as u64 })
    }
}

impl BlockIo for FileIo {
    fn read(&mut self, sector: u64, buf: &mut [u8; SECTOR]) -> bool {
        sector < self.sectors
            && self.file.seek(SeekFrom::Start(sector * SECTOR as u64)).is_ok()
            && self.file.read_exact(buf).is_ok()
    }
    fn write(&mut self, sector: u64, buf: &[u8; SECTOR]) -> bool {
        sector < self.sectors // за край образа не пишем — честный «диск кончился»
            && self.file.seek(SeekFrom::Start(sector * SECTOR as u64)).is_ok()
            && self.file.write_all(buf).is_ok()
    }
    /// Веха 89 — ёмкость образа: store теперь считает место ДО записи и не начинает коммит,
    /// который не влезет (раньше упирались в отказ `write` уже посреди записи).
    fn capacity(&mut self) -> u64 {
        self.sectors
    }
}

// ─── NAR: детерминированная сериализация дерева из nix ────────────────────────
// Токен = длина(u64 LE) + байты + выравнивание нулями до 8. Грамматика узла:
//   "(" "type" ( "regular" ["executable" ""] "contents" <данные>
//              | "symlink" "target" <цель>
//              | "directory" { "entry" "(" "name" <имя> "node" <узел> ")" } ) ")"

struct Nar<'a> {
    b: &'a [u8],
    off: usize,
}

impl<'a> Nar<'a> {
    fn bytes(&mut self) -> Result<&'a [u8], String> {
        if self.off + 8 > self.b.len() {
            return Err("NAR обрезан (нет длины токена)".into());
        }
        let len = u64::from_le_bytes(self.b[self.off..self.off + 8].try_into().unwrap()) as usize;
        self.off += 8;
        if self.off + len > self.b.len() {
            return Err("NAR обрезан (нет тела токена)".into());
        }
        let s = &self.b[self.off..self.off + len];
        self.off += len + (8 - len % 8) % 8;
        Ok(s)
    }
    fn tok(&mut self) -> Result<&'a str, String> {
        std::str::from_utf8(self.bytes()?).map_err(|_| "NAR: токен не UTF-8".into())
    }
    fn expect(&mut self, want: &str) -> Result<(), String> {
        let got = self.tok()?;
        if got == want { Ok(()) } else { Err(format!("NAR: ожидал «{want}», увидел «{got}»")) }
    }
}

/// Файл из NAR: путь внутри архива, содержимое, исполняемость.
struct NarFile {
    path: String,
    data: Vec<u8>,
    exec: bool,
}

fn nar_walk(nar: &mut Nar, path: String, out: &mut Vec<NarFile>) -> Result<(), String> {
    nar.expect("(")?;
    nar.expect("type")?;
    match nar.tok()? {
        "regular" => {
            let mut exec = false;
            let mut t = nar.tok()?;
            if t == "executable" {
                nar.expect("")?;
                exec = true;
                t = nar.tok()?;
            }
            if t != "contents" {
                return Err(format!("NAR: ожидал «contents», увидел «{t}»"));
            }
            let data = nar.bytes()?.to_vec();
            nar.expect(")")?;
            out.push(NarFile { path, data, exec });
        }
        "symlink" => {
            nar.expect("target")?;
            let target = nar.tok()?;
            eprintln!("  ! симлинк {path} → {target} пропущен (симлинков в store пока нет)");
            nar.expect(")")?;
        }
        "directory" => loop {
            match nar.tok()? {
                ")" => break,
                "entry" => {
                    nar.expect("(")?;
                    nar.expect("name")?;
                    let name = nar.tok()?.to_owned();
                    nar.expect("node")?;
                    let sub = if path.is_empty() { name } else { format!("{path}/{name}") };
                    nar_walk(nar, sub, out)?;
                    nar.expect(")")?;
                }
                t => return Err(format!("NAR: неожиданный токен «{t}» в каталоге")),
            }
        },
        t => return Err(format!("NAR: неизвестный тип узла «{t}»")),
    }
    Ok(())
}

fn parse_nar(bytes: &[u8]) -> Result<Vec<NarFile>, String> {
    let mut nar = Nar { b: bytes, off: 0 };
    nar.expect("nix-archive-1")?;
    let mut out = Vec::new();
    nar_walk(&mut nar, String::new(), &mut out)?;
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
