//! Замена крейта `byteorder` — те четыре метода, которыми пользуется распаковщик.
//!
//! Отдельная зависимость здесь не нужна: `byteorder` в дереве VOID уже есть, но он тянется как
//! зависимость сетевого стека со своими фичами, а этот крейт должен собираться **без единой
//! зависимости** — иначе унификация фич однажды включит ему `std`, которого у нас нет.

use crate::io::{Read, Result};

/// Порядок байтов.
pub trait ByteOrder {
    fn u16(buf: [u8; 2]) -> u16;
    fn u32(buf: [u8; 4]) -> u32;
    fn u64(buf: [u8; 8]) -> u64;
}

#[derive(Clone, Copy, Debug)]
pub enum BigEndian {}

#[derive(Clone, Copy, Debug)]
pub enum LittleEndian {}

impl ByteOrder for BigEndian {
    fn u16(buf: [u8; 2]) -> u16 {
        u16::from_be_bytes(buf)
    }
    fn u32(buf: [u8; 4]) -> u32 {
        u32::from_be_bytes(buf)
    }
    fn u64(buf: [u8; 8]) -> u64 {
        u64::from_be_bytes(buf)
    }
}

impl ByteOrder for LittleEndian {
    fn u16(buf: [u8; 2]) -> u16 {
        u16::from_le_bytes(buf)
    }
    fn u32(buf: [u8; 4]) -> u32 {
        u32::from_le_bytes(buf)
    }
    fn u64(buf: [u8; 8]) -> u64 {
        u64::from_le_bytes(buf)
    }
}

/// Чтение целых из потока. Нехватка байтов — `UnexpectedEof` от `read_exact`, как в апстриме:
/// декодер на этом различает «поток кончился» и «поток испорчен».
pub trait ReadBytesExt: Read {
    fn read_u8(&mut self) -> Result<u8> {
        let mut b = [0u8; 1];
        self.read_exact(&mut b)?;
        Ok(b[0])
    }

    fn read_u16<T: ByteOrder>(&mut self) -> Result<u16> {
        let mut b = [0u8; 2];
        self.read_exact(&mut b)?;
        Ok(T::u16(b))
    }

    fn read_u32<T: ByteOrder>(&mut self) -> Result<u32> {
        let mut b = [0u8; 4];
        self.read_exact(&mut b)?;
        Ok(T::u32(b))
    }

    fn read_u64<T: ByteOrder>(&mut self) -> Result<u64> {
        let mut b = [0u8; 8];
        self.read_exact(&mut b)?;
        Ok(T::u64(b))
    }
}

impl<R: Read + ?Sized> ReadBytesExt for R {}
