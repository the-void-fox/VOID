//! Замена `std::io` для VOID — ровно та часть, которой пользуется распаковщик.
//!
//! Это НЕ «свой io для системы»: у VOID нет ни файлов, ни дескрипторов, а вход и выход
//! распаковки — слайс и `Vec<u8>`. Шим существует только затем, чтобы не переписывать 3000 строк
//! чужого декодера ради смены типа читателя. Тот же приём, которым уже вендорены smoltcp, rustls
//! и ereb: подменить границу, а не тело.
//!
//! Семантика умышленно повторяет `std`, включая мелочи, от которых зависит результат:
//! ёмкость `BufReader` = 8192 (по ней декодер xz считает, сколько байт попало под CRC заголовка),
//! `Take::fill_buf` обрезает по остатку лимита, `read_exact` при нехватке даёт `UnexpectedEof`.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use core::cmp;
use core::fmt;

/// Разновидность ошибки — из `std::io::ErrorKind` нужны только эти.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    UnexpectedEof,
    WriteZero,
    InvalidData,
    InvalidInput,
    Other,
}

/// Ошибка ввода-вывода: вид + текст (текст попадает в сообщения декодера, поэтому он есть).
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    msg: String,
}

impl Error {
    pub fn new(kind: ErrorKind, msg: impl Into<String>) -> Self {
        Error {
            kind,
            msg: msg.into(),
        }
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

pub type Result<T> = core::result::Result<T, Error>;

fn eof() -> Error {
    Error::new(ErrorKind::UnexpectedEof, "failed to fill whole buffer")
}

/// Источник байтов.
pub trait Read {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize>;

    fn read_exact(&mut self, mut buf: &mut [u8]) -> Result<()> {
        while !buf.is_empty() {
            match self.read(buf)? {
                0 => return Err(eof()),
                n => {
                    let tmp = buf;
                    buf = &mut tmp[n..];
                }
            }
        }
        Ok(())
    }

    fn take(self, limit: u64) -> Take<Self>
    where
        Self: Sized,
    {
        Take { inner: self, limit }
    }
}

/// Источник, умеющий показать уже прочитанное окно, не забирая его.
pub trait BufRead: Read {
    fn fill_buf(&mut self) -> Result<&[u8]>;
    fn consume(&mut self, amt: usize);
}

/// Приёмник байтов.
pub trait Write {
    fn write(&mut self, buf: &[u8]) -> Result<usize>;

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    fn write_all(&mut self, mut buf: &[u8]) -> Result<()> {
        while !buf.is_empty() {
            match self.write(buf)? {
                0 => {
                    return Err(Error::new(
                        ErrorKind::WriteZero,
                        "failed to write whole buffer",
                    ))
                }
                n => buf = &buf[n..],
            }
        }
        Ok(())
    }
}

// Ссылка на читателя/писателя — тоже читатель/писатель: без этих трёх реализаций не работает
// `input.take(...)`, где `input: &mut R` (метод забирает `self` по значению).
impl<R: Read + ?Sized> Read for &mut R {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        (**self).read(buf)
    }
}

impl<R: BufRead + ?Sized> BufRead for &mut R {
    fn fill_buf(&mut self) -> Result<&[u8]> {
        (**self).fill_buf()
    }

    fn consume(&mut self, amt: usize) {
        (**self).consume(amt)
    }
}

impl<W: Write + ?Sized> Write for &mut W {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        (**self).write(buf)
    }

    fn flush(&mut self) -> Result<()> {
        (**self).flush()
    }
}

impl Read for &[u8] {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let n = cmp::min(buf.len(), self.len());
        buf[..n].copy_from_slice(&self[..n]);
        *self = &self[n..];
        Ok(n)
    }
}

impl BufRead for &[u8] {
    fn fill_buf(&mut self) -> Result<&[u8]> {
        Ok(*self)
    }

    fn consume(&mut self, amt: usize) {
        *self = &self[amt..];
    }
}

impl Write for alloc::vec::Vec<u8> {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        self.extend_from_slice(buf);
        Ok(buf.len())
    }
}

/// Читатель, ограниченный `limit` байтами (`std::io::Take`).
#[derive(Debug)]
pub struct Take<R> {
    inner: R,
    limit: u64,
}

impl<R: Read> Read for Take<R> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        if self.limit == 0 {
            return Ok(0);
        }
        let max = cmp::min(buf.len() as u64, self.limit) as usize;
        let n = self.inner.read(&mut buf[..max])?;
        self.limit -= n as u64;
        Ok(n)
    }
}

impl<R: BufRead> BufRead for Take<R> {
    fn fill_buf(&mut self) -> Result<&[u8]> {
        if self.limit == 0 {
            return Ok(&[]);
        }
        let buf = self.inner.fill_buf()?;
        let cap = cmp::min(buf.len() as u64, self.limit) as usize;
        Ok(&buf[..cap])
    }

    fn consume(&mut self, amt: usize) {
        let amt = cmp::min(amt as u64, self.limit) as usize;
        self.limit -= amt as u64;
        self.inner.consume(amt);
    }
}

/// Курсор по буферу в памяти (`std::io::Cursor`).
#[derive(Clone, Debug)]
pub struct Cursor<T> {
    inner: T,
    pos: u64,
}

impl<T> Cursor<T> {
    pub fn new(inner: T) -> Self {
        Cursor { inner, pos: 0 }
    }

    pub fn position(&self) -> u64 {
        self.pos
    }

    pub fn set_position(&mut self, pos: u64) {
        self.pos = pos;
    }

    pub fn get_ref(&self) -> &T {
        &self.inner
    }

    pub fn get_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T: AsRef<[u8]>> Cursor<T> {
    fn remaining(&self) -> &[u8] {
        let all = self.inner.as_ref();
        let pos = cmp::min(self.pos, all.len() as u64) as usize;
        &all[pos..]
    }
}

impl<T: AsRef<[u8]>> Read for Cursor<T> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let n = {
            let src = self.remaining();
            let n = cmp::min(buf.len(), src.len());
            buf[..n].copy_from_slice(&src[..n]);
            n
        };
        self.pos += n as u64;
        Ok(n)
    }
}

impl<T: AsRef<[u8]>> BufRead for Cursor<T> {
    fn fill_buf(&mut self) -> Result<&[u8]> {
        Ok(self.remaining())
    }

    fn consume(&mut self, amt: usize) {
        self.pos += amt as u64;
    }
}

/// Ёмкость буфера — как у `std::io::BufReader`. Число значимо: по нему совпадает объём
/// упреждающего чтения, а от него зависит, какие байты попадут под CRC заголовка блока xz.
const BUF_CAPACITY: usize = 8192;

/// Буферизующий читатель (`std::io::BufReader`) — превращает `Read` в `BufRead`.
#[derive(Debug)]
pub struct BufReader<R> {
    inner: R,
    buf: Box<[u8]>,
    pos: usize,
    cap: usize,
}

impl<R: Read> BufReader<R> {
    pub fn new(inner: R) -> Self {
        BufReader {
            inner,
            buf: vec![0u8; BUF_CAPACITY].into_boxed_slice(),
            pos: 0,
            cap: 0,
        }
    }
}

impl<R: Read> Read for BufReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        // Пустой буфер и запрос больше него — читаем мимо буфера, как это делает std.
        if self.pos == self.cap && buf.len() >= self.buf.len() {
            return self.inner.read(buf);
        }
        let n = {
            let src = self.fill_buf()?;
            let n = cmp::min(buf.len(), src.len());
            buf[..n].copy_from_slice(&src[..n]);
            n
        };
        self.consume(n);
        Ok(n)
    }
}

impl<R: Read> BufRead for BufReader<R> {
    fn fill_buf(&mut self) -> Result<&[u8]> {
        if self.pos >= self.cap {
            self.cap = self.inner.read(&mut self.buf)?;
            self.pos = 0;
        }
        Ok(&self.buf[self.pos..self.cap])
    }

    fn consume(&mut self, amt: usize) {
        self.pos = cmp::min(self.pos + amt, self.cap);
    }
}
