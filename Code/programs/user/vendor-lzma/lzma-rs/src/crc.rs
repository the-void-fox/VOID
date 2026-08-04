//! Замена крейта `crc` — CRC-32/ISO-HDLC и CRC-64/XZ, которыми xz проверяет свои поля.
//!
//! Обе величины отражённые (refin = refout = true), начинаются с единиц и заканчиваются
//! инверсией, поэтому движок один, а различаются только полиномом (в отражённом виде) и шириной.
//! Таблица на 256 входов считается в `const fn` на этапе компиляции — побайтовый вариант был бы
//! в 8 раз медленнее, а CRC64 здесь считается по ВСЕМУ распакованному блоку.
//!
//! Проверку можно было бы и выкинуть — распаковка от неё не зависит. Она остаётся сознательно:
//! архив приезжает из чужого кэша по сети, и «расползлось молча» — ровно тот исход, который в
//! этой системе считается недопустимым.

use core::fmt;

/// Разрядность CRC. Апстримный `crc` держит этот трейт ради дженериков в `CrcDigestRead`.
pub trait Width: Copy + fmt::Debug + 'static {}
impl Width for u32 {}
impl Width for u64 {}

/// Описание величины: отражённый полином, начальное значение, финальный XOR.
#[derive(Clone, Copy, Debug)]
pub struct Algorithm<W: Width> {
    pub poly_reflected: W,
    pub init: W,
    pub xorout: W,
}

/// CRC-32/ISO-HDLC: полином 0x04C11DB7, в отражённом виде 0xEDB88320.
pub const CRC_32_ISO_HDLC: Algorithm<u32> = Algorithm {
    poly_reflected: 0xEDB8_8320,
    init: 0xFFFF_FFFF,
    xorout: 0xFFFF_FFFF,
};

/// CRC-64/XZ: полином 0x42F0E1EBA9EA3693, в отражённом виде 0xC96C5795D7870F42.
pub const CRC_64_XZ: Algorithm<u64> = Algorithm {
    poly_reflected: 0xC96C_5795_D787_0F42,
    init: 0xFFFF_FFFF_FFFF_FFFF,
    xorout: 0xFFFF_FFFF_FFFF_FFFF,
};

/// Посчитанная таблица + параметры.
#[derive(Clone, Debug)]
pub struct Crc<W: Width> {
    algorithm: &'static Algorithm<W>,
    table: [W; 256],
}

/// Накопитель: `update` кормит байтами, `finalize` отдаёт результат.
#[derive(Clone, Debug)]
pub struct Digest<'a, W: Width> {
    crc: &'a Crc<W>,
    value: W,
}

macro_rules! crc_impl {
    ($w:ty, $table_fn:ident) => {
        const fn $table_fn(poly: $w) -> [$w; 256] {
            let mut table = [0; 256];
            let mut i = 0;
            while i < 256 {
                let mut v = i as $w;
                let mut bit = 0;
                while bit < 8 {
                    v = if v & 1 != 0 { (v >> 1) ^ poly } else { v >> 1 };
                    bit += 1;
                }
                table[i] = v;
                i += 1;
            }
            table
        }

        impl Crc<$w> {
            pub const fn new(algorithm: &'static Algorithm<$w>) -> Self {
                Crc {
                    algorithm,
                    table: $table_fn(algorithm.poly_reflected),
                }
            }

            pub fn digest(&self) -> Digest<'_, $w> {
                Digest {
                    crc: self,
                    value: self.algorithm.init,
                }
            }

            pub fn checksum(&self, data: &[u8]) -> $w {
                let mut d = self.digest();
                d.update(data);
                d.finalize()
            }
        }

        impl<'a> Digest<'a, $w> {
            pub fn update(&mut self, data: &[u8]) {
                for &b in data {
                    let idx = ((self.value ^ (b as $w)) & 0xFF) as usize;
                    self.value = (self.value >> 8) ^ self.crc.table[idx];
                }
            }

            pub fn finalize(self) -> $w {
                self.value ^ self.crc.algorithm.xorout
            }
        }
    };
}

crc_impl!(u32, table_u32);
crc_impl!(u64, table_u64);

#[cfg(test)]
mod tests {
    use super::*;

    /// Контрольные значения обеих величин на строке "123456789" — из каталога CRC RevEng.
    #[test]
    fn check_values() {
        const CRC32: Crc<u32> = Crc::<u32>::new(&CRC_32_ISO_HDLC);
        const CRC64: Crc<u64> = Crc::<u64>::new(&CRC_64_XZ);
        assert_eq!(CRC32.checksum(b"123456789"), 0xCBF4_3926);
        assert_eq!(CRC64.checksum(b"123456789"), 0x995D_C9BB_DF19_39FA);
    }

    /// Накопление кусками обязано совпадать с разовым подсчётом — на этом держится проверка
    /// заголовков xz, где под CRC попадают байты, приходящие порциями из читателя.
    #[test]
    fn chunked_matches_whole() {
        const CRC32: Crc<u32> = Crc::<u32>::new(&CRC_32_ISO_HDLC);
        let data = b"VOID: nar.xz -> store";
        let mut d = CRC32.digest();
        d.update(&data[..7]);
        d.update(&data[7..]);
        assert_eq!(d.finalize(), CRC32.checksum(data));
    }
}
