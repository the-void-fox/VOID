//! void-wpa — крипта WPA2-PSK (Веха 217, шаг 4 [[0021-wifi-own-stack]]).
//!
//! Четыре вещи, и все нужны, чтобы доказать точке знание пароля:
//!
//! | что | зачем |
//! |---|---|
//! | SHA-1 | на нём стоит весь WPA2-PSK; ничего современнее стандарт здесь не знает |
//! | HMAC-SHA1 | подпись, из которой собрано всё остальное |
//! | PBKDF2 | пароль + имя сети → парный мастер-ключ (4096 проходов) |
//! | PRF | мастер-ключ + случайные числа сторон → ключи сеанса |
//!
//! ## Почему отдельный крейт
//!
//! Потому что **правильность крипты доказывается только совпадением с эталоном**. Своя реализация
//! SHA-1 либо совпадает с опубликованными векторами побайтно, либо не работает вовсе — третьего
//! нет, и «вроде считает» здесь худший из возможных ответов: рукопожатие просто не сойдётся, а
//! искать причину будут в карте, в драйвере и в точке доступа.
//!
//! Векторы гоняются на ХОСТЕ, поэтому крейт стандалон со своим таргетом — как `vvsh-core`.
//!
//! ## Почему SHA-1 свой, а не крейтом
//!
//! В дереве вендорены `sha2`, `hmac`, `aes` (приехали с TLS-клиентом), а SHA-1 нет: TLS 1.3 его
//! не использует. Тянуть ради восьмидесяти строк ещё один чужой крейт в оффлайн-сборку дороже,
//! чем написать их — тем более что проверяются они теми же векторами, что и чужие.
//!
//! ## Чего здесь НЕТ
//!
//! Шифрования кадров: его делает сама карта (CCMP на борту). Хосту нужно лишь ВЫЧИСЛИТЬ ключи и
//! отдать их карте — этим и ограничен крейт.

#![cfg_attr(not(test), no_std)]

// ─── SHA-1 ──────────────────────────────────────────────────────────────────────────────────
//
// FIPS 180-4. Устарел для подписей и сломан для коллизий — но здесь он не подпись, а
// строительный блок, назначенный стандартом 802.11i, и заменить его нечем.

/// Длина свёртки SHA-1 в байтах.
pub const SHA1_LEN: usize = 20;
/// Размер блока SHA-1 — он же размер ключа HMAC после нормализации.
pub const SHA1_BLOCK: usize = 64;

/// Состояние SHA-1 поверх потока байтов.
pub struct Sha1 {
    h: [u32; 5],
    buf: [u8; SHA1_BLOCK],
    len: usize,
    total: u64,
}

impl Default for Sha1 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha1 {
    pub const fn new() -> Self {
        Sha1 {
            h: [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476, 0xc3d2_e1f0],
            buf: [0; SHA1_BLOCK],
            len: 0,
            total: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total += data.len() as u64;
        while !data.is_empty() {
            let n = (SHA1_BLOCK - self.len).min(data.len());
            self.buf[self.len..self.len + n].copy_from_slice(&data[..n]);
            self.len += n;
            data = &data[n..];
            if self.len == SHA1_BLOCK {
                self.block();
                self.len = 0;
            }
        }
    }

    /// Дописать хвост по правилу набивки и отдать свёртку.
    pub fn finish(mut self) -> [u8; SHA1_LEN] {
        let bits = self.total * 8;
        self.update(&[0x80]);
        // Длина занимает восемь последних байт блока; добиваем нулями до неё.
        while self.len != SHA1_BLOCK - 8 {
            self.update(&[0]);
        }
        self.buf[SHA1_BLOCK - 8..].copy_from_slice(&bits.to_be_bytes());
        self.block();
        let mut out = [0u8; SHA1_LEN];
        for (i, w) in self.h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
        }
        out
    }

    fn block(&mut self) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                self.buf[i * 4],
                self.buf[i * 4 + 1],
                self.buf[i * 4 + 2],
                self.buf[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = self.h;
        for (i, &wi) in w.iter().enumerate() {
            // Четыре четверти по двадцать шагов, у каждой своя функция и своя константа.
            let (f, k) = match i / 20 {
                0 => ((b & c) | (!b & d), 0x5a82_7999),
                1 => (b ^ c ^ d, 0x6ed9_eba1),
                2 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
                _ => (b ^ c ^ d, 0xca62_c1d6),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (dst, v) in self.h.iter_mut().zip([a, b, c, d, e]) {
            *dst = dst.wrapping_add(v);
        }
    }
}

/// Свёртка одним вызовом.
pub fn sha1(data: &[u8]) -> [u8; SHA1_LEN] {
    let mut h = Sha1::new();
    h.update(data);
    h.finish()
}

// ─── HMAC-SHA1 ──────────────────────────────────────────────────────────────────────────────

/// Подпись `data` ключом `key` (RFC 2104).
///
/// Ключ длиннее блока сворачивается, короче — добивается нулями. Это часть определения, а не
/// оптимизация: без неё подпись разойдётся с чужой на первом же длинном ключе.
pub fn hmac_sha1(key: &[u8], data: &[u8]) -> [u8; SHA1_LEN] {
    let mut k = [0u8; SHA1_BLOCK];
    if key.len() > SHA1_BLOCK {
        k[..SHA1_LEN].copy_from_slice(&sha1(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; SHA1_BLOCK];
    let mut opad = [0x5cu8; SHA1_BLOCK];
    for i in 0..SHA1_BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha1::new();
    inner.update(&ipad);
    inner.update(data);
    let inner = inner.finish();
    let mut outer = Sha1::new();
    outer.update(&opad);
    outer.update(&inner);
    outer.finish()
}

// ─── PBKDF2-HMAC-SHA1 ───────────────────────────────────────────────────────────────────────

/// Растянуть пароль в ключ (RFC 2898). Заполняет `out` целиком.
///
/// Медленность здесь — СВОЙСТВО, а не изъян: четыре тысячи проходов стоят человеку доли секунды
/// один раз за подключение, а перебирающему пароли — те же доли секунды на КАЖДЫЙ вариант.
pub fn pbkdf2_sha1(password: &[u8], salt: &[u8], rounds: u32, out: &mut [u8]) {
    let mut block = 1u32;
    let mut done = 0usize;
    while done < out.len() {
        // Первый проход считается от соли с номером блока, дальнейшие — от предыдущего ответа.
        let mut seed = [0u8; 64];
        let n = salt.len().min(seed.len() - 4);
        seed[..n].copy_from_slice(&salt[..n]);
        seed[n..n + 4].copy_from_slice(&block.to_be_bytes());
        let mut u = hmac_sha1(password, &seed[..n + 4]);
        let mut acc = u;
        for _ in 1..rounds {
            u = hmac_sha1(password, &u);
            for (a, b) in acc.iter_mut().zip(u.iter()) {
                *a ^= b;
            }
        }
        let take = (out.len() - done).min(SHA1_LEN);
        out[done..done + take].copy_from_slice(&acc[..take]);
        done += take;
        block += 1;
    }
}

/// Парный мастер-ключ сети: пароль и ИМЯ СЕТИ, 4096 проходов, 32 байта.
///
/// Имя сети здесь работает солью — поэтому один и тот же пароль в двух разных сетях даёт разные
/// ключи, и таблицу заранее посчитать можно только под конкретное имя.
pub fn pmk(passphrase: &[u8], ssid: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    pbkdf2_sha1(passphrase, ssid, 4096, &mut out);
    out
}

// ─── PRF ────────────────────────────────────────────────────────────────────────────────────

/// Псевдослучайная функция 802.11i: из ключа, надписи и данных — сколько угодно байт.
///
/// Устроена просто: подписываем `надпись || 0 || данные || номер` и склеиваем ответы, пока не
/// наберётся нужная длина. Проверять её отдельными векторами незачем — она тонкий слой над
/// [`hmac_sha1`], который сверен с RFC 2202; настоящая её проверка это сошедшееся рукопожатие.
pub fn prf(key: &[u8], label: &[u8], data: &[u8], out: &mut [u8]) {
    let mut i = 0u8;
    let mut done = 0usize;
    while done < out.len() {
        let mut m = [0u8; 128];
        let mut n = 0;
        for src in [label, &[0u8][..], data, &[i][..]] {
            let take = src.len().min(m.len() - n);
            m[n..n + take].copy_from_slice(&src[..take]);
            n += take;
        }
        let d = hmac_sha1(key, &m[..n]);
        let take = (out.len() - done).min(SHA1_LEN);
        out[done..done + take].copy_from_slice(&d[..take]);
        done += take;
        i += 1;
    }
}

/// Парный временный ключ сеанса: 48 байт, из которых карте нужны последние шестнадцать.
///
/// Порядок склейки задан стандартом и НЕ произволен: адреса и случайные числа идут от меньшего
/// к большему, чтобы обе стороны, считая независимо, получили одно и то же. Перепутать порядок —
/// значит получить разные ключи у станции и точки и не понять, почему подпись не сходится.
pub fn ptk(pmk: &[u8; 32], mac_a: &[u8; 6], mac_b: &[u8; 6], nonce_a: &[u8; 32], nonce_b: &[u8; 32]) -> [u8; 48] {
    let (m1, m2) = if mac_a <= mac_b { (mac_a, mac_b) } else { (mac_b, mac_a) };
    let (n1, n2) = if nonce_a <= nonce_b { (nonce_a, nonce_b) } else { (nonce_b, nonce_a) };
    let mut data = [0u8; 76];
    data[..6].copy_from_slice(m1);
    data[6..12].copy_from_slice(m2);
    data[12..44].copy_from_slice(n1);
    data[44..76].copy_from_slice(n2);
    let mut out = [0u8; 48];
    prf(pmk, b"Pairwise key expansion", &data, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    // ── SHA-1: векторы FIPS 180-2 ────────────────────────────────────────────

    #[test]
    fn sha1_совпадает_с_эталоном() {
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(hex(&sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            hex(&sha1(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }

    /// Миллион букв «a» — вектор, который ловит ошибки в СЧЁТЧИКЕ ДЛИНЫ и в набивке: на коротких
    /// входах они не проявляются вовсе.
    #[test]
    fn sha1_на_миллионе_байт() {
        let mut h = Sha1::new();
        for _ in 0..1000 {
            h.update(&[b'a'; 1000]);
        }
        assert_eq!(hex(&h.finish()), "34aa973cd4c4daa4f61eeb2bdbad27316534016f");
    }

    /// Порционная подача обязана дать то же, что и одним куском: иначе поток и одиночный вызов
    /// разойдутся, а поймается это на кадре неудачной длины.
    #[test]
    fn порции_не_меняют_свёртку() {
        let data: Vec<u8> = (0..300u32).map(|i| (i * 7) as u8).collect();
        let целиком = sha1(&data);
        for шаг in [1, 3, 64, 63, 65, 127] {
            let mut h = Sha1::new();
            for кусок in data.chunks(шаг) {
                h.update(кусок);
            }
            assert_eq!(h.finish(), целиком, "порции по {шаг} дали другую свёртку");
        }
    }

    // ── HMAC-SHA1: векторы RFC 2202 ──────────────────────────────────────────

    #[test]
    fn hmac_совпадает_с_эталоном() {
        assert_eq!(
            hex(&hmac_sha1(&[0x0b; 20], b"Hi There")),
            "b617318655057264e28bc0b6fb378c8ef146be00"
        );
        assert_eq!(
            hex(&hmac_sha1(b"Jefe", b"what do ya want for nothing?")),
            "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79"
        );
        assert_eq!(
            hex(&hmac_sha1(&[0xaa; 20], &[0xdd; 50])),
            "125d7342b9ac11cd91a39af48aa17b4f63f175d3"
        );
    }

    /// Ключ ДЛИННЕЕ блока сворачивается — это часть определения, а не оптимизация. Пароль Wi-Fi
    /// короткий, но PRF подписывает ключами всех длин, и ошибка здесь тихая.
    #[test]
    fn длинный_ключ_сворачивается() {
        assert_eq!(
            hex(&hmac_sha1(&[0xaa; 80], b"Test Using Larger Than Block-Size Key - Hash Key First")),
            "aa4ae5e15272d00e95705637ce8a3b55ed402112"
        );
    }

    // ── PBKDF2: векторы RFC 6070 ─────────────────────────────────────────────

    #[test]
    fn pbkdf2_совпадает_с_эталоном() {
        let mut o = [0u8; 20];
        pbkdf2_sha1(b"password", b"salt", 1, &mut o);
        assert_eq!(hex(&o), "0c60c80f961f0e71f3a9b524af6012062fe037a6");
        pbkdf2_sha1(b"password", b"salt", 2, &mut o);
        assert_eq!(hex(&o), "ea6c014dc72d6f8ccd1ed92ace1d41f0d8de8957");
        pbkdf2_sha1(b"password", b"salt", 4096, &mut o);
        assert_eq!(hex(&o), "4b007901b765489abead49d926f721d065a429c1");
    }

    /// Вывод длиннее одной свёртки: блоки склеиваются, и номер блока обязан входить в счёт.
    /// Ключ сети — ровно этот случай (32 байта из двадцатибайтных кусков).
    #[test]
    fn pbkdf2_склеивает_блоки() {
        let mut o = [0u8; 25];
        pbkdf2_sha1(b"passwordPASSWORDpassword", b"saltSALTsaltSALTsaltSALTsaltSALTsalt", 4096, &mut o);
        assert_eq!(hex(&o), "3d2eec4fe41c849b80c8d83662c0e44a8b291a964cf2f07038");
    }

    // ── ключ сети: векторы IEEE 802.11i ──────────────────────────────────────

    /// Эталон из стандарта: пароль `password`, сеть `IEEE`. Это и есть главная проверка крейта —
    /// если она сходится, значит пароль превращается в ключ ровно так же, как у всех остальных.
    #[test]
    fn ключ_сети_совпадает_с_эталоном() {
        assert_eq!(
            hex(&pmk(b"password", b"IEEE")),
            "f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e"
        );
    }

    #[test]
    fn имя_сети_меняет_ключ() {
        assert_ne!(pmk(b"password", b"IEEE"), pmk(b"password", b"OTHER"));
        assert_ne!(pmk(b"password", b"IEEE"), pmk(b"Password", b"IEEE"));
    }

    // ── ключи сеанса ─────────────────────────────────────────────────────────

    /// Обе стороны считают ключ НЕЗАВИСИМО и обязаны получить одно и то же — порядок склейки для
    /// того и задан «от меньшего к большему». Проверяем перестановкой аргументов: если бы порядок
    /// зависел от того, кто считает, станция и точка разошлись бы и подпись не сошлась.
    #[test]
    fn ключ_сеанса_не_зависит_от_того_кто_считает() {
        let k = pmk(b"password", b"IEEE");
        let (a, b) = ([0x02u8; 6], [0x0au8; 6]);
        let (na, nb) = ([0x11u8; 32], [0x22u8; 32]);
        assert_eq!(ptk(&k, &a, &b, &na, &nb), ptk(&k, &b, &a, &nb, &na));
    }

    #[test]
    fn ключ_сеанса_меняется_от_любого_входа() {
        let k = pmk(b"password", b"IEEE");
        let (a, b) = ([0x02u8; 6], [0x0au8; 6]);
        let (na, nb) = ([0x11u8; 32], [0x22u8; 32]);
        let базовый = ptk(&k, &a, &b, &na, &nb);
        assert_ne!(базовый, ptk(&pmk(b"other", b"IEEE"), &a, &b, &na, &nb));
        assert_ne!(базовый, ptk(&k, &[0x03; 6], &b, &na, &nb));
        assert_ne!(базовый, ptk(&k, &a, &b, &[0x12; 32], &nb));
    }

    #[test]
    fn prf_выдаёт_сколько_просят() {
        for n in [1usize, 16, 20, 21, 48, 64] {
            let mut o = vec![0u8; n];
            prf(&[0x0b; 20], b"prefix", b"data", &mut o);
            assert_eq!(o.len(), n);
            assert!(o.iter().any(|&b| b != 0), "{n} байт вышли нулями");
        }
    }
}
