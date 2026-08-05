//! Архивы бинарного кэша nixpkgs: достать из store и распаковать (Вехи 105, 106).
//!
//! Вынесено из `bin/vvsh.rs`, когда за теми же байтами пришёл второй потребитель — программа
//! `pkg`. Копировать распаковку в два бинаря нельзя по той же причине, по которой в своё время
//! вынесли формат store и формат NAR: **одна реализация формата на всех потребителей**, иначе
//! расхождение двух копий обнаруживают последним и по самым странным симптомам.
//!
//! **Подключается модулем ПО ПУТИ** (`#[path = "../archive.rs"] mod archive;`), а не через
//! библиотеку `void_user`, и причина техническая: здесь нужен `alloc`, а объявить его в
//! библиотеке значит потребовать глобальный аллокатор от ВСЕХ двух десятков программ, включая
//! те, что живут вообще без кучи (`posixfs`, `net-srv`, драйверы). Файл при этом один — и это
//! всё, что от «общего кода» требовалось.
//!
//! Архив приходит из store тремя видами: обычным объектом, блобом из кусков (так кладут `fetch`
//! и `httpsc`) и любым из них — сжатым. Сжатие определяется по СОДЕРЖИМОМУ, а не по имени корня:
//! имя в store — произвольная строка, и верить ей значило бы верить тому, кто корень завёл.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use void_user as sys;

/// Опознавательный знак `.xz` — первые шесть байт контейнера (spec, sect. 2.1.1.1).
pub const XZ_MAGIC: &[u8] = &[0xFD, b'7', b'z', b'X', b'Z', 0x00];

/// Опознавательный знак `.zst` — магия кадра zstd 0xFD2FB528 (RFC 8878, LE).
/// Второе сжатие кэша nixpkgs, и с некоторых пор ОСНОВНОЕ: замер по случайной выборке путей
/// (2026-08-04) дал 15 zstd против 7 xz.
pub const ZSTD_MAGIC: &[u8] = &[0x28, 0xB5, 0x2F, 0xFD];

/// Сколько байт хватает, чтобы опознать сжатие (обе магии короче).
const MAGIC_PROBE: usize = 6;

/// Чем сжат архив.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Packing {
    None,
    Xz,
    Zstd,
}

/// Опознать сжатие по началу данных.
pub fn packing_of(head: &[u8]) -> Packing {
    if head.starts_with(XZ_MAGIC) {
        Packing::Xz
    } else if head.starts_with(ZSTD_MAGIC) {
        Packing::Zstd
    } else {
        Packing::None
    }
}

/// Читатель поверх БЛОБА store (манифест + куски-дети).
///
/// Нужен ради одного свойства: сжатый архив не материализуется в куче целиком. Распаковщик тянет
/// байты кусок за куском, и в памяти живёт ровно один кусок. Для пакета в десятки мегабайт это
/// разница между «работает» и «не влезло».
pub struct BlobReader {
    scap: usize,
    kids: Vec<[u8; 32]>,
    next: usize,
    buf: Vec<u8>,
    pos: usize,
    filled: usize,
}

impl BlobReader {
    pub fn new(scap: usize, kids: Vec<[u8; 32]>, csize: usize) -> Self {
        BlobReader {
            scap,
            kids,
            next: 0,
            buf: vec![0u8; csize],
            pos: 0,
            filled: 0,
        }
    }
}

impl lzma_rs::io::Read for BlobReader {
    fn read(&mut self, out: &mut [u8]) -> lzma_rs::io::Result<usize> {
        let n = {
            let src = lzma_rs::io::BufRead::fill_buf(self)?;
            let n = src.len().min(out.len());
            out[..n].copy_from_slice(&src[..n]);
            n
        };
        lzma_rs::io::BufRead::consume(self, n);
        Ok(n)
    }
}

impl lzma_rs::io::BufRead for BlobReader {
    fn fill_buf(&mut self) -> lzma_rs::io::Result<&[u8]> {
        if self.pos == self.filled {
            if self.next >= self.kids.len() {
                return Ok(&[]);
            }
            let n = sys::obj_get(self.scap, &self.kids[self.next], &mut self.buf);
            if n == 0 || n > self.buf.len() {
                return Err(lzma_rs::io::Error::new(
                    lzma_rs::io::ErrorKind::InvalidData,
                    "кусок блоба не читается",
                ));
            }
            self.next += 1;
            self.pos = 0;
            self.filled = n;
        }
        Ok(&self.buf[self.pos..self.filled])
    }

    fn consume(&mut self, amt: usize) {
        self.pos = (self.pos + amt).min(self.filled);
    }
}

/// Читатель по слайсу для `ruzstd`: у него свой трейт `Read`, и чужие реализации в него,
/// разумеется, не считаются. Обёртки существуют ровно потому, что общего `std::io` в no_std-мире
/// нет и взяться ему неоткуда — каждый распаковщик описывает ввод по-своему.
pub struct SliceReader<'a>(pub &'a [u8]);

impl ruzstd::io::Read for SliceReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> Result<usize, ruzstd::io::Error> {
        let n = out.len().min(self.0.len());
        out[..n].copy_from_slice(&self.0[..n]);
        self.0 = &self.0[n..];
        Ok(n)
    }
}

impl lzma_rs::io::Read for SliceReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> lzma_rs::io::Result<usize> {
        let n = out.len().min(self.0.len());
        out[..n].copy_from_slice(&self.0[..n]);
        self.0 = &self.0[n..];
        Ok(n)
    }
}

impl lzma_rs::io::BufRead for SliceReader<'_> {
    fn fill_buf(&mut self) -> lzma_rs::io::Result<&[u8]> {
        Ok(self.0)
    }

    fn consume(&mut self, amt: usize) {
        self.0 = &self.0[amt.min(self.0.len())..];
    }
}

impl ruzstd::io::Read for BlobReader {
    fn read(&mut self, out: &mut [u8]) -> Result<usize, ruzstd::io::Error> {
        lzma_rs::io::Read::read(self, out)
            .map_err(|_| ruzstd::io::Error::from(ruzstd::io::ErrorKind::Other))
    }
}

/// Сток распакованных байт для [`stream`]: сюда они уходят кусками и нигде не копятся.
///
/// Своего трейта здесь ровно потому, что оба распаковщика описывают вывод по-разному (`lzma-rs`
/// пишет в свой `Write`, `ruzstd` даёт `Read`), а потребителю нужен один вид.
pub trait Sink {
    fn put(&mut self, data: &[u8]) -> Result<(), String>;
}

impl<F: FnMut(&[u8]) -> Result<(), String>> Sink for F {
    fn put(&mut self, data: &[u8]) -> Result<(), String> {
        self(data)
    }
}

/// Мостик «сток → `lzma_rs::io::Write`»: xz умеет только ТОЛКАТЬ вывод, и это единственная
/// причина, по которой у распаковки вообще есть подающий интерфейс (см. шапку `void_nar`).
struct SinkWriter<'a, S: Sink> {
    sink: &'a mut S,
    err: Option<String>,
}

impl<S: Sink> lzma_rs::io::Write for SinkWriter<'_, S> {
    fn write(&mut self, buf: &[u8]) -> lzma_rs::io::Result<usize> {
        match self.sink.put(buf) {
            Ok(()) => Ok(buf.len()),
            Err(e) => {
                // Чужой ошибке негде проехать через io::Error — придерживаем текст у себя и
                // отдаём его вызывающему, иначе «store не принял кусок» стало бы «ошибка xz».
                self.err = Some(e);
                Err(lzma_rs::io::Error::new(
                    lzma_rs::io::ErrorKind::Other,
                    "потребитель распаковки отказал",
                ))
            }
        }
    }
}

/// Размер порции, которой распакованные байты уходят в сток.
const STREAM_CHUNK: usize = 64 * 1024;

/// Прогнать содержимое объекта (или блоба) через распаковку, **отдавая байты кусками**.
///
/// Веха 108. Отличие от [`unpacked`] одно, но решающее: результат нигде не собирается целиком.
/// NAR настоящего пакета — десятки мегабайт (у glibc 35), и «сначала распакуем, потом разберём»
/// упиралось не в удобство, а в кучу процесса.
pub fn stream<S: Sink>(scap: usize, id: &[u8; 32], sink: &mut S) -> Result<(), String> {
    let mut head = [0u8; 512];
    let hlen = sys::obj_get(scap, id, &mut head);
    if hlen == 0 || hlen == usize::MAX {
        return Err(String::from("объект не читается"));
    }

    if let Some((_total, nchunks, csize)) = sys::http::blob_info(&head[..hlen]) {
        let mut kids = vec![[0u8; 32]; nchunks];
        if sys::obj_children(scap, id, &mut kids) != nchunks {
            return Err(String::from("список кусков блоба не сошёлся"));
        }
        let mut probe = vec![0u8; MAGIC_PROBE];
        let plen = sys::obj_get(scap, &kids[0], &mut probe);
        let packing = if plen <= MAGIC_PROBE { packing_of(&probe[..plen]) } else { Packing::None };
        let blob = BlobReader::new(scap, kids, csize);
        return unpack_reader(blob, packing, sink);
    }

    let buf = read_object(scap, id, 8 * 1024 * 1024)?;
    let packing = packing_of(&buf);
    unpack_reader(SliceReader(buf.as_slice()), packing, sink)
}

/// Общая часть [`stream`]: источник уже выбран, осталось развернуть его сжатие.
fn unpack_reader<R, S>(mut input: R, packing: Packing, sink: &mut S) -> Result<(), String>
where
    R: lzma_rs::io::BufRead + ruzstd::io::Read,
    S: Sink,
{
    match packing {
        Packing::Xz => {
            let mut w = SinkWriter { sink, err: None };
            let r = lzma_rs::xz_decompress(&mut input, &mut w);
            if let Some(e) = w.err.take() {
                return Err(e); // беда потребителя важнее нашей обёртки над ней
            }
            r.map_err(|e| alloc::format!("xz: {}", e))
        }
        Packing::Zstd => {
            let mut dec = ruzstd::decoding::StreamingDecoder::new(input)
                .map_err(|e| alloc::format!("zstd: {}", e))?;
            let mut buf = vec![0u8; STREAM_CHUNK];
            loop {
                let n = ruzstd::io::Read::read(&mut dec, &mut buf)
                    .map_err(|e| alloc::format!("zstd: {}", e))?;
                if n == 0 {
                    return Ok(());
                }
                sink.put(&buf[..n])?;
            }
        }
        Packing::None => {
            let mut buf = vec![0u8; STREAM_CHUNK];
            loop {
                let n = lzma_rs::io::Read::read(&mut input, &mut buf)
                    .map_err(|e| alloc::format!("{}", e))?;
                if n == 0 {
                    return Ok(());
                }
                sink.put(&buf[..n])?;
            }
        }
    }
}

/// Прочитать объект store целиком, не зная заранее его длины.
///
/// `SYS_OBJ_GET` отдаёт `min(длина, размер буфера)` и НЕ сообщает настоящий размер — то самое
/// молчаливое обрезание, которое в этой системе ловят поимённо. Пока ядро не научится называть
/// длину, обходим удвоением: если ответ ровно с буфер, объект мог и не поместиться — просим
/// вдвое больше и переспрашиваем.
pub fn read_object(scap: usize, id: &[u8; 32], limit: usize) -> Result<Vec<u8>, String> {
    let mut cap = 256 * 1024;
    loop {
        let mut buf = vec![0u8; cap];
        let n = sys::obj_get(scap, id, &mut buf);
        if n == 0 || n == usize::MAX {
            return Err(String::from("объект не читается"));
        }
        if n < buf.len() {
            buf.truncate(n);
            return Ok(buf);
        }
        if cap >= limit {
            return Err(alloc::format!(
                "объект больше {} МиБ — такие приезжают блобом (кусками)",
                limit / (1024 * 1024)
            ));
        }
        cap = (cap * 2).min(limit);
    }
}

/// Достать содержимое объекта (или блоба) и распаковать, если оно сжато.
///
/// `verbose` — печатать ли строку «сколько во что распаковалось»: человеку у шелла она нужна,
/// а внутри `pkg`, где распаковок несколько, — только мешала бы.
pub fn unpacked(scap: usize, id: &[u8; 32], verbose: bool) -> Result<Vec<u8>, String> {
    // Манифест блоба короткий; обычный объект в эти же 512 байт либо влезет целиком, либо
    // просто не опознается как блоб — и тогда читается обычным путём.
    let mut head = [0u8; 512];
    let hlen = sys::obj_get(scap, id, &mut head);
    if hlen == 0 || hlen == usize::MAX {
        return Err(String::from("объект не читается"));
    }

    if let Some((total, nchunks, csize)) = sys::http::blob_info(&head[..hlen]) {
        let mut kids = vec![[0u8; 32]; nchunks];
        if sys::obj_children(scap, id, &mut kids) != nchunks {
            return Err(String::from("список кусков блоба не сошёлся"));
        }
        // Первый кусок нужен дважды — опознать сжатие и потом распаковать, — поэтому читаем
        // блоб с начала в обоих случаях, а не запоминаем прочитанное.
        let mut probe = vec![0u8; MAGIC_PROBE];
        let plen = sys::obj_get(scap, &kids[0], &mut probe);
        let packing = if plen <= MAGIC_PROBE {
            packing_of(&probe[..plen])
        } else {
            Packing::None
        };
        let mut blob = BlobReader::new(scap, kids, csize);
        match packing {
            Packing::Xz => return unxz(&mut blob, total, verbose),
            Packing::Zstd => return unzstd(blob, total, verbose),
            Packing::None => {}
        }
        let mut out = Vec::with_capacity(total);
        let mut chunk = vec![0u8; csize];
        loop {
            let n = lzma_rs::io::Read::read(&mut blob, &mut chunk)
                .map_err(|e| alloc::format!("{}", e))?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..n]);
        }
        return Ok(out);
    }

    let buf = read_object(scap, id, 8 * 1024 * 1024)?;
    let packed = buf.len();
    match packing_of(&buf) {
        Packing::Xz => unxz(&mut buf.as_slice(), packed, verbose),
        Packing::Zstd => unzstd(SliceReader(buf.as_slice()), packed, verbose),
        Packing::None => Ok(buf),
    }
}

/// Распаковать `.xz` (Веха 105) — часть путей кэша до сих пор приезжает так.
pub fn unxz<R: lzma_rs::io::BufRead>(
    input: &mut R,
    packed: usize,
    verbose: bool,
) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    lzma_rs::xz_decompress(input, &mut out).map_err(|e| alloc::format!("xz: {}", e))?;
    if verbose {
        sys::write(alloc::format!("  [xz] {} → {} байт\n", packed, out.len()).as_bytes());
    }
    Ok(out)
}

/// Распаковать `.zst` (Веха 105.2) — основной формат современного кэша.
///
/// Поток, а не «весь вход в память»: `StreamingDecoder` тянет байты у источника сам, поэтому
/// сжатая копия в куче не собирается — то же свойство, что у пути xz.
pub fn unzstd<R: ruzstd::io::Read>(
    input: R,
    packed: usize,
    verbose: bool,
) -> Result<Vec<u8>, String> {
    let mut dec = ruzstd::decoding::StreamingDecoder::new(input)
        .map_err(|e| alloc::format!("zstd: {}", e))?;
    let mut out = Vec::new();
    ruzstd::io::Read::read_to_end(&mut dec, &mut out).map_err(|e| alloc::format!("zstd: {}", e))?;
    if verbose {
        sys::write(alloc::format!("  [zstd] {} → {} байт\n", packed, out.len()).as_bytes());
    }
    Ok(out)
}
